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

use std::os::fd::AsRawFd;
use std::sync::OnceLock;

use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};
use nix::sys::wait::waitpid;
use nix::unistd::{ForkResult, Pid, geteuid, getgid, getppid, getuid, pipe, read, write};

use crate::error::EngineError;
use crate::subid::{self, SubIdRange};

/// Process-wide rootless facts, established once by [`enter`].
#[derive(Debug, Clone, Copy)]
pub struct RootlessContext {
    /// We created and entered our own user namespace at startup, so the process
    /// is euid `0` inside it under a full multi-id map (host uid → `0`, plus the
    /// caller's delegated `/etc/subuid`/`/etc/subgid` range).
    pub entered_userns: bool,
    /// We hold **real** host privilege (started as uid `0` in the initial user
    /// namespace). Gate operations that need genuine host authority on this,
    /// never on `euid == 0` — which is true inside our namespace without it.
    pub host_privileged: bool,
    /// The host uid the map points container `0` at (the real uid; preserved
    /// because `getuid()` reports `0` once we are in the namespace).
    pub host_uid: u32,
    /// The host gid the map points container `0` at.
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
/// - **unprivileged** (euid != `0`): `unshare(CLONE_NEWUSER)` and apply the full
///   multi-id map (container `0` → our uid, plus the delegated `/etc/subuid`,
///   `/etc/subgid` range) via `newuidmap`/`newgidmap`, then `unshare(CLONE_NEWNS)`
///   and make the mount tree `rslave` so the in-userns overlay mount neither
///   propagates to nor receives events from the host. Requires the `uidmap`
///   helpers + a subid grant (a hard requirement; see [`crate::subid`]).
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

        // Precheck the subordinate-id delegation BEFORE unshare: a missing helper
        // or grant must fail cleanly here (leaving us in the host namespace),
        // never after `unshare(CLONE_NEWUSER)`, which would strand the process in
        // an unmapped namespace with no way back. Best-effort at the call site: a
        // bootable build (RUN in a micro-VM) never needs this, and a container
        // build re-surfaces the same requirement at the youki boundary.
        let (sub_uid, sub_gid) = subid::resolve_ranges(uid, gid)?;

        // Create the namespace and apply the full multi-id map via
        // newuidmap/newgidmap (see `apply_subordinate_maps`).
        apply_subordinate_maps(uid, gid, &sub_uid, &sub_gid)?;

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

/// Create the user namespace and apply the caller's full **multi-id**
/// subordinate map via the setuid `newuidmap`/`newgidmap` helpers.
///
/// The kernel forbids an unprivileged process from writing a multi-id map for
/// itself, so a transient child forked **before** the `unshare` (thus staying in
/// the initial namespace, where its real uid still owns the `/etc/subuid` grant)
/// runs the helpers against our pid. Ordering is enforced over a pipe: the
/// parent unshares, signals the child, the child maps and reports back.
/// `setgroups` is left at `allow` — the helpers hold `CAP_SETGID` and are exempt
/// from the deny rule, so `apt`/`dnf` sandbox setup and `RUN --user` work.
///
/// Must run single-threaded (guaranteed by the startup call site), for both the
/// `fork` and the later `unshare(CLONE_NEWUSER)`.
// `fork` + `_exit` are irreducibly unsafe; both uses are documented at their
// call sites and rely on the single-threaded invariant.
#[allow(unsafe_code)]
fn apply_subordinate_maps(
    uid: u32,
    gid: u32,
    sub_uid: &SubIdRange,
    sub_gid: &SubIdRange,
) -> Result<(), EngineError> {
    // `go`: parent → child "namespace is up, proceed". `res`: child → parent
    // status byte + (on failure) the helper's stderr.
    let (go_r, go_w) = pipe().map_err(|e| userns_error("create sync pipe", e))?;
    let (res_r, res_w) = pipe().map_err(|e| userns_error("create result pipe", e))?;

    // SAFETY: single-threaded here (enter runs before any Tokio runtime), so
    // fork() is sound — no other thread can hold a lock the child would inherit.
    match unsafe { nix::unistd::fork() }.map_err(|e| userns_error("fork map helper", e))? {
        ForkResult::Child => {
            // Helper, still in the initial user namespace. Keep it minimal: wait
            // for the parent to unshare, apply the maps, report, `_exit`.
            drop(go_w);
            drop(res_r);
            let mut go = [0u8; 1];
            let _ = read(go_r.as_raw_fd(), &mut go); // block until the ns exists
            match run_id_helpers(getppid(), uid, gid, sub_uid, sub_gid) {
                Ok(()) => {
                    let _ = write(&res_w, &[0u8]);
                }
                Err(msg) => {
                    let _ = write(&res_w, &[1u8]);
                    let _ = write(&res_w, msg.as_bytes());
                }
            }
            // `_exit`, not `exit`: skip atexit/flush — the child shares the
            // parent's fds and buffers.
            unsafe { nix::libc::_exit(0) };
        }
        ForkResult::Parent { child } => {
            drop(go_r);
            drop(res_w);
            // Unshare AFTER the fork so the child stays in the initial namespace.
            unshare(CloneFlags::CLONE_NEWUSER)
                .map_err(|e| userns_error("create user namespace", e))?;
            write(&go_w, &[1u8]).map_err(|e| userns_error("signal map helper", e))?;
            drop(go_w); // EOF for the child's read side

            let mut status = [0u8; 1];
            let n = read(res_r.as_raw_fd(), &mut status).unwrap_or(0);
            let mut msg = Vec::new();
            if n == 1 && status[0] != 0 {
                let mut buf = [0u8; 512];
                while let Ok(k) = read(res_r.as_raw_fd(), &mut buf) {
                    if k == 0 || msg.len() > 8192 {
                        break;
                    }
                    msg.extend_from_slice(&buf[..k]);
                }
            }
            let _ = waitpid(child, None);

            if n != 1 || status[0] != 0 {
                let detail = String::from_utf8_lossy(&msg);
                let detail = if detail.trim().is_empty() {
                    "the map helper exited abnormally".to_string()
                } else {
                    detail.into_owned()
                };
                return Err(EngineError::runtime(
                    format!("rootless: applying the subordinate id map failed: {detail}"),
                    None,
                ));
            }
            Ok(())
        }
    }
}

/// Run `newuidmap` then `newgidmap` against `pid`, mapping container `0` to the
/// invoking user and container `1..` onto the delegated ranges. Returns the
/// combined helper stderr on failure. Called only from the forked child.
fn run_id_helpers(
    pid: Pid,
    uid: u32,
    gid: u32,
    sub_uid: &SubIdRange,
    sub_gid: &SubIdRange,
) -> Result<(), String> {
    let (uid_triples, gid_triples) = subid::mapping_triples(uid, gid, sub_uid, sub_gid);
    run_helper("newuidmap", &subid::helper_args(pid.as_raw(), &uid_triples))?;
    run_helper("newgidmap", &subid::helper_args(pid.as_raw(), &gid_triples))?;
    Ok(())
}

fn run_helper(bin: &str, args: &[String]) -> Result<(), String> {
    let out = std::process::Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("spawning {bin}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{bin} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
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
