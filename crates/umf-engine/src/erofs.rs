//! Mount erofs-encoded lower layers as overlayfs lowers.
//!
//! `umf-oci`'s [`erofs`](umf_oci::erofs) module encodes each base OCI
//! layer into a content-addressed `.erofs` file in the layout cache.
//! This module mounts those files read-only so the build's overlay can
//! stack them as lowers instead of unpacking every layer into a
//! directory tree on every build.
//!
//! ## Why shell out to `mount`
//!
//! An erofs image is a regular file, and the raw `mount(2)` syscall
//! expects a block device for a file-backed filesystem. The `mount(8)`
//! binary transparently sets up (and, on `umount`, auto-clears) a loop
//! device for `-o loop`, which works across kernels. So — mirroring the
//! `fuse-overlayfs` shell-out in [`crate::overlay`] — we drive `mount` /
//! `umount` rather than the syscall directly.
//!
//! ## Privilege
//!
//! Kernel erofs mounts need `CAP_SYS_ADMIN`. For non-root callers we
//! shell out to `erofsfuse` (a userspace driver, no root required) when
//! it's on `PATH` — mirroring the kernel/`fuse-overlayfs` split in
//! [`crate::overlay`]. [`mount_available`] reports whether *either*
//! backend is usable; when neither is, callers fall back to the
//! directory-unpack path upstream.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use nix::mount::{MntFlags, umount2};
use tempfile::TempDir;
use tracing::debug;

use crate::error::EngineError;

/// How an erofs image is mounted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErofsBackend {
    /// Kernel `mount -t erofs -o ro,loop`. Fast, but needs `CAP_SYS_ADMIN`
    /// + a registered erofs filesystem.
    Kernel,
    /// `erofsfuse` userspace daemon. Works without root if the binary is
    /// on `PATH`.
    Fuse,
}

/// Pick the erofs mount backend for this process, memoized: kernel mount
/// when running as root with erofs registered, else `erofsfuse` if it's
/// on `PATH`, else `None` (no erofs mounting — caller falls back to the
/// directory-unpack path).
fn selected_backend() -> Option<ErofsBackend> {
    static BACKEND: OnceLock<Option<ErofsBackend>> = OnceLock::new();
    *BACKEND.get_or_init(|| {
        // erofs lowers are only for *privileged* builds. A rootless build runs
        // inside our own user namespace and unpacks layers as the namespace's
        // root, so they land owned by the build uid (= container 0). An erofs
        // lower would instead present the image's original uids, which are
        // unmapped under a single-id map — the EACCES. (erofs is also not
        // unprivileged-mountable.) So a rootless build always unpacks.
        if !crate::rootless::context().host_privileged {
            return None;
        }
        if kernel_erofs_registered() {
            Some(ErofsBackend::Kernel)
        } else if binary_on_path("erofsfuse") {
            Some(ErofsBackend::Fuse)
        } else {
            None
        }
    })
}

/// `/proc/filesystems` lists registered filesystems one per line, the fs
/// name in the last column (a leading `nodev` marker for pseudo
/// filesystems; erofs is block/loop-backed → no marker).
fn kernel_erofs_registered() -> bool {
    std::fs::read_to_string("/proc/filesystems")
        .map(|s| s.split_whitespace().any(|tok| tok == "erofs"))
        .unwrap_or(false)
}

/// Whether `name` resolves to a file on `PATH`.
fn binary_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

/// Whether this host can mount erofs at all — via the kernel (root) or
/// `erofsfuse` (rootless). Probed once and memoized.
#[must_use]
pub fn mount_available() -> bool {
    selected_backend().is_some()
}

/// A set of erofs layer images mounted read-only, newest layer first.
///
/// Each mountpoint is exposed via [`Self::mountpoints`] for use as an
/// overlayfs lower stack. Dropping the value unmounts every layer (which
/// also releases the loop devices `mount -o loop` set up) and removes the
/// mountpoint tempdir — the same teardown discipline as [`crate::overlay::Overlay`].
#[derive(Debug)]
pub struct MountedErofsLayers {
    /// Drop guard for the directory holding the per-layer mountpoints.
    _tempdir: TempDir,
    /// Absolute mountpoint paths, in the order requested (newest first).
    mountpoints: Vec<PathBuf>,
    /// Which backend mounted these — drives the matching teardown
    /// (`umount` for kernel, `fusermount -u` for erofsfuse).
    backend: ErofsBackend,
}

impl MountedErofsLayers {
    /// Mount each erofs file in `erofs_paths` read-only, in order.
    ///
    /// `erofs_paths` is the lower stack **top → bottom** (newest layer
    /// first) — pass it in the order overlayfs expects its `lowerdir`
    /// list. The returned [`Self::mountpoints`] preserve that order.
    ///
    /// # Errors
    /// [`EngineError::Runtime`] if the host can't mount erofs (neither a
    /// root kernel mount nor `erofsfuse` is available) or a mount
    /// invocation fails. On any failure, layers mounted so far are
    /// unmounted before returning, so a partial mount never leaks.
    pub fn mount(erofs_paths: &[PathBuf]) -> Result<Self, EngineError> {
        let backend = selected_backend().ok_or_else(|| {
            EngineError::runtime(
                "no erofs mount backend: need root + a registered erofs kernel module, \
                 or `erofsfuse` on PATH",
                None,
            )
        })?;

        let tempdir = TempDir::new()?;
        let mut mountpoints: Vec<PathBuf> = Vec::with_capacity(erofs_paths.len());

        for (i, erofs) in erofs_paths.iter().enumerate() {
            let mp = tempdir.path().join(format!("layer{i}"));
            std::fs::create_dir_all(&mp)?;
            if let Err(e) = mount_one(backend, erofs, &mp) {
                unmount_all(&mountpoints, backend);
                return Err(e);
            }
            mountpoints.push(mp);
        }

        debug!(layers = mountpoints.len(), backend = ?backend, "erofs lowers mounted");
        Ok(Self {
            _tempdir: tempdir,
            mountpoints,
            backend,
        })
    }

    /// The mounted layer paths, top → bottom (newest first) — ready to
    /// pass as an overlayfs lower stack.
    #[must_use]
    pub fn mountpoints(&self) -> Vec<&Path> {
        self.mountpoints.iter().map(PathBuf::as_path).collect()
    }
}

impl Drop for MountedErofsLayers {
    fn drop(&mut self) {
        unmount_all(&self.mountpoints, self.backend);
    }
}

/// Mount one erofs image read-only at `mp` via `backend`.
///
/// - Kernel: `mount -t erofs -o ro,loop` (mount(8) auto-allocates the
///   loop device and clears it on umount).
/// - Fuse: `erofsfuse <img> <mp>` (read-only by nature; no root needed).
fn mount_one(backend: ErofsBackend, erofs: &Path, mp: &Path) -> Result<(), EngineError> {
    let mut cmd = match backend {
        ErofsBackend::Kernel => {
            let mut c = Command::new("mount");
            c.arg("-t")
                .arg("erofs")
                .arg("-o")
                .arg("ro,loop")
                .arg(erofs)
                .arg(mp);
            c
        }
        ErofsBackend::Fuse => {
            let mut c = Command::new("erofsfuse");
            c.arg(erofs).arg(mp);
            c
        }
    };
    let out = cmd.output().map_err(|e| {
        EngineError::runtime(
            format!("spawning erofs mount ({backend:?}): {e}"),
            Some(Box::new(e)),
        )
    })?;
    if !out.status.success() {
        return Err(EngineError::runtime(
            format!(
                "erofs mount of {} at {} failed ({backend:?}, status={}): {}",
                erofs.display(),
                mp.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim(),
            ),
            None,
        ));
    }
    Ok(())
}

/// Unmount each mountpoint best-effort (reverse order). Kernel mounts go
/// through `umount(8)` (which releases the loop device); erofsfuse mounts
/// through `fusermount -u`. Either way, a lazy `umount2(MNT_DETACH)` is
/// the last-resort fallback. Errors are ignored — a leftover mount is the
/// kernel's problem once the last reference closes.
fn unmount_all(mountpoints: &[PathBuf], backend: ErofsBackend) {
    for mp in mountpoints.iter().rev() {
        let ok = match backend {
            ErofsBackend::Kernel => Command::new("umount")
                .arg(mp)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false),
            ErofsBackend::Fuse => crate::mount_util::fusermount_unmount(mp),
        };
        if !ok {
            let _ = umount2(mp, MntFlags::MNT_DETACH);
        }
    }
}

#[cfg(test)]
mod tests;
