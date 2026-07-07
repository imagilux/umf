//! Unit tests for the `seccomp` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use oci_spec::runtime::LinuxSeccompAction;
use std::collections::HashSet;

/// umf's conservative default RUN capability set (mirrors `bundle::default_caps`
/// — no `CAP_SYS_ADMIN`). `CAP_SYS_CHROOT` is held, so the chroot gate applies.
fn no_admin_caps() -> HashSet<Capability> {
    [
        Capability::Chown,
        Capability::DacOverride,
        Capability::Fowner,
        Capability::Setuid,
        Capability::Setgid,
        Capability::SysChroot,
    ]
    .into_iter()
    .collect()
}

/// Is `name` explicitly `SCMP_ACT_ALLOW`ed by some block in `profile`?
fn allows(profile: &LinuxSeccomp, name: &str) -> bool {
    profile.syscalls().as_ref().is_some_and(|blocks| {
        blocks.iter().any(|b| {
            b.action() == LinuxSeccompAction::ScmpActAllow && b.names().iter().any(|n| n == name)
        })
    })
}

#[test]
fn filtered_profile_denies_cap_sys_admin_syscalls_for_the_default_cap_set() {
    // The regression under test: without gate evaluation these fall through to
    // an unconditional allow, re-opening the nested-userns escape.
    let profile = filtered_profile(&no_admin_caps(), Architecture::X86_64).expect("profile");
    for denied in ["unshare", "setns", "mount", "umount2", "open_by_handle_at"] {
        assert!(
            !allows(&profile, denied),
            "`{denied}` must NOT be allowed without CAP_SYS_ADMIN"
        );
    }
    // Deny-by-default is intact and the architecture set is pinned (primary +
    // compat sub-arches close the 32-bit-ABI bypass).
    assert_eq!(profile.default_action(), LinuxSeccompAction::ScmpActErrno);
    assert_eq!(
        profile.architectures().as_ref().map(Vec::len),
        Some(3),
        "amd64 profile pins x86_64 + x86 + x32"
    );
}

#[test]
fn filtered_profile_keeps_ordinary_process_creation_and_held_cap_gates() {
    // Dropping the CAP_SYS_ADMIN block must NOT break normal RUN execution:
    // fork/exec and the arg-restricted `clone` stay allowed, and a gate on a
    // cap we DO hold (CAP_SYS_CHROOT → chroot) is kept.
    let profile = filtered_profile(&no_admin_caps(), Architecture::X86_64).expect("profile");
    for required in [
        "read", "write", "execve", "fork", "vfork", "clone", "chroot",
    ] {
        assert!(allows(&profile, required), "`{required}` must stay allowed");
    }
}

#[test]
fn filtered_profile_allows_unshare_when_cap_sys_admin_is_held() {
    // Proves the gate is evaluated, not blanket-stripped: grant CAP_SYS_ADMIN
    // and the gated block applies again.
    let mut caps = no_admin_caps();
    caps.insert(Capability::SysAdmin);
    let profile = filtered_profile(&caps, Architecture::X86_64).expect("profile");
    assert!(
        allows(&profile, "unshare"),
        "unshare is allowed once CAP_SYS_ADMIN is held"
    );
}

#[test]
fn filtered_profile_pins_aarch64_arch_set() {
    let profile = filtered_profile(&no_admin_caps(), Architecture::Aarch64).expect("profile");
    assert_eq!(profile.architectures().as_ref().map(Vec::len), Some(2));
    // Still denies the privileged block on arm64.
    assert!(!allows(&profile, "unshare"));
    assert!(allows(&profile, "execve"));
}

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
