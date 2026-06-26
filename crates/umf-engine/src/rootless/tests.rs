//! Unit tests for the rootless context derivation and error messaging.
//!
//! The namespace-entering path ([`super::enter`]) can't be exercised here:
//! `unshare(CLONE_NEWUSER)` requires a single-threaded process and the cargo
//! test harness is multi-threaded. End-to-end rootless behaviour is covered by
//! the binary-driven integration lane instead. These tests pin the pure
//! seams: the single-id map line, the passive-context invariants, and the
//! remediation hint on permission failures.

use super::*;

#[test]
fn id_map_line_is_single_id() {
    assert_eq!(id_map_line(1000), "0 1000 1");
    assert_eq!(id_map_line(0), "0 0 1");
}

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

#[test]
fn map_write_permission_denied_carries_the_hint() {
    let denied = map_error(
        "uid_map",
        &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    );
    assert!(denied.to_string().contains("AppArmor"));

    let missing = map_error(
        "uid_map",
        &std::io::Error::from(std::io::ErrorKind::NotFound),
    );
    assert!(!missing.to_string().contains("AppArmor"));
}
