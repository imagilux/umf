//! Environment-variable list utilities shared across the engine.
//!
//! Both the image-run path ([`crate::run`]) and the build-step backend
//! ([`crate::backends`]) merge a base environment with caller-supplied
//! overrides. They previously carried separate `merge_env` copies with
//! *divergent* ordering semantics — a replaced key kept its position on
//! one path but jumped to the tail on the other — so a `RUN` step and an
//! `umf run` invocation could order `$PATH` differently. This module is
//! the single source of truth.

/// Merge a base environment (left) with caller overrides (right).
///
/// An override `KEY=...` whose key matches an existing entry replaces that
/// entry **in place**, preserving its relative position; a novel key is
/// appended. Override order among novel keys is preserved. An entry without
/// a `=` is treated as a bare key and appended unchanged (matching how
/// `docker run` tolerates malformed `--env` strings).
pub(crate) fn merge_env(
    base: Vec<String>,
    overrides: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut out = base;
    for o in overrides {
        let Some(key) = o.split('=').next() else {
            continue;
        };
        let prefix = format!("{key}=");
        if let Some(existing) = out.iter_mut().find(|e| e.starts_with(&prefix)) {
            *existing = o;
        } else {
            out.push(o);
        }
    }
    out
}

#[cfg(test)]
mod tests;
