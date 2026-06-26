//! Unit tests for the `user` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

// ── Username ────────────────────────────────────────────────────────────

#[test]
fn username_accepts_login_name() {
    Username::new("nginx").unwrap();
    Username::new("_systemd").unwrap();
    Username::new("a").unwrap();
}

#[test]
fn username_accepts_trailing_dollar() {
    Username::new("nobody$").unwrap();
}

#[test]
fn username_accepts_numeric_uid() {
    Username::new("0").unwrap();
    Username::new("1000").unwrap();
    Username::new("4294967295").unwrap();
}

#[test]
fn username_accepts_user_group() {
    Username::new("nginx:nginx").unwrap();
    Username::new("0:0").unwrap();
    Username::new("nginx:1000").unwrap();
    Username::new("1000:wheel").unwrap();
}

#[test]
fn username_rejects_empty() {
    assert!(matches!(Username::new(""), Err(UsernameError::Empty)));
}

#[test]
fn username_rejects_leading_digit_in_login() {
    // "9nginx" — not all-digit, so treated as login name and rejected
    // by the leading-char rule.
    let err = Username::new("9nginx").unwrap_err();
    assert!(matches!(
        err,
        UsernameError::InvalidLeadingChar { ch: '9', offset: 0 }
    ));
}

#[test]
fn username_rejects_uppercase() {
    let err = Username::new("Nginx").unwrap_err();
    assert!(matches!(
        err,
        UsernameError::InvalidLeadingChar { ch: 'N', .. }
    ));
}

#[test]
fn username_rejects_dash_start() {
    let err = Username::new("-bad").unwrap_err();
    assert!(matches!(
        err,
        UsernameError::InvalidLeadingChar { ch: '-', .. }
    ));
}

#[test]
fn username_rejects_too_long() {
    let long = "a".repeat(33);
    let err = Username::new(long).unwrap_err();
    assert!(matches!(err, UsernameError::TooLong { len: 33, .. }));
}

#[test]
fn username_rejects_empty_group() {
    let err = Username::new("nginx:").unwrap_err();
    assert!(matches!(err, UsernameError::EmptyGroup { offset: 6 }));
}

#[test]
fn username_rejects_empty_user() {
    let err = Username::new(":wheel").unwrap_err();
    assert!(matches!(err, UsernameError::EmptyUser));
}

#[test]
fn username_rejects_numeric_overflow() {
    let err = Username::new("9999999999").unwrap_err();
    assert!(matches!(err, UsernameError::NumericOverflow { .. }));
}

#[test]
fn username_rejects_invalid_char_in_group() {
    let err = Username::new("nginx:bad/group").unwrap_err();
    assert!(matches!(
        err,
        UsernameError::InvalidChar { ch: '/', offset: 9 }
    ));
}
