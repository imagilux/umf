//! The one and only `Command::new("qemu-system-*")` in `umf-vmm`.
//!
//! Everything after the process spawns goes through QMP (typed Rust via
//! the `qapi` crate). This module owns the argv assembly + the subprocess
//! lifecycle bring-up; the trait impl in [`super`] handles control.

use std::path::Path;
use std::process::Stdio;

use tempfile::TempDir;
use tokio::process::Command;
use tracing::debug;

use crate::error::VmError;
use crate::handle::VmHandle;
use crate::runtime::{
    BootSource, ControlMode, DisplayMode, Firmware, PortForward, SerialMode, VmArch, VmSpec,
};

// `validate_spec_inputs` lives in `crate::backends::common` — shared
// across the qemu + cloud-hypervisor backends so the fail-fast disk /
// firmware / kernel existence check is consistent.

/// Spawn `binary` (`qemu-system-x86_64`, `qemu-system-aarch64`, ...)
/// with `spec` applied. Returns a [`VmHandle`] with the live child + the
/// QMP socket path when [`ControlMode::Channel`] was requested.
pub async fn spawn_qemu(binary: &str, spec: &VmSpec) -> Result<VmHandle, VmError> {
    crate::backends::common::validate_spec_inputs(spec)?;

    // QMP socket lives in a per-VM tempdir so concurrent VMs can't
    // collide. The tempdir is leaked into the handle so it survives
    // as long as the VM does; cleanup happens when the caller drops
    // the handle.
    let (sock_dir, qmp_socket) = match spec.control {
        ControlMode::Channel => {
            let dir = TempDir::new()?;
            let sock = dir.path().join("qmp.sock");
            (Some(dir), Some(sock))
        }
        ControlMode::None => (None, None),
    };

    // Split OVMF needs a writable per-run copy of the VARS store; produce
    // it here (filesystem IO) and hand `build_qemu_args` a spec whose VARS
    // path points at the copy. The copy lives in `fw_dir`, leaked like the
    // socket dir so it survives as long as the VM.
    let (fw_dir, spec) = prepare_firmware(spec)?;

    let id = format!("umf-vmm-{}", std::process::id());
    let args = build_qemu_args(&spec, qmp_socket.as_deref(), &id);
    debug!(?args, "umf-vmm: qemu argv");

    let mut cmd = Command::new(binary);
    cmd.args(&args);
    cmd.stdin(Stdio::null());
    // Route stdout/stderr per the serial mode. Never *pipe* them: nothing
    // in this crate drains a pipe, and headless QEMU streams the whole
    // guest console to stdout via `-serial mon:stdio`, so an undrained pipe
    // fills the ~64 KiB OS buffer and deadlocks the VM mid-boot.
    //   * Inherit — forward the console + any QEMU start-up errors to the
    //     caller's terminal (`umf run`).
    //   * File    — the guest console is captured to the file; suppress
    //     QEMU's own chatter so it doesn't bleed into a build's stdout.
    match &spec.serial {
        SerialMode::Inherit => {
            cmd.stdout(Stdio::inherit());
            cmd.stderr(Stdio::inherit());
        }
        SerialMode::File(_) => {
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
        }
    }
    // Kill the VMM if its handle is dropped — e.g. a `wait` future cancelled
    // by a build-step timeout — so a stuck guest never orphans a qemu
    // process on the host.
    cmd.kill_on_drop(true);

    let child = cmd.spawn().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            VmError::BinaryNotFound(binary.to_string())
        } else {
            VmError::Io(err)
        }
    })?;

    // Leak the tempdirs: the QMP socket + the writable VARS copy need to
    // outlive this function. The handle owns the cleanup story (today we
    // just let the OS reclaim on process exit; a future Drop impl could
    // remove them).
    if let Some(dir) = sock_dir {
        let _ = dir.keep();
    }
    if let Some(dir) = fw_dir {
        let _ = dir.keep();
    }

    Ok(VmHandle {
        child: Some(child),
        control_socket: qmp_socket,
        id,
    })
}

/// For a split-OVMF boot, copy the host VARS template into a fresh
/// per-run tempdir and return a spec whose VARS path points at the copy
/// (plus the tempdir to keep alive). QEMU opens the VARS pflash unit
/// read-write to persist EFI variables, so booting the host template
/// directly would mutate a shared, often root-owned file and let
/// concurrent VMs stomp each other's NVRAM. Every other boot shape is a
/// no-op: the spec is returned untouched and no tempdir is created.
fn prepare_firmware(spec: &VmSpec) -> Result<(Option<TempDir>, VmSpec), VmError> {
    let BootSource::Disk {
        path,
        firmware: Some(Firmware::Pflash { code, vars }),
    } = &spec.boot
    else {
        return Ok((None, spec.clone()));
    };

    let dir = TempDir::new()?;
    let vars_copy = dir.path().join("OVMF_VARS.fd");
    std::fs::copy(vars, &vars_copy).map_err(|err| VmError::InputUnusable {
        path: vars.clone(),
        reason: format!("copying VARS store to a writable per-run location: {err}"),
    })?;

    let patched = VmSpec {
        boot: BootSource::Disk {
            path: path.clone(),
            firmware: Some(Firmware::Pflash {
                code: code.clone(),
                vars: vars_copy,
            }),
        },
        ..spec.clone()
    };
    Ok((Some(dir), patched))
}

/// Assemble the qemu argv from `spec`.
///
/// Split out so we can unit-test argv construction without spawning
/// anything. The behaviour matches what an operator would type by hand:
///
/// * `-machine <board>,accel=kvm` when `spec.kvm` is set (falls back to
///   TCG when KVM was requested but not accessible — that's the runtime's
///   job to check; here we honour the spec at face value). The board is
///   arch-driven: `q35` on x86_64, `virt` on aarch64. aarch64's `virt`
///   has no default CPU, so a `-cpu` is mandatory there; we set one on
///   both arches anyway — `host` under KVM, `max` under TCG.
/// * `-m <MiB>` + `-smp <cpus>`.
/// * `-display none -serial mon:stdio` when [`DisplayMode::None`] so the
///   serial console + QMP monitor share stdio without a graphical window.
/// * `-qmp unix:<sock>,server,nowait` whenever [`ControlMode::Channel`]
///   was requested.
/// * Boot source: `-drive file=<disk>,if=virtio,format=raw` for
///   `BootSource::Disk`, plus firmware when supplied (`-bios <fd>` for a
///   single-file blob, or a read-only CODE + writable VARS `-drive
///   if=pflash` pair for the split OVMF layout); or `-kernel <vmlinuz>
///   -initrd <initrd> -append "<cmdline>"` for `BootSource::DirectKernel`.
/// * `-netdev user,id=net0,hostfwd=tcp::<host>-:<guest>` (one `hostfwd`
///   per port forward) + `-device virtio-net-pci,netdev=net0`.
pub(crate) fn build_qemu_args(spec: &VmSpec, qmp_socket: Option<&Path>, id: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::with_capacity(24);

    args.push("-name".into());
    args.push(id.to_string());

    // Machine board is arch-specific: x86 uses `q35`, aarch64 uses `virt`
    // (there is no aarch64 equivalent of `q35`). The accelerator suffix is
    // honoured verbatim from `spec.kvm` — the runtime decides whether KVM
    // is actually reachable before building the spec.
    let board = match spec.arch {
        VmArch::X86_64 => "q35",
        VmArch::Aarch64 => "virt",
    };
    let accel = if spec.kvm { "kvm" } else { "tcg" };
    args.push("-machine".into());
    args.push(format!("{board},accel={accel}"));
    // `-cpu` is mandatory on aarch64 `virt` (no default CPU) and harmless
    // on x86 `q35`, so we always set it. `host` passes the physical CPU
    // through under KVM; `max` exposes the richest model the TCG emulator
    // can synthesise (the conservative aarch64 fallback would be
    // `cortex-a57`, but `max` is broadly supported and what x86 already
    // used). `-enable-kvm` stays x86-shaped — on aarch64 the `accel=kvm`
    // suffix already selects it and a bare `-enable-kvm` is redundant.
    if spec.kvm {
        if matches!(spec.arch, VmArch::X86_64) {
            args.push("-enable-kvm".into());
        }
        args.push("-cpu".into());
        args.push("host".into());
    } else {
        args.push("-cpu".into());
        args.push("max".into());
    }
    args.push("-m".into());
    args.push(format!("{}", spec.memory_mib));
    args.push("-smp".into());
    args.push(format!("{}", spec.cpus));
    args.push("-no-reboot".into());

    match spec.display {
        DisplayMode::None => {
            args.push("-display".into());
            args.push("none".into());
            args.push("-serial".into());
            args.push(match &spec.serial {
                SerialMode::Inherit => "mon:stdio".to_string(),
                SerialMode::File(path) => format!("file:{}", path.display()),
            });
        }
        DisplayMode::Window => {
            // Let qemu pick its default front-end (SDL/GTK).
        }
    }

    if let Some(sock) = qmp_socket {
        args.push("-qmp".into());
        args.push(format!("unix:{},server,nowait", sock.display()));
    }

    match &spec.boot {
        BootSource::Disk { path, firmware } => {
            args.push("-drive".into());
            args.push(format!("file={},if=virtio,format=raw", path.display(),));
            match firmware {
                Some(Firmware::Bios(fw)) => {
                    args.push("-bios".into());
                    args.push(fw.display().to_string());
                }
                Some(Firmware::Pflash { code, vars }) => {
                    // Split OVMF: read-only CODE plus writable VARS, each its
                    // own pflash unit. `vars` is the per-run copy the caller
                    // prepared (see `prepare_firmware`), so the host template
                    // is never mutated and concurrent VMs don't share NVRAM.
                    args.push("-drive".into());
                    args.push(format!(
                        "if=pflash,format=raw,unit=0,readonly=on,file={}",
                        code.display(),
                    ));
                    args.push("-drive".into());
                    args.push(format!(
                        "if=pflash,format=raw,unit=1,file={}",
                        vars.display(),
                    ));
                }
                None => {}
            }
        }
        BootSource::DirectKernel {
            kernel,
            initrd,
            cmdline,
        } => {
            args.push("-kernel".into());
            args.push(kernel.display().to_string());
            args.push("-initrd".into());
            args.push(initrd.display().to_string());
            args.push("-append".into());
            args.push(cmdline.clone());
        }
    }

    // virtio-9p shares (-virtfs). The guest mounts each by its tag; the
    // mapped-xattr security model stores guest uid/gid as xattrs so a
    // staging tree round-trips ownership without host root.
    for (i, share) in spec.shares.iter().enumerate() {
        args.push("-virtfs".into());
        args.push(format!(
            "local,path={},mount_tag={},security_model=mapped-xattr,id=umf9p{i}",
            share.host_path.display(),
            share.mount_tag,
        ));
    }

    // User-mode networking. One -netdev with all hostfwd specs collapsed
    // into the same comma-list; virtio-net-pci attaches it to the guest.
    let mut netdev = String::from("user,id=net0");
    for pf in &spec.port_forwards {
        netdev.push_str(&format_hostfwd(*pf));
    }
    args.push("-netdev".into());
    args.push(netdev);
    args.push("-device".into());
    args.push("virtio-net-pci,netdev=net0".into());

    args
}

fn format_hostfwd(pf: PortForward) -> String {
    let proto = if pf.tcp { "tcp" } else { "udp" };
    format!(
        ",hostfwd={proto}::{host}-:{guest}",
        host = pf.host_port,
        guest = pf.guest_port,
    )
}

#[cfg(test)]
mod tests;
