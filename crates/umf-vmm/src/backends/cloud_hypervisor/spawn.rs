//! The one and only `Command::new("cloud-hypervisor")` in `umf-vmm`.
//!
//! Spawns the daemon with `--api-socket path=<sock>`; everything that
//! controls the VM after that goes through `cloud-hypervisor-client`'s
//! typed REST API (see [`super`]).

// The port-forward path launches the daemon inside a caller-supplied netns by
// `setns`-ing the forked child (a `pre_exec` hook on a borrowed raw fd) — two
// irreducibly-unsafe operations the workspace otherwise bans, each justified
// with a `SAFETY` note below.
#![allow(unsafe_code)]

use std::os::fd::BorrowedFd;
use std::os::unix::process::CommandExt;
use std::process::Stdio;

use nix::sched::{CloneFlags, setns};
use tempfile::TempDir;
use tokio::process::Command;
use tracing::debug;

use crate::error::VmError;
use crate::handle::VmHandle;
use crate::runtime::VmSpec;

/// Spawn `cloud-hypervisor --api-socket path=<sock>` and return a
/// handle carrying both the child process and the socket path.
///
/// The actual VM configuration (`PUT /vm.create`) happens later through
/// the REST client — the daemon comes up with no VM at all, ready to
/// accept commands. That mirrors how cloud-hypervisor is intended to
/// be operated and keeps the spawn step minimal.
pub async fn spawn_cloud_hypervisor(binary: &str, spec: &VmSpec) -> Result<VmHandle, VmError> {
    crate::backends::common::validate_spec_inputs(spec)?;

    // The socket lives in a per-VM tempdir so concurrent invocations
    // don't trip over each other.
    let dir = TempDir::new()?;
    let socket_path = dir.path().join("ch-api.sock");

    let id = format!("umf-vmm-ch-{}", std::process::id());
    let args = vec![
        "--api-socket".to_string(),
        format!("path={}", socket_path.display()),
    ];
    debug!(?args, "umf-vmm: cloud-hypervisor argv");

    // When the caller pre-built a tap network (cloud-hypervisor port
    // forwarding), launch the daemon inside that netns so it can open the tap.
    // We enter the namespace by `setns`-ing the forked child before exec (the
    // native equivalent of `ip netns exec`). Only the network namespace is
    // entered, not the mount namespace, so the `--api-socket` path stays on the
    // shared filesystem and host REST control still works.
    let mut cmd = match &spec.net {
        Some(net) => {
            let netns_fd = net.netns_fd;
            let mut std_cmd = std::process::Command::new(binary);
            // SAFETY: the closure runs in the forked child between fork and
            // exec, so it must be async-signal-safe: it only calls `setns` (a
            // bare syscall) on a raw fd the caller's `umf-networking` guard
            // keeps open for the child's lifetime — no allocation, no locks.
            unsafe {
                std_cmd.pre_exec(move || {
                    // SAFETY: `netns_fd` is valid for the child's lifetime;
                    // `borrow_raw` only wraps it for the `setns` call.
                    let ns = BorrowedFd::borrow_raw(netns_fd);
                    setns(ns, CloneFlags::CLONE_NEWNET).map_err(std::io::Error::from)?;
                    Ok(())
                });
            }
            Command::from(std_cmd)
        }
        None => Command::new(binary),
    };
    cmd.args(&args);
    cmd.stdin(Stdio::null());
    // Inherit (don't pipe) the child's stdout/stderr. Nothing in this crate
    // drains those pipes, so a piped-but-undrained fd would fill the OS pipe
    // buffer and stall the daemon once it has logged enough. Inheriting
    // forwards cloud-hypervisor's diagnostics straight to the caller.
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    // Kill the daemon if its handle is dropped, so we never orphan a
    // cloud-hypervisor process on the host.
    cmd.kill_on_drop(true);

    let child = cmd.spawn().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            VmError::BinaryNotFound(binary.to_string())
        } else {
            VmError::Io(err)
        }
    })?;

    // Leak the tempdir: the API socket needs to outlive this function.
    // Cleanup is delegated to OS reclaim on process exit (the same shape
    // the qemu backend uses for its QMP socket).
    let _ = dir.keep();

    Ok(VmHandle {
        child: Some(child),
        control_socket: Some(socket_path),
        id,
    })
}
