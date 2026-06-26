//! Unit tests for the `spawn` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use super::*;

fn base_spec() -> VmSpec {
    VmSpec::from_disk(PathBuf::from("/tmp/fake-disk.img"))
}

#[test]
fn disk_boot_passes_drive_with_virtio() {
    let args = build_qemu_args(&base_spec(), None, "test");
    let drive_idx = args.iter().position(|a| a == "-drive").expect("`-drive`");
    let drive_val = &args[drive_idx + 1];
    assert!(drive_val.contains("file=/tmp/fake-disk.img"));
    assert!(drive_val.contains("if=virtio"));
    assert!(drive_val.contains("format=raw"));
}

#[test]
fn kvm_flag_emits_enable_kvm_and_cpu_host() {
    let args = build_qemu_args(&base_spec(), None, "test");
    assert!(args.iter().any(|a| a == "-enable-kvm"));
    let cpu_idx = args.iter().position(|a| a == "-cpu").expect("`-cpu`");
    assert_eq!(args[cpu_idx + 1], "host");
}

#[test]
fn no_kvm_falls_back_to_cpu_max() {
    let mut spec = base_spec();
    spec.kvm = false;
    let args = build_qemu_args(&spec, None, "test");
    assert!(!args.iter().any(|a| a == "-enable-kvm"));
    let cpu_idx = args.iter().position(|a| a == "-cpu").expect("`-cpu`");
    assert_eq!(args[cpu_idx + 1], "max");
}

#[test]
fn headless_uses_serial_mon_stdio() {
    let args = build_qemu_args(&base_spec(), None, "test");
    let display_idx = args.iter().position(|a| a == "-display").expect("display");
    assert_eq!(args[display_idx + 1], "none");
    let serial_idx = args.iter().position(|a| a == "-serial").expect("serial");
    assert_eq!(args[serial_idx + 1], "mon:stdio");
}

#[test]
fn graphical_mode_omits_display_and_serial() {
    let mut spec = base_spec();
    spec.display = DisplayMode::Window;
    let args = build_qemu_args(&spec, None, "test");
    assert!(!args.iter().any(|a| a == "-display"));
    assert!(!args.iter().any(|a| a == "-serial"));
}

#[test]
fn qmp_socket_emits_unix_server_nowait() {
    let sock = PathBuf::from("/tmp/qmp.sock");
    let args = build_qemu_args(&base_spec(), Some(&sock), "test");
    let qmp_idx = args.iter().position(|a| a == "-qmp").expect("`-qmp`");
    assert_eq!(args[qmp_idx + 1], "unix:/tmp/qmp.sock,server,nowait");
}

#[test]
fn no_qmp_socket_means_no_qmp_arg() {
    let args = build_qemu_args(&base_spec(), None, "test");
    assert!(!args.iter().any(|a| a == "-qmp"));
}

#[test]
fn port_forward_becomes_netdev_hostfwd() {
    let mut spec = base_spec();
    spec.port_forwards = vec![
        PortForward {
            host_port: 8080,
            guest_port: 80,
            tcp: true,
        },
        PortForward {
            host_port: 5353,
            guest_port: 53,
            tcp: false,
        },
    ];
    let args = build_qemu_args(&spec, None, "test");
    let netdev_idx = args.iter().position(|a| a == "-netdev").expect("netdev");
    let netdev_val = &args[netdev_idx + 1];
    assert!(netdev_val.contains("user,id=net0"));
    assert!(netdev_val.contains("hostfwd=tcp::8080-:80"));
    assert!(netdev_val.contains("hostfwd=udp::5353-:53"));
}

#[test]
fn direct_kernel_boot_uses_kernel_initrd_append() {
    let spec = VmSpec {
        boot: BootSource::DirectKernel {
            kernel: PathBuf::from("/k/vmlinuz"),
            initrd: PathBuf::from("/k/initrd.img"),
            cmdline: "console=ttyS0 quiet".to_string(),
        },
        ..base_spec()
    };
    let args = build_qemu_args(&spec, None, "test");
    let kernel_idx = args.iter().position(|a| a == "-kernel").expect("`-kernel`");
    assert_eq!(args[kernel_idx + 1], "/k/vmlinuz");
    let initrd_idx = args.iter().position(|a| a == "-initrd").expect("`-initrd`");
    assert_eq!(args[initrd_idx + 1], "/k/initrd.img");
    let append_idx = args.iter().position(|a| a == "-append").expect("`-append`");
    assert_eq!(args[append_idx + 1], "console=ttyS0 quiet");
    // Disk boot's `-drive` must not appear.
    assert!(!args.iter().any(|a| a == "-drive"));
}

#[test]
fn run_micro_vm_emits_9p_share_and_file_serial() {
    let spec = VmSpec {
        boot: BootSource::DirectKernel {
            kernel: PathBuf::from("/k/vmlinuz"),
            initrd: PathBuf::from("/k/initrd.img"),
            cmdline: "console=ttyS0 quiet panic=1".to_string(),
        },
        shares: vec![crate::runtime::NinePShare {
            host_path: PathBuf::from("/var/staging"),
            mount_tag: "umfstage".to_string(),
        }],
        serial: SerialMode::File(PathBuf::from("/tmp/serial.log")),
        ..base_spec()
    };
    let args = build_qemu_args(&spec, None, "test");
    let joined = args.join(" ");
    // 9p share: the tag the guest mounts by + the mapped-xattr model the
    // staging round-trip depends on (must match the build's expectation).
    assert!(joined.contains("mount_tag=umfstage"), "{joined}");
    assert!(joined.contains("path=/var/staging"), "{joined}");
    assert!(joined.contains("security_model=mapped-xattr"), "{joined}");
    // Serial captured to a file, not multiplexed onto stdio.
    let serial_idx = args.iter().position(|a| a == "-serial").expect("-serial");
    assert_eq!(args[serial_idx + 1], "file:/tmp/serial.log");
    assert!(!joined.contains("mon:stdio"), "{joined}");
    // Still a direct-kernel boot with outbound user-net.
    assert!(joined.contains("-kernel /k/vmlinuz"), "{joined}");
    assert!(joined.contains("virtio-net-pci,netdev=net0"), "{joined}");
}

#[test]
fn firmware_override_emits_bios_arg() {
    let spec = VmSpec {
        boot: BootSource::Disk {
            path: PathBuf::from("/disk.raw"),
            firmware: Some(Firmware::Bios(PathBuf::from("/firmware/OVMF.fd"))),
        },
        ..base_spec()
    };
    let args = build_qemu_args(&spec, None, "test");
    let bios_idx = args.iter().position(|a| a == "-bios").expect("`-bios`");
    assert_eq!(args[bios_idx + 1], "/firmware/OVMF.fd");
    // The split-layout pflash drives must not appear for a single-file blob.
    assert!(!args.iter().any(|a| a.contains("if=pflash")));
}

#[test]
fn split_firmware_emits_two_pflash_drives() {
    let spec = VmSpec {
        boot: BootSource::Disk {
            path: PathBuf::from("/disk.raw"),
            firmware: Some(Firmware::Pflash {
                code: PathBuf::from("/usr/share/OVMF/OVMF_CODE.fd"),
                vars: PathBuf::from("/run/umf/OVMF_VARS.fd"),
            }),
        },
        ..base_spec()
    };
    let args = build_qemu_args(&spec, None, "test");
    // No `-bios` for the split layout: firmware comes in via pflash.
    assert!(!args.iter().any(|a| a == "-bios"));
    let pflash: Vec<&String> = args.iter().filter(|a| a.contains("if=pflash")).collect();
    assert_eq!(
        pflash.len(),
        2,
        "expected CODE + VARS pflash units: {args:?}"
    );
    // CODE is unit 0, read-only.
    assert!(pflash[0].contains("unit=0"), "{:?}", pflash[0]);
    assert!(pflash[0].contains("readonly=on"), "{:?}", pflash[0]);
    assert!(
        pflash[0].contains("file=/usr/share/OVMF/OVMF_CODE.fd"),
        "{:?}",
        pflash[0]
    );
    // VARS is unit 1, writable (no readonly flag).
    assert!(pflash[1].contains("unit=1"), "{:?}", pflash[1]);
    assert!(!pflash[1].contains("readonly=on"), "{:?}", pflash[1]);
    assert!(
        pflash[1].contains("file=/run/umf/OVMF_VARS.fd"),
        "{:?}",
        pflash[1]
    );
}

#[test]
fn prepare_firmware_copies_vars_and_repoints_spec() {
    // A split-OVMF spec gets its VARS path rewritten to a fresh writable
    // copy; CODE and the disk path are preserved.
    let src = TempDir::new().expect("src tempdir");
    let vars_src = src.path().join("OVMF_VARS.fd");
    std::fs::write(&vars_src, b"nvram-template").expect("write template");

    let spec = VmSpec {
        boot: BootSource::Disk {
            path: PathBuf::from("/disk.raw"),
            firmware: Some(Firmware::Pflash {
                code: PathBuf::from("/usr/share/OVMF/OVMF_CODE.fd"),
                vars: vars_src.clone(),
            }),
        },
        ..base_spec()
    };
    let (dir, patched) = prepare_firmware(&spec).expect("prepare");
    let dir = dir.expect("split layout must allocate a tempdir");
    let BootSource::Disk {
        path,
        firmware: Some(Firmware::Pflash { code, vars }),
    } = &patched.boot
    else {
        panic!("expected a split-OVMF disk boot, got {:?}", patched.boot);
    };
    assert_eq!(path, &PathBuf::from("/disk.raw"));
    assert_eq!(code, &PathBuf::from("/usr/share/OVMF/OVMF_CODE.fd"));
    // VARS now points inside the per-run tempdir, not at the host template.
    assert_ne!(vars, &vars_src);
    assert!(vars.starts_with(dir.path()), "{vars:?}");
    assert_eq!(
        std::fs::read(vars).expect("read copy"),
        b"nvram-template",
        "the copy must duplicate the template contents",
    );
    // The host template is untouched.
    assert!(vars_src.is_file());
}

#[test]
fn prepare_firmware_is_noop_for_single_file_bios() {
    let spec = VmSpec {
        boot: BootSource::Disk {
            path: PathBuf::from("/disk.raw"),
            firmware: Some(Firmware::Bios(PathBuf::from("/firmware/OVMF.fd"))),
        },
        ..base_spec()
    };
    let (dir, patched) = prepare_firmware(&spec).expect("prepare");
    assert!(dir.is_none(), "single-file firmware needs no tempdir");
    assert!(matches!(
        patched.boot,
        BootSource::Disk {
            firmware: Some(Firmware::Bios(_)),
            ..
        }
    ));
}

#[test]
fn id_lands_in_name_arg() {
    let args = build_qemu_args(&base_spec(), None, "my-vm-id");
    let name_idx = args.iter().position(|a| a == "-name").expect("`-name`");
    assert_eq!(args[name_idx + 1], "my-vm-id");
}

/// Read the value following the first occurrence of `flag` in `args`.
fn arg_after<'a>(args: &'a [String], flag: &str) -> &'a str {
    let idx = args
        .iter()
        .position(|a| a == flag)
        .unwrap_or_else(|| panic!("missing {flag} in {args:?}"));
    &args[idx + 1]
}

#[test]
fn x86_64_kvm_uses_q35_machine_and_enable_kvm() {
    // Default base_spec is x86_64 + kvm: q35 board, accel=kvm, the x86
    // `-enable-kvm`, and `-cpu host`.
    let mut spec = base_spec();
    spec.arch = VmArch::X86_64;
    spec.kvm = true;
    let args = build_qemu_args(&spec, None, "test");
    assert_eq!(arg_after(&args, "-machine"), "q35,accel=kvm");
    assert!(args.iter().any(|a| a == "-enable-kvm"));
    assert_eq!(arg_after(&args, "-cpu"), "host");
}

#[test]
fn aarch64_kvm_uses_virt_machine_cpu_host_no_enable_kvm() {
    // aarch64 + kvm: the `virt` board (no q35 on ARM), accel=kvm, and
    // `-cpu host`. The x86-only `-enable-kvm` must NOT appear — on ARM
    // `accel=kvm` already selects it and a bare `-enable-kvm` is wrong.
    let mut spec = base_spec();
    spec.arch = VmArch::Aarch64;
    spec.kvm = true;
    let args = build_qemu_args(&spec, None, "test");
    assert_eq!(arg_after(&args, "-machine"), "virt,accel=kvm");
    assert_eq!(arg_after(&args, "-cpu"), "host");
    assert!(
        !args.iter().any(|a| a == "-enable-kvm"),
        "aarch64 must not emit the x86 `-enable-kvm`: {args:?}",
    );
    // `virt` has no default CPU — a `-cpu` is mandatory, so it must exist.
    assert!(args.iter().any(|a| a == "-cpu"), "{args:?}");
}

#[test]
fn aarch64_tcg_uses_virt_machine_and_cpu_max() {
    // No KVM (software emulation): `virt` board with accel=tcg and the
    // richest synthesised CPU model, `max`.
    let mut spec = base_spec();
    spec.arch = VmArch::Aarch64;
    spec.kvm = false;
    let args = build_qemu_args(&spec, None, "test");
    assert_eq!(arg_after(&args, "-machine"), "virt,accel=tcg");
    assert_eq!(arg_after(&args, "-cpu"), "max");
    assert!(!args.iter().any(|a| a == "-enable-kvm"));
}

#[test]
fn x86_64_tcg_uses_q35_machine_and_cpu_max() {
    let mut spec = base_spec();
    spec.arch = VmArch::X86_64;
    spec.kvm = false;
    let args = build_qemu_args(&spec, None, "test");
    assert_eq!(arg_after(&args, "-machine"), "q35,accel=tcg");
    assert_eq!(arg_after(&args, "-cpu"), "max");
    assert!(!args.iter().any(|a| a == "-enable-kvm"));
}

#[test]
fn aarch64_pflash_firmware_emits_two_pflash_drives_not_bios() {
    // aarch64 AAVMF arrives as a CODE/VARS pflash pair; it renders as two
    // `if=pflash` drives, never `-bios` (which is invalid on ARM).
    let spec = VmSpec {
        arch: VmArch::Aarch64,
        boot: BootSource::Disk {
            path: PathBuf::from("/disk.raw"),
            firmware: Some(Firmware::Pflash {
                code: PathBuf::from("/usr/share/AAVMF/AAVMF_CODE.fd"),
                vars: PathBuf::from("/run/umf/AAVMF_VARS.fd"),
            }),
        },
        ..base_spec()
    };
    let args = build_qemu_args(&spec, None, "test");
    assert!(!args.iter().any(|a| a == "-bios"), "{args:?}");
    let pflash: Vec<&String> = args.iter().filter(|a| a.contains("if=pflash")).collect();
    assert_eq!(pflash.len(), 2, "expected CODE + VARS pflash: {args:?}");
    assert!(pflash[0].contains("file=/usr/share/AAVMF/AAVMF_CODE.fd"));
    assert!(pflash[0].contains("readonly=on"));
    assert!(pflash[1].contains("file=/run/umf/AAVMF_VARS.fd"));
    assert!(!pflash[1].contains("readonly=on"));
}
