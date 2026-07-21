//! Unit tests for the rootless context derivation and error messaging.
//!
//! The namespace-entering path ([`super::enter`]) can't be exercised here:
//! `unshare(CLONE_NEWUSER)` + the `newuidmap` fork dance require a
//! single-threaded process and the cargo test harness is multi-threaded.
//! End-to-end rootless behaviour is covered by the binary-driven integration
//! lane instead (and the subid map computation by `crate::subid` tests). These
//! tests pin the pure seams: the passive-context invariants and the remediation
//! hint on a permission-denied namespace failure.

use super::*;

#[test]
fn passive_context_never_reports_an_entered_userns() {
    // Deriving from process identity never claims we entered our own
    // namespace — only `enter()` sets that.
    assert!(!derive_passive_context().entered_userns);
}

#[test]
fn passive_context_unprivileged_is_not_host_privileged() {
    // A non-root test process must not be reported as host-privileged; a
    // root CI lane legitimately is, so only assert the unprivileged direction.
    if !nix::unistd::geteuid().is_root() {
        assert!(!derive_passive_context().host_privileged);
    }
}

#[test]
fn eperm_carries_the_userns_remediation_hint() {
    let denied = userns_error("create user namespace", nix::errno::Errno::EPERM);
    assert!(
        denied
            .to_string()
            .contains("apparmor_restrict_unprivileged_userns"),
        "EPERM should explain the unprivileged-userns restriction: {denied}"
    );

    let other = userns_error("create user namespace", nix::errno::Errno::ENOENT);
    assert!(
        !other
            .to_string()
            .contains("apparmor_restrict_unprivileged_userns"),
        "non-EPERM failures should not append the userns hint: {other}"
    );
}
