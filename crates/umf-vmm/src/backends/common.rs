//! Shared utilities the per-backend modules reach into.
//!
//! Both the QEMU and Cloud Hypervisor backends share more than their
//! spawn helpers: spec-input validation, the process-reaping `wait`, and
//! the identical no-control branches of `info` / `shutdown`. Those
//! backend-agnostic pieces live here so the two trait impls stay thin and
//! can't drift apart.
//!
//! The boot / graceful-shutdown poll loops are *not* shared: their bodies
//! mutably borrow a backend-specific control handle (`&mut` QMP stream /
//! REST client) across each await, which a generic `poll_until(predicate)`
//! can't express on stable Rust without boxing every future. They stay
//! inline in each backend, small and explicit.

use std::path::Path;

use crate::error::VmError;
use crate::handle::VmHandle;
use crate::runtime::{BootSource, Firmware, VmInfo, VmSpec, VmStatus};

/// Validate that every path the spec references actually exists and
/// is a regular file. Surfaces typed [`VmError::InputUnusable`] so the
/// CLI can format a precise diagnostic.
pub(crate) fn validate_spec_inputs(spec: &VmSpec) -> Result<(), VmError> {
    match &spec.boot {
        BootSource::Disk { path, firmware } => {
            ensure_readable(path, "disk image")?;
            match firmware {
                Some(Firmware::Bios(fw)) => ensure_readable(fw, "firmware")?,
                Some(Firmware::Pflash { code, vars }) => {
                    ensure_readable(code, "firmware code (OVMF_CODE)")?;
                    ensure_readable(vars, "firmware vars (OVMF_VARS)")?;
                }
                None => {}
            }
        }
        BootSource::DirectKernel { kernel, initrd, .. } => {
            ensure_readable(kernel, "kernel image")?;
            ensure_readable(initrd, "initramfs")?;
        }
    }
    Ok(())
}

fn ensure_readable(path: &Path, label: &str) -> Result<(), VmError> {
    if !path.exists() {
        return Err(VmError::InputUnusable {
            path: path.to_path_buf(),
            reason: format!("{label} not found"),
        });
    }
    if !path.is_file() {
        return Err(VmError::InputUnusable {
            path: path.to_path_buf(),
            reason: format!("{label} is not a regular file"),
        });
    }
    Ok(())
}

/// Reap the spawned VMM process and return its exit code. Identical for
/// every backend — the control surface differs, but waiting on the child
/// is just `tokio::process::Child::wait`. Returns `None` when the child
/// was already taken (a prior `wait`) or was killed by a signal.
///
/// # Errors
/// [`VmError::Io`] from the underlying child wait.
pub(crate) async fn wait(vm: &mut VmHandle) -> Result<Option<i32>, VmError> {
    let Some(mut child) = vm.child.take() else {
        return Ok(None);
    };
    let status = child.wait().await.map_err(VmError::Io)?;
    Ok(status.code())
}

/// The `info` result for a [`crate::ControlMode::None`] VM: there's no
/// control channel to query, so the runtime can't tell whether the guest
/// is still running — [`VmStatus::Unknown`], not `Stopped`.
pub(crate) fn no_control_info() -> VmInfo {
    VmInfo {
        status: VmStatus::Unknown,
        detail: "no control channel (ControlMode::None)".to_string(),
    }
}

/// The no-control `shutdown` path: with no channel to ask politely, the
/// only lever is to kill the VMM child. Best-effort — ignores the
/// `start_kill` result (the child may have already exited).
pub(crate) fn kill_child(vm: &mut VmHandle) {
    if let Some(child) = vm.child.as_mut() {
        let _ = child.start_kill();
    }
}

#[cfg(test)]
mod tests;
