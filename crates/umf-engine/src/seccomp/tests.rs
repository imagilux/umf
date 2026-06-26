//! Unit tests for the `seccomp` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use oci_spec::runtime::LinuxSeccompAction;

#[test]
fn default_profile_is_deny_by_default_with_a_real_allowlist() {
    let profile = default_profile().expect("vendored default profile must parse");
    // Deny-by-default: anything not on the allowlist returns an errno.
    assert_eq!(profile.default_action(), LinuxSeccompAction::ScmpActErrno);
    // A non-empty allowlist of explicitly-permitted syscalls.
    let blocks = profile
        .syscalls()
        .as_ref()
        .expect("syscall allowlist present");
    assert!(!blocks.is_empty(), "allowlist must have syscall blocks");
    let total: usize = blocks.iter().map(|b| b.names().len()).sum();
    assert!(
        total > 100,
        "default allowlist should permit the full set of safe syscalls, got {total}"
    );
    // Sanity: common, must-allow syscalls are present somewhere.
    let allows = |name: &str| {
        blocks.iter().any(|b| {
            b.action() == LinuxSeccompAction::ScmpActAllow && b.names().iter().any(|n| n == name)
        })
    };
    for required in ["read", "write", "execve", "openat", "mmap"] {
        assert!(allows(required), "default profile must allow `{required}`");
    }
}
