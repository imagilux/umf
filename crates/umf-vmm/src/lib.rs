//! UMF VMM abstraction — a trait-shaped seam between the CLI / builder
//! and the per-VMM control surfaces (QEMU via QMP, Cloud Hypervisor via
//! its REST API).
//!
//! ## Position in the dependency tree
//!
//! ```text
//! umf-core
//!   └── umf-oci
//!         └── umf-vmm           (this crate — VmRuntime trait + backends)
//!               └── umf-builder
//! ```
//!
//! `umf-vmm` deliberately does **not** depend on `umf-oci` or
//! `umf-engine` — it's a pure VMM control layer. Callers (the CLI, the
//! builder) feed it a [`VmSpec`] and get back a [`VmHandle`] they can
//! drive through the [`VmRuntime`] trait. Image / disk resolution is
//! the caller's job.
//!
//! ## Backends
//!
//! - [`backends::qemu::QemuRuntime`] — QEMU/KVM, controlled post-spawn
//!   via [QMP](https://qemu.readthedocs.io/en/latest/interop/qmp-intro.html).
//!   The single `Command::new("qemu-system-<arch>")` invocation lives
//!   inside [`backends::qemu::spawn`]; every subsequent interaction
//!   (boot-ready detection, status queries, graceful shutdown) goes
//!   through the typed `qapi` crate.
//! - [`backends::cloud_hypervisor::CloudHypervisorRuntime`] — a sibling
//!   backend behind the same trait, using the OpenAPI-generated
//!   `cloud-hypervisor-client` over a Unix REST socket. Its single
//!   subprocess spawn lives in [`backends::cloud_hypervisor::spawn`];
//!   every post-spawn interaction goes through the typed REST client.
//!
//! ## Why a separate crate
//!
//! Both the build-path (per-RUN micro-VMs in `umf-builder/src/vm_runner.rs`)
//! and the run-path (booting a finished VM image from `umf run`) need
//! the same QEMU plumbing. Pulling the VMM control surface out of the
//! builder gives both call sites a typed Rust API rather than scattered
//! `Command::new("qemu-system-*")` calls, and is a prerequisite for
//! adding Cloud Hypervisor without duplicating spawn logic.
//!
//! The run-path lives in this crate today; the build-path's
//! `vm_runner.rs` migration is tracked as a follow-up (its
//! direct-kernel + 9p + marker-file lifecycle is structurally
//! different from the disk + QMP lifecycle of `umf run`, so the
//! migration deserves its own design pass to avoid forcing one
//! shape into the other).

pub mod backends;
pub mod error;
pub mod handle;
pub mod runtime;

pub use error::VmError;
pub use handle::VmHandle;
pub use runtime::{
    BootSource, ControlMode, DisplayMode, Firmware, NinePShare, PortForward, SerialMode, TapNet,
    VmArch, VmInfo, VmRuntime, VmSpec, VmStatus,
};
