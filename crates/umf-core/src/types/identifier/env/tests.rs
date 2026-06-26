//! Unit tests for the `env` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

// ── EnvVarName ──────────────────────────────────────────────────────────

#[test]
fn env_var_name_accepts_canonical() {
    EnvVarName::new("PATH").unwrap();
    EnvVarName::new("_LEADING_UNDERSCORE").unwrap();
    EnvVarName::new("With_Mixed_Case_99").unwrap();
}

#[test]
fn env_var_name_rejects_empty() {
    assert!(matches!(EnvVarName::new(""), Err(EnvVarNameError::Empty)));
}

#[test]
fn env_var_name_rejects_leading_digit() {
    let err = EnvVarName::new("2BAD").unwrap_err();
    assert!(matches!(
        err,
        EnvVarNameError::InvalidLeadingChar { ch: '2' }
    ));
    assert_eq!(err.offset(), Some(0));
}

#[test]
fn env_var_name_rejects_dash() {
    let err = EnvVarName::new("HAS-DASH").unwrap_err();
    assert!(matches!(
        err,
        EnvVarNameError::InvalidChar { ch: '-', offset: 3 }
    ));
}

// ── EnvVarValue ─────────────────────────────────────────────────────────

#[test]
fn env_var_value_accepts_anything_utf8() {
    EnvVarValue::new("/usr/bin:/usr/local/bin").unwrap();
    EnvVarValue::new("").unwrap();
    EnvVarValue::new("éñ ✨").unwrap();
}

#[test]
fn env_var_value_rejects_nul() {
    let err = EnvVarValue::new("a\0b").unwrap_err();
    assert!(matches!(err, EnvVarValueError::NulByte { offset: 1 }));
}
