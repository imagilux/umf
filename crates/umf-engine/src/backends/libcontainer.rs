//! Youki-`libcontainer`-backed [`ContainerRuntime`].
//!
//! Thin wrapper around `libcontainer::container::ContainerBuilder`:
//! installs the per-RUN-step `RunSpec` mutations onto the bundle's
//! `config.json`, drives the builder through `as_init → build → start →
//! waitpid → delete`, and returns the exit status.
//!
//! **Out of scope (intentional):** overlay setup, upper-dir capture,
//! image-config persistence between steps. Those are caller concerns —
//! see [`crate::overlay::Overlay`] and the per-target lowering code in
//! `umf-builder` for the orchestration layer.
//!
//! ## Privilege requirement
//!
//! Kernel namespaces + cgroup setup are libcontainer's job; the backend
//! doesn't request additional capabilities beyond what libcontainer
//! needs for an unprivileged init. Rootless support depends on the
//! host having user-namespace setup and (if the caller wraps with
//! [`crate::overlay::Overlay`]) `CAP_SYS_ADMIN` for the overlay mount,
//! which is the actual blocker.

use std::path::PathBuf;

use libcontainer::container::builder::ContainerBuilder;
use libcontainer::syscall::syscall::SyscallType;
use nix::sys::wait::{WaitStatus, waitpid};
use oci_spec::runtime::{LinuxNamespaceType, Spec};

use crate::backends::noop::apply_run_spec_to_bundle;
use crate::bundle::Bundle;
use crate::error::EngineError;
use crate::runtime::{ContainerRuntime, RunOutcome, RunSpec};

/// libcontainer-backed [`ContainerRuntime`].
///
/// Holds the path to libcontainer's per-instance state dir (a tempdir
/// is fine; libcontainer creates per-id subdirs underneath).
#[derive(Debug, Clone)]
pub struct LibcontainerRuntime {
    /// Where libcontainer stores its per-container state files. Each
    /// `RunSpec::id` gets a subdirectory. Equivalent to youki CLI's
    /// `--root` flag (default `/run/youki`).
    state_root: PathBuf,
}

impl LibcontainerRuntime {
    /// Build a runtime that stores libcontainer state under `state_root`.
    /// The directory is created if it does not exist.
    ///
    /// # Errors
    /// Filesystem error if the directory can't be created.
    pub fn new(state_root: impl Into<PathBuf>) -> Result<Self, EngineError> {
        let state_root = state_root.into();
        std::fs::create_dir_all(&state_root)?;
        Ok(Self { state_root })
    }
}

impl ContainerRuntime for LibcontainerRuntime {
    #[tracing::instrument(
        level = "info",
        name = "umf.engine.run_step",
        skip(self, bundle, spec),
        fields(id = %spec.id, argv_len = spec.argv.len())
    )]
    fn run(&self, bundle: &mut Bundle, spec: &RunSpec) -> Result<RunOutcome, EngineError> {
        // 1. Install the per-RUN-step mutations on the spec.
        apply_run_spec_to_bundle(bundle, spec)?;

        // 2. Rootless: we can't reach into youki's netns after it creates one
        // (its /proc ns files are root-owned → EACCES), so hand youki a
        // netns we own and have configured (loopback up, plus the selected
        // egress backend) to JOIN via the spec path. Held across the container's
        // lifetime; dropped after, releasing the namespace.
        let _rootless_net = if crate::rootless::context().host_privileged {
            None
        } else {
            setup_rootless_net(bundle)?
        };

        // 3. Flush the spec and drive libcontainer through build → start → wait
        // → delete. The container joins our netns when its path was set above.
        bundle.write_spec()?;
        let exit_code = drive_container(&spec.id, bundle.path(), &self.state_root)?;

        Ok(RunOutcome {
            exit_code: Some(exit_code),
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }
}

/// Point the container's network namespace at `path` so youki `setns`-joins the
/// namespace we created and configured, rather than unsharing its own. The
/// network namespace is always present in the spec (added by
/// `bundle::build_runtime_spec`); we set its `path`.
pub(crate) fn set_network_namespace_path(
    spec: &mut Spec,
    path: &std::path::Path,
) -> Result<(), EngineError> {
    let net = spec
        .linux_mut()
        .as_mut()
        .and_then(|l| l.namespaces_mut().as_mut())
        .and_then(|ns| {
            ns.iter_mut()
                .find(|n| n.typ() == LinuxNamespaceType::Network)
        })
        .ok_or_else(|| EngineError::runtime("spec has no network namespace to join", None))?;
    net.set_path(Some(path.to_path_buf()));
    Ok(())
}

/// Set up a rootless RUN/run step's network: an [`umf_networking::RootlessNet`]
/// (the netns we own, loopback up, plus the selected egress backend), with the
/// container's spec pointed at it so youki joins it. Returns the guard to hold
/// for the container's lifetime.
///
/// A failure to set up `native` (the default, best-effort sovereign backend) or
/// `none` degrades to an isolated loopback netns (logged, not fatal): a host
/// that can't stand up the gateway still runs builds it could before, and a RUN
/// that genuinely needs the network then fails with its own clear error. Only
/// `pasta` — an explicit external-binary opt-in — is fatal on failure, since the
/// operator deliberately asked for it and installed `passt`.
pub(crate) fn setup_rootless_net(
    bundle: &mut Bundle,
) -> Result<Option<umf_networking::RootlessNet>, EngineError> {
    let mode = crate::rootless::egress_mode();
    let policy = crate::rootless::egress_policy();
    match umf_networking::RootlessNet::setup(mode, policy) {
        Ok(rn) => {
            set_network_namespace_path(bundle.spec_mut(), rn.spec_path())?;
            Ok(Some(rn))
        }
        Err(e) if mode != umf_networking::EgressMode::Pasta => {
            tracing::warn!(error = %e, ?mode, "rootless: egress setup failed; RUN gets loopback-only networking");
            Ok(None)
        }
        Err(e) => Err(EngineError::runtime(
            format!("rootless egress ({mode:?}) setup failed: {e}"),
            Some(Box::new(e)),
        )),
    }
}

/// Drive a libcontainer instance through its full lifecycle:
/// `as_init → build → start → waitpid → delete`, returning the init
/// process's exit code.
///
/// Shared by [`LibcontainerRuntime::run`] (per-RUN-step execution) and
/// [`crate::run::run_image`] (`umf run <ref>`) — both prepare a bundle's
/// `config.json` then drive it identically. The container's state files
/// land under `state_root`, keyed by `container_id`.
///
/// State teardown via `Container::delete` is best-effort: the exit code is
/// already captured by then, so a delete failure is informational and does
/// not fail the call.
///
/// # Errors
/// [`EngineError::Runtime`] if the builder, start, or `waitpid` step fails.
pub(crate) fn drive_container(
    container_id: &str,
    bundle_path: &std::path::Path,
    state_root: &std::path::Path,
) -> Result<i32, EngineError> {
    let mut container = ContainerBuilder::new(container_id.to_string(), SyscallType::default())
        .with_root_path(state_root.to_path_buf())
        .map_err(|e| {
            EngineError::runtime(
                format!("ContainerBuilder::with_root_path: {e}"),
                Some(Box::new(e)),
            )
        })?
        .as_init(bundle_path)
        // Rootless drives the systemd cgroup manager (user DBus → delegated
        // scope); real root keeps the fs cgroup manager. See bundle.rs.
        .with_systemd(!crate::rootless::context().host_privileged)
        .with_detach(false)
        .build()
        .map_err(|e| {
            EngineError::runtime(format!("Container build failed: {e}"), Some(Box::new(e)))
        })?;

    // After `build()` the init process is forked and parked in the "created"
    // state, so its network namespace already exists at /proc/<pid>/ns/net but
    // nothing runs in it yet — the right moment to wire NAT'd egress in, before
    // `start()` lets the RUN command exec. The guard is held until the end of
    // this function (past wait + delete), then tears the veth + nft rule down.
    //
    // Best-effort: a setup failure (e.g. no CAP_NET_ADMIN, or a host without
    // forwarding) is logged, not fatal — the RUN still executes with loopback
    // only, exactly as it did before this wiring existed. Commands that need
    // the network fail with their own clear errors.
    let _net = setup_container_network(&container);

    container.start().map_err(|e| {
        EngineError::runtime(format!("Container start failed: {e}"), Some(Box::new(e)))
    })?;

    let exit_code = wait_for_container_exit(&container)?;

    // Best-effort cleanup of libcontainer state. We've already captured
    // the exit code; a delete failure is informational.
    let _ = container.delete(true);

    Ok(exit_code)
}

/// Wire NAT'd egress into a freshly-built (pre-start) container's net
/// namespace. Returns the drop guard that tears it down, or `None` if the
/// container has no PID or setup failed (both non-fatal — see caller).
fn setup_container_network(
    container: &libcontainer::container::Container,
) -> Option<umf_networking::ContainerNet> {
    let pid = container.pid()?;
    // Rootless: we run in our own user namespace with no authority over the
    // host's init netns, so the host veth + nft masquerade path can't apply.
    // Skip it (rather than attempt-and-warn) — the RUN step runs loopback-only.
    // Userspace rootless egress (pasta/slirp) is tracked separately.
    if !crate::rootless::context().host_privileged {
        tracing::debug!(
            pid = pid.as_raw(),
            "rootless RUN step: loopback-only networking (host NAT egress needs real root)"
        );
        return None;
    }
    match umf_networking::ContainerNet::setup(
        pid.as_raw() as u32,
        &umf_networking::NetOptions::default(),
    ) {
        Ok(net) => {
            tracing::debug!(
                pid = pid.as_raw(),
                host_if = net.host_ifname(),
                container_ip = %net.container_ip(),
                "RUN-step network up (NAT'd egress)"
            );
            Some(net)
        }
        Err(e) => {
            tracing::warn!(
                pid = pid.as_raw(),
                error = %e,
                "RUN-step network setup failed; the step runs with loopback only"
            );
            None
        }
    }
}

/// Wait for the container's init process to exit and return its
/// numeric exit code (or `128 + signal` for signal kills — best-effort
/// translation).
pub(crate) fn wait_for_container_exit(
    container: &libcontainer::container::Container,
) -> Result<i32, EngineError> {
    let Some(pid) = container.pid() else {
        return Err(EngineError::runtime(
            "container has no PID after start (libcontainer didn't fork an init?)",
            None,
        ));
    };
    let status = waitpid(pid, None).map_err(|e| {
        EngineError::runtime(format!("waitpid({pid}) failed: {e}"), Some(Box::new(e)))
    })?;
    Ok(match status {
        WaitStatus::Exited(_, code) => code,
        WaitStatus::Signaled(_, sig, _) => 128 + (sig as i32),
        WaitStatus::Stopped(_, _)
        | WaitStatus::PtraceEvent(_, _, _)
        | WaitStatus::PtraceSyscall(_)
        | WaitStatus::Continued(_)
        | WaitStatus::StillAlive => -1,
    })
}
