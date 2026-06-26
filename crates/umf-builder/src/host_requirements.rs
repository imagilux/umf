//! Host-side runtime requirements — what tools / kernel features the build
//! host needs for a given AST, plus the corresponding detector.
//!
//! UMF embeds everything it can (artifact parsing, OCI-layout
//! management, libcontainer-driven RUN execution, GPT + FAT +
//! SquashFS + CPIO emission, …). The container target runs entirely
//! in-process via `umf-engine`; nothing on the host PATH is required
//! for it. The host requirements that remain are bootable-target specific:
//!
//! * **Container target** — no external runtime. The engine is
//!   linked in via `libcontainer`.
//! * **bootable target — disk artifacts** — needs nothing beyond the Rust
//!   toolchain. The whole pipeline is in-tree.
//! * **bootable target — `RUN` directives** — needs `qemu-system-<arch>`,
//!   ideally with KVM accessible at `/dev/kvm`. Without KVM, QEMU
//!   falls back to TCG (software emulation) — works, but slow enough
//!   to be surprising during a build, so we surface that as a warning.
//!
//! The intent is to surface missing runtimes at `umf build` start,
//! *before* we parse + resolve + stage anything — so the operator
//! gets a "install qemu first" message in seconds, not after a
//! five-minute rootfs pull.

use std::path::PathBuf;

use thiserror::Error;
use umf_core::architecture::Architecture;
use umf_core::ast::{Ast, Directive, Stage};

// ── What the AST needs ──────────────────────────────────────────────────────

/// What runtimes the build will exercise — derived from the AST shape.
///
/// All flags default to `false`; [`compute_requirements`] sets them based
/// on which directives the source uses.
///
/// The container target needs no external runtime — `umf-engine` links
/// `libcontainer` directly. This struct therefore only covers the VM
/// target's QEMU + KVM dependency.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequiredRuntimes {
    /// QEMU binary. Set when the AST builds the bootable target *and* has at
    /// least one `RUN` directive — which needs a micro-VM to execute
    /// against the in-progress disk image.
    pub qemu: bool,
    /// KVM device — `/dev/kvm` accessible. Strict-required by the VM RUN
    /// backend for usable performance; QEMU falls back to TCG software
    /// emulation otherwise but it's slow enough to surface as a warning.
    pub kvm: bool,
}

/// Compute the runtime requirements for `ast`, given whether the build is
/// `bootable` (the caller determines this by introspecting the resolved `FROM`
/// artifact's type label — `FROM` a kernel ⇒ bootable).
///
/// Container builds need no host runtime (umf-engine is in-process). A bootable
/// build with `RUN` directives executes them in a micro-VM, so it needs
/// QEMU + KVM. This is a fast early hint; `build_vm` still errors precisely if
/// QEMU is missing at RUN time.
pub fn compute_requirements(ast: &Ast, bootable: bool) -> RequiredRuntimes {
    let mut req = RequiredRuntimes::default();
    if bootable {
        for stage in &ast.stages {
            if has_run_directive(stage) {
                req.qemu = true;
                req.kvm = true;
            }
        }
    }
    req
}

fn has_run_directive(stage: &Stage) -> bool {
    stage
        .directives
        .iter()
        .any(|d| matches!(d, Directive::Run(_)))
}

// ── What the host actually has ──────────────────────────────────────────────

/// Snapshot of what the build host provides — the result of running
/// [`verify_requirements`].
#[derive(Debug, Clone)]
pub struct DetectedRuntimes {
    /// `qemu-system-x86_64` (or arch-equivalent) on `PATH`.
    pub qemu_path: Option<PathBuf>,
    /// `cloud-hypervisor` on `PATH` — the alternative VMM backend
    /// (`umf run --vmm ch`). Optional: QEMU is the default.
    pub cloud_hypervisor_path: Option<PathBuf>,
    /// `/dev/kvm` status — whether the device exists and is readable.
    pub kvm_status: KvmStatus,
}

/// State of `/dev/kvm` on the build host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KvmStatus {
    /// `/dev/kvm` exists and the current process can open it.
    Accessible,
    /// `/dev/kvm` exists but the current process can't open it (group
    /// membership / permission issue).
    PresentNoPermission,
    /// `/dev/kvm` doesn't exist (no KVM on this kernel / wrong host /
    /// macOS / Windows / WSL without nested virt).
    Absent,
}

impl KvmStatus {
    /// `true` when the kernel can give us hardware-accelerated KVM.
    pub const fn is_accessible(self) -> bool {
        matches!(self, Self::Accessible)
    }
}

// ── Errors ──────────────────────────────────────────────────────────────────

/// One missing runtime, captured for assembly into [`MissingRuntimeError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingRuntime {
    /// `qemu-system-<arch>` absent. The architecture variant records
    /// which arch the build is targeting so the hint can name the right
    /// binary.
    Qemu(Architecture),
}

impl MissingRuntime {
    /// One-line human-readable installation hint.
    pub fn hint(self) -> String {
        match self {
            Self::Qemu(arch) => format!(
                "install `{}` — required by bootable-target `RUN` directives \
                 (the micro-VM backend boots the in-progress disk to execute the command)",
                arch.qemu_binary_name(),
            ),
        }
    }
}

/// One or more required runtimes were missing.
#[derive(Debug, Error)]
#[error(
    "host is missing {} required runtime(s): {missing:?}\n\
     hints:\n{}",
    .missing.len(),
    .missing
        .iter()
        .map(|m| format!("  - {m:?}: {}", m.hint()))
        .collect::<Vec<_>>()
        .join("\n"),
)]
pub struct MissingRuntimeError {
    /// The runtimes that were absent.
    pub missing: Vec<MissingRuntime>,
}

// ── Verifier ────────────────────────────────────────────────────────────────

/// Detect what's installed on the host and confirm it satisfies `req`.
///
/// Returns `Ok(DetectedRuntimes)` when everything required is present. KVM
/// is intentionally *not* fatal — when QEMU is present but KVM isn't, the
/// returned `kvm_status` records the situation and the caller decides
/// whether to warn or fall through to TCG.
pub fn verify_requirements(
    req: &RequiredRuntimes,
) -> Result<DetectedRuntimes, MissingRuntimeError> {
    verify_requirements_for(req, Architecture::host())
}

/// Variant of [`verify_requirements`] that detects the QEMU binary for
/// a specific target architecture rather than the build host's. Use
/// when the AST sets a cross-arch `--platform`.
pub fn verify_requirements_for(
    req: &RequiredRuntimes,
    architecture: Architecture,
) -> Result<DetectedRuntimes, MissingRuntimeError> {
    let detected = detect_all_for(architecture);
    let mut missing: Vec<MissingRuntime> = Vec::new();
    if req.qemu && detected.qemu_path.is_none() {
        missing.push(MissingRuntime::Qemu(architecture));
    }
    if missing.is_empty() {
        Ok(detected)
    } else {
        Err(MissingRuntimeError { missing })
    }
}

/// Detect every known runtime, regardless of what `RequiredRuntimes`
/// asked for. Useful for `umf doctor` and CLI introspection — the full
/// snapshot regardless of what a specific build needs.
///
/// Shorthand for [`detect_all_for`] with [`Architecture::host`] — i.e.
/// detection probes the QEMU binary matching the build host's CPU.
pub fn detect_all() -> DetectedRuntimes {
    detect_all_for(Architecture::host())
}

/// Detect every known runtime, looking up the QEMU binary appropriate
/// for `architecture` (so a host building `linux/arm64` VMs detects
/// `qemu-system-aarch64` rather than `qemu-system-x86_64`).
pub fn detect_all_for(architecture: Architecture) -> DetectedRuntimes {
    DetectedRuntimes {
        qemu_path: which_on_path(architecture.qemu_binary_name()),
        cloud_hypervisor_path: which_on_path("cloud-hypervisor"),
        kvm_status: probe_kvm(),
    }
}

fn probe_kvm() -> KvmStatus {
    let path = std::path::Path::new("/dev/kvm");
    if !path.exists() {
        return KvmStatus::Absent;
    }
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(_) => KvmStatus::Accessible,
        Err(_) => KvmStatus::PresentNoPermission,
    }
}

/// Resolve `name` to the first matching executable on `PATH`, or `None` when
/// it's absent. Used by the host preflight and reused by `umf doctor` to
/// surface optional helper binaries.
pub fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// ── Container RUN-step network egress ─────────────────────────────────────────

/// Host readiness for giving container `RUN` steps NAT'd network egress.
///
/// `umf-engine` wires each `RUN`'s isolated network namespace out through the
/// host (veth + `nft` masquerade — see the `umf-networking` crate). That needs
/// three things on the host, surfaced here for `umf doctor`. None is fatal — a
/// `RUN` step that doesn't touch the network builds fine without any of them,
/// and egress setup is best-effort — but a build whose `RUN` runs `apt`/`git`
/// on a host missing these will see the command fail, so it's worth reporting.
#[derive(Debug, Clone)]
pub struct NetworkEgress {
    /// `nft` binary on `PATH` — used to program the masquerade rule.
    pub nft_path: Option<PathBuf>,
    /// `dnsmasq` binary on `PATH` — the default DHCP/DNS served in a
    /// cloud-hypervisor VM's port-forward netns (an operator may run their own
    /// instead). Advisory.
    pub dnsmasq_path: Option<PathBuf>,
    /// `net.ipv4.ip_forward` state. UMF turns this on itself per build, but a
    /// host whose sysctl forces it off will break egress.
    pub ip_forward: SysctlState,
    /// Whether a netfilter `forward`-hook chain defaults to `drop`. A
    /// default-drop FORWARD policy silently blocks NAT'd egress and UMF can't
    /// fix it (the operator must allow the traffic). `Unknown` when the ruleset
    /// can't be read — typically because `umf doctor` wasn't run as root.
    pub forward_policy: ForwardPolicy,
}

/// State of a boolean sysctl.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysctlState {
    /// Set to `1`.
    Enabled,
    /// Set to `0`.
    Disabled,
    /// Couldn't be read.
    Unknown,
}

/// Default policy of the host's netfilter `forward` hook, as it affects NAT'd
/// container egress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardPolicy {
    /// No `forward`-hook chain defaults to `drop` — egress will be forwarded.
    Accept,
    /// At least one `forward`-hook chain defaults to `drop` — NAT'd egress is
    /// blocked until the operator adds an accept rule for it.
    Drop,
    /// The ruleset couldn't be inspected (no `nft`, or insufficient privilege).
    Unknown,
}

/// Probe the host's container-egress readiness. Best-effort: the FORWARD-policy
/// check shells out to `nft` (allowed for networking, like `ip`) and needs
/// privilege to read the ruleset, degrading to [`ForwardPolicy::Unknown`].
pub fn detect_network_egress() -> NetworkEgress {
    NetworkEgress {
        nft_path: which_on_path("nft"),
        dnsmasq_path: which_on_path("dnsmasq"),
        ip_forward: read_bool_sysctl("/proc/sys/net/ipv4/ip_forward"),
        forward_policy: probe_forward_policy(),
    }
}

fn read_bool_sysctl(path: &str) -> SysctlState {
    match std::fs::read_to_string(path) {
        Ok(s) => match s.trim() {
            "1" => SysctlState::Enabled,
            _ => SysctlState::Disabled,
        },
        Err(_) => SysctlState::Unknown,
    }
}

fn probe_forward_policy() -> ForwardPolicy {
    let output = std::process::Command::new("nft")
        .args(["list", "ruleset"])
        .output();
    match output {
        Ok(o) if o.status.success() => parse_forward_policy(&String::from_utf8_lossy(&o.stdout)),
        // `nft` absent, or not permitted to read the ruleset (non-root).
        _ => ForwardPolicy::Unknown,
    }
}

/// Scan an `nft list ruleset` dump for a base chain hooked into `forward` whose
/// default policy is `drop`. nft prints these on one line, e.g.
/// `type filter hook forward priority filter; policy drop;`.
fn parse_forward_policy(ruleset: &str) -> ForwardPolicy {
    let blocks = ruleset.lines().any(|line| {
        let l = line.trim();
        l.contains("hook forward") && l.contains("policy drop")
    });
    if blocks {
        ForwardPolicy::Drop
    } else {
        ForwardPolicy::Accept
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
