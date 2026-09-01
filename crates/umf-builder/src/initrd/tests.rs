//! Unit tests for the `initrd` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

fn seed_busybox_shaped_staging(release: &str) -> BuildStaging {
    let staging = BuildStaging::new().expect("staging");
    let root = staging.path();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::write(root.join("bin/busybox"), b"#fake-busybox-ELF").unwrap();
    // Modules tree.
    let modules_dir = root
        .join("lib")
        .join("modules")
        .join(release)
        .join("kernel");
    std::fs::create_dir_all(modules_dir.join("drivers/block")).unwrap();
    std::fs::create_dir_all(modules_dir.join("drivers/virtio")).unwrap();
    std::fs::create_dir_all(modules_dir.join("fs/squashfs")).unwrap();
    std::fs::write(modules_dir.join("drivers/block/virtio_blk.ko"), b"VBLK").unwrap();
    std::fs::write(modules_dir.join("drivers/virtio/virtio.ko"), b"VRTO").unwrap();
    std::fs::write(modules_dir.join("drivers/virtio/virtio_ring.ko"), b"VRNG").unwrap();
    std::fs::write(modules_dir.join("drivers/virtio/virtio_pci.ko"), b"VPCI").unwrap();
    std::fs::write(modules_dir.join("fs/squashfs/squashfs.ko"), b"SQFS").unwrap();
    // Some random non-essential module we must not pick up.
    std::fs::write(modules_dir.join("drivers/virtio/virtio_net.ko"), b"NETP").unwrap();
    staging
}

fn synthetic_kernel_layout(staging_root: &Path, release: &str) -> KernelLayout {
    KernelLayout {
        release: release.into(),
        vmlinuz: staging_root.join("boot").join(format!("vmlinuz-{release}")),
        modules: staging_root.join("lib").join("modules").join(release),
    }
}

#[test]
fn rejects_staging_without_busybox() {
    let staging = BuildStaging::new().expect("staging");
    let kernel = synthetic_kernel_layout(staging.path(), "6.6.79");
    let err = generate_initramfs(&staging, &kernel).unwrap_err();
    match err {
        InitrdError::MissingBusybox(p) => assert!(p.to_string_lossy().ends_with("bin/busybox")),
        other => panic!("expected MissingBusybox, got {other:?}"),
    }
}

#[test]
fn picks_only_essential_modules() {
    let release = "6.6.79";
    let staging = seed_busybox_shaped_staging(release);
    let kernel = synthetic_kernel_layout(staging.path(), release);
    let (_, report) = generate_initramfs(&staging, &kernel).expect("generate");
    // 5 essential modules (virtio, virtio_ring, virtio_pci, virtio_blk,
    // squashfs); virtio_net is intentionally excluded.
    assert_eq!(report.modules_count, 5, "report: {report:?}");
}

#[test]
fn produces_valid_gzip_cpio() {
    let release = "6.6.79";
    let staging = seed_busybox_shaped_staging(release);
    let kernel = synthetic_kernel_layout(staging.path(), release);
    let (bytes, report) = generate_initramfs(&staging, &kernel).expect("generate");

    // First two bytes are gzip's 0x1f 0x8b magic.
    assert_eq!(&bytes[..2], &[0x1f, 0x8b], "missing gzip magic");
    assert_eq!(bytes.len(), report.compressed_size_bytes);

    // Decompress and confirm CPIO magic.
    use flate2::read::GzDecoder;
    use std::io::Read as _;
    let mut decoder = GzDecoder::new(&bytes[..]);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .expect("gunzip initramfs");
    assert_eq!(decompressed.len(), report.cpio_size_bytes);
    // CPIO newc archives start with the ASCII string `070701`.
    assert_eq!(&decompressed[..6], b"070701", "missing CPIO newc magic");
}

#[test]
fn init_script_references_modules_and_squashfs_mount() {
    let release = "6.6.79";
    let staging = seed_busybox_shaped_staging(release);
    let kernel = synthetic_kernel_layout(staging.path(), release);
    let (bytes, _) = generate_initramfs(&staging, &kernel).expect("generate");

    use flate2::read::GzDecoder;
    use std::io::Read as _;
    let mut decoder = GzDecoder::new(&bytes[..]);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed).expect("gunzip");
    let text = String::from_utf8_lossy(&decompressed);
    // Modules are listed in `UMF_MODS` and loaded by a retry loop rather than
    // one `insmod` line each, so assert the same intent against that shape:
    // the paths are referenced, and something loads them.
    assert!(
        text.contains("/lib/modules/6.6.79/"),
        "init does not reference the embedded modules"
    );
    assert!(
        text.contains("insmod \"$_m\""),
        "init does not load the embedded modules"
    );
    assert!(
        text.contains("mount -t squashfs"),
        "init missing squashfs mount"
    );
    assert!(
        text.contains("switch_root /sysroot"),
        "init missing switch_root"
    );
}

#[test]
fn boot_init_honours_root_from_the_kernel_cmdline() {
    // `umf compile` writes `root=PARTLABEL=ROOTFS` precisely so one disk boots
    // wherever the root device enumerates differently. The initramfs used to
    // ignore it and hardcode /dev/vda2 with an /dev/sda2 fallback, which is
    // why an init-system image could not boot from NVMe.
    let release = "6.6.79";
    let staging = seed_busybox_shaped_staging(release);
    let kernel = synthetic_kernel_layout(staging.path(), release);
    let (bytes, _) = generate_initramfs(&staging, &kernel).expect("generate");

    use flate2::read::GzDecoder;
    use std::io::Read as _;
    let mut decoder = GzDecoder::new(&bytes[..]);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed).expect("gunzip");
    let text = String::from_utf8_lossy(&decompressed);

    assert!(
        text.contains("/proc/cmdline"),
        "init must read the kernel cmdline",
    );
    assert!(
        text.contains("root=*) ROOT=\"${_arg#root=}\""),
        "init must extract root= from the cmdline",
    );
    assert!(
        text.contains("findfs"),
        "init must resolve PARTLABEL=/PARTUUID=/UUID= forms via findfs",
    );
    // The historical probe survives as a fallback, now including NVMe and MMC.
    for node in ["/dev/vda2", "/dev/sda2", "/dev/nvme0n1p2", "/dev/mmcblk0p2"] {
        assert!(
            text.contains(node),
            "fallback probe should still consider {node}",
        );
    }
    // And a total failure says so rather than hanging on a mount error.
    assert!(
        text.contains("no root device found"),
        "init should diagnose an unresolvable root device",
    );
}

#[test]
fn boot_initramfs_carries_non_virtio_storage_drivers() {
    // Even with the cmdline honoured, a root device needs a driver in the
    // initramfs. The allowlist was virtio-only, so NVMe hardware had nothing
    // to bind the disk with.
    let release = "6.6.79";
    let staging = seed_busybox_shaped_staging(release);
    let modules_root = staging.path().join("lib/modules").join(release);
    // Seed a few driver-shaped module files the allowlist should now pick up.
    let kernel_dir = modules_root.join("kernel/drivers/nvme/host");
    std::fs::create_dir_all(&kernel_dir).expect("mkdir");
    for m in ["nvme.ko", "nvme_core.ko"] {
        std::fs::write(kernel_dir.join(m), b"\x7fELF-ish").expect("write module");
    }
    let ata_dir = modules_root.join("kernel/drivers/ata");
    std::fs::create_dir_all(&ata_dir).expect("mkdir");
    std::fs::write(ata_dir.join("ahci.ko"), b"\x7fELF-ish").expect("write module");

    let kernel = synthetic_kernel_layout(staging.path(), release);
    let (bytes, report) = generate_initramfs(&staging, &kernel).expect("generate");

    use flate2::read::GzDecoder;
    use std::io::Read as _;
    let mut decoder = GzDecoder::new(&bytes[..]);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed).expect("gunzip");
    let text = String::from_utf8_lossy(&decompressed);

    for driver in ["nvme", "nvme_core", "ahci"] {
        assert!(
            text.contains(&format!("{driver}.ko")),
            "{driver} should be embedded for bare-metal boot; report={report:?}",
        );
    }
}

#[test]
fn module_loading_retries_so_dependency_order_does_not_matter() {
    // Modules are collected in path order, not dependency order (nvme needs
    // nvme_core, ahci needs libahci), and `insmod` of a module whose
    // dependency is not yet loaded fails. Repeated passes make the ordering
    // irrelevant instead of relying on the walk happening to be correct.
    let release = "6.6.79";
    let staging = seed_busybox_shaped_staging(release);
    let kernel = synthetic_kernel_layout(staging.path(), release);
    let (bytes, _) = generate_initramfs(&staging, &kernel).expect("generate");

    use flate2::read::GzDecoder;
    use std::io::Read as _;
    let mut decoder = GzDecoder::new(&bytes[..]);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed).expect("gunzip");
    let text = String::from_utf8_lossy(&decompressed);

    assert!(
        text.contains("for _pass in"),
        "module loading should retry across passes",
    );
    assert!(
        text.contains("_loaded=1"),
        "the retry loop should track whether a pass made progress",
    );
}

#[test]
fn the_generated_boot_init_is_valid_shell() {
    // This script is generated, never reviewed as a file, and only ever runs
    // at boot — where a syntax error is an unbootable image with a message
    // nobody sees. `sh -n` parses without executing, so the guard is cheap
    // and catches exactly that class of mistake.
    let release = "6.6.79";
    let staging = seed_busybox_shaped_staging(release);
    let kernel = synthetic_kernel_layout(staging.path(), release);
    let (bytes, _) = generate_initramfs(&staging, &kernel).expect("generate");

    use flate2::read::GzDecoder;
    use std::io::Read as _;
    let mut decoder = GzDecoder::new(&bytes[..]);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed).expect("gunzip");
    let text = String::from_utf8_lossy(&decompressed);

    // The cpio payload embeds the script verbatim; slice it out by its
    // shebang and its final line.
    let start = text.find("#!/bin/sh").expect("init script present");
    let end_marker = "exec switch_root /sysroot /sbin/init\n";
    let end = text[start..]
        .find(end_marker)
        .map(|i| start + i + end_marker.len())
        .expect("init script terminator present");
    let script = &text[start..end];

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("init");
    std::fs::write(&path, script.as_bytes()).expect("write script");

    let out = std::process::Command::new("sh")
        .arg("-n")
        .arg(&path)
        .output()
        .expect("run sh -n");
    assert!(
        out.status.success(),
        "generated init is not valid shell:\n{}\n--- script ---\n{script}",
        String::from_utf8_lossy(&out.stderr),
    );
}
