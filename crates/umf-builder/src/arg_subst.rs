//! Build-time `ARG` (`${VAR}` / `$VAR`) substitution scope.
//!
//! Shared by the container engine ([`crate::engine_build`]) and the bootable
//! builder ([`crate::bootable`]) so a `${VAR}` means the same thing on both
//! targets: it resolves against the build-global `ARG` scope (pre-`FROM` `ARG`s
//! resolved against `--build-arg`), extended positionally by any in-stage `ARG`.
//!
//! The precedence for a single `ARG` is fixed here — a `--build-arg` override,
//! then the declared default, then any value already in scope — so both targets
//! agree, and there is one place to read it. The actual expansion is the pure,
//! IO-free [`umf_core::subst::substitute`].
//!
//! Substitution drives the value a directive *executes / stores* and is folded
//! into the layer-cache key (so a changed `ARG` rebuilds correctly); it must
//! never enter the image history — callers keep the original `${VAR}` text there
//! (the no-leak guarantee).

use std::collections::BTreeMap;

use umf_core::ast::{Arg, Ast};

/// Resolve the build-global `ARG` scope from an [`Ast`]'s pre-`FROM` `ARG`s.
///
/// Each global `ARG`'s value is its `--build-arg` override if one was supplied,
/// else its declared default; an `ARG` with neither is left out of scope, so a
/// reference to it stays verbatim. Globals are applied in source order, so a
/// later global may inherit an earlier one's value when re-declared without a
/// default (via [`apply_arg_to_scope`]'s in-scope fallback).
pub(crate) fn resolve_global_args(
    ast: &Ast,
    build_args: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut scope = BTreeMap::new();
    for arg in &ast.global_args {
        apply_arg_to_scope(&mut scope, arg, build_args);
    }
    scope
}

/// Apply one `ARG` to a substitution `scope` positionally (Docker semantics).
///
/// Precedence: a `--build-arg` override, then the `ARG`'s declared default, then
/// any value already in scope (e.g. a build-global of the same name re-declared
/// without a default). An `ARG` with none of these leaves the name unset, so
/// references to it stay verbatim rather than expanding to the empty string.
pub(crate) fn apply_arg_to_scope(
    scope: &mut BTreeMap<String, String>,
    arg: &Arg,
    build_args: &BTreeMap<String, String>,
) {
    let name = arg.name.value.as_str();
    let value = build_args
        .get(name)
        .cloned()
        .or_else(|| arg.default.as_ref().map(|d| d.value.as_str().to_string()))
        .or_else(|| scope.get(name).cloned());
    if let Some(v) = value {
        scope.insert(name.to_string(), v);
    }
}

/// Expand `${VAR}` / `$VAR` references in `text` against `scope`. Unknown names
/// are left verbatim (so shell constructs like `$HOME` / `$(date)` survive).
pub(crate) fn subst_with(scope: &BTreeMap<String, String>, text: &str) -> String {
    umf_core::subst::substitute(text, |n| scope.get(n).map(String::as_str))
}

#[cfg(test)]
mod tests;
