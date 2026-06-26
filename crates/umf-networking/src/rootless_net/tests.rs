//! Unit tests for rootless egress mode parsing. The pasta backend and
//! `RootlessNet::setup` need a real namespace + the `pasta` binary, so they are
//! exercised by the binary-driven rootless CI lane, not here.

#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn egress_mode_parses_canonical_and_case_insensitively() {
    assert_eq!("none".parse::<EgressMode>().unwrap(), EgressMode::None);
    assert_eq!("pasta".parse::<EgressMode>().unwrap(), EgressMode::Pasta);
    assert_eq!("native".parse::<EgressMode>().unwrap(), EgressMode::Native);
    assert_eq!(
        "  PaStA  ".parse::<EgressMode>().unwrap(),
        EgressMode::Pasta
    );
}

#[test]
fn egress_mode_rejects_unknown() {
    let err = "bogus".parse::<EgressMode>().unwrap_err();
    assert!(err.to_string().contains("bogus"));
    assert!(err.to_string().contains("none"));
}

#[test]
fn egress_mode_default_is_native() {
    // The sovereign in-process backend is the default.
    assert_eq!(EgressMode::default(), EgressMode::Native);
}
