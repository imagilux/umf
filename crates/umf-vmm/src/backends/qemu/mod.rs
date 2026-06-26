//! QEMU/KVM backend for [`crate::VmRuntime`].
//!
//! Spawns `qemu-system-<arch>` as a subprocess (the one unavoidable
//! `Command::new` lives in [`spawn`]) and controls it via QMP using the
//! `qapi` crate's tokio variant. Everything after spawn — boot-wait,
//! status, graceful shutdown — goes through typed QMP commands; no
//! string parsing of qemu's output.

pub mod spawn;

use std::time::Duration;

use async_trait::async_trait;
use qapi::futures::QmpStreamTokio;
use qapi::qmp;
use qapi::qmp::RunState;
use tokio::time::{Instant, sleep};
use tracing::{debug, info, warn};

use crate::error::VmError;
use crate::handle::VmHandle;
use crate::runtime::{BOOT_READY_TIMEOUT, VmInfo, VmRuntime, VmSpec, VmStatus};

/// QEMU/KVM impl of [`VmRuntime`].
///
/// Stateless apart from the binary name — instances are cheap to clone
/// and share. The binary can be pinned (`with_binary`, e.g. an absolute
/// `qemu-system-aarch64` path) or left unset, in which case it's derived
/// from the spec's [`crate::VmArch`] at spawn time so an aarch64 spec
/// reaches for `qemu-system-aarch64` rather than the x86 binary.
#[derive(Debug, Clone)]
pub struct QemuRuntime {
    /// Explicit `qemu-system-<arch>` binary (name on `PATH` or absolute
    /// path). `None` ⇒ derive from `spec.arch` at `create` time.
    binary: Option<String>,
    /// Grace period for ACPI `system_powerdown` before we fall back to
    /// `quit`. Default 30 s.
    graceful_shutdown_timeout: Duration,
}

impl Default for QemuRuntime {
    fn default() -> Self {
        Self {
            binary: None,
            graceful_shutdown_timeout: Duration::from_secs(30),
        }
    }
}

impl QemuRuntime {
    /// Build a runtime pinned to `binary` (e.g. an absolute
    /// `qemu-system-aarch64` path). Overrides the per-spec arch
    /// derivation — use it when the caller already resolved the binary.
    #[must_use]
    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            binary: Some(binary.into()),
            ..Self::default()
        }
    }

    /// Resolve the `qemu-system-<arch>` binary to spawn for `spec`: the
    /// pinned override when one was set, otherwise the default binary name
    /// for the spec's architecture.
    fn binary_for(&self, spec: &VmSpec) -> String {
        match &self.binary {
            Some(b) => b.clone(),
            None => spec.arch.qemu_binary_name().to_string(),
        }
    }

    /// Borrow the pinned binary name, if one was set (for `umf doctor`
    /// surfacing). `None` when the binary is derived per-spec.
    #[must_use]
    pub fn binary(&self) -> Option<&str> {
        self.binary.as_deref()
    }
}

/// Per-request timeout for a single QMP command and the capability handshake.
/// The boot/shutdown poll loops only check their deadline *between* calls, so
/// without this a wedged-but-connected qemu (KVM stall, silent QMP) would hang
/// a single `execute().await` forever.
const QMP_RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Open a QMP channel over the Unix socket the QEMU spawn helper exposed.
///
/// QEMU listens with `,server,nowait` so the bind happens before we
/// connect, but the listen-then-accept race means our first few
/// `connect`s can fail with `ConnectionRefused`. We retry briefly to
/// absorb that.
async fn open_qmp(
    socket: &std::path::Path,
) -> Result<
    qapi::futures::QapiStream<
        QmpStreamTokio<tokio::io::ReadHalf<tokio::net::UnixStream>>,
        QmpStreamTokio<tokio::io::WriteHalf<tokio::net::UnixStream>>,
    >,
    VmError,
> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match QmpStreamTokio::open_uds(socket).await {
            Ok(negotiation) => {
                let stream = tokio::time::timeout(QMP_RPC_TIMEOUT, negotiation.negotiate())
                    .await
                    .map_err(|_| VmError::Control("QMP capability negotiation timed out".into()))?
                    .map_err(|e| VmError::Control(format!("QMP negotiate: {e}")))?;
                return Ok(stream);
            }
            Err(_) if Instant::now() < deadline => {
                sleep(Duration::from_millis(50)).await;
            }
            Err(err) => {
                return Err(VmError::Control(format!(
                    "couldn't connect to QMP socket {sock}: {err}",
                    sock = socket.display(),
                )));
            }
        }
    }
}

fn run_state_to_vm_status(state: RunState) -> VmStatus {
    match state {
        RunState::running => VmStatus::Running,
        RunState::paused | RunState::prelaunch | RunState::suspended => VmStatus::Paused,
        RunState::shutdown | RunState::guest_panicked => VmStatus::ShuttingDown,
        RunState::internal_error | RunState::io_error | RunState::watchdog => {
            VmStatus::ShuttingDown
        }
        RunState::debug
        | RunState::inmigrate
        | RunState::postmigrate
        | RunState::finish_migrate
        | RunState::restore_vm
        | RunState::save_vm
        | RunState::colo => VmStatus::Booting,
    }
}

#[async_trait]
impl VmRuntime for QemuRuntime {
    #[tracing::instrument(level = "info", name = "umf.vmm.qemu.create", skip(self, spec))]
    async fn create(&self, spec: &VmSpec) -> Result<VmHandle, VmError> {
        let binary = self.binary_for(spec);
        spawn::spawn_qemu(&binary, spec).await
    }

    #[tracing::instrument(level = "info",
        name = "umf.vmm.qemu.boot", skip(self, vm), fields(id = %vm.id))]
    async fn boot(&self, vm: &mut VmHandle) -> Result<(), VmError> {
        let Some(socket) = vm.control_socket.clone() else {
            debug!(id = %vm.id, "qemu boot: no control channel; nothing to wait for");
            return Ok(());
        };
        let mut qmp = open_qmp(&socket).await?;

        let deadline = Instant::now() + BOOT_READY_TIMEOUT;
        loop {
            let status = tokio::time::timeout(QMP_RPC_TIMEOUT, qmp.execute(qmp::query_status {}))
                .await
                .map_err(|_| VmError::Control("QMP query-status timed out".into()))?
                .map_err(|e| VmError::Control(format!("QMP query-status: {e}")))?;
            if matches!(status.status, RunState::running) {
                info!(id = %vm.id, "qemu boot: guest running");
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(VmError::BootFailed(format!(
                    "guest did not reach `running` within {BOOT_READY_TIMEOUT:?} (last status: {:?})",
                    status.status,
                )));
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    #[tracing::instrument(level = "info",
        name = "umf.vmm.qemu.info", skip(self, vm), fields(id = %vm.id))]
    async fn info(&self, vm: &VmHandle) -> Result<VmInfo, VmError> {
        let Some(socket) = vm.control_socket.clone() else {
            return Ok(crate::backends::common::no_control_info());
        };
        let mut qmp = open_qmp(&socket).await?;
        let status = tokio::time::timeout(QMP_RPC_TIMEOUT, qmp.execute(qmp::query_status {}))
            .await
            .map_err(|_| VmError::Control("QMP query-status timed out".into()))?
            .map_err(|e| VmError::Control(format!("QMP query-status: {e}")))?;
        Ok(VmInfo {
            status: run_state_to_vm_status(status.status),
            detail: format!("{:?}", status.status),
        })
    }

    #[tracing::instrument(
        level = "info",
        name = "umf.vmm.qemu.shutdown",
        skip(self, vm),
        fields(id = %vm.id, graceful = graceful)
    )]
    async fn shutdown(&self, vm: &mut VmHandle, graceful: bool) -> Result<(), VmError> {
        let Some(socket) = vm.control_socket.clone() else {
            crate::backends::common::kill_child(vm);
            return Ok(());
        };
        let mut qmp = open_qmp(&socket).await?;

        if graceful {
            tokio::time::timeout(QMP_RPC_TIMEOUT, qmp.execute(qmp::system_powerdown {}))
                .await
                .map_err(|_| VmError::Control("QMP system_powerdown timed out".into()))?
                .map_err(|e| VmError::Control(format!("QMP system_powerdown: {e}")))?;
            let deadline = Instant::now() + self.graceful_shutdown_timeout;
            while Instant::now() < deadline {
                if let Some(child) = vm.child.as_mut()
                    && child.try_wait().map_err(VmError::Io)?.is_some()
                {
                    return Ok(());
                }
                if let Some(info) =
                    tokio::time::timeout(QMP_RPC_TIMEOUT, qmp.execute(qmp::query_status {}))
                        .await
                        .ok()
                        .and_then(Result::ok)
                    && matches!(info.status, RunState::shutdown)
                {
                    return Ok(());
                }
                sleep(Duration::from_millis(200)).await;
            }
            warn!(
                id = %vm.id,
                "qemu graceful shutdown timed out; sending QMP `quit`"
            );
            // Fall through to the hard-stop path.
        }

        let quit_failed = tokio::time::timeout(QMP_RPC_TIMEOUT, qmp.execute(qmp::quit {}))
            .await
            .map(|r| r.is_err())
            .unwrap_or(true);
        if quit_failed && let Some(child) = vm.child.as_mut() {
            let _ = child.start_kill();
        }
        Ok(())
    }

    #[tracing::instrument(level = "info",
        name = "umf.vmm.qemu.wait", skip(self, vm), fields(id = %vm.id))]
    async fn wait(&self, vm: &mut VmHandle) -> Result<Option<i32>, VmError> {
        crate::backends::common::wait(vm).await
    }
}

#[cfg(test)]
mod tests;
