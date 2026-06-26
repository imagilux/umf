//! Identifier-class newtypes — label keys, env/arg names, stage names,
//! secret ids, service unit names, usernames.
//!
//! Each type validates only the syntax of its argument and stores the value
//! as a private `String` so consumers (parser tests, builder, render) keep
//! working through [`AsRef<str>`], [`Display`](std::fmt::Display), and the
//! `as_str` accessor.
//!
//! The newtypes are grouped by domain into submodules and re-exported here so
//! every type stays reachable at `crate::types::identifier::*` (and, via the
//! parent module's re-export, at `crate::types::*`).

mod env;
mod label;
mod stage;
mod unit;
mod user;

pub use env::{EnvVarName, EnvVarNameError, EnvVarValue, EnvVarValueError};
pub use label::{LabelKey, LabelKeyError, LabelValue, LabelValueError};
pub use stage::{SecretId, SecretIdError, StageName, StageNameError};
pub use unit::{ServiceUnitName, ServiceUnitNameError, UnitSuffix};
pub use user::{Username, UsernameError};

/// Reject an embedded NUL byte in an otherwise-opaque value.
///
/// Shared by the value newtypes ([`EnvVarValue`], [`LabelValue`]) whose only
/// constraint is the absence of a NUL — the kernel's argv separator, which also
/// breaks OCI manifest serialization. Returns the byte offset of the first NUL
/// so each caller can map it onto its own `NulByte` error variant.
fn reject_nul(raw: &str) -> Result<(), usize> {
    match raw.find('\0') {
        Some(pos) => Err(pos),
        None => Ok(()),
    }
}
