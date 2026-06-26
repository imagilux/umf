//! Concrete [`crate::runtime::ContainerRuntime`] backends.
//!
//! - [`noop`] — a no-op backend that exercises the trait wiring without
//!   launching a real container; useful for tests and environments
//!   without namespace support.
//! - [`libcontainer`] — youki-backed real container execution
//!   (kernel overlayfs + namespaces + cgroups). Requires `CAP_SYS_ADMIN`
//!   for the overlay mount in this revision; rootless support lands in
//!   a follow-up.

pub mod libcontainer;
pub mod noop;

pub use libcontainer::LibcontainerRuntime;
pub use noop::NoopRuntime;
