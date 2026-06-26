//! Unit tests for the `stage` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

// ── StageName ───────────────────────────────────────────────────────────

#[test]
fn stage_name_accepts_identifiers() {
    StageName::new("builder").unwrap();
    StageName::new("runtime-stage").unwrap();
    StageName::new("_internal").unwrap();
    StageName::new("S1").unwrap();
}

#[test]
fn stage_name_rejects_leading_digit() {
    let err = StageName::new("1stage").unwrap_err();
    assert!(matches!(
        err,
        StageNameError::InvalidLeadingChar { ch: '1' }
    ));
}

#[test]
fn stage_name_rejects_dot() {
    let err = StageName::new("stage.one").unwrap_err();
    assert!(matches!(
        err,
        StageNameError::InvalidChar { ch: '.', offset: 5 }
    ));
}

// ── SecretId ────────────────────────────────────────────────────────────

#[test]
fn secret_id_accepts_identifier() {
    SecretId::new("signing-key").unwrap();
}

#[test]
fn secret_id_rejects_slash() {
    let err = SecretId::new("group/key").unwrap_err();
    assert!(matches!(err, SecretIdError::InvalidChar { ch: '/', .. }));
}
