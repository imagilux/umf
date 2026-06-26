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
    assert!(
        text.contains("insmod /lib/modules/6.6.79/"),
        "init missing insmod lines"
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
