//! Unit tests for the `url` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn accepts_https() {
    let u = HttpsUrl::new("https://example.com/foo").unwrap();
    assert!(u.is_https());
    assert_eq!(u.authority_and_path(), "example.com/foo");
}

#[test]
fn accepts_http() {
    let u = HttpsUrl::new("http://example.com/").unwrap();
    assert!(!u.is_https());
}

#[test]
fn rejects_missing_scheme() {
    assert!(matches!(
        HttpsUrl::new("example.com").unwrap_err(),
        HttpsUrlError::MissingScheme
    ));
}

#[test]
fn rejects_ftp_scheme() {
    assert!(matches!(
        HttpsUrl::new("ftp://example.com").unwrap_err(),
        HttpsUrlError::MissingScheme
    ));
}

#[test]
fn rejects_empty_authority() {
    assert!(matches!(
        HttpsUrl::new("https://").unwrap_err(),
        HttpsUrlError::EmptyAuthority
    ));
}

#[test]
fn rejects_nul() {
    let err = HttpsUrl::new("https://e\0x.com").unwrap_err();
    assert!(matches!(err, HttpsUrlError::NulByte { offset: 9 }));
}

#[test]
fn placeholder_url_parses_leniently() {
    // `ADD https://host/${VER}.tar /` — accepted now, resolved post-substitution.
    let u = HttpsUrl::new_allowing_placeholders("https://host/${VER}.tar").unwrap();
    assert_eq!(u.as_str(), "https://host/${VER}.tar");
    assert!(u.is_https());
    // A placeholder in the authority is tolerated too.
    let u = HttpsUrl::new_allowing_placeholders("http://${HOST}/x").unwrap();
    assert!(!u.is_https());
}

#[test]
fn placeholder_path_still_requires_a_scheme() {
    // The scheme is the one part a placeholder can't excuse.
    assert!(matches!(
        HttpsUrl::new_allowing_placeholders("${HOST}/x").unwrap_err(),
        HttpsUrlError::MissingScheme
    ));
}

#[test]
fn no_placeholder_falls_back_to_strict_new() {
    // Without a placeholder, the lenient entry point is exactly `new`.
    assert!(HttpsUrl::new_allowing_placeholders("https://example.com/foo").is_ok());
    assert!(matches!(
        HttpsUrl::new_allowing_placeholders("ftp://example.com").unwrap_err(),
        HttpsUrlError::MissingScheme
    ));
}
