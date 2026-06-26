//! Unit tests for the `qemu` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn default_runtime_has_no_pinned_binary() {
    // No explicit binary ⇒ derived per-spec at spawn time.
    let rt = QemuRuntime::default();
    assert_eq!(rt.binary(), None);
}

#[test]
fn default_runtime_derives_binary_from_spec_arch() {
    use crate::runtime::VmArch;
    let rt = QemuRuntime::default();
    let mut spec = VmSpec::from_disk(std::path::PathBuf::from("/disk.img"));
    spec.arch = VmArch::X86_64;
    assert_eq!(rt.binary_for(&spec), "qemu-system-x86_64");
    spec.arch = VmArch::Aarch64;
    assert_eq!(rt.binary_for(&spec), "qemu-system-aarch64");
}

#[test]
fn with_binary_pins_and_overrides_spec_arch() {
    use crate::runtime::VmArch;
    let rt = QemuRuntime::with_binary("/opt/qemu/bin/qemu-system-aarch64");
    assert_eq!(rt.binary(), Some("/opt/qemu/bin/qemu-system-aarch64"));
    // A pinned binary wins even if the spec says x86_64.
    let mut spec = VmSpec::from_disk(std::path::PathBuf::from("/disk.img"));
    spec.arch = VmArch::X86_64;
    assert_eq!(rt.binary_for(&spec), "/opt/qemu/bin/qemu-system-aarch64");
}

#[test]
fn run_state_mapping_covers_expected_buckets() {
    assert_eq!(run_state_to_vm_status(RunState::running), VmStatus::Running);
    assert_eq!(run_state_to_vm_status(RunState::paused), VmStatus::Paused);
    assert_eq!(
        run_state_to_vm_status(RunState::prelaunch),
        VmStatus::Paused
    );
    assert_eq!(
        run_state_to_vm_status(RunState::shutdown),
        VmStatus::ShuttingDown,
    );
    assert_eq!(
        run_state_to_vm_status(RunState::guest_panicked),
        VmStatus::ShuttingDown,
    );
    assert_eq!(
        run_state_to_vm_status(RunState::internal_error),
        VmStatus::ShuttingDown,
    );
    assert_eq!(run_state_to_vm_status(RunState::debug), VmStatus::Booting);
    assert_eq!(
        run_state_to_vm_status(RunState::inmigrate),
        VmStatus::Booting,
    );
}
