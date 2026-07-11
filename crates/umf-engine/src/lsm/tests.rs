//! Unit tests for the `lsm` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn apparmor_defaults_to_umf_default_when_loaded() {
    // No env → the shipped `umf-default` profile, applied when loaded.
    let got = resolve_apparmor(None, true, |n| n == "umf-default");
    assert_eq!(got.as_deref(), Some("umf-default"));
}

#[test]
fn apparmor_skips_when_unavailable_or_not_loaded() {
    // AppArmor off on the host → nothing, regardless of "loaded".
    assert_eq!(resolve_apparmor(None, false, |_| true), None);
    // Available but the profile isn't loaded → skip (never break the build).
    assert_eq!(resolve_apparmor(None, true, |_| false), None);
}

#[test]
fn apparmor_honours_env_override_and_opt_out() {
    // Explicit profile name, applied when that one is loaded.
    let got = resolve_apparmor(Some("custom".into()), true, |n| n == "custom");
    assert_eq!(got.as_deref(), Some("custom"));
    // Named profile not loaded → skip even though another is.
    assert_eq!(
        resolve_apparmor(Some("custom".into()), true, |n| n == "umf-default"),
        None
    );
    // Explicit opt-out.
    assert_eq!(
        resolve_apparmor(Some("unconfined".into()), true, |_| true),
        None
    );
    assert_eq!(resolve_apparmor(Some("".into()), true, |_| true), None);
    assert_eq!(resolve_apparmor(Some("  ".into()), true, |_| true), None);
}

#[test]
fn selinux_label_applies_only_when_enforcing_and_nonempty() {
    // Enforcing + a label → applied (trimmed).
    assert_eq!(
        resolve_selinux(Some(" system_u:system_r:container_t:s0 ".into()), true).as_deref(),
        Some("system_u:system_r:container_t:s0")
    );
    // Not enforcing → never applied (would break RUN on a permissive host).
    assert_eq!(
        resolve_selinux(Some("system_u:system_r:container_t:s0".into()), false),
        None
    );
    // No label / empty → nothing.
    assert_eq!(resolve_selinux(None, true), None);
    assert_eq!(resolve_selinux(Some("".into()), true), None);
}

#[test]
fn default_config_is_fully_unconfined() {
    // The Default is the regression-free baseline: no LSM fields set.
    let cfg = LsmConfig::default();
    assert_eq!(cfg.apparmor_profile, None);
    assert_eq!(cfg.selinux_process_label, None);
    assert_eq!(cfg.selinux_mount_label, None);
}
