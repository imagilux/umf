//! Unit tests for the `oci` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

/// Each valid form must be accepted and preserved verbatim. `OciReference`
/// is a validated newtype, so we assert acceptance + round-trip rather than
/// the (no longer stored) host/port/repository/tag/digest decomposition.
#[test]
fn accepts_valid_references() {
    const DIGEST: &str = "sha256:bc1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcd";
    for raw in [
        "alpine",                         // bare repository, no tag
        "alpine:3.21",                    // simple tag
        "imagilux/kernel-linux:7.0",      // org/image path + tag
        "quay.io:5000/imagilux/k:7.0",    // host:port/path:tag
        "localhost/myimage:dev",          // localhost host
        &format!("alpine@{DIGEST}"),      // digest only
        &format!("alpine:3.21@{DIGEST}"), // tag + digest
    ] {
        let r = OciReference::new(raw).expect("valid reference");
        assert_eq!(r.as_str(), raw);
    }
}

#[test]
fn rejects_empty() {
    assert!(matches!(
        OciReference::new(""),
        Err(OciReferenceError::Empty)
    ));
}

#[test]
fn rejects_uppercase_path() {
    let err = OciReference::new("Alpine:3.21").unwrap_err();
    assert!(matches!(
        err,
        OciReferenceError::PathComponentBadEdgeChar { ch: 'A', .. }
    ));
}

#[test]
fn rejects_empty_tag() {
    assert!(matches!(
        OciReference::new("alpine:").unwrap_err(),
        OciReferenceError::EmptyTag { .. }
    ));
}

#[test]
fn rejects_bad_tag_char() {
    // `quay.io/img:bad/tag` — the `/` inside the would-be tag means our
    // parser reclassifies the trailing `:` as a host-port separator, so
    // the failure comes from the host part. The point is the same: this
    // shape is rejected.
    let err = OciReference::new("quay.io/img:bad/tag").unwrap_err();
    assert!(
        matches!(
            err,
            OciReferenceError::PathComponentBadChar { .. }
                | OciReferenceError::TagBadChar { .. }
                | OciReferenceError::HostPortInvalid { .. }
                | OciReferenceError::PathComponentBadEdgeChar { .. }
        ),
        "got: {err:?}"
    );
}

#[test]
fn rejects_bad_port() {
    let err = OciReference::new("quay.io:bad/img:1").unwrap_err();
    assert!(matches!(err, OciReferenceError::HostPortInvalid { .. }));
}

#[test]
fn rejects_double_underscore_run_long() {
    // `___` (three underscores) violates the separator rule (max __).
    assert!(matches!(
        OciReference::new("foo___bar").unwrap_err(),
        OciReferenceError::PathComponentBadSeparator { .. }
    ));
}

#[test]
fn rejects_double_dot_in_path() {
    assert!(matches!(
        OciReference::new("foo..bar").unwrap_err(),
        OciReferenceError::PathComponentBadSeparator { .. }
    ));
}

#[test]
fn rejects_digest_too_short() {
    assert!(matches!(
        OciReference::new("alpine@sha256:abc").unwrap_err(),
        OciReferenceError::DigestHexTooShort { len: 3, .. }
    ));
}

#[test]
fn rejects_digest_bad_hex() {
    let err = OciReference::new(
        "alpine@sha256:zz1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcd",
    )
    .unwrap_err();
    assert!(matches!(
        err,
        OciReferenceError::DigestBadHexChar { ch: 'z', .. }
    ));
}
