//! UMF AST → OCI artifacts.
//!
//! Owns L0 introspection, FROM resolution, container-target RUN execution
//! through [`umf_engine`], sparse disk image emission, nftables rule
//! generation, and secret mount handling.
//!
//! OCI primitives (image emission, registry client, on-disk layout cache,
//! tar-archive import, build staging) live in the sibling crate
//! [`umf_oci`]; the runtime substrate (libcontainer-backed RUN execution,
//! overlayfs management) lives in [`umf_engine`]. This crate consumes
//! both.
//!
//! Depends on `umf-core` + `umf-oci` + `umf-engine`; deliberately does
//! **not** depend on `umf-parser` — the AST is the shared interface
//! between the two.

mod arg_subst;
pub mod bench;
pub mod bootable;
pub mod engine_build;
mod fsutil;
pub mod host_requirements;
pub mod initrd;
pub mod introspect;
pub mod kernel;
pub mod metrics;
pub mod resolver;
pub mod runtime_config;
pub mod runtime_writer;
pub mod vm_runner;
