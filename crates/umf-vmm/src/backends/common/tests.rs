//! Unit tests for the `common` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn no_control_info_is_unknown_not_stopped() {
    // A ControlMode::None VM may well still be running; `info` must not
    // claim it has stopped just because there's no channel to ask.
    let info = no_control_info();
    assert_eq!(info.status, VmStatus::Unknown);
    assert_ne!(info.status, VmStatus::Stopped);
    assert!(info.detail.contains("ControlMode::None"));
}

#[tokio::test]
async fn wait_on_reaped_handle_returns_none() {
    // No child to wait on (already taken / never spawned) ⇒ `None`,
    // not an error.
    let mut vm = VmHandle::new("umf-vmm-test");
    assert_eq!(wait(&mut vm).await.expect("wait"), None);
}
