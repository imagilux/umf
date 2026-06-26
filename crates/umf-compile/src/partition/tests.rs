//! Unit tests for the `partition` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use std::fs;
use tempfile::tempdir;

#[test]
fn partition_view_round_trips_within_bounds() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("dev.img");
    {
        let f = File::create(&path).expect("create");
        f.set_len(1024).expect("set_len");
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("reopen");
    let mut view = PartitionView::new(file, 256, 256, "ROOTFS");
    view.write_all(b"hello world").expect("write");
    view.seek(SeekFrom::Start(0)).expect("rewind");
    let mut buf = vec![0u8; 11];
    view.read_exact(&mut buf).expect("read");
    assert_eq!(&buf, b"hello world");

    let raw = fs::read(&path).expect("read raw");
    assert_eq!(&raw[256..267], b"hello world");
    assert!(raw[..256].iter().all(|&b| b == 0));
}

#[test]
fn partition_view_refuses_writes_past_end() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("dev.img");
    {
        let f = File::create(&path).expect("create");
        f.set_len(64).expect("set_len");
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("reopen");
    let mut view = PartitionView::new(file, 0, 16, "ROOTFS");
    view.write_all(b"sixteen-bytes!!!").expect("write 16");
    let err = view.write(b"X").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::WriteZero);
    // The overflow diagnostic names the actual partition, not a hardcoded
    // "ESP": `PartitionView` backs the ROOTFS write too.
    assert!(
        err.to_string().contains("ROOTFS partition full"),
        "message: {err}"
    );
}

#[test]
fn cache_variant_packs_geometry_as_two_hex_fields() {
    let geom = DiskGeometry {
        disk_size_bytes: 0x1234,
        esp_size_bytes: 0xABCD,
    };
    // Two zero-padded 16-hex-digit fields, concatenated (32 chars total):
    // disk 0x1234 then esp 0xabcd.
    let variant = geom.cache_variant();
    assert_eq!(variant.len(), 32);
    assert_eq!(variant, "0000000000001234000000000000abcd");
    // Distinct geometries produce distinct keys; identical ones collide.
    let swapped = DiskGeometry {
        disk_size_bytes: 0xABCD,
        esp_size_bytes: 0x1234,
    };
    assert_ne!(geom.cache_variant(), swapped.cache_variant());
    assert_eq!(geom.cache_variant(), geom.cache_variant());
}

#[test]
fn default_geometry_uses_the_documented_defaults() {
    let geom = DiskGeometry::default();
    assert_eq!(geom.disk_size_bytes, DEFAULT_DISK_SIZE_BYTES);
    assert_eq!(geom.esp_size_bytes, DEFAULT_ESP_SIZE_BYTES);
    assert_eq!(geom.disk_size_bytes, 2 * 1024 * 1024 * 1024);
    assert_eq!(geom.esp_size_bytes, 500 * 1024 * 1024);
}

#[test]
fn boot_cmdline_uses_partlabel_not_a_bus_specific_node() {
    let cmd = boot_cmdline("", Architecture::X86_64);
    // Bus-agnostic root reference: the same disk must boot on
    // virtio / NVMe / SATA without a hardcoded /dev/vdaN node.
    assert!(cmd.contains("root=PARTLABEL=ROOTFS"), "cmdline: {cmd}");
    assert!(
        !cmd.contains("/dev/vda"),
        "must not hardcode a bus-specific root node: {cmd}"
    );
    assert!(cmd.contains("rootfstype=squashfs"));
    // The appliance fragment is appended verbatim.
    assert!(boot_cmdline(" init=/app", Architecture::X86_64).ends_with(" init=/app"));
}

#[test]
fn boot_cmdline_serial_console_tracks_target_arch() {
    // x86 16550 (`ttyS0`) vs aarch64 PL011 (`ttyAMA0`): a disk told the wrong
    // console device boots with no serial output. The console must follow the
    // *target* arch, not the build host.
    assert!(boot_cmdline("", Architecture::X86_64).contains("console=ttyS0,115200n8"));
    assert!(boot_cmdline("", Architecture::Aarch64).contains("console=ttyAMA0,115200n8"));
}

#[test]
fn compute_disk_plan_rejects_undersized_disk() {
    let err = compute_disk_plan(&DiskGeometry {
        disk_size_bytes: 1024,
        esp_size_bytes: 500 * 1024 * 1024,
    })
    .expect_err("must reject");
    assert!(matches!(err, CompileError::DiskTooSmall { .. }));
}

#[test]
fn compute_disk_plan_rejects_absurd_esp_size_without_overflow() {
    // An ESP size near u64::MAX overflows the `esp + GPT overhead` sum. The
    // checked math must reject it cleanly (Err, no panic) instead of
    // wrapping to a tiny `required` that would sneak past the guard.
    let err = compute_disk_plan(&DiskGeometry {
        disk_size_bytes: DEFAULT_DISK_SIZE_BYTES,
        esp_size_bytes: u64::MAX,
    })
    .expect_err("absurd esp_size must be rejected");
    assert!(matches!(err, CompileError::DiskTooSmall { .. }));
}

#[test]
fn project_disk_writes_squashfs_rootfs_and_loader_entry() {
    let dir = tempdir().expect("tempdir");
    let rootfs = dir.path().join("rootfs");
    fs::create_dir_all(rootfs.join("etc")).unwrap();
    fs::write(rootfs.join("etc/os-release"), b"NAME=umf\n").unwrap();
    let vmlinuz = dir.path().join("vmlinuz-7.0");
    fs::write(&vmlinuz, b"fake-kernel-image").unwrap();
    let mut efi = vec![b'M', b'Z', 0x90, 0x00];
    efi.extend_from_slice(&[0u8; 252]);

    let out = dir.path().join("disk.img");
    let proj = project_disk(
        &out,
        &DiskInputs {
            geometry: DiskGeometry {
                disk_size_bytes: 256 * 1024 * 1024,
                esp_size_bytes: 64 * 1024 * 1024,
            },
            rootfs_dir: &rootfs,
            vmlinuz: &vmlinuz,
            kernel_release: "7.0",
            bootloader_efi: Some(&efi),
            initrd: Some((&[0x1f, 0x8b, b'I', b'N'], "initramfs-7.0.img")),
            architecture: Architecture::X86_64,
            extra_cmdline: "",
        },
    )
    .expect("project");

    let bytes = fs::read(&out).expect("read disk");
    assert_eq!(&bytes[510..512], &[0x55, 0xAA], "protective MBR");
    assert_eq!(&bytes[512..520], b"EFI PART", "GPT signature");

    // ROOTFS partition starts with the squashfs magic.
    let start = proj.rootfs_start_bytes as usize;
    assert_eq!(
        &bytes[start..start + 4],
        &[0x68, 0x73, 0x71, 0x73],
        "squashfs magic"
    );

    // ESP: bootloader at the fallback path + a loader entry referencing the
    // kernel, initramfs, and squashfs root.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&out)
        .expect("reopen");
    let view = PartitionView::new(file, proj.esp_start_bytes, proj.esp_size_bytes, "ESP");
    let espfs = FileSystem::new(view, FsOptions::new()).expect("mount esp");
    let root = espfs.root_dir();

    let boot = root
        .open_dir("EFI")
        .expect("EFI")
        .open_dir("BOOT")
        .expect("BOOT");
    let mut bootx = boot
        .open_file(Architecture::X86_64.uefi_fallback_filename())
        .expect("bootloader on ESP");
    let mut bbuf = Vec::new();
    bootx.read_to_end(&mut bbuf).expect("read bootloader");
    assert_eq!(&bbuf[..2], b"MZ");

    let mut entry = root
        .open_dir("loader")
        .expect("loader")
        .open_dir("entries")
        .expect("entries")
        .open_file("umf.conf")
        .expect("umf.conf");
    let mut econf = String::new();
    entry.read_to_string(&mut econf).expect("read entry");
    assert!(econf.contains("linux /vmlinuz-7.0"), "entry: {econf}");
    assert!(
        econf.contains("initrd /initramfs-7.0.img"),
        "entry: {econf}"
    );
    assert!(econf.contains("rootfstype=squashfs"), "entry: {econf}");
}

#[test]
fn project_disk_bootloader_none_builds_a_uki() {
    if !crate::uki::ukify_available() {
        eprintln!("skipping UKI test: ukify (systemd-ukify) not installed");
        return;
    }
    let dir = tempdir().expect("tempdir");
    let rootfs = dir.path().join("rootfs");
    fs::create_dir_all(&rootfs).unwrap();
    let vmlinuz = dir.path().join("vmlinuz-7.0");
    fs::write(&vmlinuz, b"fake-kernel-image").unwrap();

    let out = dir.path().join("disk.img");
    let proj = project_disk(
        &out,
        &DiskInputs {
            geometry: DiskGeometry {
                disk_size_bytes: 256 * 1024 * 1024,
                esp_size_bytes: 64 * 1024 * 1024,
            },
            rootfs_dir: &rootfs,
            vmlinuz: &vmlinuz,
            kernel_release: "7.0",
            bootloader_efi: None,
            initrd: None,
            architecture: Architecture::X86_64,
            extra_cmdline: " init=/myapp",
        },
    )
    .expect("project uki");

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&out)
        .expect("reopen");
    let view = PartitionView::new(file, proj.esp_start_bytes, proj.esp_size_bytes, "ESP");
    let espfs = FileSystem::new(view, FsOptions::new()).expect("mount esp");
    let root = espfs.root_dir();

    // The UKI sits at the firmware fallback path; no loader/ tree.
    assert!(
        root.open_dir("loader").is_err(),
        "UKI build must not write a loader/ tree"
    );
    let boot = root
        .open_dir("EFI")
        .expect("EFI")
        .open_dir("BOOT")
        .expect("BOOT");
    let mut uki = boot
        .open_file(Architecture::X86_64.uefi_fallback_filename())
        .expect("UKI at fallback path");
    let mut ubuf = Vec::new();
    uki.read_to_end(&mut ubuf).expect("read uki");
    assert_eq!(&ubuf[..2], b"MZ", "UKI must be a PE/EFI binary");
    assert!(
        ubuf.windows(b"init=/myapp".len())
            .any(|w| w == b"init=/myapp"),
        "UKI must embed init=/myapp in its cmdline"
    );
}
