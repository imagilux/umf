//! Optional Linux Security Module (AppArmor / SELinux) confinement for RUN steps.
//!
//! seccomp + capabilities + user namespaces + masked/read-only paths already
//! contain a RUN step; an LSM profile is a defence-in-depth backstop that
//! mirrors runc/crun defaults. It is applied **only** when the host actually
//! supports it and a profile/label is available, and skips cleanly otherwise —
//! a missing or unloaded profile must never break a build. So on a host with no
//! LSM (or one where nothing is loaded) the runtime spec is byte-for-byte what
//! it was before, which keeps the default path regression-free.
//!
//! We deliberately do **not** auto-*load* an AppArmor profile: loading needs
//! init-userns privilege (umf's rootless path runs youki in a nested userns)
//! and validation across host userlands, so it belongs to the operator (or a
//! future `umf` setup step), not to every RUN. Instead we *detect* an
//! already-loaded profile — defaulting to `umf-default`, overridable via
//! `UMF_APPARMOR_PROFILE` — and apply an operator-supplied SELinux label
//! (`UMF_SELINUX_LABEL` / `UMF_SELINUX_MOUNT_LABEL`) when the host is enforcing.
//! The shipped `umf-default` profile lives at
//! `crates/umf-engine/resources/apparmor/umf-default` (load it with
//! `apparmor_parser -r`).

use std::path::Path;

/// Env var overriding the AppArmor profile name to apply (default
/// [`DEFAULT_APPARMOR_PROFILE`]). `unconfined` or empty disables AppArmor
/// confinement explicitly.
pub const APPARMOR_ENV: &str = "UMF_APPARMOR_PROFILE";
/// Env var supplying the SELinux **process** label to apply on an enforcing host.
pub const SELINUX_LABEL_ENV: &str = "UMF_SELINUX_LABEL";
/// Env var supplying the SELinux **mount** label for the container rootfs on an
/// enforcing host.
pub const SELINUX_MOUNT_LABEL_ENV: &str = "UMF_SELINUX_MOUNT_LABEL";

/// The profile name detected + applied by default (the one umf ships).
const DEFAULT_APPARMOR_PROFILE: &str = "umf-default";

const APPARMOR_PROFILES: &str = "/sys/kernel/security/apparmor/profiles";
const SELINUX_ENFORCE: &str = "/sys/fs/selinux/enforce";

/// Resolved LSM confinement for a RUN step. Every field defaults to `None`
/// (unconfined by that LSM), so where an LSM is absent the sandbox is exactly
/// what it was before this layer existed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LsmConfig {
    /// `process.apparmorProfile`, when an AppArmor profile is loaded + selected.
    pub apparmor_profile: Option<String>,
    /// `process.selinuxLabel`, when the host is SELinux-enforcing and a label
    /// was supplied.
    pub selinux_process_label: Option<String>,
    /// `linux.mountLabel`, likewise.
    pub selinux_mount_label: Option<String>,
}

impl LsmConfig {
    /// Resolve the confinement from the host state and environment.
    #[must_use]
    pub fn detect() -> Self {
        let enforcing = selinux_enforcing();
        Self {
            apparmor_profile: resolve_apparmor(
                std::env::var(APPARMOR_ENV).ok(),
                apparmor_available(),
                apparmor_profile_loaded,
            ),
            selinux_process_label: resolve_selinux(
                std::env::var(SELINUX_LABEL_ENV).ok(),
                enforcing,
            ),
            selinux_mount_label: resolve_selinux(
                std::env::var(SELINUX_MOUNT_LABEL_ENV).ok(),
                enforcing,
            ),
        }
    }
}

/// Decide the AppArmor profile to apply (pure; the host checks are injected so
/// this is unit-testable). `env` is the raw `UMF_APPARMOR_PROFILE` value.
/// Returns the profile name only when AppArmor is available **and** the chosen
/// profile is actually loaded, so an absent/unloaded profile is a clean skip.
fn resolve_apparmor(
    env: Option<String>,
    available: bool,
    is_loaded: impl Fn(&str) -> bool,
) -> Option<String> {
    let name = match env.as_deref().map(str::trim) {
        // Explicit opt-out.
        Some("") | Some("unconfined") => return None,
        Some(name) => name.to_string(),
        None => DEFAULT_APPARMOR_PROFILE.to_string(),
    };
    (available && is_loaded(&name)).then_some(name)
}

/// A SELinux label applies only on an enforcing host and only when non-empty.
fn resolve_selinux(env: Option<String>, enforcing: bool) -> Option<String> {
    let label = env?;
    let label = label.trim();
    (enforcing && !label.is_empty()).then(|| label.to_string())
}

/// Whether AppArmor is enabled on the host (its securityfs interface exists).
fn apparmor_available() -> bool {
    Path::new(APPARMOR_PROFILES).exists()
}

/// Whether an AppArmor profile named `name` is currently loaded. The `profiles`
/// file lists one `"<name> (<mode>)"` entry per line.
fn apparmor_profile_loaded(name: &str) -> bool {
    let Ok(list) = std::fs::read_to_string(APPARMOR_PROFILES) else {
        return false;
    };
    list.lines()
        .filter_map(|l| l.split_whitespace().next())
        .any(|p| p == name)
}

/// Whether the host runs SELinux in enforcing mode.
fn selinux_enforcing() -> bool {
    matches!(
        std::fs::read_to_string(SELINUX_ENFORCE)
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1")
    )
}

#[cfg(test)]
mod tests;
