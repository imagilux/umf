//! [`VmHandle`] — typed wrapper around the spawned VMM process plus the
//! optional control-channel socket path.

use std::path::PathBuf;

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
        }
    }
}
