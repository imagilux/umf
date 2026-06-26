//! Kernel layout detection — locate vmlinuz + modules tree in a staging dir.
//!
//! In VM builds the kernel artifact arrives via `FROM` and its layers are
//! unpacked into the build staging by the standard OCI pipeline (the same
//! one ROOTFS uses). After both ROOTFS and FROM kernel layers are unpacked,
//! [`detect_kernel_layout`] scans the resulting tree to find:
//!
//! * `<staging>/boot/vmlinuz-<release>` — the kernel image, copied to the ESP.
//! * `<staging>/lib/modules/<release>/` — the modules tree, picked up by the
//!   initramfs generator and shipped into the rootfs partition.
//!
//! The expected on-staging layout is documented here rather than in the
//! user-facing spec because it's a reference-implementation convention for
//! how a kernel artifact must be packaged, not a DSL surface.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::debug;

use umf_oci::staging::BuildStaging;

// ── Errors ──────────────────────────────────────────────────────────────────

/// Errors produced by [`detect_kernel_layout`].
#[derive(Debug, Error)]
pub enum KernelLayoutError {
    /// No `boot/vmlinuz*` file was found in staging. The FROM kernel artifact
    /// is malformed (or wasn't unpacked yet).
    #[error("no boot/vmlinuz-* found in staging — FROM kernel artifact malformed")]
    NoVmlinuz,

    /// `lib/modules/` doesn't exist in staging, contains no release
    /// directory, or the matching release directory is empty.
    #[error(
        "no usable lib/modules/<release>/ tree in staging (release: {release}) \
         — FROM kernel artifact malformed"
    )]
    NoModules {
        /// Release the layout detection settled on (or `<none>` when no
        /// directory existed at all).
        release: String,
    },

    /// I/O error walking the staging tree.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

// ── Result ──────────────────────────────────────────────────────────────────

/// Where the kernel ended up inside the staging tree.
///
/// All paths are absolute and live under [`BuildStaging::path`].
#[derive(Debug, Clone)]
pub struct KernelLayout {
    /// Release identifier (e.g. `"7.0"`, `"6.6.79"`, `"lts"`).
    pub release: String,
    /// Absolute path to the vmlinuz binary inside staging.
    pub vmlinuz: PathBuf,
    /// Absolute path to `<staging>/lib/modules/<release>/`.
    pub modules: PathBuf,
}

// ── Public entry ────────────────────────────────────────────────────────────

/// Detect the kernel binary + modules tree inside `staging`.
///
/// Strategy:
/// 1. Enumerate `lib/modules/<release>/` directories — that's where the
///    release name comes from (it's the canonical place a kernel records it).
/// 2. Enumerate `boot/vmlinuz*` files.
/// 3. Prefer a vmlinuz whose suffix matches an existing release; fall back
///    to the alphabetically-first vmlinuz + alphabetically-first release.
///
/// Both lookups are read-only and deterministic across runs.
pub fn detect_kernel_layout(staging: &BuildStaging) -> Result<KernelLayout, KernelLayoutError> {
    let releases = collect_module_releases(&staging.path().join("lib").join("modules"))?;
    if releases.is_empty() {
        return Err(KernelLayoutError::NoModules {
            release: "<none>".into(),
        });
    }

    let vmlinuzes = collect_vmlinuz(&staging.path().join("boot"))?;
    if vmlinuzes.is_empty() {
        return Err(KernelLayoutError::NoVmlinuz);
    }

    let modules_root = staging.path().join("lib").join("modules");

    // Try to pair a vmlinuz suffix with an existing release.
    for vmlinuz in &vmlinuzes {
        if let Some(suffix) = vmlinuz_suffix(vmlinuz)
            && releases.iter().any(|r| r == &suffix)
        {
            let modules = modules_root.join(&suffix);
            if dir_is_non_empty(&modules)? {
                debug!(release = %suffix, vmlinuz = %vmlinuz.display(), "kernel: layout detected (suffix match)");
                return Ok(KernelLayout {
                    release: suffix,
                    vmlinuz: vmlinuz.clone(),
                    modules,
                });
            }
        }
    }

    // No suffix match — fall back to the alphabetically-first pair.
    let release = releases[0].clone();
    let modules = modules_root.join(&release);
    if !dir_is_non_empty(&modules)? {
        return Err(KernelLayoutError::NoModules { release });
    }
    let vmlinuz = vmlinuzes[0].clone();
    debug!(%release, vmlinuz = %vmlinuz.display(), "kernel: layout detected (fallback)");
    Ok(KernelLayout {
        release,
        vmlinuz,
        modules,
    })
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn collect_module_releases(root: &Path) -> std::io::Result<Vec<String>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut releases: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            releases.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    releases.sort();
    Ok(releases)
}

fn collect_vmlinuz(boot: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !boot.is_dir() {
        return Ok(Vec::new());
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(boot)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if s.starts_with("vmlinuz") {
            candidates.push(entry.path());
        }
    }
    candidates.sort();
    Ok(candidates)
}

fn vmlinuz_suffix(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy();
    name.strip_prefix("vmlinuz-").map(|s| s.to_string())
}

fn dir_is_non_empty(dir: &Path) -> std::io::Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    Ok(std::fs::read_dir(dir)?.next().is_some())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
