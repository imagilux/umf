//! End-to-end boot-smoke: build a real bootable image from a minimal
//! Alpine `linux-virt` kernel + static-busybox rootfs fixture, `umf compile` it
//! to a disk, and BOOT it under QEMU/KVM, asserting a userspace marker on the
//! serial console. This is the validation that turns "emits a bootable disk"
//! into "produces bootable artifacts".
//!
//! Gated: runs only when `UMF_BOOT_SMOKE=1` and the host has `/dev/kvm`,
//! `qemu-system-x86_64`, `ukify`, and split OVMF. The fixture comes from
//! `UMF_BOOT_FIXTURE=<dir>` (built by `scripts/make-boot-fixture.sh`) or is
//! built on demand via `docker`. Skips cleanly (eprintln) otherwise, mirroring
//! the `UMF_ENGINE_SMOKE` gating of the container-RUN tests.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::Command as AssertCommand;
use umf_core::l0::{L0Kind, Payload};
use umf_oci::image::{ContainerConfig, ImageConfig, LayerSource, emit_image};
use umf_oci::registry::ImageLayout;

/// Must match `scripts/make-boot-fixture.sh`'s default `UMF_BOOT_MARKER`.
const MARKER: &str = "UMF-BOOT-OK-7f3a2c9d";

/// Locate split OVMF (UEFI firmware) across common distro paths. The compiled
/// disk is GPT/UEFI, so SeaBIOS cannot boot it. Returns `(CODE, VARS)`.
fn find_ovmf() -> Option<(PathBuf, PathBuf)> {
    for (code, vars) in [
        (
            "/usr/share/OVMF/OVMF_CODE_4M.fd",
            "/usr/share/OVMF/OVMF_VARS_4M.fd",
        ),
        (
            "/usr/share/OVMF/OVMF_CODE.fd",
            "/usr/share/OVMF/OVMF_VARS.fd",
        ),
        (
            "/usr/share/edk2/ovmf/OVMF_CODE.fd",
            "/usr/share/edk2/ovmf/OVMF_VARS.fd",
        ),
        (
            "/usr/share/qemu/edk2-x86_64-code.fd",
            "/usr/share/qemu/edk2-i386-vars.fd",
        ),
    ] {
        if Path::new(code).exists() && Path::new(vars).exists() {
            return Some((code.into(), vars.into()));
        }
    }
    None
}

fn have(bin: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn skip(reason: &str) {
    eprintln!("SKIP boot_smoke: {reason}");
}

/// The canonical ref the resolver/FROM-probe looks up (`Reference::whole()`,
/// e.g. `local/kernel:test` -> `docker.io/local/kernel:test`). Seeding under
/// the canonical key mirrors what a real registry pull would cache, so the
/// build resolves the artifact offline instead of trying to pull it.
fn canonical(reference: &str) -> String {
    reference
        .parse::<oci_client::Reference>()
        .map_or_else(|_| reference.to_string(), |r| r.whole())
}

/// Resolve the fixture dir: `UMF_BOOT_FIXTURE` if it points at a built fixture,
/// otherwise build one into `tmp` via the script (needs docker).
fn fixture_dir(tmp: &Path) -> Option<PathBuf> {
    if let Ok(d) = std::env::var("UMF_BOOT_FIXTURE") {
        let p = PathBuf::from(d);
        if p.join("release").is_file() {
            return Some(p);
        }
    }
    if !have("docker") {
        skip("no prebuilt UMF_BOOT_FIXTURE and docker is absent");
        return None;
    }
    let out = tmp.join("fixture");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/make-boot-fixture.sh");
    let status = Command::new("bash")
        .arg(&script)
        .arg(&out)
        .status()
        .expect("run make-boot-fixture.sh");
    assert!(status.success(), "make-boot-fixture.sh failed");
    Some(out)
}

#[test]
fn boots_to_userspace_under_qemu() {
    if std::env::var("UMF_BOOT_SMOKE").as_deref() != Ok("1") {
        skip("set UMF_BOOT_SMOKE=1 to run the QEMU/KVM boot-smoke");
        return;
    }
    for (bin, why) in [("qemu-system-x86_64", "boot"), ("ukify", "the uki flavor")] {
        if !have(bin) {
            skip(&format!("{bin} absent (needed for {why})"));
            return;
        }
    }
    let Some((ovmf_code, ovmf_vars)) = find_ovmf() else {
        skip("no OVMF / UEFI firmware found");
        return;
    };
    // KVM when available (fast, ~90s); otherwise software emulation (TCG), which
    // still validates the boot but is much slower, so allow a generous timeout.
    let kvm = Path::new("/dev/kvm").exists();
    let (accel, cpu, boot_timeout) = if kvm {
        ("kvm", "host", "180")
    } else {
        ("tcg", "max", "600")
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let Some(fixture) = fixture_dir(tmp.path()) else {
        return;
    };
    let release = std::fs::read_to_string(fixture.join("release"))
        .expect("read release")
        .trim()
        .to_string();
    assert!(!release.is_empty(), "empty kernel release");

    // --- seed the kernel + rootfs OCI artifacts into a fresh layout ---
    let layout_dir = tmp.path().join("layout");
    let layout = ImageLayout::init(&layout_dir).expect("init layout");

    let kernel_layer = LayerSource::from_directory(&fixture.join("kernel")).expect("kernel layer");
    let mut kernel_container = ContainerConfig::default();
    kernel_container.labels.insert(
        "org.imagilux.umf.kernel.release".to_string(),
        release.clone(),
    );
    let kernel_cfg = ImageConfig {
        umf_type: L0Kind::Payload(Payload::Kernel),
        container: kernel_container,
        ..ImageConfig::default()
    };
    emit_image(
        &layout,
        &[kernel_layer],
        &kernel_cfg,
        &canonical("local/kernel:test"),
    )
    .expect("emit kernel");

    let rootfs_layer = LayerSource::from_directory(&fixture.join("rootfs")).expect("rootfs layer");
    let rootfs_cfg = ImageConfig {
        umf_type: L0Kind::Payload(Payload::Rootfs),
        ..ImageConfig::default()
    };
    emit_image(
        &layout,
        &[rootfs_layer],
        &rootfs_cfg,
        &canonical("local/rootfs:test"),
    )
    .expect("emit rootfs");

    // --- recipe: bootable (FROM a kernel), uki flavor, init-system shape so the
    //     UMF initramfs loads virtio_blk + squashfs before mounting the rootfs.
    let recipe = tmp.path().join("boot.umf");
    std::fs::write(
        &recipe,
        "FROM local/kernel:test\n\
         LABEL org.imagilux.umf.flavor=uki\n\
         ADD local/rootfs:test /\n\
         ENTRYPOINT systemd\n",
    )
    .expect("write recipe");
    let layout_arg = layout_dir.to_str().unwrap();

    // --- umf build (no RUN steps ⇒ no micro-VM) ---
    let build = AssertCommand::cargo_bin("umf")
        .expect("umf binary")
        .args([
            "build",
            "--tag",
            "local/boot:test",
            recipe.to_str().unwrap(),
        ])
        .args(["--layout-dir", layout_arg])
        .output()
        .expect("run umf build");
    assert!(
        build.status.success(),
        "umf build failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );

    // --- umf compile -> raw disk image ---
    let disk = tmp.path().join("disk.img");
    let compile = AssertCommand::cargo_bin("umf")
        .expect("umf binary")
        .args(["compile", "local/boot:test", "-o", disk.to_str().unwrap()])
        .args(["--layout-dir", layout_arg])
        .output()
        .expect("run umf compile");
    assert!(
        compile.status.success(),
        "umf compile failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr),
    );
    assert!(disk.is_file(), "compile produced no disk image");

    // --- boot under QEMU/KVM with split OVMF; capture the serial console ---
    let vars = tmp.path().join("OVMF_VARS.fd");
    std::fs::copy(&ovmf_vars, &vars).expect("copy OVMF vars");

    let qemu = Command::new("timeout")
        .args(["-k", "5", boot_timeout, "qemu-system-x86_64"])
        .args([
            "-machine",
            &format!("q35,accel={accel}"),
            "-cpu",
            cpu,
            "-m",
            "1024",
        ])
        .args([
            "-drive",
            &format!("file={},if=virtio,format=raw", disk.display()),
        ])
        .args([
            "-drive",
            &format!(
                "if=pflash,unit=0,readonly=on,format=raw,file={}",
                ovmf_code.display()
            ),
        ])
        .args([
            "-drive",
            &format!("if=pflash,unit=1,format=raw,file={}", vars.display()),
        ])
        .args(["-display", "none", "-serial", "stdio", "-no-reboot"])
        .output()
        .expect("run qemu");

    let serial = format!(
        "{}{}",
        String::from_utf8_lossy(&qemu.stdout),
        String::from_utf8_lossy(&qemu.stderr)
    );
    assert!(
        serial.contains(MARKER),
        "boot did not reach userspace: marker {MARKER:?} not seen on serial.\n\
         ---- QEMU serial log ----\n{serial}\n---- end serial log ----",
    );
    eprintln!("boot-smoke OK: observed userspace marker {MARKER:?} on the serial console");
}
