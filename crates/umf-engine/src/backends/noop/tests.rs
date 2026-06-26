//! Unit tests for the `noop` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn parse_user_spec_accepts_uid_and_uid_gid() {
    let u = parse_user_spec("1000").expect("uid only");
    assert_eq!(u.uid(), 1000);
    assert_eq!(u.gid(), 0);

    let u = parse_user_spec("1000:1001").expect("uid:gid");
    assert_eq!(u.uid(), 1000);
    assert_eq!(u.gid(), 1001);
}

#[test]
fn parse_user_spec_rejects_garbage() {
    assert!(matches!(
        parse_user_spec("bob").unwrap_err(),
        EngineError::Runtime { .. }
    ));
}
