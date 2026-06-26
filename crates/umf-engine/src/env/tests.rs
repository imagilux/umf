//! Unit tests for the `env` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn env_override_replaces_matching_key() {
    let merged = merge_env(
        vec!["PATH=/usr/bin".into(), "FOO=bar".into()],
        vec!["FOO=baz".into()],
    );
    assert_eq!(merged, vec!["PATH=/usr/bin", "FOO=baz"]);
}

#[test]
fn env_override_appends_new_key() {
    let merged = merge_env(
        vec!["PATH=/usr/bin".into()],
        vec!["FOO=bar".into(), "BAR=baz".into()],
    );
    assert_eq!(merged, vec!["PATH=/usr/bin", "FOO=bar", "BAR=baz"]);
}

#[test]
fn env_override_preserves_image_relative_order() {
    let merged = merge_env(
        vec!["A=1".into(), "B=2".into(), "C=3".into()],
        vec!["B=replaced".into(), "D=4".into()],
    );
    assert_eq!(merged, vec!["A=1", "B=replaced", "C=3", "D=4"]);
}

#[test]
fn malformed_override_without_equals_is_appended_unchanged() {
    // No `=` ⇒ not a valid env entry; we treat the whole string as the
    // key and append it (matching `docker run`'s tolerance). The point
    // of the test is that a malformed entry never derails a following
    // well-formed one.
    let merged = merge_env(vec!["A=1".into()], vec!["NOEQUALS".into(), "B=2".into()]);
    assert!(merged.contains(&"A=1".to_string()));
    assert!(merged.contains(&"B=2".to_string()));
}
