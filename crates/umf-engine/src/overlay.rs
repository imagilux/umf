//! Overlayfs setup so RUN-step writes land in a captured upper-dir.
//!
//! Wrap an existing bundle's rootfs with an overlay whose upper-dir we
//! can later snapshot and pack into a layer.
//!
//! ## Why overlay
//!
//! Without an overlay, RUN-step writes would mutate the bundle's
//! `rootfs/` directly. That's fine for a one-shot smoke test but useless
//! for layer caching — there'd be no diff to pack into a new layer.
//! With an overlay, the bundle's `rootfs/` becomes the read-only lower
//! and writes land in a separate upper-dir that we capture verbatim
//! after the container exits.
//!
//! ## Rootful vs rootless — backend selection
//!
//! Kernel overlayfs is unrestricted only for root or processes with
//! `CAP_SYS_ADMIN`. For non-root callers we shell out to
//! `fuse-overlayfs` — a userspace daemon that provides the same
//! semantics via FUSE without needing kernel mount privileges.
//!
//! [`Overlay::mount`] picks at call time:
//! - effective uid `== 0` → kernel `mount -t overlay` (fast path,
//!   in-process, no extra runtime dep)
//! - effective uid `!= 0` → shell out to `fuse-overlayfs` (requires
//!   the binary on `PATH` and a correctly-configured `/etc/subuid`/
//!   `/etc/subgid` for unprivileged user namespaces)
//!
//! Operators can force a backend with the `UMF_OVERLAY_BACKEND` env
//! var (`kernel` / `fuse`) — useful for testing or when running with
//! `sudo` on a host where fuse-overlayfs is the desired path. When
//! neither is available the mount fails with a clear diagnostic.

use std::path::{Path, PathBuf};
use std::process::Command;

use nix::mount::{MntFlags, MsFlags, mount, umount2};
use tempfile::TempDir;
use tracing::debug;

use crate::error::EngineError;

/// Which mechanism backs an [`Overlay`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayBackend {
    /// Kernel `mount -t overlay`. Requires `CAP_SYS_ADMIN`.
    Kernel,
    /// `fuse-overlayfs` shell-out. Works without root if the binary
    /// is on PATH and `/etc/subuid`/`/etc/subgid` are set up.
    FuseOverlayfs,
}

impl OverlayBackend {
    /// Pick a backend appropriate for the current process. Honours an
    /// explicit `UMF_OVERLAY_BACKEND=kernel|fuse` override if set.
    fn auto() -> Self {
        match std::env::var("UMF_OVERLAY_BACKEND")
            .as_deref()
            .map(str::trim)
        {
            Ok("kernel") => return Self::Kernel,
            Ok("fuse" | "fuse-overlayfs") => return Self::FuseOverlayfs,
            Ok(other) if !other.is_empty() => {
                // Unknown value — log a warning and fall through to
                // detection. Don't fail the build for a typo here;
                // the actual mount will surface a clear error if it
                // doesn't work.
                debug!(
                    value = other,
                    "ignoring unknown UMF_OVERLAY_BACKEND value; using auto-detection"
                );
            }
            _ => {}
        }
        if nix::unistd::Uid::current().is_root() {
            Self::Kernel
        } else {
            Self::FuseOverlayfs
        }
    }
}

/// Mount via the kernel `mount -t overlay` syscall.
fn mount_kernel(
    merged: &Path,
    options: &str,
    lowerdir: &str,
    upper: &Path,
    work: &Path,
) -> Result<(), EngineError> {
    mount(
        Some("overlay"),
        merged,
        Some("overlay"),
        MsFlags::empty(),
        Some(options),
    )
    .map_err(|e| {
        EngineError::runtime(
            format!(
                "kernel overlayfs mount failed (lowers=[{lowerdir}], upper={}, work={}, merged={}): {}. \
                Kernel overlayfs requires CAP_SYS_ADMIN — run as root, or set \
                UMF_OVERLAY_BACKEND=fuse to use fuse-overlayfs instead.",
                upper.display(),
                work.display(),
                merged.display(),
                e,
            ),
            Some(Box::new(e)),
        )
    })?;
    Ok(())
}

/// Mount via `fuse-overlayfs` shell-out. Works without root if the
/// binary is on PATH and the host has unprivileged user namespaces
/// enabled.
fn mount_fuse_overlayfs(
    merged: &Path,
    options: &str,
    lowerdir: &str,
    upper: &Path,
    work: &Path,
) -> Result<(), EngineError> {
    let out = Command::new("fuse-overlayfs")
        .args(["-o", options])
        .arg(merged)
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => EngineError::runtime(
                "fuse-overlayfs not found on PATH; install it (typically `dnf install fuse-overlayfs` \
                 or `apt install fuse-overlayfs`) or set UMF_OVERLAY_BACKEND=kernel and run as root",
                Some(Box::new(e)),
            ),
            _ => EngineError::runtime(
                format!("failed to spawn fuse-overlayfs: {e}"),
                Some(Box::new(e)),
            ),
        })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(EngineError::runtime(
            format!(
                "fuse-overlayfs failed (status={}, lowers=[{lowerdir}], upper={}, work={}, merged={}): {}",
                out.status,
                upper.display(),
                work.display(),
                merged.display(),
                stderr.trim(),
            ),
            None,
        ));
    }
    Ok(())
}

/// Unmount a fuse-overlayfs mount via `fusermount3 -u` (falling back to
/// `fusermount -u`); if neither succeeds, detach lazily via
/// `umount2(MNT_DETACH)`.
///
/// Wraps the shared [`crate::mount_util::fusermount_unmount`] helper to
/// add overlay's error-returning contract: the public [`Overlay::unmount`]
/// surfaces a hard failure only when both the FUSE unmount and the
/// `umount2` fallback fail.
fn fusermount_unmount(merged: &Path) -> Result<(), EngineError> {
    if crate::mount_util::fusermount_unmount(merged) {
        return Ok(());
    }
    debug!(
        merged = %merged.display(),
        "fusermount unmount unavailable or failed; falling back to umount2(MNT_DETACH)",
    );
    // Last-resort: detach via umount2 — won't actually call the FUSE
    // daemon to tear down cleanly, but releases the mount point.
    umount2(merged, MntFlags::MNT_DETACH).map_err(|e| {
        EngineError::runtime(
            format!(
                "fusermount unmount failed and umount2({}) fallback also failed: {e}",
                merged.display(),
            ),
            Some(Box::new(e)),
        )
    })?;
    Ok(())
}

/// A mounted overlayfs whose upper-dir captures the filesystem diff.
///
/// Dropping the value attempts an unmount via `umount2(MNT_DETACH)` and
/// removes the staging tempdir. If the runtime is still using the mount
/// when `Overlay` is dropped, the detach is lazy — the kernel cleans
/// the mount when the last reference closes. The upper-dir is moved
/// out beforehand (see [`Self::persist_upper`]) if the caller wants to
/// retain the captured diff.
#[derive(Debug)]
pub struct Overlay {
    /// Drop guard for the overlay staging directory. Holds
    /// `upper/`, `work/`, `merged/` until [`Self::persist_upper`] or
    /// drop.
    tempdir: Option<TempDir>,
    /// `tempdir/merged/` — what the container runtime sees as its
    /// rootfs.
    merged: PathBuf,
    /// `tempdir/upper/` — captures writes.
    upper: PathBuf,
    /// `tempdir/work/` — overlayfs scratch (kernel requirement).
    work: PathBuf,
    /// Whether overlay is currently mounted (we can drop the
    /// `Overlay` either before or after an explicit unmount).
    mounted: bool,
    /// Which backend mounted the overlay — drives the corresponding
    /// teardown call ([`umount2`] vs `fusermount -u`).
    backend: OverlayBackend,
}

impl Overlay {
    /// Mount an overlayfs with `lowers` as the read-only layer stack.
    ///
    /// `lowers` is ordered **top → bottom** (first entry takes precedence
    /// over later ones). For a single-lower mount, pass a slice of length 1.
    /// For stacked builds (one lower per RUN-step diff plus a base
    /// rootfs at the bottom), pass them in that order.
    ///
    /// Creates a fresh tempdir holding `upper/`, `work/`, and `merged/`,
    /// then issues `mount -t overlay`. The caller points the container's
    /// spec at [`Self::merged`] before running.
    ///
    /// # Errors
    /// - `lowers` is empty (overlayfs requires at least one lowerdir).
    /// - Filesystem or mount-side failure. The kernel backend most often
    ///   fails with "operation not permitted" without `CAP_SYS_ADMIN`;
    ///   the fuse-overlayfs backend most often fails with "fuse-overlayfs
    ///   not found on PATH" or "unprivileged user namespaces are
    ///   disabled".
    pub fn mount(lowers: &[&Path]) -> Result<Self, EngineError> {
        Self::mount_with_backend(lowers, OverlayBackend::auto())
    }

    /// Like [`Self::mount`] but with an explicit backend choice.
    /// Mostly for tests and operators forcing a specific behaviour.
    ///
    /// # Errors
    /// See [`Self::mount`].
    pub fn mount_with_backend(
        lowers: &[&Path],
        backend: OverlayBackend,
    ) -> Result<Self, EngineError> {
        if lowers.is_empty() {
            return Err(EngineError::runtime(
                "Overlay::mount requires at least one lower directory",
                None,
            ));
        }

        let tempdir = TempDir::new()?;
        let upper = tempdir.path().join("upper");
        let work = tempdir.path().join("work");
        let merged = tempdir.path().join("merged");
        std::fs::create_dir_all(&upper)?;
        std::fs::create_dir_all(&work)?;
        std::fs::create_dir_all(&merged)?;

        // Build the lowerdir string. Overlayfs uses `:` as the separator;
        // we don't quote (paths with `:` would break, but no recipe-emitted
        // path should contain one — and we'd notice immediately).
        let lowerdir = lowers
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(":");
        let options = format!(
            "lowerdir={lowerdir},upperdir={},workdir={}",
            upper.display(),
            work.display(),
        );

        match backend {
            OverlayBackend::Kernel => mount_kernel(&merged, &options, &lowerdir, &upper, &work)?,
            OverlayBackend::FuseOverlayfs => {
                mount_fuse_overlayfs(&merged, &options, &lowerdir, &upper, &work)?;
            }
        }

        debug!(
            backend = ?backend,
            lowers = %lowerdir,
            merged = %merged.display(),
            "overlay mounted",
        );

        Ok(Self {
            tempdir: Some(tempdir),
            merged,
            upper,
            work,
            mounted: true,
            backend,
        })
    }

    /// The merged path the runtime should treat as its rootfs.
    #[must_use]
    pub fn merged(&self) -> &Path {
        &self.merged
    }

    /// The upper-dir path. Reads from here to package the captured
    /// diff as a layer.
    #[must_use]
    pub fn upper(&self) -> &Path {
        &self.upper
    }

    /// Unmount the overlay. Idempotent; safe to call before drop.
    ///
    /// Dispatches to the backend used at mount time — `umount2` for
    /// kernel overlayfs, `fusermount -u` for fuse-overlayfs.
    ///
    /// # Errors
    /// Surfaces the underlying unmount failure when something more
    /// serious than "already unmounted" happens.
    pub fn unmount(&mut self) -> Result<(), EngineError> {
        if !self.mounted {
            return Ok(());
        }
        match self.backend {
            OverlayBackend::Kernel => {
                umount2(&self.merged, MntFlags::MNT_DETACH).map_err(|e| {
                    EngineError::runtime(
                        format!("umount2({}) failed: {}", self.merged.display(), e),
                        Some(Box::new(e)),
                    )
                })?;
            }
            OverlayBackend::FuseOverlayfs => fusermount_unmount(&self.merged)?,
        }
        self.mounted = false;
        Ok(())
    }

    /// Which backend mounted this overlay. Useful for diagnostics and
    /// for tests that gate on backend availability.
    #[must_use]
    pub fn backend(&self) -> OverlayBackend {
        self.backend
    }

    /// Unmount the overlay and move the upper-dir into a caller-owned
    /// tempdir, returning a [`PersistedUpper`] guard.
    ///
    /// The captured diff outlives this `Overlay` (which is consumed) and
    /// stays on disk until the returned [`PersistedUpper`] is dropped.
    /// Use this when the upper-dir needs to be retained for layer
    /// packaging or for stacking under future overlays.
    ///
    /// # Errors
    /// Surfaces unmount and rename failures.
    pub fn persist_upper(mut self) -> Result<PersistedUpper, EngineError> {
        self.unmount()?;
        let persist = TempDir::new()?;
        let dst = persist.path().join("upper");
        std::fs::rename(&self.upper, &dst)?;
        // We've extracted the upper; let the staging tempdir drop normally
        // (it now only contains the work/ + merged/ directories).
        self.tempdir = None;
        Ok(PersistedUpper {
            _tempdir: persist,
            path: dst,
        })
    }

    /// Borrow the work-dir path (overlayfs scratch). Exposed mostly for
    /// diagnostics; callers don't usually need it.
    #[must_use]
    pub fn work(&self) -> &Path {
        &self.work
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // Best-effort cleanup. Ignore errors — Drop can't propagate, and
        // a leftover mount is the kernel's problem (it cleans up when
        // the last fd closes).
        let _ = self.unmount();
    }
}

/// A captured overlayfs upper-dir, owned by a tempdir whose lifetime is
/// tied to this wrapper.
///
/// Returned by [`Overlay::persist_upper`]; drop it to delete the upper
/// from disk. Use [`Self::path`] to get the directory's path (e.g. to
/// pack it as a layer or stack it as a lower under future overlays).
#[derive(Debug)]
pub struct PersistedUpper {
    /// Drop guard. The captured upper-dir lives at `_tempdir/upper/`.
    _tempdir: TempDir,
    /// Absolute path to the captured upper directory.
    path: PathBuf,
}

impl PersistedUpper {
    /// Path to the captured upper directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Wrap a caller-owned tempdir + directory path into a
    /// `PersistedUpper`-shaped guard.
    ///
    /// Used by build orchestrators that synthesise an upper-dir
    /// out-of-band (e.g. for `ADD` steps, which copy files directly
    /// into a fresh directory rather than capturing the diff of a
    /// container run) and want it to integrate with the rest of the
    /// upper-dir plumbing.
    ///
    /// The caller is responsible for guaranteeing that `path` lives
    /// under `tempdir` — drop semantics rely on it.
    #[must_use]
    pub fn from_owned_tempdir(tempdir: TempDir, path: PathBuf) -> Self {
        Self {
            _tempdir: tempdir,
            path,
        }
    }
}

#[cfg(test)]
mod tests;
