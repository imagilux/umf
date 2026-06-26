//! Shared types for the UMF reference implementation.
//!
//! This crate hosts the AST, L0 introspection, and
//! `org.imagilux.umf.*` OCI label namespace constants. It has no external IO. Both `umf-parser` and
//! `umf-builder` depend on this crate and nothing more — keeping the dependency
//! graph a strict tree with the AST as the shared contract.

pub mod architecture;
pub mod ast;
pub mod boot;
pub mod l0;
pub mod label;
pub mod subst;
pub mod types;
