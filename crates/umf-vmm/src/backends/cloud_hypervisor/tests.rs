//! Unit tests for the `cloud_hypervisor` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use std::path::PathBuf;

#[test]
fn default_runtime_uses_cloud_hypervisor_binary() {
    let rt = CloudHypervisorRuntime::default();
    assert_eq!(rt.binary(), "cloud-hypervisor");
}

#[test]
fn with_binary_overrides() {
    let rt = CloudHypervisorRuntime::with_binary("ch-alt");
    assert_eq!(rt.binary(), "ch-alt");
}

#[test]
fn vm_state_mapping_covers_all_variants() {
    assert_eq!(vm_state_to_vm_status(VmState::Created), VmStatus::Booting);
    assert_eq!(vm_state_to_vm_status(VmState::Running), VmStatus::Running);
    assert_eq!(vm_state_to_vm_status(VmState::Paused), VmStatus::Paused);
    assert_eq!(
        vm_state_to_vm_status(VmState::Shutdown),
        VmStatus::ShuttingDown,
    );
}

#[test]
fn build_vm_config_rejects_disk_without_firmware() {
    let spec = VmSpec::from_disk(PathBuf::from("/tmp/disk.img"));
    let err = build_vm_config(&spec).expect_err("missing firmware should reject");
    let msg = err.to_string();
    assert!(msg.contains("requires --firmware"), "got: {msg}");
    assert!(msg.contains("OVMF") || msg.contains("EDK II"), "got: {msg}");
}

#[test]
fn build_vm_config_disk_with_firmware_succeeds() {
    let spec = VmSpec {
        boot: BootSource::Disk {
            path: PathBuf::from("/tmp/disk.img"),
            firmware: Some(Firmware::Bios(PathBuf::from(
                "/usr/share/edk2/x64/CLOUDHV.fd",
            ))),
        },
        ..VmSpec::from_disk(PathBuf::from("/unused"))
    };
    let cfg = build_vm_config(&spec).expect("should build");
    assert!(cfg.payload.firmware.is_some());
    let disks = cfg.disks.expect("disks set");
    assert_eq!(disks.len(), 1);
    assert_eq!(disks[0].path.as_deref(), Some("/tmp/disk.img"));
}

#[test]
fn build_vm_config_direct_kernel_sets_payload_without_firmware() {
    // Direct-kernel boot is the firmware-free path: the kernel is the payload,
    // and no disk is attached.
    let spec = VmSpec {
        boot: BootSource::DirectKernel {
            kernel: PathBuf::from("/boot/vmlinuz"),
            initrd: PathBuf::from("/boot/initrd.img"),
            cmdline: "console=ttyS0 quiet panic=1".to_string(),
        },
        ..VmSpec::from_disk(PathBuf::from("/unused"))
    };
    let cfg = build_vm_config(&spec).expect("direct-kernel build succeeds without firmware");
    assert_eq!(cfg.payload.kernel.as_deref(), Some("/boot/vmlinuz"));
    assert_eq!(cfg.payload.initramfs.as_deref(), Some("/boot/initrd.img"));
    assert_eq!(
        cfg.payload.cmdline.as_deref(),
        Some("console=ttyS0 quiet panic=1"),
    );
    assert!(
        cfg.payload.firmware.is_none(),
        "direct-kernel needs no firmware payload",
    );
    assert!(cfg.disks.is_none(), "no disk on the direct-kernel path");
}

#[test]
fn build_vm_config_split_firmware_uses_code_half() {
    // Cloud Hypervisor has no separate VARS pflash; the split layout
    // collapses to its CODE path as the single firmware payload.
    let spec = VmSpec {
        boot: BootSource::Disk {
            path: PathBuf::from("/tmp/disk.img"),
            firmware: Some(Firmware::Pflash {
                code: PathBuf::from("/usr/share/OVMF/OVMF_CODE.fd"),
                vars: PathBuf::from("/usr/share/OVMF/OVMF_VARS.fd"),
            }),
        },
        ..VmSpec::from_disk(PathBuf::from("/unused"))
    };
    let cfg = build_vm_config(&spec).expect("should build");
    assert_eq!(
        cfg.payload.firmware.as_deref(),
        Some("/usr/share/OVMF/OVMF_CODE.fd"),
    );
}

#[test]
fn build_vm_config_propagates_cpus_and_memory() {
    let spec = VmSpec {
        boot: BootSource::Disk {
            path: PathBuf::from("/disk"),
            firmware: Some(Firmware::Bios(PathBuf::from("/fw"))),
        },
        memory_mib: 2048,
        cpus: 4,
        ..VmSpec::from_disk(PathBuf::from("/unused"))
    };
    let cfg = build_vm_config(&spec).expect("build");
    let cpus = cfg.cpus.expect("cpus set");
    assert_eq!(cpus.boot_vcpus, 4_i32);
    let mem = cfg.memory.expect("memory set");
    assert_eq!(mem.size, 2048_i64 * 1024 * 1024);
}

#[test]
fn build_vm_config_rejects_port_forwards_without_a_tap_net() {
    // Port-forwards reach CH only through `umf run`'s tap orchestration
    // (`spec.net`). Bare port_forwards with no net would be silently dropped.
    let spec = VmSpec {
        boot: BootSource::Disk {
            path: PathBuf::from("/disk"),
            firmware: Some(Firmware::Bios(PathBuf::from("/fw"))),
        },
        port_forwards: vec![crate::runtime::PortForward {
            host_port: 8080,
            guest_port: 80,
            tcp: true,
        }],
        ..VmSpec::from_disk(PathBuf::from("/unused"))
    };
    let err = build_vm_config(&spec).expect_err("port forwards without a net must reject");
    let msg = err.to_string();
    assert!(msg.contains("port forwarding"), "got: {msg}");
    assert!(msg.contains("qemu"), "got: {msg}");
}

#[test]
fn build_vm_config_attaches_the_tap_from_net() {
    let spec = VmSpec {
        boot: BootSource::DirectKernel {
            kernel: PathBuf::from("/k"),
            initrd: PathBuf::from("/i"),
            cmdline: "console=ttyS0".to_string(),
        },
        net: Some(crate::runtime::TapNet {
            netns_fd: -1,
            tap: "umftap1".to_string(),
        }),
        ..VmSpec::from_disk(PathBuf::from("/unused"))
    };
    let cfg = build_vm_config(&spec).expect("net wiring builds");
    let nets = cfg.net.expect("net set");
    assert_eq!(nets.len(), 1);
    assert_eq!(nets[0].tap.as_deref(), Some("umftap1"));
}
