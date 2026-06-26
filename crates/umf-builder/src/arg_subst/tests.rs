//! Unit tests for the shared `ARG` substitution scope.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;

use umf_core::ast::{Arg, Ast, Span, Spanned};
use umf_core::types::{EnvVarName, EnvVarValue};

use super::{apply_arg_to_scope, resolve_global_args, subst_with};

fn arg(name: &str, default: Option<&str>) -> Arg {
    Arg {
        name: Spanned::new(EnvVarName::new(name).unwrap(), Span::new(0, 0)),
        default: default.map(|d| Spanned::new(EnvVarValue::new(d).unwrap(), Span::new(0, 0))),
        span: Span::new(0, 0),
    }
}

fn no_build_args() -> BTreeMap<String, String> {
    BTreeMap::new()
}

#[test]
fn default_seeds_scope() {
    let mut scope = BTreeMap::new();
    apply_arg_to_scope(&mut scope, &arg("TAG", Some("3.21")), &no_build_args());
    assert_eq!(scope.get("TAG").map(String::as_str), Some("3.21"));
}

#[test]
fn build_arg_overrides_default() {
    let mut scope = BTreeMap::new();
    let mut build_args = BTreeMap::new();
    build_args.insert("TAG".to_string(), "edge".to_string());
    apply_arg_to_scope(&mut scope, &arg("TAG", Some("3.21")), &build_args);
    assert_eq!(scope.get("TAG").map(String::as_str), Some("edge"));
}

#[test]
fn arg_without_default_or_override_stays_unset() {
    let mut scope = BTreeMap::new();
    apply_arg_to_scope(&mut scope, &arg("TAG", None), &no_build_args());
    assert!(!scope.contains_key("TAG"), "scope: {scope:?}");
}

#[test]
fn redeclare_without_default_preserves_existing() {
    // `ARG X=1` then a bare `ARG X` keeps 1 — the in-scope fallback. This is
    // why globals can be resolved through `apply_arg_to_scope`.
    let mut scope = BTreeMap::new();
    apply_arg_to_scope(&mut scope, &arg("X", Some("1")), &no_build_args());
    apply_arg_to_scope(&mut scope, &arg("X", None), &no_build_args());
    assert_eq!(scope.get("X").map(String::as_str), Some("1"));
}

#[test]
fn build_arg_for_undeclared_default_still_wins_on_redeclare() {
    // A bare in-stage `ARG X` with a `--build-arg X=...` picks up the override
    // even with no declared default (Docker semantics).
    let mut scope = BTreeMap::new();
    let mut build_args = BTreeMap::new();
    build_args.insert("X".to_string(), "fromcli".to_string());
    apply_arg_to_scope(&mut scope, &arg("X", None), &build_args);
    assert_eq!(scope.get("X").map(String::as_str), Some("fromcli"));
}

#[test]
fn resolve_global_args_reads_pre_from_args() {
    let ast = Ast {
        global_args: vec![arg("A", Some("1")), arg("B", Some("2")), arg("C", None)],
        stages: Vec::new(),
    };
    let scope = resolve_global_args(&ast, &no_build_args());
    assert_eq!(scope.get("A").map(String::as_str), Some("1"));
    assert_eq!(scope.get("B").map(String::as_str), Some("2"));
    // C has no default and no override — left out of scope.
    assert!(!scope.contains_key("C"), "scope: {scope:?}");
}

#[test]
fn subst_with_expands_known_and_keeps_unknown() {
    let mut scope = BTreeMap::new();
    scope.insert("TAG".to_string(), "3.21".to_string());
    assert_eq!(subst_with(&scope, "alpine:${TAG}"), "alpine:3.21");
    // Unknown names pass through verbatim (shell `$HOME`, `${MISSING}`).
    assert_eq!(subst_with(&scope, "echo $HOME"), "echo $HOME");
    assert_eq!(subst_with(&scope, "${MISSING}"), "${MISSING}");
}
