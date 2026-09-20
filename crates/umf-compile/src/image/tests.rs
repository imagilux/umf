//! Unit tests for the `image` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::partition::PartitionView;
use fatfs::{FileSystem, FsOptions};
use std::fs::OpenOptions;
use std::io::{Read, Seek};
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

    let report = compile_image(&layout, reference, &out, small_geometry(), Some(&efi), None)
        .expect("compile");

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
    let err = compile_image(&layout, reference, &out, small_geometry(), None, None).unwrap_err();
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
        None,
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

/// A `rootfs.fs` naming a filesystem UMF cannot write is still rejected —
/// but by *name*, not by "anything other than squashfs". The label is the
/// image's only statement about its root filesystem, so quietly substituting
/// the default would project a disk the operator never asked for, under a
/// cmdline claiming it was theirs.
#[test]
fn compile_image_rejects_an_unwritable_rootfs_fs_label() {
    let dir = tempdir().expect("dir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let reference = "example.invalid/btrfs-rootfs:1";
    emit_bootable_custom(&layout, reference, &[(label::ROOTFS_FS, "btrfs")]);
    let result = compile_to_tmp(&layout, reference, dir.path());
    let Err(CompileError::UnsupportedRootfsFs { value, supported }) = result else {
        panic!("an unwritable rootfs.fs must be rejected, got {result:?}");
    };
    assert_eq!(value, "btrfs");
    for fs in RootfsFs::ALL {
        assert!(
            supported.contains(fs.as_str()),
            "the error should list {fs}: {supported}",
        );
    }
}

// ── Root-filesystem selection ───────────────────────────────────────────────

/// `--fs` beats the label, the label beats the default, and the default is
/// squashfs.
///
/// The precedence runs that way because the filesystem is a property of the
/// *disk*, not of the image: the layers are byte-identical whichever one is
/// written, so the same bootable image must be projectable to squashfs on one
/// node and ext4 on another. The label is the build's recorded default, which
/// is what keeps images built before `--fs` existed projecting exactly as they
/// always did.
#[test]
fn the_fs_flag_overrides_the_label_which_overrides_the_default() {
    let squashfs = "squashfs".to_string();
    let ext4 = "ext4".to_string();

    // No flag, no label ⇒ the in-process default.
    assert_eq!(
        resolve_rootfs_fs(None, None).expect("default"),
        RootfsFs::Squashfs,
    );
    // No flag, a label ⇒ the label.
    assert_eq!(
        resolve_rootfs_fs(None, Some(&ext4)).expect("label"),
        RootfsFs::Ext4,
    );
    // A flag always wins — including over a label that disagrees, which is
    // the entire point of the flag.
    assert_eq!(
        resolve_rootfs_fs(Some(RootfsFs::Erofs), Some(&squashfs)).expect("override"),
        RootfsFs::Erofs,
    );
    // ...and including over a label naming something unwritable: the operator
    // has told us exactly what to write, so the image's broken default is
    // no longer load-bearing.
    let bogus = "btrfs".to_string();
    assert_eq!(
        resolve_rootfs_fs(Some(RootfsFs::Ext4), Some(&bogus)).expect("override wins"),
        RootfsFs::Ext4,
    );
}

/// Every filesystem is reachable through the flag. A `--fs` value that parsed
/// but then resolved to something else would produce a disk silently unlike
/// the one requested.
#[test]
fn every_filesystem_is_selectable_through_the_flag() {
    for fs in RootfsFs::ALL {
        assert_eq!(
            resolve_rootfs_fs(Some(fs), None).expect("override"),
            fs,
            "--fs {fs} must resolve to {fs}",
        );
    }
}

/// The explicit `rootfs.fs=squashfs` label still compiles, and still picks
/// squashfs — the historical behaviour, unchanged by making the value set
/// wider.
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

/// End to end, per filesystem: the bytes in the ROOTFS partition and the
/// `rootfstype=` on the loader entry must name the **same** filesystem.
///
/// This is the invariant the whole design rests on. `umf compile` is the
/// single writer of both, precisely so they cannot disagree — and if they ever
/// did, the disk would carry one filesystem while telling the kernel (and the
/// initramfs, which reads `rootfstype=` back) to mount another. Nothing else
/// in the build would notice: the image is valid, the disk is well-formed, and
/// it simply fails to boot.
#[test]
fn the_partition_bytes_and_the_cmdline_name_the_same_filesystem() {
    for fs in RootfsFs::ALL {
        let Some(()) = mkfs_available_or_skip(fs) else {
            continue;
        };
        let dir = tempdir().expect("dir");
        let layout = ImageLayout::init(dir.path()).expect("layout");
        let reference = "example.invalid/fs-agreement:1";
        emit_bootable_custom(&layout, reference, &[]);

        let efi = dir.path().join("fake.efi");
        std::fs::write(&efi, fake_efi()).unwrap();
        let out = dir.path().join("disk.img");
        let report = compile_image(
            &layout,
            reference,
            &out,
            small_geometry(),
            Some(&efi),
            Some(fs),
        )
        .unwrap_or_else(|e| panic!("compile with --fs {fs}: {e}"));

        assert_eq!(report.rootfs_fs, fs, "report must name the filesystem used");

        // 1. The ROOTFS partition carries that filesystem's superblock.
        let disk = std::fs::File::open(&out).expect("open disk");
        let mut view = PartitionView::new(
            disk,
            report.projection.rootfs_start_bytes,
            report.projection.rootfs_size_bytes,
            "ROOTFS",
        );
        let (offset, magic) = crate::filesystem::tests::superblock_magic(fs);
        let mut head = vec![0u8; offset + magic.len()];
        view.seek(std::io::SeekFrom::Start(0)).expect("seek");
        view.read_exact(&mut head).expect("read rootfs head");
        assert_eq!(
            &head[offset..],
            magic,
            "--fs {fs} must write a {fs} superblock into the ROOTFS partition",
        );

        // 2. The loader entry's cmdline names the same one, and no other.
        let disk = std::fs::File::open(&out).expect("open disk");
        let esp = PartitionView::new(
            disk,
            report.projection.esp_start_bytes,
            report.projection.esp_size_bytes,
            "ESP",
        );
        let esp_fs = FileSystem::new(esp, FsOptions::new()).expect("esp fat");
        let mut entry = esp_fs
            .root_dir()
            .open_dir("loader")
            .expect("loader")
            .open_dir("entries")
            .expect("entries")
            .open_file("umf.conf")
            .expect("umf.conf");
        let mut conf = String::new();
        entry.read_to_string(&mut conf).expect("read entry");
        assert!(
            conf.contains(&format!("rootfstype={fs}")),
            "--fs {fs} must put rootfstype={fs} on the cmdline: {conf}",
        );
        for other in RootfsFs::ALL {
            if other != fs {
                assert!(
                    !conf.contains(&format!("rootfstype={other}")),
                    "cmdline for {fs} must not also name {other}: {conf}",
                );
            }
        }
    }
}

/// Skip (loudly) when the host lacks the tool; `UMF_REQUIRE_MKFS=1` turns the
/// skip into a failure so a CI lane cannot report green having exercised only
/// the in-process writer.
fn mkfs_available_or_skip(fs: RootfsFs) -> Option<()> {
    let Some(tool) = fs.host_mkfs() else {
        return Some(());
    };
    if crate::filesystem::tests::tool_is_available(tool) {
        return Some(());
    }
    assert!(
        !std::env::var("UMF_REQUIRE_MKFS")
            .is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes")),
        "UMF_REQUIRE_MKFS is set but `{tool}` is missing — this lane is supposed to \
         exercise the {fs} path end to end and would otherwise report green having \
         projected only squashfs",
    );
    eprintln!("skipping end-to-end {fs} projection: `{tool}` not on PATH");
    None
}

/// Lead from the audit backlog (#40): the `BootloaderUnavailable` remedies
/// must be things that exist.
///
/// It previously read *"install systemd-boot on the host, or pass
/// --bootloader-path"*. `resolve_bootloader`'s own doc comment says there is
/// **no host fallback** — the disk has to be reproducible from the image
/// alone — and that the override argument is a library test seam with no CLI
/// flag behind it. An operator who hit this error did two things that could
/// not help and then searched `--help` for a flag that was never there.
#[test]
fn the_missing_bootloader_error_offers_only_remedies_that_exist() {
    let err = CompileError::BootloaderUnavailable {
        kind: "systemd-boot".to_string(),
        tried: "usr/lib/systemd/boot/efi/systemd-bootx64.efi (in image)".to_string(),
    };
    let text = err.to_string();

    assert!(
        !text.contains("--bootloader-path"),
        "names a CLI flag that does not exist: {text}",
    );
    assert!(
        !text.contains("on the host"),
        "offers a host fallback the resolver explicitly does not have: {text}",
    );
    // The two real ways out.
    assert!(
        text.contains("image rootfs"),
        "must point at the in-image bootloader path: {text}",
    );
    assert!(text.contains("uki"), "must offer the uki flavor: {text}");
}
