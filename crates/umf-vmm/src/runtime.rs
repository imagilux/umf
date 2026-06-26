//! The `VmRuntime` trait and its supporting value types.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::VmError;
use crate::handle::VmHandle;

/// How long the disk-boot poll loops wait for the guest to reach the VMM's
/// `running` / `Running` state (QEMU's vCPU executing, Cloud Hypervisor booted)
/// before declaring a boot failure. This is the hypervisor reaching its run
/// state, not guest userspace coming up, so it stays fast even under TCG
/// software emulation. Shared by both backends so the deadline and its
/// diagnostic message can't drift apart.
pub(crate) const BOOT_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Guest CPU architecture the VMM should emulate / virtualise.
///
/// `umf-vmm` is deliberately free of any `umf-core` dependency (it's a
/// pure VMM control surface), so it carries its own minimal arch enum
/// rather than reusing `umf_core::Architecture`. The caller maps its own
/// architecture type onto this one when it builds the [`VmSpec`].
///
/// The architecture drives three argv decisions in the QEMU backend that
/// x86 and ARM disagree on:
///
/// * the machine type — `q35` (x86) vs `virt` (aarch64, which has no
///   board with a default CPU, hence the explicit `-cpu` below);
/// * the `-cpu` model — `host` under KVM, `max` under TCG, on both;
/// * the firmware wiring — x86 can take a single-file blob via `-bios`,
///   while aarch64 AAVMF is always a CODE/VARS pflash pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VmArch {
    /// 64-bit x86 (Intel / AMD). QEMU machine `q35`.
    #[default]
    X86_64,
    /// 64-bit ARM (Apple Silicon, AWS Graviton, Raspberry Pi). QEMU
    /// machine `virt`.
    Aarch64,
}

impl VmArch {
    /// Architecture matching the build host, resolved at compile time.
    /// Hosts on an arch this crate doesn't model fall back to
    /// [`VmArch::X86_64`] — the loudest failure surface, mirroring
    /// `umf_core::Architecture::host`.
    #[must_use]
    pub const fn host() -> Self {
        if cfg!(target_arch = "aarch64") {
            Self::Aarch64
        } else {
            Self::X86_64
        }
    }

    /// Name of the `qemu-system-<arch>` binary for this architecture.
    /// Used by [`crate::backends::qemu::QemuRuntime`] to pick its
    /// subprocess when the caller didn't pin one explicitly.
    #[must_use]
    pub const fn qemu_binary_name(self) -> &'static str {
        match self {
            Self::X86_64 => "qemu-system-x86_64",
            Self::Aarch64 => "qemu-system-aarch64",
        }
    }

    /// Linux serial-console device for this architecture's primary UART (the
    /// kernel `console=` token). x86's `q35` exposes a 16550 (`ttyS0`);
    /// aarch64's `virt` board a PL011 (`ttyAMA0`). Mirrors
    /// [`umf_core::architecture::Architecture::serial_console`].
    #[must_use]
    pub const fn serial_console(self) -> &'static str {
        match self {
            Self::X86_64 => "ttyS0",
            Self::Aarch64 => "ttyAMA0",
        }
    }
}

/// UEFI firmware payload for a disk boot, in one of the two layouts
/// shipped by host distributions.
///
/// Older / self-contained packages ship a single `OVMF.fd` that bundles
/// the read-only firmware code and the variable store together;
/// QEMU consumes it with `-bios`. Modern Debian/Fedora/Arch instead ship
/// a *split* layout (a read-only `OVMF_CODE.fd` plus a writable template
/// `OVMF_VARS.fd`) that must be wired as two `-drive if=pflash` units
/// (Cloud Hypervisor takes the code half as its firmware payload). The
/// caller resolves which shape the host has; the backend renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Firmware {
    /// Single-file OVMF/EDK II blob (`-bios <path>`).
    Bios(PathBuf),
    /// Split CODE/VARS pflash layout. `code` is read-only; `vars` is the
    /// writable variable store (the backend boots from a per-run copy so
    /// concurrent VMs don't share NVRAM and the host template stays
    /// pristine).
    Pflash {
        /// Read-only firmware code (`OVMF_CODE.fd`).
        code: PathBuf,
        /// Writable variable store (`OVMF_VARS.fd`).
        vars: PathBuf,
    },
}

/// What the runtime should boot.
#[derive(Debug, Clone)]
pub enum BootSource {
    /// Boot a finished disk image (the `umf run` shape). The disk
    /// carries its own bootloader + ESP + rootfs; the VMM hands control
    /// to firmware which finds the EFI loader and continues.
    Disk {
        /// Absolute path to the sparse raw disk image.
        path: PathBuf,
        /// Optional firmware override (OVMF / EDK II), single-file or
        /// split CODE/VARS pflash. When `None`, the backend uses its
        /// default UEFI firmware (`-bios` lookup for QEMU, `--firmware`
        /// for Cloud Hypervisor).
        firmware: Option<Firmware>,
    },
    /// Direct kernel boot (the per-RUN micro-VM shape used by the build
    /// path's `vm_runner`). Skips firmware + bootloader entirely; the
    /// VMM jumps straight to the kernel with the supplied initrd +
    /// command line.
    ///
    /// Reserved for the vm_runner migration tracked as a follow-up;
    /// not exercised by the run-path today.
    DirectKernel {
        /// Path to the kernel image (vmlinuz).
        kernel: PathBuf,
        /// Path to the initramfs.
        initrd: PathBuf,
        /// Kernel command line (`console=ttyS0 ...`).
        cmdline: String,
    },
}

/// Display surface the guest connects to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    /// No display; serial console only (default for `umf run` headless).
    None,
    /// Spawn the VMM's native graphical window (SDL/GTK).
    Window,
}

/// How the host talks to the VMM after spawn. The backend's lifecycle
/// methods are best-effort no-ops when [`Self::None`].
#[derive(Debug, Clone)]
pub enum ControlMode {
    /// Open a control channel after spawn. The backend supplies the
    /// socket path; the host can issue `info` / `shutdown` / hotplug
    /// commands.
    Channel,
    /// One-shot — spawn and `wait` only. No status queries, no graceful
    /// shutdown commands. Used by the build-path's per-RUN micro-VMs
    /// where the guest powers itself off when its work is done.
    None,
}

/// One host:guest port forward (user-mode networking).
#[derive(Debug, Clone, Copy)]
pub struct PortForward {
    /// Port on the host the VMM should bind. `0` ⇒ kernel picks.
    pub host_port: u16,
    /// Port inside the guest the host port maps to.
    pub guest_port: u16,
    /// TCP (`true`) or UDP (`false`).
    pub tcp: bool,
}

/// A host directory shared into the guest over virtio-9p (`-virtfs`).
///
/// The guest mounts it by `mount_tag` (`mount -t 9p <tag> /mnt`). Used by
/// the build path's per-RUN micro-VM to expose the staging tree so the
/// guest can read the command + write its results back to the host.
#[derive(Debug, Clone)]
pub struct NinePShare {
    /// Host directory to export.
    pub host_path: PathBuf,
    /// Tag the guest mounts the share by.
    pub mount_tag: String,
}

/// Where the guest's serial console is routed.
#[derive(Debug, Clone)]
pub enum SerialMode {
    /// Forward the serial console to the host's stdout/stderr — the
    /// `umf run` shape, where the user watches the console live. Wired
    /// as `-serial mon:stdio` for a headless guest.
    Inherit,
    /// Capture the serial console to a file the caller reads after the
    /// VM exits — the per-RUN micro-VM shape, where the build collects
    /// the command's output without it landing on the build's own
    /// stdout. Wired as `-serial file:<path>`.
    File(PathBuf),
}

/// Pre-built host networking to attach a guest to: launch the VM inside the
/// network namespace referenced by `netns_fd` and attach `tap` as its NIC. Set
/// up by the `umf run` CLI via `umf-networking` (the cloud-hypervisor
/// port-forward path); umf-vmm only honours it, so the crate stays a pure
/// control surface with no networking dependency of its own.
#[derive(Debug, Clone)]
pub struct TapNet {
    /// Raw fd of the network namespace the VMM joins. The backend `setns`-es the
    /// forked VMM child into it before exec (the native equivalent of `ip netns
    /// exec`). The fd is owned by the CLI's `umf-networking` guard, which keeps
    /// it open for the VM's lifetime.
    pub netns_fd: std::os::fd::RawFd,
    /// Tap device the guest attaches as its NIC.
    pub tap: String,
}

/// The spec a [`VmRuntime`] consumes to spawn one VM.
#[derive(Debug, Clone)]
pub struct VmSpec {
    /// Guest CPU architecture. Selects the QEMU machine type (`q35` vs
    /// `virt`), the `-cpu` model, and how firmware is wired (`-bios` vs
    /// pflash). Defaults to the build host's architecture.
    pub arch: VmArch,
    /// What to boot.
    pub boot: BootSource,
    /// Guest RAM in MiB.
    pub memory_mib: u32,
    /// Guest vCPU count.
    pub cpus: u32,
    /// Request hardware acceleration. Honoured verbatim: `true` wires
    /// `accel=kvm` (QEMU) / KVM mode (Cloud Hypervisor), `false` selects
    /// software emulation. No backend probes `/dev/kvm` — deciding
    /// whether KVM is actually accessible (and demoting this flag to
    /// `false` when it isn't) is the caller's responsibility before it
    /// builds the spec.
    pub kvm: bool,
    /// Headless or graphical.
    pub display: DisplayMode,
    /// Port forwards (host:guest). The QEMU backend implements these via
    /// user-mode networking (`hostfwd`). Cloud Hypervisor has no user-mode
    /// networking, so its port-forwarding is wired host-side instead via
    /// [`net`](Self::net) (set up by `umf run`).
    pub port_forwards: Vec<PortForward>,
    /// Pre-built host networking the guest attaches to (the cloud-hypervisor
    /// port-forward path). `None` for QEMU / no-port-forward.
    pub net: Option<TapNet>,
    /// How the host controls the guest after spawn.
    pub control: ControlMode,
    /// virtio-9p directory shares exported into the guest. Empty for a
    /// plain disk boot.
    pub shares: Vec<NinePShare>,
    /// Where the guest serial console is routed (host stdio vs a capture
    /// file). Honoured by the QEMU backend; the cloud-hypervisor backend
    /// is disk-boot only and treats every mode as inherit.
    pub serial: SerialMode,
}

impl VmSpec {
    /// Sensible defaults for the `umf run` use case: 1 GiB RAM, 2 vCPUs,
    /// KVM enabled, headless, QMP-controllable, no port forwards.
    #[must_use]
    pub fn from_disk(disk: PathBuf) -> Self {
        Self {
            arch: VmArch::host(),
            boot: BootSource::Disk {
                path: disk,
                firmware: None,
            },
            memory_mib: 1024,
            cpus: 2,
            kvm: true,
            display: DisplayMode::None,
            port_forwards: Vec::new(),
            net: None,
            control: ControlMode::Channel,
            shares: Vec::new(),
            serial: SerialMode::Inherit,
        }
    }
}

/// Coarse VM lifecycle state surfaced by [`VmRuntime::info`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmStatus {
    /// VMM has spawned but the guest isn't running yet (firmware /
    /// bootloader stage).
    Booting,
    /// Guest is running.
    Running,
    /// Guest is paused (suspend / breakpoint).
    Paused,
    /// Guest has shut down; VMM process may still be live.
    ShuttingDown,
    /// VMM process has exited.
    Stopped,
    /// State can't be determined. Returned by [`VmRuntime::info`] for a
    /// [`ControlMode::None`] VM: there's no control channel to query, so
    /// the runtime can't tell whether the guest is still running or has
    /// already powered itself off. Distinct from [`Self::Stopped`], which
    /// asserts the process *has* exited.
    Unknown,
}

/// Snapshot of the VM's state at the time of [`VmRuntime::info`].
#[derive(Debug, Clone)]
pub struct VmInfo {
    /// Coarse lifecycle state.
    pub status: VmStatus,
    /// Free-form backend-specific detail (e.g. QMP's `query-status`
    /// `status` string). Useful for logging; structured data goes in
    /// the typed fields above.
    pub detail: String,
}

/// A VMM control surface — spawn, boot-wait, query, shutdown, wait.
///
/// One impl per backend (QEMU, Cloud Hypervisor, future Firecracker).
/// All methods are async because the underlying control channels
/// (QMP, REST over Unix socket) are async-first.
#[async_trait]
pub trait VmRuntime: Send + Sync {
    /// Spawn the VMM with `spec` applied. Returns a handle the caller
    /// drives through the rest of the trait. The VMM process is alive
    /// when this returns; the guest may still be in firmware/bootloader
    /// stage — use [`Self::boot`] to wait until it's running.
    ///
    /// # Errors
    /// [`VmError::BinaryNotFound`], [`VmError::InputUnusable`],
    /// [`VmError::Io`], or a backend-specific [`VmError::Backend`].
    async fn create(&self, spec: &VmSpec) -> Result<VmHandle, VmError>;

    /// Block until the guest reports `Running` (or the deadline expires).
    /// No-op when [`ControlMode::None`] — there's no channel to query.
    ///
    /// # Errors
    /// [`VmError::Control`] if the channel breaks; [`VmError::BootFailed`]
    /// if the VMM exits before the guest comes up.
    async fn boot(&self, vm: &mut VmHandle) -> Result<(), VmError>;

    /// Query coarse status via the control channel. Returns
    /// [`VmStatus::Unknown`] when [`ControlMode::None`] — without a
    /// QMP/REST channel the runtime can't tell what stage the guest is
    /// in (it may still be running).
    ///
    /// # Errors
    /// [`VmError::Control`].
    async fn info(&self, vm: &VmHandle) -> Result<VmInfo, VmError>;

    /// Ask the guest to shut down. When `graceful=true` the backend
    /// sends an ACPI / `system_powerdown` / `vm.shutdown` request and
    /// waits up to its internal deadline; when `false` (or after the
    /// deadline) it kills the VMM process. After this returns, the VMM
    /// process may still be live — call [`Self::wait`] to collect the
    /// exit status.
    ///
    /// # Errors
    /// [`VmError::Control`], [`VmError::ShutdownTimeout`],
    /// [`VmError::Backend`].
    async fn shutdown(&self, vm: &mut VmHandle, graceful: bool) -> Result<(), VmError>;

    /// Wait for the VMM process to exit. Returns its raw exit code
    /// (`Some(n)`) or `None` if the process was killed by a signal.
    ///
    /// # Errors
    /// [`VmError::Io`] from the underlying child wait.
    async fn wait(&self, vm: &mut VmHandle) -> Result<Option<i32>, VmError>;
}
