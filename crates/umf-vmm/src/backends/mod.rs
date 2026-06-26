//! Per-VMM backend implementations of [`crate::VmRuntime`].

pub mod cloud_hypervisor;
pub(crate) mod common;
pub mod qemu;

pub use cloud_hypervisor::CloudHypervisorRuntime;
pub use qemu::QemuRuntime;
