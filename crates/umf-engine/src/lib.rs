//! UMF-native container build engine.
//!
//! Materialises an OCI image (pulled into a [`umf_oci::registry::ImageLayout`])
//! into a runnable OCI bundle, executes a single command in it via a
//! configurable [`runtime::ContainerRuntime`] backend, captures the
//! upper-layer filesystem diff, and hands it back to the caller (typically
//! `umf-builder`) so it can be packed into a new image layer.
//!
//! ## Position in the dependency tree
//!
//! ```text
//! umf-core
//!   └── umf-oci         (OCI primitives — layout, manifest, layer emission)
//!         └── umf-engine    (this crate — bundle prep + RUN execution)
//!               └── umf-builder  (target lowering, recipe → engine calls)
//! ```
//!
//! `umf-engine` deliberately does **not** depend on `umf-parser` or `umf-builder`
//! — recipe AST and target lowering are concerns of the caller. The engine
//! exposes a low-level primitive: "run argv in a container built from this
//! image, give me back the exit status and the upper-dir filesystem delta".
//!
//! ## Module map
//!
//! - [`bundle`] — turn a pulled image into an on-disk OCI bundle (rootfs
//!   directory + `config.json`) ready for a runc-compatible runtime to
//!   consume. No execution — this is the producer side.
//! - [`runtime`] — the [`ContainerRuntime`] trait
//!   plus the [`RunSpec`] / [`RunOutcome`]
//!   value types. Backends implement this trait.
//! - [`backends`] — backend implementations. Currently a no-op stub
//!   ([`backends::NoopRuntime`]) for tests/dry-runs and a real
//!   libcontainer-backed runtime ([`backends::LibcontainerRuntime`]) for
//!   actual execution.
//! - [`overlay`] — kernel overlayfs setup so RUN-step writes land in a
//!   captured upper-dir the caller can package as a layer.
//! - [`rootless`] — enter one user namespace at process start so an
//!   unprivileged build (overlay + youki) runs inside it, single-id mapped.
//!   Exposes the process-wide [`rootless::RootlessContext`].
//! - [`erofs`] — mount erofs-encoded base layers (produced by
//!   [`umf_oci::erofs`]) read-only as overlayfs lowers, instead of
//!   unpacking every layer into a directory tree per build.
//! - [`run`] — user-facing "run a pre-built image" entrypoint that powers
//!   `umf run <ref>`. Translates an image's ENTRYPOINT + CMD into a
//!   runtime spec (with caller overrides on top) and drives libcontainer
//!   end-to-end.
//! - [`seccomp`] — the vendored containerd/Docker default seccomp profile
//!   (deny-by-default + safe-syscall allowlist) applied to every RUN step's
//!   runtime spec.
//! - [`error`] — [`EngineError`] is the crate's single
//!   public error type; backends map their native errors into it.

pub mod backends;
pub mod bundle;
mod env;
pub mod erofs;
pub mod error;
pub mod hooks;
pub mod lsm;
mod mount_util;
pub mod overlay;
pub mod rootless;
pub mod run;
pub mod runtime;
pub mod seccomp;

pub use error::EngineError;
pub use hooks::{BuildHook, HookAction, NoopHook, SharedHook, StepInfo, StepKind, noop_shared};
pub use rootless::{RootlessContext, context as rootless_context, enter as enter_rootless_userns};
pub use run::{RunOptions, RunResult, run_image};
pub use runtime::{ContainerRuntime, RunOutcome, RunSpec};
