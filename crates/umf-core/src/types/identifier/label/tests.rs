//! Unit tests for the `label` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

// ── LabelKey ────────────────────────────────────────────────────────────

#[test]
fn label_key_accepts_reverse_dns() {
    let k = LabelKey::new("org.imagilux.umf.author").unwrap();
    assert_eq!(k.as_str(), "org.imagilux.umf.author");
    assert_eq!(k.to_string(), "org.imagilux.umf.author");
}

#[test]
fn label_key_accepts_single_segment() {
    LabelKey::new("name").unwrap();
    LabelKey::new("name-with-dashes").unwrap();
}

#[test]
fn label_key_rejects_empty() {
    assert!(matches!(LabelKey::new(""), Err(LabelKeyError::Empty)));
}

#[test]
fn label_key_rejects_leading_digit() {
    let err = LabelKey::new("2bad").unwrap_err();
    assert!(matches!(
        err,
        LabelKeyError::InvalidSegmentStart { ch: '2', offset: 0 }
    ));
    assert_eq!(err.offset(), Some(0));
}

#[test]
fn label_key_rejects_uppercase() {
    let err = LabelKey::new("Org.bad").unwrap_err();
    assert!(matches!(err, LabelKeyError::InvalidSegmentStart { .. }));
}

#[test]
fn label_key_rejects_trailing_dot() {
    let err = LabelKey::new("foo.").unwrap_err();
    assert!(matches!(
        err,
        LabelKeyError::TrailingSeparator { offset: 3 }
    ));
}

#[test]
fn label_key_rejects_double_dot() {
    let err = LabelKey::new("foo..bar").unwrap_err();
    assert!(matches!(
        err,
        LabelKeyError::ConsecutiveSeparators { offset: 4 }
    ));
}

#[test]
fn label_key_rejects_segment_starting_with_digit() {
    let err = LabelKey::new("foo.2bar").unwrap_err();
    assert!(matches!(
        err,
        LabelKeyError::InvalidSegmentStart { ch: '2', offset: 4 }
    ));
}

#[test]
fn label_key_rejects_underscore() {
    let err = LabelKey::new("foo_bar").unwrap_err();
    assert!(matches!(
        err,
        LabelKeyError::InvalidChar { ch: '_', offset: 3 }
    ));
}

// ── LabelValue ──────────────────────────────────────────────────────────

#[test]
fn label_value_accepts_arbitrary_utf8() {
    LabelValue::new("hello world — émoji 🚀").unwrap();
    LabelValue::new("").unwrap();
}

#[test]
fn label_value_rejects_nul() {
    let err = LabelValue::new("foo\0bar").unwrap_err();
    assert!(matches!(err, LabelValueError::NulByte { offset: 3 }));
}
