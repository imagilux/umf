//! One user namespace for the whole rootless build.
//!
//! An unprivileged `umf build`/`run` must operate inside a user namespace it
//! **owns**, established **before** youki runs. The reasons are concrete:
//!
//! - The rootfs overlay is mounted in-process with the kernel `mount(2)`
//!   syscall (no `fuse-overlayfs` subprocess). The kernel only permits that
//!   unprivileged when the mounter is root inside a user namespace and mounts
//!   into a mount namespace that namespace owns.
//! - Image layers are unpacked as the namespace's root, so image uid/gid `0`
//!   lands on disk as our host uid (= container `0`). That is what dissolves
//!   the EACCES: there is no second, differently-mapped namespace.
//! - youki is an **in-process** `libcontainer` call. With this namespace
//!   already entered it sees euid `0`, takes its ordinary *rootful* path, and
//!   creates **no** nested user namespace. The RUN step executes in our
//!   namespace. One namespace, no mismatch.
//!
//! [`enter`] is therefore the single place that creates the namespace, and the
//! resulting [`RootlessContext`] is the process-wide source of truth for "are
//! we in our own userns?" and "do we hold *real* host privilege?". Code that
//! used to branch on `euid == 0` must consult [`context`] instead: after
//! [`enter`] the euid is `0` inside our namespace **without** host authority,
//! so operations that need genuine privilege (erofs mounts, host cgroup
//! writes, host veth/nftables) are gated on [`RootlessContext::host_privileged`].

use std::sync::OnceLock;

use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};
use nix::unistd::{geteuid, getgid, getuid};

use crate::error::EngineError;

/// Process-wide rootless facts, established once by [`enter`].
#[derive(Debug, Clone, Copy)]
pub struct RootlessContext {
    /// We created and entered our own user namespace at startup, so the
    /// process is euid `0` inside it under a single-id map (host uid → `0`).
    pub entered_userns: bool,
    /// We hold **real** host privilege (started as uid `0` in the initial user
    /// namespace). Gate operations that need genuine host authority on this,
    /// never on `euid == 0` — which is true inside our namespace without it.
    pub host_privileged: bool,
    /// The host uid the single-id map points container `0` at (the real uid;
    /// preserved because `getuid()` reports `0` once we are in the namespace).
    pub host_uid: u32,
    /// The host gid the single-id map points container `0` at.
    pub host_gid: u32,
}

static CONTEXT: OnceLock<RootlessContext> = OnceLock::new();

/// The rootless context for this process.
///
/// When [`enter`] was never called (a library consumer driving the engine
/// without the CLI's startup hook), this derives a conservative context from
/// the current process identity and memoizes it: `entered_userns = false`,
/// `host_privileged` iff euid `0` in the initial user namespace. Such a
/// consumer keeps the legacy behaviour — an unprivileged build then asks youki
/// for its own user namespace via the spec (see `bundle::build_runtime_spec`).
#[must_use]
pub fn context() -> RootlessContext {
    *CONTEXT.get_or_init(derive_passive_context)
}

/// The systemd-notation `cgroupsPath` for a rootless RUN container, or `None`
/// for a host-privileged build (which keeps youki's fs cgroup manager).
///
/// Returns `umf.slice:umf:<leaf>` so youki's systemd manager asks the user's
/// systemd (over the session bus — `is_true_root` is userns-aware, so euid 0
/// inside our single-id namespace still resolves to the user bus) to create a
/// delegated transient `umf-<leaf>.scope` grouped under one `umf.slice`. The
/// scope is **per RUN step**: `container_id` is unique per step (a `RunSpec::id`
/// invariant), so scope names never repeat and we avoid systemd's
/// async-teardown name-reuse race. systemd treats `-` as a slice-hierarchy
/// separator, so the leaf is flattened to underscores.
#[must_use]
pub fn cgroup_scope_path(container_id: &str) -> Option<std::path::PathBuf> {
    if context().host_privileged {
        return None;
    }
    let leaf = container_id.replace('-', "_");
    Some(std::path::PathBuf::from(format!("umf.slice:umf:{leaf}")))
}

/// Enter a single private user namespace for an unprivileged build.
///
/// **Must run while the process is single-threaded** — `unshare(CLONE_NEWUSER)`
/// returns `EINVAL` otherwise. The CLI calls this from `cli::run` after parsing
/// and tracing setup (neither spawns a thread) and **before** any Tokio runtime
/// is built. Behaviour by starting identity:
///
/// - **real root** (euid `0`, initial userns): records `host_privileged = true`
///   and returns — the existing rootful overlay/youki paths apply unchanged.
/// - **already euid `0` in a non-initial userns** (e.g. invoked under
///   `unshare -r` or by podman): records `entered_userns = true`,
///   `host_privileged = false` and returns — we build inside the namespace we
///   were handed; no nesting.
/// - **unprivileged** (euid != `0`): `unshare(CLONE_NEWUSER)`, write a single-id
///   map (container `0` → our uid), then `unshare(CLONE_NEWNS)` and make the
///   mount tree `rslave` so the in-userns overlay mount neither propagates to
///   nor receives events from the host.
///
/// Idempotent: the [`RootlessContext`] is recorded once and returned on every
/// later call.
///
/// # Errors
/// [`EngineError::Runtime`] if creating the namespace or writing the id maps
/// fails. On hosts that restrict unprivileged user namespaces (e.g. Ubuntu's
/// `kernel.apparmor_restrict_unprivileged_userns`) the error carries the
/// remediation.
pub fn enter() -> Result<RootlessContext, EngineError> {
    if let Some(ctx) = CONTEXT.get() {
        return Ok(*ctx);
    }

    let ctx = if geteuid().is_root() {
        let initial = in_initial_user_namespace();
        RootlessContext {
            entered_userns: !initial,
            host_privileged: initial,
            host_uid: getuid().as_raw(),
            host_gid: getgid().as_raw(),
        }
    } else {
        let uid = getuid().as_raw();
        let gid = getgid().as_raw();

        unshare(CloneFlags::CLONE_NEWUSER).map_err(|e| userns_error("create user namespace", e))?;
        write_single_id_maps(uid, gid)?;
        // We are now root in the new namespace, so we may unshare a private
        // mount namespace to own the overlay mount.
        unshare(CloneFlags::CLONE_NEWNS).map_err(|e| userns_error("create mount namespace", e))?;
        mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REC | MsFlags::MS_SLAVE,
            None::<&str>,
        )
        .map_err(|e| userns_error("make / rslave", e))?;

        RootlessContext {
            entered_userns: true,
            host_privileged: false,
            host_uid: uid,
            host_gid: gid,
        }
    };

    // A racing initializer (e.g. a passive `context()` read) is impossible
    // before this single-threaded call, but tolerate it: keep the first value.
    let _ = CONTEXT.set(ctx);
    Ok(*CONTEXT.get().unwrap_or(&ctx))
}

/// Write the single-id identity map for the freshly-unshared user namespace,
/// in-process — no `newuidmap`/`newgidmap` helper. Container `0` maps to the
/// caller's host uid/gid (size 1). `setgroups` is denied first: the kernel
/// refuses an unprivileged `gid_map` write otherwise.
fn write_single_id_maps(uid: u32, gid: u32) -> Result<(), EngineError> {
    std::fs::write("/proc/self/setgroups", b"deny").map_err(|e| map_error("setgroups", &e))?;
    std::fs::write("/proc/self/gid_map", id_map_line(gid)).map_err(|e| map_error("gid_map", &e))?;
    std::fs::write("/proc/self/uid_map", id_map_line(uid)).map_err(|e| map_error("uid_map", &e))?;
    Ok(())
}

/// The single-id `/proc/self/{uid,gid}_map` line: container `0` → host `id`,
/// range size 1. The only mapping an unprivileged process may install for
/// itself without the setuid `newuidmap`/`newgidmap` helpers.
fn id_map_line(host: u32) -> String {
    format!("0 {host} 1")
}

/// Whether this process lives in the initial user namespace (its `uid_map` is
/// the kernel identity map `0 0 4294967295`). Reading failure is treated as
/// "initial" — the only callers are the euid-`0` branch, where the realistic
/// case is real root.
fn in_initial_user_namespace() -> bool {
    std::fs::read_to_string("/proc/self/uid_map")
        .map(|m| {
            let mut cols = m.split_whitespace();
            matches!(
                (cols.next(), cols.next(), cols.next()),
                (Some("0"), Some("0"), Some("4294967295"))
            )
        })
        .unwrap_or(true)
}

fn derive_passive_context() -> RootlessContext {
    RootlessContext {
        entered_userns: false,
        host_privileged: geteuid().is_root() && in_initial_user_namespace(),
        host_uid: getuid().as_raw(),
        host_gid: getgid().as_raw(),
    }
}

/// Wrap a namespace-syscall failure, appending the unprivileged-userns
/// remediation when the kernel denied the operation (`EPERM`).
fn userns_error(step: &str, err: nix::errno::Errno) -> EngineError {
    let mut msg = format!("rootless: failed to {step}: {err}");
    if err == nix::errno::Errno::EPERM {
        msg.push_str(USERNS_DENIED_HINT);
    }
    EngineError::runtime(msg, Some(Box::new(err)))
}

/// Wrap a `/proc/self/{uid,gid}_map`/`setgroups` write failure, with the same
/// remediation on `PermissionDenied` (where Ubuntu's AppArmor policy surfaces).
fn map_error(file: &str, err: &std::io::Error) -> EngineError {
    let mut msg = format!("rootless: failed to write /proc/self/{file}: {err}");
    if err.kind() == std::io::ErrorKind::PermissionDenied {
        msg.push_str(USERNS_DENIED_HINT);
    }
    EngineError::runtime(msg, None)
}

const USERNS_DENIED_HINT: &str = ". Unprivileged user namespaces appear to be \
restricted on this host. On Ubuntu 24.04+ this is \
`kernel.apparmor_restrict_unprivileged_userns=1`: either grant `/usr/bin/umf` an \
AppArmor profile with the `userns,` permission, or set the sysctl to 0. \
Otherwise check `sysctl user.max_user_namespaces` (> 0) and that the kernel was \
built with `CONFIG_USER_NS`. Run as root to build without a user namespace.";

/// The rootless egress mode for this process, resolved once. Set by the CLI from
/// `--rootless-net` (which takes precedence over the environment); otherwise
/// derived from `UMF_ROOTLESS_NET` on first read.
static EGRESS_MODE: OnceLock<umf_networking::EgressMode> = OnceLock::new();

/// The selected rootless egress mode (defaults to `UMF_ROOTLESS_NET`, or
/// [`umf_networking::EgressMode::Native`] when unset). Consulted by the engine
/// when it sets up a rootless RUN step's network.
#[must_use]
pub fn egress_mode() -> umf_networking::EgressMode {
    *EGRESS_MODE.get_or_init(umf_networking::EgressMode::from_env)
}

/// Set the rootless egress mode explicitly — the CLI's `--rootless-net`, which
/// wins over `UMF_ROOTLESS_NET`. Must be called before the first
/// [`egress_mode`] read; a no-op afterwards.
pub fn set_egress_mode(mode: umf_networking::EgressMode) {
    let _ = EGRESS_MODE.set(mode);
}

/// Set the egress mode from a `--rootless-net` argument value, taking precedence
/// over `UMF_ROOTLESS_NET`. Returns a human-readable message on an unrecognised
/// value so the CLI can report it.
///
/// # Errors
/// The value isn't one of `none` / `pasta` / `native`.
pub fn set_egress_mode_from_arg(value: &str) -> Result<(), String> {
    let mode = value
        .parse::<umf_networking::EgressMode>()
        .map_err(|e| e.to_string())?;
    set_egress_mode(mode);
    Ok(())
}

/// The rootless egress SSRF policy for this process, resolved once. Set by the
/// CLI from `--rootless-net-allow` (which takes precedence over the
/// environment); otherwise derived from `UMF_ROOTLESS_NET_ALLOW` on first read.
static EGRESS_POLICY: OnceLock<umf_networking::ssrf::EgressPolicy> = OnceLock::new();

/// The active SSRF egress policy (defaults from `UMF_ROOTLESS_NET_ALLOW`, or the
/// secure deny-all default when unset; a malformed env value also yields
/// deny-all, failing closed). Consulted by the native egress gateway on every
/// re-originated connection.
#[must_use]
pub fn egress_policy() -> umf_networking::ssrf::EgressPolicy {
    EGRESS_POLICY
        .get_or_init(|| umf_networking::ssrf::EgressPolicy::from_env().unwrap_or_default())
        .clone()
}

/// Set the SSRF egress policy explicitly — the CLI's `--rootless-net-allow`,
/// which wins over `UMF_ROOTLESS_NET_ALLOW`. Must be called before the first
/// [`egress_policy`] read; a no-op afterwards.
pub fn set_egress_policy(policy: umf_networking::ssrf::EgressPolicy) {
    let _ = EGRESS_POLICY.set(policy);
}

/// Set the egress policy from a `--rootless-net-allow` allow-list spec (a
/// comma/space-separated list of categories to re-allow). Returns a
/// human-readable message on an unrecognised category.
///
/// # Errors
/// The spec names a category that isn't recognised.
pub fn set_egress_policy_from_arg(spec: &str) -> Result<(), String> {
    let policy =
        umf_networking::ssrf::EgressPolicy::from_allow_list(spec).map_err(|e| e.to_string())?;
    set_egress_policy(policy);
    Ok(())
}

#[cfg(test)]
mod tests;
