//! `umf run` — execute a previously-built image. Container artifacts
//! run via the linked-in libcontainer runtime; VM artifacts dispatch
//! to a `umf-vmm` backend (`--vmm=qemu|ch` + `--disk`).

use std::path::{Path, PathBuf};

use oci_client::Reference;
use thiserror::Error;
use tracing::info;
use umf_oci::registry::ImageLayout;
use umf_vmm::{Firmware, VmArch};

use crate::cli::util::{self, CredentialError};

/// Errors surfaced by `umf run`. Distinct from
/// [`crate::cli::build::CliBuildError`] so the run-path can report its
/// own concerns (image not runnable, container runtime failure, target
/// mismatch) with target-appropriate diagnostics.
#[derive(Debug, Error)]
pub(crate) enum CliRunError {
    #[error("layout dir: {0}")]
    LayoutDir(String),
    #[error("read password from stdin: {0}")]
    PasswordStdin(std::io::Error),
    #[error("--password-stdin requires --username")]
    PasswordStdinWithoutUsername,
    #[error("invalid OCI reference {reference:?}: {err}")]
    BadReference {
        reference: String,
        err: oci_client::ParseError,
    },
    #[error("registry: {0}")]
    Registry(#[from] umf_oci::registry::RegistryError),
    #[error("engine: {0}")]
    Engine(#[from] umf_engine::EngineError),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "{reference} is a {kind} component artifact (consumed by `FROM` / `ADD --from=<image>` \
         in downstream builds, or installed by `umf compile`); it isn't intended to run \
         standalone."
    )]
    NotRunnableComponent { reference: String, kind: String },
    #[error("unknown --vmm backend `{0}` (supported: qemu, ch)")]
    UnknownVmmBackend(String),
    #[error("VM network setup (cloud-hypervisor port forwarding): {0}")]
    VmNet(#[from] umf_networking::NetError),
    #[error("--vmm needs either a bootable-OS image reference or an explicit --disk PATH")]
    VmmWithoutDisk,
    #[error("--disk only meaningful with --vmm=<backend>")]
    DiskWithoutVmm,
    #[error(
        "--{flag} is a VM-boot flag and has no effect on container {reference}. \
         Use it with a bootable image or `--vmm`/`--disk`."
    )]
    VmFlagOnContainer { flag: String, reference: String },
    #[error("invalid port forward `{spec}`: {reason}")]
    BadPortForward { spec: String, reason: String },
    #[error("vmm: {0}")]
    Vmm(#[from] umf_vmm::VmError),
    #[error("compile: {0}")]
    Compile(#[from] umf_compile::CompileError),
    #[error(
        "no UEFI firmware for {arch} found at the usual host paths — {hint}, \
         or pass --firmware PATH to boot this image"
    )]
    NoUefiFirmware {
        /// Target architecture the firmware lookup was for (`x86_64` /
        /// `aarch64`).
        arch: String,
        /// Arch-specific install hint (which package ships the firmware).
        hint: String,
    },
}

impl From<CredentialError> for CliRunError {
    fn from(err: CredentialError) -> Self {
        match err {
            CredentialError::PasswordStdinWithoutUsername => Self::PasswordStdinWithoutUsername,
            CredentialError::PasswordStdin(e) => Self::PasswordStdin(e),
        }
    }
}

/// Bundled `umf run` flags. Threaded through `run_run` to avoid a
/// many-argument function signature.
pub(crate) struct RunArgs<'a> {
    pub(crate) reference: &'a str,
    pub(crate) interactive: bool,
    pub(crate) env_overrides: &'a [String],
    pub(crate) entrypoint: Option<&'a str>,
    pub(crate) keep_bundle: bool,
    pub(crate) layout_dir_override: Option<&'a Path>,
    pub(crate) insecure_registry: bool,
    pub(crate) username: Option<&'a str>,
    pub(crate) password_stdin: bool,
    pub(crate) cmd: &'a [String],
    // Bootable-target flags (`--vmm`, `--disk`, ...) — only meaningful when
    // dispatching to the VM path. The reference may be the literal
    // `--disk` placeholder convention for now, until VM artifacts get
    // OCI-layout integration.
    pub(crate) vmm: Option<&'a str>,
    pub(crate) disk: Option<&'a Path>,
    pub(crate) firmware: Option<&'a Path>,
    pub(crate) memory: Option<u32>,
    pub(crate) cpus: Option<u32>,
    pub(crate) port_forwards: &'a [String],
    pub(crate) dhcp_command: Option<&'a str>,
    pub(crate) graphic: bool,
}

/// Resolve + introspect the image, reject non-container targets, then
/// run it. `--vmm` + `--disk` short-circuits to the VM path.
pub(crate) fn run_run(args: RunArgs<'_>) -> Result<i32, CliRunError> {
    // Fast paths: --vmm without --disk (and vice versa) are user errors
    // worth surfacing before we touch the filesystem.
    // --disk without --vmm is meaningless — a raw disk needs a backend to boot.
    if args.disk.is_some() && args.vmm.is_none() {
        return Err(CliRunError::DiskWithoutVmm);
    }
    // Explicit raw-disk boot: `--vmm` + `--disk` shortcut. Skips OCI introspect
    // entirely — a raw disk path isn't OCI-cataloged.
    if let (Some(backend), Some(disk)) = (args.vmm, args.disk) {
        // A `--firmware PATH` override is always a single-file blob (`-bios`).
        let firmware = args.firmware.map(|p| Firmware::Bios(p.to_path_buf()));
        // A raw `--disk` carries no OCI arch metadata; assume the host's
        // architecture. (A cross-arch raw-disk boot via TCG is an advanced
        // case the OCI-introspected `run_bootable` path covers.)
        return run_vm(backend, disk.to_path_buf(), firmware, VmArch::host(), &args);
    }

    let layout_dir = match args.layout_dir_override {
        Some(p) => p.to_path_buf(),
        None => util::default_layout_dir().map_err(CliRunError::LayoutDir)?,
    };
    let layout = ImageLayout::init(&layout_dir)?;
    info!(layout = %layout_dir.display(), "layout ready");

    // Resolve the image: try the layout first; pull from the implied
    // registry on miss. Pulling is async (oci-client); the rest of the
    // run path is sync (libcontainer waitpid).
    let reference: Reference = args
        .reference
        .parse()
        .map_err(|err: oci_client::ParseError| CliRunError::BadReference {
            reference: args.reference.to_string(),
            err,
        })?;

    util::pull_if_missing::<CliRunError>(
        &layout,
        &reference,
        args.reference,
        args.username,
        args.password_stdin,
        args.insecure_registry,
    )?;

    // Now the image is in the layout. Introspect to find the target type.
    let profile = umf_builder::introspect::introspect(&layout, args.reference)?;

    // Bootable-OS image: project it to a disk (cached) and boot it in a VM —
    // no `--disk` needed. `run` auto-compiles on demand, the inverse of `build`.
    if matches!(profile.kind, umf_core::l0::L0Kind::Bootable) {
        return run_bootable(&layout, args.reference, &profile.manifest_digest, &args);
    }
    // `--vmm` is only meaningful for a bootable image or an explicit `--disk`;
    // on a container it's a user error.
    if args.vmm.is_some() {
        return Err(CliRunError::VmmWithoutDisk);
    }
    // The remaining VM-boot flags (`--firmware`, `--memory`, ...) are read only
    // on the VM path. On a container they'd be silently ignored — reject with a
    // pointer to the right shape rather than dropping them.
    if let Some(flag) = vm_only_flag_set(&args) {
        return Err(CliRunError::VmFlagOnContainer {
            flag: flag.to_string(),
            reference: args.reference.to_string(),
        });
    }
    dispatch_runnable(args.reference, &profile.kind)?;

    let options = umf_engine::RunOptions {
        entrypoint_override: args.entrypoint.map(|s| vec![s.to_string()]),
        cmd_override: if args.cmd.is_empty() {
            None
        } else {
            Some(args.cmd.to_vec())
        },
        env_overrides: args.env_overrides.to_vec(),
        interactive: args.interactive,
        keep_bundle: args.keep_bundle,
        ..umf_engine::RunOptions::default()
    };

    // Record the container run in the process registry (visible via `umf
    // ps`). A `?` failure below drops the guard, recording the run as
    // failed; a clean finish records the workload's exit code.
    let guard = super::process::RunningGuard::start(
        super::process::ProcessKind::Container,
        args.reference,
        "run",
        Some(args.reference.to_string()),
        None,
    );
    let result = umf_engine::run_image(&layout, args.reference, &options)?;
    if let Some(path) = result.bundle_path {
        eprintln!("bundle preserved at: {}", path.display());
    }
    let code = result.exit_code.unwrap_or(1);
    guard.exited(code);
    Ok(code)
}

/// Bootable-target run: dispatch the requested backend with a `VmSpec`
/// assembled from the CLI flags. Both `qemu` and `ch` (Cloud
/// Hypervisor) are wired in; the trait abstraction in `umf-vmm`
/// keeps the dispatch site free of backend-specific code beyond
/// the constructor selection.
fn run_vm(
    backend: &str,
    disk: PathBuf,
    firmware: Option<Firmware>,
    arch: VmArch,
    args: &RunArgs<'_>,
) -> Result<i32, CliRunError> {
    use umf_vmm::{
        BootSource, ControlMode, DisplayMode, SerialMode, VmRuntime, VmSpec,
        backends::{cloud_hypervisor::CloudHypervisorRuntime, qemu::QemuRuntime},
    };

    let mut port_forwards = Vec::with_capacity(args.port_forwards.len());
    for spec in args.port_forwards {
        port_forwards.push(parse_port_forward(spec)?);
    }

    let mut spec = VmSpec {
        arch,
        boot: BootSource::Disk {
            path: disk.clone(),
            firmware,
        },
        memory_mib: args.memory.unwrap_or(1024),
        cpus: args.cpus.unwrap_or(2),
        kvm: true,
        display: if args.graphic {
            DisplayMode::Window
        } else {
            DisplayMode::None
        },
        port_forwards,
        net: None,
        control: ControlMode::Channel,
        shares: Vec::new(),
        serial: SerialMode::Inherit,
    };

    // Cloud Hypervisor has no user-mode hostfwd: set up a per-VM tap + nft
    // DNAT (umf-networking, with a detached dnsmasq) and attach the guest to
    // it. The QEMU backend keeps using `port_forwards` (hostfwd) directly. The
    // guard lives to the end of this function — it tears the netns/tap down
    // after the VM exits.
    let _vmnet = if matches!(backend, "ch" | "cloud-hypervisor") && !spec.port_forwards.is_empty() {
        let mapped: Vec<umf_networking::PortForward> = spec
            .port_forwards
            .iter()
            .map(|p| umf_networking::PortForward {
                host_port: p.host_port,
                guest_port: p.guest_port,
                tcp: p.tcp,
            })
            .collect();
        let dhcp = parse_dhcp_command(args.dhcp_command);
        let vmnet = umf_networking::VmNet::setup(std::process::id(), &mapped, &dhcp)?;
        info!(
            tap = %vmnet.tap_name(),
            guest = %vmnet.guest_ip(),
            "cloud-hypervisor port-forward network up",
        );
        spec.net = Some(umf_vmm::TapNet {
            netns_fd: vmnet.netns_raw_fd(),
            tap: vmnet.tap_name().to_string(),
        });
        Some(vmnet)
    } else {
        None
    };

    // Record the VM run in the process registry (visible via `umf ps`).
    let disk_name = disk
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("disk")
        .to_string();
    let guard = super::process::RunningGuard::start(
        super::process::ProcessKind::Vm,
        disk_name,
        format!("run --vmm {backend}"),
        Some(disk.display().to_string()),
        None,
    );
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(async move {
        let runner: Box<dyn VmRuntime> = match backend {
            "qemu" => Box::new(QemuRuntime::default()),
            "ch" | "cloud-hypervisor" => Box::new(CloudHypervisorRuntime::default()),
            other => return Err(CliRunError::UnknownVmmBackend(other.to_string())),
        };
        let mut vm = runner.create(&spec).await?;
        runner.boot(&mut vm).await?;
        info!(backend = %backend, "VM running — Ctrl-C to shut down");
        // We don't install a signal handler in this PR; rely on the
        // child receiving SIGINT via the foreground process group when
        // the user Ctrl-Cs us. Signal-driven graceful shutdown via
        // tokio::signal lands as a follow-up (the abstraction is ready;
        // the wiring is just orthogonal to this scope).
        let exit = runner.wait(&mut vm).await?;
        Ok::<i32, CliRunError>(exit.unwrap_or(0))
    });
    match result {
        Ok(code) => {
            guard.exited(code);
            Ok(code)
        }
        Err(e) => {
            guard.failed();
            Err(e)
        }
    }
}

/// Bootable-OS run: project the image to a disk (cached) via the `umf-compile`
/// projector, then boot the disk in a VM. The fused equivalent of
/// `umf compile <ref>` followed by `umf run --vmm=qemu --disk <block>`.
fn run_bootable(
    layout: &ImageLayout,
    reference: &str,
    image_digest: &str,
    args: &RunArgs<'_>,
) -> Result<i32, CliRunError> {
    use umf_compile::DiskGeometry;

    // Default geometry, matching `umf compile`. The block is sparse and
    // content-addressed on (image digest + geometry), so a repeat `run` of the
    // same image reuses the cached block — the shared `cache_variant()` keeps
    // the key identical to `umf compile` / `umf save`.
    let geometry = DiskGeometry::default();
    let variant = geometry.cache_variant();
    let block = layout.block_cache_path(image_digest, &variant)?;
    if block.is_file() {
        info!(block = %block.display(), "bootable block cache hit");
    } else {
        info!(reference, "compiling bootable image before boot");
        umf_compile::compile_image(layout, reference, &block, geometry, None)?;
    }
    println!("Compiled {reference} -> {} ; booting…", block.display());

    // Boot the disk on the host's architecture. (The projected disk is
    // byte-identical across VM/bare-metal, but the firmware + machine type
    // are arch-specific; cross-arch boot of a foreign image is out of scope
    // for the auto-boot path.)
    let arch = VmArch::host();

    // UEFI firmware: the `--firmware` override (always a single-file blob),
    // else firmware discovered on the host for this arch — x86 OVMF
    // (single-file or split CODE/VARS) or aarch64 AAVMF (always a CODE/VARS
    // pflash pair). QEMU defaults to SeaBIOS / no firmware, which can't boot
    // a UEFI/GPT disk.
    let firmware = match args.firmware {
        Some(p) => Firmware::Bios(p.to_path_buf()),
        None => find_uefi_firmware(arch).ok_or_else(|| CliRunError::NoUefiFirmware {
            arch: arch_label(arch),
            hint: firmware_install_hint(arch),
        })?,
    };
    let backend = args.vmm.unwrap_or("qemu");
    run_vm(backend, block, Some(firmware), arch, args)
}

/// Single-file OVMF/EDK II blobs usable with QEMU's `-bios` (x86_64 only —
/// aarch64 AAVMF is always wired as pflash). Relative to a filesystem root
/// (`/` in production) so the resolver can be unit-tested against a fake
/// directory tree.
const OVMF_SINGLE_CANDIDATES: &[&str] = &[
    "usr/share/OVMF/OVMF.fd",
    "usr/share/qemu/OVMF.fd",
    "usr/share/edk2/ovmf/OVMF.fd",
    "usr/share/edk2-ovmf/OVMF.fd",
    "usr/share/ovmf/OVMF.fd",
];

/// Split CODE/VARS OVMF pairs (x86_64): the default layout on modern Debian,
/// Fedora, and Arch, where a read-only `*_CODE*` blob pairs with a writable
/// `*_VARS*` variable store. Paths are relative to a filesystem root so the
/// resolver is unit-testable; the first pair whose *both* halves exist wins.
/// Ordered most to least specific (the `.4m`/sized variants before the bare
/// names).
const OVMF_SPLIT_CANDIDATES: &[(&str, &str)] = &[
    // Debian / Ubuntu (`ovmf` package).
    (
        "usr/share/OVMF/OVMF_CODE_4M.fd",
        "usr/share/OVMF/OVMF_VARS_4M.fd",
    ),
    ("usr/share/OVMF/OVMF_CODE.fd", "usr/share/OVMF/OVMF_VARS.fd"),
    // Fedora / RHEL (`edk2-ovmf`).
    (
        "usr/share/edk2/ovmf/OVMF_CODE.fd",
        "usr/share/edk2/ovmf/OVMF_VARS.fd",
    ),
    // Arch (`edk2-ovmf`), sized `.4m` variants.
    (
        "usr/share/edk2/x64/OVMF_CODE.4m.fd",
        "usr/share/edk2/x64/OVMF_VARS.4m.fd",
    ),
    (
        "usr/share/edk2/x64/OVMF_CODE.fd",
        "usr/share/edk2/x64/OVMF_VARS.fd",
    ),
    // openSUSE (`qemu-ovmf-x86_64`).
    (
        "usr/share/qemu/ovmf-x86_64-code.bin",
        "usr/share/qemu/ovmf-x86_64-vars.bin",
    ),
];

/// Split CODE/VARS AAVMF pairs (aarch64). The ARM UEFI firmware is always
/// wired as pflash — there is no `-bios` form. Distros ship it either as a
/// proper `AAVMF_CODE.fd` / `AAVMF_VARS.fd` pair (Debian/Ubuntu's
/// `qemu-efi-aarch64`, Fedora's `edk2-aarch64`) or as the single
/// `QEMU_EFI.fd` blob alongside a `QEMU_VARS.fd` template. Both halves of a
/// pair must exist for it to match.
const AAVMF_SPLIT_CANDIDATES: &[(&str, &str)] = &[
    // Debian / Ubuntu (`qemu-efi-aarch64`).
    (
        "usr/share/AAVMF/AAVMF_CODE.fd",
        "usr/share/AAVMF/AAVMF_VARS.fd",
    ),
    // Fedora / RHEL (`edk2-aarch64`).
    (
        "usr/share/edk2/aarch64/QEMU_EFI-silent-pflash.raw",
        "usr/share/edk2/aarch64/vars-template-pflash.raw",
    ),
    (
        "usr/share/edk2/aarch64/QEMU_EFI-pflash.raw",
        "usr/share/edk2/aarch64/vars-template-pflash.raw",
    ),
    // Arch (`edk2-aarch64`).
    (
        "usr/share/edk2/aarch64/QEMU_CODE.fd",
        "usr/share/edk2/aarch64/QEMU_VARS.fd",
    ),
    // Debian's `qemu-efi-aarch64` also drops the bundled blob here.
    (
        "usr/share/qemu-efi-aarch64/QEMU_EFI.fd",
        "usr/share/qemu-efi-aarch64/QEMU_VARS.fd",
    ),
];

/// Discover host UEFI firmware for QEMU, for `arch`. On x86_64 this prefers a
/// single-file `OVMF.fd` (`-bios`), then a split CODE/VARS pflash pair (the
/// default on modern distros). On aarch64 only the split AAVMF pflash layout
/// exists (there is no `-bios` form). Returns `None` when no usable layout is
/// present.
fn find_uefi_firmware(arch: VmArch) -> Option<Firmware> {
    resolve_uefi_firmware(Path::new("/"), arch)
}

/// A representative host UEFI firmware path for the host architecture, or
/// `None` when none is discoverable. Used by `umf doctor` to report OVMF / AAVMF
/// readiness; reuses [`find_uefi_firmware`] so the candidate paths stay in one
/// place. Returns the single-file blob, or the CODE half of a split pflash pair.
pub(crate) fn host_uefi_firmware() -> Option<std::path::PathBuf> {
    find_uefi_firmware(VmArch::host()).map(|fw| match fw {
        Firmware::Bios(path) => path,
        Firmware::Pflash { code, .. } => code,
    })
}

/// Resolve UEFI firmware under `root` (`/` in production) for `arch`. The
/// split-layout candidates only match when *both* the CODE and VARS halves
/// exist, so a half-installed package never yields an unbootable pflash pair.
fn resolve_uefi_firmware(root: &Path, arch: VmArch) -> Option<Firmware> {
    match arch {
        VmArch::X86_64 => {
            for rel in OVMF_SINGLE_CANDIDATES {
                let p = root.join(rel);
                if p.is_file() {
                    return Some(Firmware::Bios(p));
                }
            }
            resolve_split(root, OVMF_SPLIT_CANDIDATES)
        }
        VmArch::Aarch64 => resolve_split(root, AAVMF_SPLIT_CANDIDATES),
    }
}

/// First split CODE/VARS pair under `root` whose *both* halves exist.
fn resolve_split(root: &Path, candidates: &[(&str, &str)]) -> Option<Firmware> {
    for (code_rel, vars_rel) in candidates {
        let code = root.join(code_rel);
        let vars = root.join(vars_rel);
        if code.is_file() && vars.is_file() {
            return Some(Firmware::Pflash { code, vars });
        }
    }
    None
}

/// Human-readable arch label for diagnostics (`x86_64` / `aarch64`).
fn arch_label(arch: VmArch) -> String {
    match arch {
        VmArch::X86_64 => "x86_64".to_string(),
        VmArch::Aarch64 => "aarch64".to_string(),
    }
}

/// Arch-specific firmware-package install hint for the "no firmware found"
/// diagnostic.
fn firmware_install_hint(arch: VmArch) -> String {
    match arch {
        VmArch::X86_64 => "install OVMF (e.g. the `ovmf` package)".to_string(),
        VmArch::Aarch64 => {
            "install AAVMF (e.g. the `qemu-efi-aarch64` or `edk2-aarch64` package)".to_string()
        }
    }
}

/// Parse `host:guest` or `host:guest/proto` (proto in {tcp, udp},
/// default tcp). Returns a `umf_vmm::PortForward`.
fn parse_port_forward(spec: &str) -> Result<umf_vmm::PortForward, CliRunError> {
    let (pair, tcp) = if let Some(stripped) = spec.strip_suffix("/tcp") {
        (stripped, true)
    } else if let Some(stripped) = spec.strip_suffix("/udp") {
        (stripped, false)
    } else {
        (spec, true)
    };
    let (host_s, guest_s) = pair
        .split_once(':')
        .ok_or_else(|| CliRunError::BadPortForward {
            spec: spec.to_string(),
            reason: "expected `host:guest`".into(),
        })?;
    let host_port = host_s
        .parse::<u16>()
        .map_err(|e| CliRunError::BadPortForward {
            spec: spec.to_string(),
            reason: format!("host port: {e}"),
        })?;
    let guest_port = guest_s
        .parse::<u16>()
        .map_err(|e| CliRunError::BadPortForward {
            spec: spec.to_string(),
            reason: format!("guest port: {e}"),
        })?;
    Ok(umf_vmm::PortForward {
        host_port,
        guest_port,
        tcp,
    })
}

/// Map the `--dhcp-command` flag to the in-namespace DHCP daemon: absent means
/// the default `dnsmasq`; `none` (any case) launches nothing; anything else is a
/// whitespace-split argv launched, `setns`'d, into the VM netns. Whitespace
/// tokenisation is intentional (no shell quoting); wrap a quoted command in a
/// script if you need one.
fn parse_dhcp_command(flag: Option<&str>) -> umf_networking::DhcpDaemon {
    match flag {
        None => umf_networking::DhcpDaemon::Dnsmasq,
        Some(s) if s.trim().eq_ignore_ascii_case("none") => umf_networking::DhcpDaemon::None,
        Some(s) => {
            umf_networking::DhcpDaemon::Custom(s.split_whitespace().map(str::to_string).collect())
        }
    }
}

/// Name the first VM-boot-only flag the user set, if any. Used to reject these
/// flags on the container path where they're read by nothing. `--vmm` / `--disk`
/// are handled by their own guards and deliberately excluded here.
fn vm_only_flag_set(args: &RunArgs<'_>) -> Option<&'static str> {
    if args.firmware.is_some() {
        Some("firmware")
    } else if args.memory.is_some() {
        Some("memory")
    } else if args.cpus.is_some() {
        Some("cpus")
    } else if !args.port_forwards.is_empty() {
        Some("port-forward")
    } else if args.dhcp_command.is_some() {
        Some("dhcp-command")
    } else if args.graphic {
        Some("graphic")
    } else {
        None
    }
}

/// Reject non-container artifacts up front with a target-aware diagnostic.
/// Container-shaped kinds (Container, KernelBuildEnv, and the lenient
/// Unknown bucket) fall through to the engine.
fn dispatch_runnable(reference: &str, kind: &umf_core::l0::L0Kind) -> Result<(), CliRunError> {
    use umf_core::l0::{L0Kind, Payload};
    match kind {
        L0Kind::Container | L0Kind::KernelBuildEnv | L0Kind::Unknown(_) | L0Kind::Scratch => Ok(()),
        // Bootable images are auto-compiled and booted by `run_bootable` (see
        // `run_run`) before `dispatch_runnable` is ever called, so this arm is
        // structurally unreachable; it exists only to keep the match total.
        L0Kind::Bootable => {
            unreachable!("bootable images are handled by run_bootable before dispatch_runnable")
        }
        L0Kind::Payload(p) => {
            let kind_str = match p {
                Payload::Kernel => "kernel",
                Payload::Rootfs => "rootfs",
                Payload::Bootloader => "bootloader",
                Payload::Firmware => "firmware",
            };
            Err(CliRunError::NotRunnableComponent {
                reference: reference.to_string(),
                kind: kind_str.into(),
            })
        }
    }
}

#[cfg(test)]
mod tests;
