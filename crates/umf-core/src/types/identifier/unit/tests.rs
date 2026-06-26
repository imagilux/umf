//! Unit tests for the `unit` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

// ── ServiceUnitName ─────────────────────────────────────────────────────

#[test]
fn service_unit_name_no_suffix_is_bare() {
    let u = ServiceUnitName::new("nginx").unwrap();
    assert_eq!(u.as_str(), "nginx");
    assert_eq!(u.bare_name(), "nginx");
    assert_eq!(u.suffix(), None);
    assert_eq!(u.with_default_suffix(UnitSuffix::Service), "nginx.service");
}

#[test]
fn service_unit_name_with_service_suffix() {
    let u = ServiceUnitName::new("nginx.service").unwrap();
    assert_eq!(u.as_str(), "nginx.service");
    assert_eq!(u.bare_name(), "nginx");
    assert_eq!(u.suffix(), Some(UnitSuffix::Service));
    assert_eq!(u.with_default_suffix(UnitSuffix::Service), "nginx.service");
}

#[test]
fn service_unit_name_with_instance() {
    let u = ServiceUnitName::new("getty@tty1.service").unwrap();
    assert_eq!(u.bare_name(), "getty@tty1");
    assert_eq!(u.suffix(), Some(UnitSuffix::Service));
}

#[test]
fn service_unit_name_accepts_all_known_types() {
    for s in [
        "x.service",
        "x.socket",
        "x.timer",
        "x.target",
        "x.path",
        "x.mount",
        "x.device",
        "x.swap",
        "x.slice",
        "x.scope",
        "x.automount",
    ] {
        ServiceUnitName::new(s).unwrap_or_else(|e| panic!("`{s}` should parse: {e}"));
    }
}

#[test]
fn service_unit_name_rejects_unknown_suffix() {
    let err = ServiceUnitName::new("nginx.serivce").unwrap_err();
    assert!(matches!(
        err,
        ServiceUnitNameError::UnknownSuffix { ref suffix, .. } if suffix == "serivce"
    ));
    assert_eq!(err.offset(), Some(6));
}

#[test]
fn service_unit_name_rejects_invalid_char() {
    let err = ServiceUnitName::new("nginx!service").unwrap_err();
    assert!(matches!(
        err,
        ServiceUnitNameError::InvalidChar { ch: '!', .. }
    ));
}

#[test]
fn service_unit_name_rejects_empty() {
    assert!(matches!(
        ServiceUnitName::new(""),
        Err(ServiceUnitNameError::Empty)
    ));
}

#[test]
fn service_unit_name_rejects_empty_bare_name() {
    assert!(matches!(
        ServiceUnitName::new(".service"),
        Err(ServiceUnitNameError::EmptyBareName)
    ));
}
