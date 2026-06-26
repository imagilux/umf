//! Cloud Hypervisor backend for [`crate::VmRuntime`].
//!
//! Spawns `cloud-hypervisor` as a subprocess (the one unavoidable
//! `Command::new` lives in [`spawn`]) and controls it over its REST
//! API via the [`cloud_hypervisor_client`] crate (auto-generated from
//! the upstream OpenAPI YAML — see `xtask regen-ch-client` for the
//! fallback regeneration path if the published crate ever lags
//! upstream).

pub mod spawn;

use std::time::Duration;

use async_trait::async_trait;
use cloud_hypervisor_client::SocketBasedApiClient;
use cloud_hypervisor_client::apis::DefaultApi;
use cloud_hypervisor_client::models::{
    CpusConfig, DiskConfig, MemoryConfig, NetConfig, PayloadConfig, VmConfig, VmInfo as ChVmInfo,
    VmState,
};
use cloud_hypervisor_client::socket_based_api_client;
use tokio::time::{Instant, sleep};
use tracing::{debug, info, warn};

use crate::error::VmError;
use crate::handle::VmHandle;
use crate::runtime::{
    BOOT_READY_TIMEOUT, BootSource, Firmware, VmInfo, VmRuntime, VmSpec, VmStatus,
};

/// Cloud Hypervisor impl of [`VmRuntime`].
///
/// Mirrors the [`super::qemu::QemuRuntime`] shape: stateless apart
/// from the binary name + a configurable graceful-shutdown timeout.
#[derive(Debug, Clone)]
pub struct CloudHypervisorRuntime {
    /// Name of the `cloud-hypervisor` binary on `PATH`. Override via
    /// [`Self::with_binary`] for non-standard install paths.
    binary: String,
    /// Grace period for ACPI `power_button_vm` before we fall back to
    /// `shutdown_vmm`. Default 30 s.
    graceful_shutdown_timeout: Duration,
}

impl Default for CloudHypervisorRuntime {
    fn default() -> Self {
        Self {
            binary: "cloud-hypervisor".to_string(),
            graceful_shutdown_timeout: Duration::from_secs(30),
        }
    }
}

impl CloudHypervisorRuntime {
    /// Build a runtime that uses `binary` instead of the default
    /// `cloud-hypervisor`.
    #[must_use]
    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            ..Self::default()
        }
    }

    /// Borrow the binary name (for `umf doctor` surfacing).
    #[must_use]
    pub fn binary(&self) -> &str {
        &self.binary
    }
}

fn open_client(socket: &std::path::Path) -> SocketBasedApiClient {
    socket_based_api_client(socket)
}

/// Per-request timeout for a single cloud-hypervisor REST control call, so a
/// wedged-but-connected daemon (guest stalled, VMM hung) can't hang the caller
/// forever. The boot/shutdown poll loops only check their deadline
/// *between* calls.
const CH_RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Wait briefly for the daemon to start accepting REST commands. The
/// `--api-socket path=...` bind is the daemon's first action after
/// argv parse, but the listen-then-accept race still leaves room for
/// a few early connection-refused responses. `vmm_ping_get` is the
/// canonical "is the daemon ready" probe.
async fn wait_for_daemon(client: &SocketBasedApiClient) -> Result<(), VmError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match client.vmm_ping_get().await {
            Ok(_) => return Ok(()),
            Err(_) if Instant::now() < deadline => {
                sleep(Duration::from_millis(50)).await;
            }
            Err(err) => {
                return Err(VmError::Control(format!(
                    "cloud-hypervisor daemon never came up: {err:?}",
                )));
            }
        }
    }
}

fn vm_state_to_vm_status(state: VmState) -> VmStatus {
    match state {
        VmState::Created => VmStatus::Booting,
        VmState::Running => VmStatus::Running,
        VmState::Paused => VmStatus::Paused,
        VmState::Shutdown => VmStatus::ShuttingDown,
    }
}

/// Translate our `VmSpec` into a `cloud_hypervisor_client::models::VmConfig`.
///
/// Constraints:
///
/// - Cloud Hypervisor cannot boot a raw disk without a `payload`
///   (kernel or firmware). We require the caller to supply a firmware
///   path via `BootSource::Disk { firmware: Some(...) }` — the CLI
///   surfaces this as `--firmware PATH`.
/// - Direct-kernel boot needs no firmware: the kernel *is* the payload.
///   `BootSource::DirectKernel` maps straight to the `PayloadConfig`
///   `kernel` / `initramfs` / `cmdline` fields (the firmware-free fast path
///   the `--vmm ch` docs advertise).
fn build_vm_config(spec: &VmSpec) -> Result<VmConfig, VmError> {
    let payload = match &spec.boot {
        BootSource::Disk { firmware, .. } => {
            let firmware = firmware.as_ref().ok_or_else(|| {
                VmError::backend(
                    "cloud-hypervisor requires --firmware PATH (it cannot boot a raw disk \
                     without a firmware payload — pass an OVMF / EDK II image)",
                    None,
                )
            })?;
            // Cloud Hypervisor takes a single firmware payload. A single-file
            // blob maps directly; for a split CODE/VARS layout we hand it the
            // CODE half (its `CLOUDHV.fd` / OVMF builds carry the variable
            // store internally, so there is no separate VARS pflash to wire).
            let fw_path = match firmware {
                Firmware::Bios(p) => p,
                Firmware::Pflash { code, .. } => code,
            };
            let mut p = PayloadConfig::new();
            p.firmware = Some(fw_path.to_string_lossy().into_owned());
            p
        }
        BootSource::DirectKernel {
            kernel,
            initrd,
            cmdline,
        } => {
            let mut p = PayloadConfig::new();
            p.kernel = Some(kernel.to_string_lossy().into_owned());
            p.initramfs = Some(initrd.to_string_lossy().into_owned());
            p.cmdline = Some(cmdline.clone());
            p
        }
    };

    let mut cfg = VmConfig::new(payload);
    let cpu_count = i32::try_from(spec.cpus).unwrap_or(i32::MAX);
    let mut cpus = CpusConfig::new(cpu_count, cpu_count);
    cpus.boot_vcpus = cpu_count;
    cfg.cpus = Some(cpus);
    let mem_size = i64::from(spec.memory_mib) * 1024 * 1024;
    let mut mem = MemoryConfig::new(mem_size);
    mem.size = mem_size;
    cfg.memory = Some(mem);

    if let BootSource::Disk { path, .. } = &spec.boot {
        let mut disk = DiskConfig::new();
        disk.path = Some(path.to_string_lossy().into_owned());
        cfg.disks = Some(vec![disk]);
    }

    // Cloud Hypervisor has no user-mode networking, so host port-forwarding is
    // wired host-side: `umf run` sets up a tap + nft DNAT (umf-networking) and
    // hands us `spec.net`, and we attach the tap. Port-forwards without that
    // orchestration would be silently dropped, so refuse them instead.
    if !spec.port_forwards.is_empty() && spec.net.is_none() {
        return Err(VmError::backend(
            "cloud-hypervisor port forwarding goes through `umf run`'s tap setup \
             (`umf run --vmm ch --port-forward …`); or use `--vmm qemu`",
            None,
        ));
    }
    if let Some(net) = &spec.net {
        let mut nic = NetConfig::new();
        nic.tap = Some(net.tap.clone());
        cfg.net = Some(vec![nic]);
    }

    Ok(cfg)
}

#[async_trait]
impl VmRuntime for CloudHypervisorRuntime {
    #[tracing::instrument(level = "info",
        name = "umf.vmm.ch.create", skip(self, spec), fields(binary = %self.binary))]
    async fn create(&self, spec: &VmSpec) -> Result<VmHandle, VmError> {
        let handle = spawn::spawn_cloud_hypervisor(&self.binary, spec).await?;

        // Build the VM but don't boot it yet — `boot` does that as the
        // separate trait method so callers can split create / boot if
        // they want pre-boot diagnostics.
        let Some(socket) = handle.control_socket.clone() else {
            return Err(VmError::backend(
                "cloud-hypervisor spawn returned no control socket (bug)",
                None,
            ));
        };
        let client = open_client(&socket);
        wait_for_daemon(&client).await?;

        let config = build_vm_config(spec)?;
        tokio::time::timeout(CH_RPC_TIMEOUT, client.create_vm(config))
            .await
            .map_err(|_| VmError::Control("cloud-hypervisor create_vm timed out".into()))?
            .map_err(|err| VmError::Control(format!("cloud-hypervisor create_vm: {err:?}")))?;

        debug!(id = %handle.id, "cloud-hypervisor: vm.create succeeded");
        Ok(handle)
    }

    #[tracing::instrument(level = "info",
        name = "umf.vmm.ch.boot", skip(self, vm), fields(id = %vm.id))]
    async fn boot(&self, vm: &mut VmHandle) -> Result<(), VmError> {
        let Some(socket) = vm.control_socket.clone() else {
            debug!(id = %vm.id, "cloud-hypervisor boot: no control socket; nothing to wait for");
            return Ok(());
        };
        let client = open_client(&socket);
        tokio::time::timeout(CH_RPC_TIMEOUT, client.boot_vm())
            .await
            .map_err(|_| VmError::Control("cloud-hypervisor boot_vm timed out".into()))?
            .map_err(|err| VmError::Control(format!("cloud-hypervisor boot_vm: {err:?}")))?;

        let deadline = Instant::now() + BOOT_READY_TIMEOUT;
        loop {
            let info = tokio::time::timeout(CH_RPC_TIMEOUT, client.vm_info_get())
                .await
                .map_err(|_| VmError::Control("cloud-hypervisor vm_info_get timed out".into()))?
                .map_err(|err| {
                    VmError::Control(format!("cloud-hypervisor vm_info_get: {err:?}"))
                })?;
            if matches!(info.state, VmState::Running) {
                info!(id = %vm.id, "cloud-hypervisor boot: guest running");
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(VmError::BootFailed(format!(
                    "guest did not reach `Running` within {BOOT_READY_TIMEOUT:?} (last state: {:?})",
                    info.state,
                )));
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    #[tracing::instrument(level = "info",
        name = "umf.vmm.ch.info", skip(self, vm), fields(id = %vm.id))]
    async fn info(&self, vm: &VmHandle) -> Result<VmInfo, VmError> {
        let Some(socket) = vm.control_socket.clone() else {
            return Ok(crate::backends::common::no_control_info());
        };
        let client = open_client(&socket);
        let info: ChVmInfo = tokio::time::timeout(CH_RPC_TIMEOUT, client.vm_info_get())
            .await
            .map_err(|_| VmError::Control("cloud-hypervisor vm_info_get timed out".into()))?
            .map_err(|err| VmError::Control(format!("cloud-hypervisor vm_info_get: {err:?}")))?;
        Ok(VmInfo {
            status: vm_state_to_vm_status(info.state),
            detail: format!("{:?}", info.state),
        })
    }

    #[tracing::instrument(
        level = "info",
        name = "umf.vmm.ch.shutdown",
        skip(self, vm),
        fields(id = %vm.id, graceful = graceful)
    )]
    async fn shutdown(&self, vm: &mut VmHandle, graceful: bool) -> Result<(), VmError> {
        let Some(socket) = vm.control_socket.clone() else {
            crate::backends::common::kill_child(vm);
            return Ok(());
        };
        let client = open_client(&socket);

        if graceful {
            tokio::time::timeout(CH_RPC_TIMEOUT, client.power_button_vm())
                .await
                .map_err(|_| VmError::Control("cloud-hypervisor power_button_vm timed out".into()))?
                .map_err(|err| {
                    VmError::Control(format!("cloud-hypervisor power_button_vm: {err:?}"))
                })?;
            let deadline = Instant::now() + self.graceful_shutdown_timeout;
            while Instant::now() < deadline {
                if let Some(child) = vm.child.as_mut()
                    && child.try_wait().map_err(VmError::Io)?.is_some()
                {
                    return Ok(());
                }
                if let Some(info) = tokio::time::timeout(CH_RPC_TIMEOUT, client.vm_info_get())
                    .await
                    .ok()
                    .and_then(Result::ok)
                    && matches!(info.state, VmState::Shutdown)
                {
                    // The VM stopped but the daemon may still be alive
                    // (cloud-hypervisor doesn't auto-exit on guest
                    // shutdown unless told to). Ask the daemon to quit
                    // so `wait` returns promptly.
                    let _ = tokio::time::timeout(CH_RPC_TIMEOUT, client.shutdown_vmm()).await;
                    return Ok(());
                }
                sleep(Duration::from_millis(200)).await;
            }
            warn!(
                id = %vm.id,
                "cloud-hypervisor graceful shutdown timed out; killing daemon"
            );
            // Fall through to the hard-stop path.
        }

        let stop_failed = tokio::time::timeout(CH_RPC_TIMEOUT, client.shutdown_vmm())
            .await
            .map(|r| r.is_err())
            .unwrap_or(true);
        if stop_failed && let Some(child) = vm.child.as_mut() {
            let _ = child.start_kill();
        }
        Ok(())
    }

    #[tracing::instrument(level = "info",
        name = "umf.vmm.ch.wait", skip(self, vm), fields(id = %vm.id))]
    async fn wait(&self, vm: &mut VmHandle) -> Result<Option<i32>, VmError> {
        crate::backends::common::wait(vm).await
    }
}

#[cfg(test)]
mod tests;
