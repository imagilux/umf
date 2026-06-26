//! Unit tests for the `image` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::partition::PartitionView;
use fatfs::{FileSystem, FsOptions};
use std::fs::OpenOptions;
use std::io::Read;
use tempfile::tempdir;
use umf_oci::image::{ContainerConfig, ImageConfig, LayerSource, emit_image};

fn fake_efi() -> Vec<u8> {
    let mut b = vec![b'M', b'Z', 0x90, 0x00];
    b.extend_from_slice(&[0u8; 252]);
    b
}

fn small_geometry() -> DiskGeometry {
    DiskGeometry {
        disk_size_bytes: 256 * 1024 * 1024,
        esp_size_bytes: 64 * 1024 * 1024,
    }
}

/// Emit a bootable-OS image into `layout`: a rootfs layer carrying
/// `boot/vmlinuz-<rel>` plus the `type=bootable` boot manifest.
fn emit_bootable(layout: &ImageLayout, reference: &str, release: &str) {
    let rootfs = tempdir().expect("rootfs dir");
    let boot = rootfs.path().join("boot");
    std::fs::create_dir_all(&boot).unwrap();
    std::fs::write(
        boot.join(format!("vmlinuz-{release}")),
        b"fake-kernel-image",
    )
    .unwrap();

    let mut labels = BTreeMap::new();
    labels.insert(label::TYPE.to_string(), "bootable".to_string());
    labels.insert(label::ENTRYPOINT.to_string(), "appliance".to_string());
    labels.insert(label::KERNEL_RELEASE.to_string(), release.to_string());
    labels.insert(
        label::KERNEL_VMLINUZ.to_string(),
        format!("/boot/vmlinuz-{release}"),
    );
    labels.insert(label::KERNEL_CMDLINE.to_string(), "init=/myapp".to_string());
    labels.insert(label::ROOTFS_FS.to_string(), "squashfs".to_string());
    labels.insert(label::FLAVOR.to_string(), "systemd-boot".to_string());

    let layer = LayerSource::from_directory(rootfs.path()).expect("layer");
    let config = ImageConfig {
        architecture: "amd64".to_string(),
        os: "linux".to_string(),
        umf_type: L0Kind::Bootable,
        container: ContainerConfig {
            labels,
            ..ContainerConfig::default()
        },
        ..ImageConfig::default()
    };
    emit_image(layout, &[layer], &config, reference).expect("emit");
}

#[test]
fn compile_image_projects_a_bootable_disk() {
    let dir = tempdir().expect("dir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let reference = "example.invalid/bootable:test";
    emit_bootable(&layout, reference, "7.0");

    let efi = dir.path().join("fake.efi");
    std::fs::write(&efi, fake_efi()).unwrap();
    let out = dir.path().join("disk.img");

    let report =
        compile_image(&layout, reference, &out, small_geometry(), Some(&efi)).expect("compile");

    assert_eq!(report.flavor, "systemd-boot");
    assert_eq!(report.entrypoint, "appliance");
    assert!(!report.source_digest.is_empty());

    let bytes = std::fs::read(&out).expect("read disk");
    assert_eq!(&bytes[510..512], &[0x55, 0xAA], "protective MBR missing");
    assert_eq!(&bytes[512..520], b"EFI PART", "GPT signature missing");

    // The kernel landed on the ESP.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&out)
        .expect("reopen");
    let view = PartitionView::new(
        file,
        report.projection.esp_start_bytes,
        report.projection.esp_size_bytes,
        "ESP",
    );
    let fs = FileSystem::new(view, FsOptions::new()).expect("mount esp");
    let mut vmlinuz = fs
        .root_dir()
        .open_file("vmlinuz-7.0")
        .expect("vmlinuz on ESP");
    let mut vbuf = Vec::new();
    vmlinuz.read_to_end(&mut vbuf).expect("read vmlinuz");
    assert_eq!(vbuf, b"fake-kernel-image");
}

#[test]
fn resolve_bootloader_uses_in_image_bootloader() {
    // The classic bootloader is read from the rootfs when no override is
    // given (there is no host fallback — the image must ship its own).
    let rootfs = tempdir().expect("rootfs");
    let arch = Architecture::from_arch_str("amd64").expect("arch");
    let efi_dir = rootfs.path().join("usr/lib/systemd/boot/efi");
    std::fs::create_dir_all(&efi_dir).unwrap();
    std::fs::write(efi_dir.join(arch.systemd_boot_filename()), b"in-image-efi").unwrap();

    let bytes = resolve_bootloader(arch, None, rootfs.path()).expect("resolve");
    assert_eq!(bytes, b"in-image-efi");
}

#[test]
fn resolve_bootloader_override_beats_in_image() {
    let rootfs = tempdir().expect("rootfs");
    let arch = Architecture::from_arch_str("amd64").expect("arch");
    let efi_dir = rootfs.path().join("usr/lib/systemd/boot/efi");
    std::fs::create_dir_all(&efi_dir).unwrap();
    std::fs::write(efi_dir.join(arch.systemd_boot_filename()), b"in-image-efi").unwrap();
    let override_efi = rootfs.path().join("override.efi");
    std::fs::write(&override_efi, b"override-efi").unwrap();

    let bytes = resolve_bootloader(arch, Some(&override_efi), rootfs.path()).expect("resolve");
    assert_eq!(bytes, b"override-efi");
}

#[test]
fn resolve_bootloader_in_image_symlink_escape_is_not_followed() {
    // SECURITY: an in-image bootloader that's a symlink pointing outside the
    // rootfs must not be read (it could leak a host file onto the disk). It
    // is refused; with no host fallback, the resolve errors.
    let outside = tempdir().expect("outside");
    let secret = outside.path().join("host-secret.efi");
    std::fs::write(&secret, b"host-secret").unwrap();

    let rootfs = tempdir().expect("rootfs");
    let arch = Architecture::from_arch_str("amd64").expect("arch");
    let efi_dir = rootfs.path().join("usr/lib/systemd/boot/efi");
    std::fs::create_dir_all(&efi_dir).unwrap();
    std::os::unix::fs::symlink(&secret, efi_dir.join(arch.systemd_boot_filename())).unwrap();

    let result = resolve_bootloader(arch, None, rootfs.path());
    // The escaped secret is never returned; with no host fallback this is a
    // BootloaderUnavailable error.
    assert!(
        matches!(result, Err(CompileError::BootloaderUnavailable { .. })),
        "symlink escape must be refused (no host fallback): {result:?}"
    );
}

#[test]
fn compile_image_rejects_non_bootable() {
    let dir = tempdir().expect("dir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let reference = "example.invalid/container:test";

    let mut labels = BTreeMap::new();
    labels.insert(label::TYPE.to_string(), "container".to_string());
    let config = ImageConfig {
        umf_type: L0Kind::Container,
        container: ContainerConfig {
            labels,
            ..ContainerConfig::default()
        },
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &config, reference).expect("emit");

    let out = dir.path().join("disk.img");
    let err = compile_image(&layout, reference, &out, small_geometry(), None).unwrap_err();
    assert!(
        matches!(err, CompileError::NotBootable { .. }),
        "got {err:?}"
    );
}

#[test]
fn compile_image_missing_reference_is_an_error() {
    let dir = tempdir().expect("dir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let out = dir.path().join("disk.img");
    let err = compile_image(
        &layout,
        "example.invalid/absent:1",
        &out,
        small_geometry(),
        None,
    )
    .unwrap_err();
    assert!(matches!(err, CompileError::Oci(_)), "got {err:?}");
}

/// Emit a `type=bootable` image into `layout` from `rootfs` (the test's own
/// layer tree), plus the standard boot manifest with caller-supplied label
/// overrides — lets a test inject a malicious value.
fn seed_custom_image(
    layout: &ImageLayout,
    reference: &str,
    rootfs: &std::path::Path,
    overrides: &[(&str, &str)],
) {
    let mut labels = std::collections::BTreeMap::new();
    labels.insert(label::TYPE.to_string(), "bootable".to_string());
    labels.insert(label::ENTRYPOINT.to_string(), "appliance".to_string());
    labels.insert(label::KERNEL_RELEASE.to_string(), "7.0".to_string());
    labels.insert(
        label::KERNEL_VMLINUZ.to_string(),
        "/boot/vmlinuz-7.0".to_string(),
    );
    labels.insert(label::FLAVOR.to_string(), "systemd-boot".to_string());
    for (k, v) in overrides {
        labels.insert((*k).to_string(), (*v).to_string());
    }
    let layer = LayerSource::from_directory(rootfs).expect("layer");
    let config = ImageConfig {
        architecture: "amd64".to_string(),
        os: "linux".to_string(),
        umf_type: L0Kind::Bootable,
        container: ContainerConfig {
            labels,
            ..ContainerConfig::default()
        },
        ..ImageConfig::default()
    };
    emit_image(layout, &[layer], &config, reference).expect("emit");
}

/// `seed_custom_image` over a benign rootfs that carries a real
/// `boot/vmlinuz-7.0`.
fn emit_bootable_custom(layout: &ImageLayout, reference: &str, overrides: &[(&str, &str)]) {
    let rootfs = tempdir().expect("rootfs");
    std::fs::create_dir_all(rootfs.path().join("boot")).unwrap();
    std::fs::write(rootfs.path().join("boot/vmlinuz-7.0"), b"kernel").unwrap();
    seed_custom_image(layout, reference, rootfs.path(), overrides);
}

fn compile_to_tmp(
    layout: &ImageLayout,
    reference: &str,
    dir: &std::path::Path,
) -> Result<CompileReport, CompileError> {
    let efi = dir.join("fake.efi");
    std::fs::write(&efi, fake_efi()).unwrap();
    compile_image(
        layout,
        reference,
        &dir.join("disk.img"),
        small_geometry(),
        Some(&efi),
    )
}

/// SECURITY (regression for the confirmed exploit): a `kernel.vmlinuz` label
/// that traverses out of the rootfs via `..` would otherwise copy a HOST
/// file onto the ESP. It must be rejected, not followed.
#[test]
fn compile_image_rejects_vmlinuz_label_traversal() {
    let dir = tempdir().expect("dir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let reference = "example.invalid/evil-traversal:1";
    emit_bootable_custom(
        &layout,
        reference,
        &[(
            label::KERNEL_VMLINUZ,
            "../../../../../../../../../../../../etc/hostname",
        )],
    );
    let result = compile_to_tmp(&layout, reference, dir.path());
    assert!(
        matches!(result, Err(CompileError::UnsafeLabelPath { .. })),
        "vmlinuz `..` traversal must be rejected, got {result:?}"
    );
}

/// SECURITY: the symlink route — a layer planting `boot/evil -> /etc/hostname`
/// and a `kernel.vmlinuz` pointing at it (no `..`, so containment relies on
/// canonicalization) — must also be rejected.
#[test]
fn compile_image_rejects_vmlinuz_symlink_escape() {
    let dir = tempdir().expect("dir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let reference = "example.invalid/evil-symlink:1";

    let rootfs = tempdir().expect("rootfs");
    std::fs::create_dir_all(rootfs.path().join("boot")).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", rootfs.path().join("boot/evil")).unwrap();
    seed_custom_image(
        &layout,
        reference,
        rootfs.path(),
        &[(label::KERNEL_VMLINUZ, "/boot/evil")],
    );

    let result = compile_to_tmp(&layout, reference, dir.path());
    assert!(
        matches!(result, Err(CompileError::UnsafeLabelPath { .. })),
        "vmlinuz symlink escape must be rejected, got {result:?}"
    );
}

/// SECURITY: a `kernel.cmdline` carrying a newline would inject extra
/// systemd-boot directives into the loader entry — must be rejected.
#[test]
fn compile_image_rejects_cmdline_control_chars() {
    let dir = tempdir().expect("dir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let reference = "example.invalid/evil-cmdline:1";
    emit_bootable_custom(
        &layout,
        reference,
        &[(label::KERNEL_CMDLINE, "quiet\nlinux /EFI/BOOT/evil.efi")],
    );
    let result = compile_to_tmp(&layout, reference, dir.path());
    assert!(
        matches!(result, Err(CompileError::UnsafeLabelValue { .. })),
        "cmdline newline injection must be rejected, got {result:?}"
    );
}

/// The legitimate path still works: a well-formed `kernel.vmlinuz` inside the
/// rootfs compiles fine (guards reject escapes, not normal use).
#[test]
fn compile_image_accepts_in_rootfs_vmlinuz() {
    let dir = tempdir().expect("dir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let reference = "example.invalid/good:1";
    emit_bootable_custom(&layout, reference, &[]);
    let result = compile_to_tmp(&layout, reference, dir.path());
    assert!(
        result.is_ok(),
        "well-formed bootable image must compile, got {result:?}"
    );
}

/// Regression: an image declaring a non-squashfs `rootfs.fs` must be
/// rejected — the projector only writes squashfs, and silently compiling
/// `ext4` to squashfs would misrepresent the on-disk filesystem.
#[test]
fn compile_image_rejects_non_squashfs_rootfs_fs() {
    let dir = tempdir().expect("dir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let reference = "example.invalid/ext4-rootfs:1";
    emit_bootable_custom(&layout, reference, &[(label::ROOTFS_FS, "ext4")]);
    let result = compile_to_tmp(&layout, reference, dir.path());
    assert!(
        matches!(result, Err(CompileError::Io(_))),
        "non-squashfs rootfs.fs must be rejected, got {result:?}"
    );
}

/// The explicit `rootfs.fs=squashfs` label still compiles (the guard rejects
/// only the unimplemented filesystems, not the supported one).
#[test]
fn compile_image_accepts_explicit_squashfs_rootfs_fs() {
    let dir = tempdir().expect("dir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let reference = "example.invalid/squashfs-rootfs:1";
    emit_bootable_custom(&layout, reference, &[(label::ROOTFS_FS, "squashfs")]);
    let result = compile_to_tmp(&layout, reference, dir.path());
    assert!(
        result.is_ok(),
        "explicit squashfs rootfs.fs must compile, got {result:?}"
    );
}

/// SECURITY (regression for the boot-entry filename-injection finding): a
/// `kernel.vmlinuz` pointing at a real file whose *name* carries a newline
/// must be rejected. The basename is interpolated into `linux /<name>` in
/// the loader entry, so a newline would inject an extra directive line
/// (e.g. `options nokaslr`). `rootfs_subpath` contains the path but not the
/// filename charset, so this is caught by the filename guard.
#[test]
fn compile_image_rejects_vmlinuz_filename_control_chars() {
    let dir = tempdir().expect("dir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let reference = "example.invalid/evil-filename:1";

    // A real file inside the rootfs whose name carries a newline (Linux
    // permits any byte but `/` and NUL in a filename).
    let rootfs = tempdir().expect("rootfs");
    std::fs::create_dir_all(rootfs.path().join("boot")).unwrap();
    let evil_name = "vmlinuz\noptions nokaslr";
    std::fs::write(rootfs.path().join("boot").join(evil_name), b"x").unwrap();
    seed_custom_image(
        &layout,
        reference,
        rootfs.path(),
        &[(label::KERNEL_VMLINUZ, "/boot/vmlinuz\noptions nokaslr")],
    );

    let result = compile_to_tmp(&layout, reference, dir.path());
    assert!(
        matches!(result, Err(CompileError::UnsafeLabelValue { .. })),
        "vmlinuz filename with a control char must be rejected, got {result:?}"
    );
}
