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
fn init_script_references_modules_and_mounts_the_root() {
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
        text.contains("/sysroot"),
        "init does not mount the root onto /sysroot"
    );
    assert!(
        text.contains("switch_root /sysroot"),
        "init missing switch_root"
    );
}

/// The root filesystem is chosen at **projection** time by `umf compile --fs`,
/// long after this initramfs was generated, so the init script must not name
/// one. It reads `rootfstype=` back from the cmdline the projector wrote —
/// exactly as it already reads `root=` for the device.
///
/// Baking a type in here instead is what made one built image projectable to
/// only one filesystem: `umf compile --fs ext4` would have produced a disk of
/// ext4 bytes whose initramfs still ran `mount -t squashfs`, and it would not
/// boot.
#[test]
fn boot_init_takes_the_root_filesystem_from_the_cmdline() {
    let script = build_boot_init_script("7.0.0-umf", &[], Path::new("/lib/modules"));

    assert!(
        script.contains("rootfstype="),
        "init must read rootfstype= from the cmdline: {script}",
    );
    assert!(
        script.contains("mount -t \"$ROOTFSTYPE\""),
        "init must mount with the type it read, not a literal: {script}",
    );
    // No hardcoded filesystem anywhere in the mount path.
    for fs in umf_core::boot::RootfsFs::ALL {
        assert!(
            !script.contains(&format!("mount -t {fs}")),
            "init must not hardcode `mount -t {fs}`: {script}",
        );
    }
    // And a cmdline that carried no rootfstype= must still mount, by letting
    // the kernel try every filesystem the modules registered.
    assert!(
        script.contains("mount -o ro \"$ROOT\" /sysroot"),
        "init needs a no-type fallback mount: {script}",
    );
}

/// The initramfs cannot know which filesystem the disk will be projected
/// with, so it carries the driver for every one `umf compile --fs` accepts.
/// A missing module here is an unbootable disk for that filesystem, and the
/// failure is a kernel panic at switch_root rather than anything this crate
/// would catch.
#[test]
fn boot_initramfs_carries_a_driver_for_every_projectable_filesystem() {
    let script = build_boot_init_script("7.0.0-umf", &[], Path::new("/lib/modules"));
    // The module set is embedded in the script's `UMF_MODS` list via the
    // allowlist; assert the allowlist itself names each filesystem.
    let allow = super::modules_allowlist_for_test(&InitramfsFlavor::Boot);
    for fs in umf_core::boot::RootfsFs::ALL {
        assert!(
            allow.contains(&fs.as_str()),
            "boot initramfs must carry the {fs} driver; allowlist: {allow:?}",
        );
    }
    let _ = script;
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

/// The run-flavour init brings the NIC up and takes a DHCP lease, which is
/// what gives a bootable build's `RUN` steps network access at all.
///
/// Pinned because this was misread once: the micro-VM's `VmSpec` sets
/// `net: None`, which means "no pre-built `TapNet`" (the Cloud Hypervisor
/// port-forward path), NOT "no NIC" — the QEMU backend attaches `-netdev user`
/// unconditionally. Reading that field as "no network" produced a roadmap item
/// claiming bootable `RUN` steps could not reach the network, which was wrong.
/// If this assertion ever has to be deleted, the docs and the roadmap need
/// revisiting with it: `docs/known-limitations.md` states that the egress
/// exists but is unpoliced, which is only half true if the NIC stops coming up.
#[test]
fn the_run_init_brings_up_the_nic_and_takes_a_dhcp_lease() {
    let release = "7.0.0-umf";
    let script = build_run_init_script(release, &[], Path::new("/lib/modules"));
    assert!(
        script.contains("eth0"),
        "the run init must configure a NIC:\n{script}"
    );
    assert!(
        script.contains("udhcpc"),
        "the run init must take a DHCP lease so the NIC is usable:\n{script}"
    );
    // The loopback is brought up too — some tooling binds to it.
    assert!(script.contains("lo"), "loopback must come up:\n{script}");
}

/// The run-flavour init is generated by string concatenation, so a syntax
/// error is invisible until a guest silently fails to boot mid-build. Check it
/// the same way the boot init is checked.
#[test]
fn the_generated_run_init_is_valid_shell() {
    let script = build_run_init_script("7.0.0-umf", &[], Path::new("/lib/modules"));
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("init");
    std::fs::write(&path, &script).expect("write init");
    let out = std::process::Command::new("sh")
        .arg("-n")
        .arg(&path)
        .output()
        .expect("run sh -n");
    assert!(
        out.status.success(),
        "generated run init is not valid shell:\n{}\n--- script ---\n{script}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The static-config branch must actually configure the NIC and the resolver,
/// and the resolver must be bind-mounted rather than written through the 9p
/// share — a plain write would bake the build host's nameservers into the
/// image layer.
#[test]
fn the_run_init_configures_a_staged_static_network() {
    let script = build_run_init_script("7.0.0-umf", &[], Path::new("/lib/modules"));
    assert!(script.contains(".umf-net"), "must read the staged config");
    assert!(script.contains("UMF_GW"), "must set a default route");
    assert!(
        script.contains("mount --bind /etc/resolv.conf /sysroot/etc/resolv.conf"),
        "resolv.conf must be bind-mounted, not written into the layer:\n{script}"
    );
    // DHCP stays as the fallback for the user-mode-stack path.
    assert!(script.contains("udhcpc"), "DHCP fallback must remain");
}

/// Regression, from a real boot failure: selecting `ext4` must also embed
/// `jbd2`, `mbcache` and `crc16`.
///
/// The allowlist names drivers, not what drivers need. `squashfs` needs
/// nothing, so a flat list looked correct until a second filesystem was
/// added — and the failure gives no hint at this code. `ext4.ko` loads,
/// every `jbd2_*` symbol resolves to nothing, `mount` returns `EINVAL`,
/// PID 1 exits and the kernel panics at `switch_root`:
///
/// ```text
/// ext4: Unknown symbol jbd2_journal_init_inode (err -2)
/// mount: mounting /dev/vda2 on /sysroot failed: Invalid argument
/// Kernel panic - not syncing: Attempted to kill init!
/// ```
#[test]
fn a_filesystem_driver_brings_its_dependencies_with_it() {
    let tree = tempfile::tempdir().expect("tempdir");
    let root = tree.path();

    // A module tree shaped like Alpine's: the driver in one place, the
    // libraries it needs scattered elsewhere.
    for rel in [
        "kernel/fs/ext4/ext4.ko.gz",
        "kernel/fs/jbd2/jbd2.ko.gz",
        "kernel/fs/mbcache.ko.gz",
        "kernel/lib/crc16.ko.gz",
        "kernel/fs/squashfs/squashfs.ko.gz",
        // Present in the tree but reachable from nothing in the allowlist.
        "kernel/fs/xfs/xfs.ko.gz",
    ] {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"\x1f\x8b").unwrap();
    }
    std::fs::write(
        root.join("modules.dep"),
        "kernel/fs/ext4/ext4.ko.gz: kernel/fs/jbd2/jbd2.ko.gz kernel/fs/mbcache.ko.gz \
         kernel/lib/crc16.ko.gz\n\
         kernel/fs/jbd2/jbd2.ko.gz:\n\
         kernel/fs/squashfs/squashfs.ko.gz:\n\
         kernel/fs/xfs/xfs.ko.gz:\n",
    )
    .unwrap();

    let picked = collect_modules_for(root, &InitramfsFlavor::Boot).expect("collect");
    let names: Vec<String> = picked
        .iter()
        .map(|p| module_stem(&p.file_name().unwrap().to_string_lossy()))
        .collect();

    for needed in ["ext4", "jbd2", "mbcache", "crc16"] {
        assert!(
            names.iter().any(|n| n == needed),
            "`{needed}` must be embedded — ext4 cannot mount without it: {names:?}",
        );
    }
    assert!(
        !names.iter().any(|n| n == "xfs"),
        "dependency resolution must not drag in unrelated modules: {names:?}",
    );
}

/// A tree with no `modules.dep` still yields the allowlisted modules. Some
/// kernel packages ship without one, and fewer modules is the behaviour
/// that predates dependency resolution — never an error.
#[test]
fn a_tree_without_modules_dep_still_collects_the_allowlist() {
    let tree = tempfile::tempdir().expect("tempdir");
    let root = tree.path();
    for rel in ["kernel/fs/squashfs/squashfs.ko", "kernel/fs/ext4/ext4.ko"] {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"stub").unwrap();
    }

    let picked = collect_modules_for(root, &InitramfsFlavor::Boot).expect("collect");
    let names: Vec<String> = picked
        .iter()
        .map(|p| module_stem(&p.file_name().unwrap().to_string_lossy()))
        .collect();
    assert!(names.iter().any(|n| n == "squashfs"), "{names:?}");
    assert!(names.iter().any(|n| n == "ext4"), "{names:?}");
}

/// A dependency naming a module the tree does not carry is skipped, not
/// fatal: the kernel compiled it in, so there is nothing to embed.
#[test]
fn a_dependency_absent_from_the_tree_is_not_an_error() {
    let tree = tempfile::tempdir().expect("tempdir");
    let root = tree.path();
    let path = root.join("kernel/fs/erofs/erofs.ko");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"stub").unwrap();
    std::fs::write(
        root.join("modules.dep"),
        "kernel/fs/erofs/erofs.ko: kernel/lib/lz4/lz4_decompress.ko\n",
    )
    .unwrap();

    let picked = collect_modules_for(root, &InitramfsFlavor::Boot).expect("collect");
    assert_eq!(picked.len(), 1, "only the module that exists: {picked:?}");
}

/// Regression, from the second boot failure of the same feature: the
/// dependency lookup must survive `modules.dep` naming files that no longer
/// exist under those names.
///
/// `scripts/make-boot-fixture.sh` gunzips every `.ko.gz` in the tree,
/// because busybox `insmod` cannot read a compressed module — but
/// `modules.dep` is copied verbatim and still says `…/jbd2.ko.gz`. A
/// path-keyed lookup matches nothing, resolves no dependencies, and
/// degrades silently to the flat allowlist: the same `Unknown symbol
/// jbd2_*` panic as before the fix, with nothing to show the resolution
/// step ran at all.
#[test]
fn dependencies_resolve_when_modules_dep_names_a_stale_compression_suffix() {
    let tree = tempfile::tempdir().expect("tempdir");
    let root = tree.path();

    // On disk: decompressed, exactly as the fixture leaves them.
    for rel in [
        "kernel/fs/ext4/ext4.ko",
        "kernel/fs/jbd2/jbd2.ko",
        "kernel/fs/mbcache.ko",
        "kernel/lib/crc16.ko",
    ] {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"stub").unwrap();
    }
    // In modules.dep: the `.ko.gz` names depmod originally saw.
    std::fs::write(
        root.join("modules.dep"),
        "kernel/fs/ext4/ext4.ko.gz: kernel/fs/jbd2/jbd2.ko.gz kernel/fs/mbcache.ko.gz \
         kernel/lib/crc16.ko.gz\n",
    )
    .unwrap();

    let picked = collect_modules_for(root, &InitramfsFlavor::Boot).expect("collect");
    let names: Vec<String> = picked
        .iter()
        .map(|p| module_stem(&p.file_name().unwrap().to_string_lossy()))
        .collect();

    for needed in ["ext4", "jbd2", "mbcache", "crc16"] {
        assert!(
            names.iter().any(|n| n == needed),
            "`{needed}` must resolve despite the stale `.ko.gz` in modules.dep: {names:?}",
        );
    }
}

/// Regression, from the third boot failure of the same feature: ext4 needs
/// a `crc32c` *crypto* provider, which no dependency graph will reveal.
///
/// `mkfs.ext4` enables `metadata_csum` by default, so mounting asks the
/// crypto API for a "crc32c" shash via `crypto_alloc_shash`. That is a
/// runtime request by algorithm name, not a symbol reference, so it does
/// not appear in `modules.dep` and `with_dependencies` cannot infer it.
/// The mount then fails with `ENOENT` and exactly one line of explanation:
///
/// ```text
/// EXT4-fs (vda2): Cannot load crc32c driver.
/// mount: mounting /dev/vda2 on /sysroot failed: No such file or directory
/// ```
#[test]
fn the_boot_initramfs_carries_a_crc32c_provider() {
    let boot = modules_allowlist_for_test(&InitramfsFlavor::Boot);
    assert!(
        boot.contains(&"crc32c_generic"),
        "ext4 with metadata_csum cannot mount without a crc32c shash: {boot:?}",
    );
    assert!(
        boot.contains(&"libcrc32c"),
        "the crc32c wrapper the filesystems link against: {boot:?}",
    );
}

/// `-` and `_` are the same character in a module name. x86 spells the
/// accelerated driver `crc32c-intel.ko` while every reference to it uses an
/// underscore, so matching the raw filename silently drops it.
#[test]
fn module_matching_ignores_dash_versus_underscore() {
    let tree = tempfile::tempdir().expect("tempdir");
    let root = tree.path();
    let path = root.join("kernel/arch/x86/crypto/crc32c-intel.ko");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"stub").unwrap();

    let picked = collect_modules_for(root, &InitramfsFlavor::Boot).expect("collect");
    assert_eq!(
        picked.len(),
        1,
        "`crc32c_intel` must match the file spelled `crc32c-intel.ko`: {picked:?}",
    );
}
