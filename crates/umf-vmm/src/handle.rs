//! [`VmHandle`] — typed wrapper around the spawned VMM process plus the
//! optional control-channel socket path.

use std::path::{Path, PathBuf};

use tempfile::TempDir;
use tokio::process::Child;

/// A live VMM instance the host can drive.
///
/// Owned by the caller for the duration of the VM. The backends spawn the
/// VMM child with `kill_on_drop`, so dropping a `VmHandle` — or cancelling
/// a [`crate::VmRuntime::wait`] future, which takes the child with it —
/// terminates the VMM rather than orphaning it. That matters for the build
/// path, where a per-RUN micro-VM that wedges past its timeout must not
/// leak a qemu process. For an orderly stop, drive
/// [`crate::VmRuntime::shutdown`] + [`crate::VmRuntime::wait`] explicitly.
#[derive(Debug)]
pub struct VmHandle {
    /// The spawned VMM child process. `None` once [`crate::VmRuntime::wait`]
    /// has reaped it — subsequent calls to lifecycle methods will see
    /// `None` and treat the VM as already-exited.
    pub child: Option<Child>,
    /// Path to the control-channel socket the backend opened (QMP Unix
    /// socket for QEMU, REST API socket for Cloud Hypervisor). `None`
    /// when the spec asked for [`crate::ControlMode::None`].
    pub control_socket: Option<PathBuf>,
    /// Container-id style label for diagnostics. Backends populate this
    /// with whatever identifier they expose in their own logs (e.g.
    /// QEMU's `-name`).
    pub id: String,
    /// Temporary directories whose lifetime is tied to this VM: the control
    /// socket's parent, and the writable per-run copy of a split-firmware VARS
    /// store. The VMM holds these open for as long as it runs, so they cannot
    /// be removed at the end of the spawn call.
    ///
    /// Holding the [`TempDir`] values here rather than calling
    /// `TempDir::keep()` is what makes cleanup automatic: their own `Drop`
    /// removes the directories when the handle goes out of scope. Previously
    /// both backends detached them with `keep()` and nothing ever removed
    /// them, on the stated reasoning that the OS would reclaim them at process
    /// exit — which is not true of the system temp directory. A bootable build
    /// spawns one micro-VM per `RUN` step, so the leak grew with recipe
    /// length.
    ///
    /// Private on purpose: the field is an ownership token, and the type's
    /// guarantee is that nothing outside this crate can detach it and
    /// reintroduce the leak.
    scratch: Vec<TempDir>,
}

impl VmHandle {
    /// Construct an empty handle. Backends fill in `child` / socket /
    /// id during their `create` call.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            child: None,
            control_socket: None,
            id: id.into(),
            scratch: Vec::new(),
        }
    }

    /// Tie a temporary directory's lifetime to this handle, so it is removed
    /// when the handle drops. Backends call this for any scratch directory the
    /// VMM needs to outlive the spawn call.
    pub fn own_scratch(&mut self, dir: TempDir) {
        self.scratch.push(dir);
    }

    /// The scratch directories this handle currently owns. Diagnostics and
    /// tests only — the paths stop existing once the handle drops.
    #[must_use]
    pub fn scratch_paths(&self) -> Vec<&Path> {
        self.scratch.iter().map(TempDir::path).collect()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    /// The whole point of `scratch`: a directory handed to the handle is gone
    /// once the handle drops. Both backends previously detached theirs with
    /// `TempDir::keep()` and nothing removed them, so every VM spawn left a
    /// directory in the system temp dir — one per `RUN` step in a bootable
    /// build.
    #[test]
    fn dropping_the_handle_removes_the_scratch_directories() {
        let a = TempDir::new().expect("tempdir a");
        let b = TempDir::new().expect("tempdir b");
        let (pa, pb) = (a.path().to_path_buf(), b.path().to_path_buf());
        // A file inside, so this also proves a non-empty directory is removed
        // rather than a bare `rmdir` that would silently fail.
        std::fs::write(pa.join("qmp.sock"), b"x").expect("seed a");

        let mut handle = VmHandle::new("umf-vmm-scratch-test");
        handle.own_scratch(a);
        handle.own_scratch(b);
        assert_eq!(handle.scratch_paths(), vec![pa.as_path(), pb.as_path()]);
        assert!(
            pa.is_dir() && pb.is_dir(),
            "both exist while the handle lives"
        );

        drop(handle);

        assert!(
            !pa.exists(),
            "scratch dir must be removed when the handle drops"
        );
        assert!(
            !pb.exists(),
            "every owned scratch dir is removed, not just the first"
        );
    }

    /// A handle that owns nothing is still well-formed — `ControlMode::None`
    /// creates no socket directory and no firmware copy.
    #[test]
    fn a_handle_without_scratch_drops_cleanly() {
        let handle = VmHandle::new("umf-vmm-no-scratch");
        assert!(handle.scratch_paths().is_empty());
        drop(handle);
    }
}
