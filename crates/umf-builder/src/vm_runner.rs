//! VM RUN backend — execute a single `RUN` directive in a micro-VM and
//! capture the result.
//!
//! The flow per RUN step:
//!
//! 1. Write the command to `<staging>/.umf-cmd` and env to
//!    `<staging>/.umf-env` so the guest reads them via the 9p share.
//! 2. Generate a run-flavour initramfs ([`crate::initrd::InitramfsFlavor::Run`]).
//! 3. Hand a [`umf_vmm::VmSpec`] (direct kernel boot + a 9p share of the
//!    staging dir + serial captured to a file) to the [`umf_vmm`] QEMU
//!    backend. Boot is fast (~1 s with KVM) because we skip OVMF / GPT /
//!    FAT entirely.
//! 4. The guest's init mounts the 9p share, sources `.umf-env`, chroots
//!    into `/sysroot`, exec's the command, writes the exit code to
//!    `<staging>/.umf-run-exit`, powers off.
//! 5. The host reads the exit code, reads the captured serial output,
//!    cleans up the helper files.
//!
//! Result: filesystem mutations the RUN performed are already on the host
//! (the 9p share writes through), and the host sees stdout/stderr + exit
//! code without any per-RUN disk-diff machinery. Subsequent RUN steps
//! see the cumulative state.
//!
//! The actual `qemu-system-*` spawn lives in [`umf_vmm`]'s QEMU backend —
//! this module owns only the build-specific lifecycle (helper files,
//! initramfs, exit-code marker) around it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;
use tokio::time::Instant;
use tracing::{info, warn};

use umf_vmm::backends::qemu::QemuRuntime;
use umf_vmm::{
    BootSource, ControlMode, DisplayMode, NinePShare, SerialMode, VmArch, VmRuntime, VmSpec,
};

use crate::initrd::{InitramfsFlavor, InitrdError, generate_initramfs_with_flavor};
use crate::kernel::KernelLayout;
use umf_oci::staging::BuildStaging;

// ── Errors ──────────────────────────────────────────────────────────────────

/// Errors produced by the VM RUN backend.
#[derive(Debug, Error)]
pub enum RunStepError {
    /// QEMU is required for this RUN step but not on `PATH`. The
    /// host-requirements preflight should catch this earlier; the error
    /// is here in case `run_step_vm` is invoked directly.
    #[error("qemu-system-x86_64 not on PATH — required to execute VM RUN steps")]
    QemuMissing,

    /// QEMU spawned but exited with a non-zero / signalled status. The
    /// guest powers off cleanly (`-no-reboot` ⇒ qemu exits 0), so a bad
    /// *QEMU* status means the VM itself faulted (panic / OOM / kill); the
    /// guest's own exit code lives in the staging marker file.
    #[error("qemu exited abnormally (status code {code:?})")]
    QemuAbnormalExit {
        /// QEMU's process exit code, or `None` if it was killed by a signal.
        code: Option<i32>,
    },

    /// The guest's init wrote nothing to `<staging>/.umf-run-exit` —
    /// usually a sign the kernel panicked before reaching the chroot, or
    /// the 9p mount couldn't be established.
    #[error(
        "guest init never wrote the exit-code marker — likely a kernel \
         panic or missing 9p / virtio modules in /lib/modules/<release>/"
    )]
    GuestNeverFinished,

    /// `.umf-run-exit` was present but its contents weren't a valid
    /// integer (corrupt / partial write).
    #[error("guest wrote a malformed exit code: {0:?}")]
    MalformedExitCode(String),

    /// Initramfs generation for this RUN step failed.
    #[error("initrd: {0}")]
    Initrd(#[from] InitrdError),

    /// The umf-vmm layer failed to spawn or wait on the micro-VM.
    #[error("vmm: {0}")]
    Vmm(#[from] umf_vmm::VmError),

    /// Underlying I/O error (writing helper files, reading exit code, ...).
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),

    /// The RUN's timeout elapsed before the guest powered off. The guest
    /// was killed; staging state may be partial.
    #[error(
        "RUN timed out after {timeout:?} — guest was forcibly stopped, \
         staging may be partially modified"
    )]
    Timeout {
        /// Configured timeout.
        timeout: Duration,
    },
}

// ── Configuration + result ──────────────────────────────────────────────────

/// Input configuration for a single VM RUN step.
#[derive(Debug, Clone)]
pub struct RunStepConfig {
    /// Shell command to execute inside the chroot, via `/bin/sh -c`.
    pub command: String,
    /// Environment variables to set before the command runs. Sourced as
    /// `KEY=VALUE` lines.
    pub env: BTreeMap<String, String>,
    /// Path to the `qemu-system-<arch>` binary. From
    /// [`crate::host_requirements::DetectedRuntimes`].
    pub qemu_path: PathBuf,
    /// Whether `/dev/kvm` is accessible — drives the choice between
    /// hardware acceleration (`-enable-kvm`) and TCG (software emulation,
    /// much slower).
    pub kvm_available: bool,
    /// Guest RAM in MiB. Defaults to half the host's RAM, clamped to
    /// `[1 GiB, 8 GiB]`; override with `UMF_RUN_MEMORY_MIB`.
    pub memory_mib: u32,
    /// Guest CPU count. Defaults to the host's available parallelism;
    /// override with `UMF_RUN_CPUS`.
    pub cpus: u32,
    /// Wall-clock timeout for the VM. Defaults to 5 minutes (20 under TCG /
    /// no KVM); override with `UMF_RUN_TIMEOUT_SECS`.
    pub timeout: Duration,
}

impl RunStepConfig {
    /// Host-derived defaults given a detected QEMU + KVM availability. Guest
    /// RAM, vCPU count, and the wall-clock timeout are sized from the host
    /// rather than a fixed 512 MiB / 2 vCPU / 5 min (which throttled large
    /// hosts and OOM'd or timed out heavy `RUN` steps), each overridable via a
    /// `UMF_RUN_*` env var.
    pub fn new(qemu_path: PathBuf, kvm_available: bool, command: String) -> Self {
        Self {
            command,
            env: BTreeMap::new(),
            qemu_path,
            kvm_available,
            memory_mib: default_run_memory_mib(),
            cpus: default_run_cpus(),
            timeout: default_run_timeout(kvm_available),
        }
    }
}

// ── Host-derived RUN resource defaults ───────────────────────────────────────

/// Minimum guest RAM for a RUN micro-VM (MiB). Below this even trivial package
/// transactions thrash, so the host-derived default never drops under it.
const MIN_RUN_MEMORY_MIB: u32 = 1024;

/// Upper bound on the *host-derived* default guest RAM (MiB). KVM demand-pages
/// guest memory, so the guest only consumes what the build actually touches;
/// this caps the figure handed to QEMU so a 256 GiB host doesn't spec an absurd
/// allocation. An explicit `UMF_RUN_MEMORY_MIB` is honoured above this.
const MAX_DEFAULT_RUN_MEMORY_MIB: u32 = 8192;

/// Base RUN wall-clock timeout in seconds (5 minutes).
const RUN_TIMEOUT_BASE_SECS: u64 = 5 * 60;

/// Multiplier applied to the RUN timeout when KVM is unavailable: TCG software
/// emulation is far slower, so a heavy `RUN` needs a longer ceiling before it
/// is declared hung.
const RUN_TCG_TIMEOUT_FACTOR: u64 = 4;

/// Parse a positive `u32` from environment variable `key`, or `None` if unset,
/// empty, unparseable, or zero.
fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|&n| n > 0)
}

/// Parse a positive `u64` from environment variable `key` (same rules as
/// [`env_u32`]).
fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|&n| n > 0)
}

/// Default guest vCPU count: the host's available parallelism (so a 64-core
/// host isn't throttled to 2), overridable via `UMF_RUN_CPUS`. Always >= 1.
fn default_run_cpus() -> u32 {
    if let Some(n) = env_u32("UMF_RUN_CPUS") {
        return n;
    }
    std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(2)
        .max(1)
}

/// Default guest RAM (MiB): half the host's total, clamped to
/// `[MIN_RUN_MEMORY_MIB, MAX_DEFAULT_RUN_MEMORY_MIB]`. An explicit
/// `UMF_RUN_MEMORY_MIB` overrides (with no upper bound, still floored at the
/// minimum).
fn default_run_memory_mib() -> u32 {
    if let Some(m) = env_u32("UMF_RUN_MEMORY_MIB") {
        return m.max(MIN_RUN_MEMORY_MIB);
    }
    let host = host_total_memory_mib().unwrap_or(0);
    (host / 2).clamp(MIN_RUN_MEMORY_MIB, MAX_DEFAULT_RUN_MEMORY_MIB)
}

/// Default RUN wall-clock timeout: [`RUN_TIMEOUT_BASE_SECS`], multiplied by
/// [`RUN_TCG_TIMEOUT_FACTOR`] when KVM is unavailable, overridable via
/// `UMF_RUN_TIMEOUT_SECS`.
fn default_run_timeout(kvm_available: bool) -> Duration {
    if let Some(secs) = env_u64("UMF_RUN_TIMEOUT_SECS") {
        return Duration::from_secs(secs);
    }
    let secs = if kvm_available {
        RUN_TIMEOUT_BASE_SECS
    } else {
        RUN_TIMEOUT_BASE_SECS * RUN_TCG_TIMEOUT_FACTOR
    };
    Duration::from_secs(secs)
}

/// Host total RAM in MiB, read from `/proc/meminfo` (Linux-only, matching the
/// rest of the builder). `None` if the file is unreadable or malformed, in
/// which case callers fall back to the minimum.
fn host_total_memory_mib() -> Option<u32> {
    parse_meminfo_total_mib(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

/// Extract `MemTotal` (reported in kB) from `/proc/meminfo` contents and return
/// it in MiB. Split out from [`host_total_memory_mib`] so the parser is
/// unit-testable without touching the real `/proc`.
fn parse_meminfo_total_mib(contents: &str) -> Option<u32> {
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return u32::try_from(kb / 1024).ok();
        }
    }
    None
}

/// Result of a successful VM RUN step.
#[derive(Debug, Clone)]
pub struct RunStepResult {
    /// Exit code the guest's command returned (as recorded by the init
    /// script via `$?`). `0` means success.
    pub exit_code: i32,
    /// Combined stdout + stderr captured from the guest's serial console.
    pub serial_output: String,
    /// Wall-clock time from QEMU spawn to QEMU exit.
    pub duration: Duration,
}

// ── Public entry ────────────────────────────────────────────────────────────

/// Execute one RUN step: spin up a micro-VM that chroots into the
/// staging tree, runs `config.command`, and returns the exit code +
/// serial output.
///
/// Filesystem mutations the command makes are already persisted on the
/// host (via the 9p share) by the time this function returns.
pub async fn run_step_vm(
    staging: &BuildStaging,
    kernel: &KernelLayout,
    config: &RunStepConfig,
) -> Result<RunStepResult, RunStepError> {
    if !config.qemu_path.is_file() {
        return Err(RunStepError::QemuMissing);
    }
    info!(
        cmd = %short_command(&config.command),
        kernel = %kernel.release,
        kvm = config.kvm_available,
        "vm_runner: starting RUN step",
    );

    // 1. Stage the helper files so the guest's init can read them.
    let cmd_path = staging.path().join(".umf-cmd");
    let env_path = staging.path().join(".umf-env");
    let exit_path = staging.path().join(".umf-run-exit");
    std::fs::write(&cmd_path, config.command.as_bytes())?;
    if !config.env.is_empty() {
        let body: String = config
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&env_path, body.as_bytes())?;
    }
    // Pre-clear any prior exit marker (re-runs).
    let _ = std::fs::remove_file(&exit_path);

    // 2. Generate the run-flavour initramfs.
    let (initrd_bytes, _report) =
        generate_initramfs_with_flavor(staging, kernel, InitramfsFlavor::Run)?;
    let scratch = tempfile::Builder::new()
        .prefix("umf-run-initrd-")
        .tempdir()?;
    let initrd_path = scratch.path().join("initrd.img");
    std::fs::write(&initrd_path, &initrd_bytes)?;

    // 3. Build the micro-VM spec: direct-kernel boot, the staging tree
    //    shared in over 9p, serial captured to a file, no control channel
    //    (the guest powers itself off when the command finishes).
    let serial_path = scratch.path().join("serial.log");
    // The qemu binary path already encodes the target arch (the host preflight
    // resolved `qemu-system-<arch>` for the build's platform); derive both the
    // machine type and the serial console from it, so an aarch64 build's
    // micro-VM gets `-machine virt` + `console=ttyAMA0` rather than x86
    // `q35` + `ttyS0` (the wrong console yields empty serial capture).
    let vm_arch = arch_from_qemu_path(&config.qemu_path);
    let spec = VmSpec {
        arch: vm_arch,
        boot: BootSource::DirectKernel {
            kernel: kernel.vmlinuz.clone(),
            initrd: initrd_path.clone(),
            cmdline: format!("console={} quiet panic=1", vm_arch.serial_console()),
        },
        memory_mib: config.memory_mib,
        cpus: config.cpus,
        kvm: config.kvm_available,
        display: DisplayMode::None,
        port_forwards: Vec::new(),
        net: None,
        control: ControlMode::None,
        shares: vec![NinePShare {
            host_path: staging.path().to_path_buf(),
            mount_tag: crate::bootable::MOUNT_TAG_STAGING.to_string(),
        }],
        serial: SerialMode::File(serial_path.clone()),
    };

    // 4. Spawn through umf-vmm and wait for the guest to power off,
    //    bounded by the configured timeout. On timeout the handle drops
    //    and umf-vmm's `kill_on_drop` terminates qemu — no orphan.
    let runtime = QemuRuntime::with_binary(config.qemu_path.to_string_lossy().into_owned());
    let started = Instant::now();
    let mut handle = runtime.create(&spec).await?;
    let exit = match tokio::time::timeout(config.timeout, runtime.wait(&mut handle)).await {
        Ok(res) => res?,
        Err(_) => {
            warn!("vm_runner: timeout — qemu killed via kill_on_drop");
            return Err(RunStepError::Timeout {
                timeout: config.timeout,
            });
        }
    };
    let duration = started.elapsed();

    // 5. QEMU's own status is a fault signal: the guest powers off cleanly
    //    (`-no-reboot` ⇒ qemu exits 0), so a non-zero / signalled qemu
    //    means the VM itself faulted (panic / OOM / kill).
    if exit != Some(0) {
        return Err(RunStepError::QemuAbnormalExit { code: exit });
    }

    // 6. Read the guest's exit code from the staging marker file.
    let exit_code_raw =
        std::fs::read_to_string(&exit_path).map_err(|_| RunStepError::GuestNeverFinished)?;
    let exit_code: i32 = exit_code_raw
        .trim()
        .parse()
        .map_err(|_| RunStepError::MalformedExitCode(exit_code_raw.clone()))?;

    // 7. Clean up helper files so subsequent RUNs / the final squashfs
    //    don't see UMF-internal markers.
    let _ = std::fs::remove_file(&cmd_path);
    let _ = std::fs::remove_file(&env_path);
    let _ = std::fs::remove_file(&exit_path);

    // 8. The guest console was captured to the serial file.
    let serial_output = std::fs::read_to_string(&serial_path).unwrap_or_default();

    info!(
        exit_code,
        duration = ?duration,
        "vm_runner: RUN step completed",
    );
    Ok(RunStepResult {
        exit_code,
        serial_output,
        duration,
    })
}

/// Infer the guest [`VmArch`] from the resolved `qemu-system-<arch>`
/// binary path. The host preflight already picked the binary matching the
/// build's target platform, so its filename is the authoritative arch
/// signal here. Defaults to [`VmArch::host`] when the name carries no
/// recognised arch token (e.g. a wrapper script) — the same loud-default
/// posture the rest of the codebase takes.
fn arch_from_qemu_path(qemu_path: &std::path::Path) -> VmArch {
    let name = qemu_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if name.contains("aarch64") || name.contains("arm64") {
        VmArch::Aarch64
    } else if name.contains("x86_64") || name.contains("amd64") {
        VmArch::X86_64
    } else {
        VmArch::host()
    }
}

fn short_command(cmd: &str) -> String {
    let oneline = cmd.replace('\n', " ").trim().to_string();
    if oneline.chars().count() > 64 {
        // Cut on a char boundary: the old `&oneline[..64]` byte-slice panicked
        // when a RUN command had a multi-byte char straddling byte 64.
        let head: String = oneline.chars().take(64).collect();
        format!("{head}...")
    } else {
        oneline
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
