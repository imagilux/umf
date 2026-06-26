//! UKI (Unified Kernel Image) assembly for `flavor=uki` projections.
//!
//! With no bootloader, the firmware can't supply a kernel command line or
//! initrd — so they must be embedded. A UKI wraps the kernel, the (optional)
//! initramfs, and the cmdline into a single `systemd-stub` `.efi` the firmware
//! boots directly from the ESP fallback path. The projector assembles it with
//! `ukify`, using ukify's default systemd-stub. This is the consuming half of
//! Strategy X: the kernel artifact stays a plain `vmlinuz` and is wrapped here,
//! where the cmdline (`root=`, `init=`) and initramfs are known.

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use tempfile::TempDir;
use umf_core::architecture::Architecture;

use crate::error::CompileError;

/// Whether `ukify` (systemd-ukify) is on `PATH`. Memoized. `flavor=uki`
/// requires it — there is no fallback, since a UKI is the only way to boot
/// with no bootloader.
#[must_use]
pub fn ukify_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new("ukify")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

/// Assemble a UKI from `vmlinuz` + optional `initrd` bytes + `cmdline` via
/// `ukify build`, returning the resulting `.efi` bytes. Uses ukify's default
/// systemd-stub.
///
/// # Errors
/// [`CompileError::UkifyUnavailable`] when `ukify` isn't on `PATH`;
/// [`CompileError::UkifyFailed`] when the `ukify` invocation fails.
pub fn build_uki(
    vmlinuz: &Path,
    initrd: Option<&[u8]>,
    cmdline: &str,
    uname: &str,
    architecture: Architecture,
) -> Result<Vec<u8>, CompileError> {
    // ukify wraps the kernel in systemd's EFI stub, and it picks the *host's*
    // stub; there is no flag here to source the target-arch stub. So a UKI built
    // for a foreign arch would embed an x86 stub around an aarch64 kernel (or
    // vice versa) and be unbootable. Until the stub is sourced per-arch from the
    // image rootfs, refuse cross-arch UKI loudly rather than emit an unbootable
    // disk (audit B); same-arch UKI is unaffected.
    let host = Architecture::host();
    if architecture != host {
        return Err(CompileError::CrossArchUki {
            target: architecture,
            host,
        });
    }
    if !ukify_available() {
        return Err(CompileError::UkifyUnavailable);
    }

    let tmp = TempDir::new()?;
    let out = tmp.path().join("uki.efi");

    let mut cmd = Command::new("ukify");
    cmd.arg("build")
        .arg("--linux")
        .arg(vmlinuz)
        .arg("--cmdline")
        .arg(cmdline)
        // Pass the kernel release explicitly (we know it from the boot-manifest
        // labels) so ukify doesn't scrape it out of the `vmlinuz` PE — which
        // both avoids a redundant read and lets the projection proceed when the
        // kernel image isn't introspectable.
        .arg("--uname")
        .arg(uname)
        .arg("--output")
        .arg(&out);

    // The initramfs is embedded as the `.initrd` section; absent for the
    // appliance shape (binary ENTRYPOINT), where the kernel execs `init=`
    // directly off the root partition.
    if let Some(bytes) = initrd {
        let initrd_path = tmp.path().join("initrd.img");
        std::fs::write(&initrd_path, bytes)?;
        cmd.arg("--initrd").arg(&initrd_path);
    }

    let output = cmd
        .output()
        .map_err(|e| CompileError::UkifyFailed(format!("spawning ukify: {e}")))?;
    if !output.status.success() {
        return Err(CompileError::UkifyFailed(format!(
            "ukify build failed (status={}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        )));
    }

    Ok(std::fs::read(&out)?)
}

#[cfg(test)]
mod tests;
