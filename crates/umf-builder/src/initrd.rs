//! INITRD generation — pack a minimal initramfs that the kernel can pivot
//! into the squashfs ROOTFS at boot.
//!
//! After the kernel-install step has
//! placed `vmlinuz-<release>` + `lib/modules/<release>/` into the staging
//! tree, this module emits a gzipped CPIO newc archive carrying:
//!
//! * `/init` — a shell script that loads `virtio_blk` + `squashfs` modules,
//!   mounts `/dev/vda2` (the disk's second partition) as the rootfs, and
//!   `switch_root`s into it.
//! * `/bin/busybox` — copied from the staging rootfs. Provides `sh`,
//!   `mount`, `insmod`, `switch_root`, etc. via the multi-call binary.
//! * `/bin/sh` and friends — symlinks into `/bin/busybox`.
//! * Essential kernel modules from the staging modules tree.
//! * `/sysroot` — empty mountpoint for the final rootfs.
//!
//! The CPIO writer is a self-contained ~200-line implementation of the
//! newc format (the private `cpio_newc` module) — no crate dependency beyond `flate2` for
//! gzip compression, which umf-builder already pulls in.
//!
//! The init script is intentionally minimal and busybox-shaped: it
//! assumes a multi-call `/bin/busybox` exists in the rootfs (a wide
//! range of minimal distros satisfy this — Alpine, Buildroot, custom
//! immutable images) and that the kernel's virtio-blk + SquashFS
//! modules are present under `/lib/modules/<release>/kernel/`. Distros
//! that don't ship a busybox-style binary or place modules elsewhere
//! will eventually need their own init-script template — the script
//! emission is the per-distro extension point, not the wider initramfs
//! assembly.

use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;
use thiserror::Error;
use tracing::{debug, info};

use crate::kernel::KernelLayout;
use umf_oci::staging::BuildStaging;

mod cpio_newc;
pub use cpio_newc::{CpioEntry, CpioKind, CpioWriter};

// ── Errors ──────────────────────────────────────────────────────────────────

/// Errors produced by [`generate_initramfs`].
#[derive(Debug, Error)]
pub enum InitrdError {
    /// The staging tree didn't carry the host binaries the init script
    /// needs (`/bin/busybox`). Without a usable shell-and-utilities
    /// binary the produced initramfs would panic on `exec` of `/init`.
    #[error(
        "staging rootfs is missing `{0}` — initramfs needs busybox (or an \
         equivalent multi-call binary) to bring up the kernel's userland"
    )]
    MissingBusybox(PathBuf),

    /// Underlying I/O error.
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
}

// ── Result ──────────────────────────────────────────────────────────────────

/// Description of a successfully generated initramfs.
#[derive(Debug, Clone)]
pub struct InitrdReport {
    /// Final filename used on the ESP (e.g. `"initramfs-6.6.79.img"`).
    pub filename: String,
    /// Uncompressed CPIO size, in bytes.
    pub cpio_size_bytes: usize,
    /// Compressed (`gzip`) size, in bytes — this is what the firmware
    /// reads.
    pub compressed_size_bytes: usize,
    /// Number of modules embedded in the initramfs.
    pub modules_count: usize,
}

// ── Public entry ────────────────────────────────────────────────────────────

/// What the produced initramfs is *for* — boot the rootfs vs run a single
/// command against a 9p-shared staging dir then power off.
#[derive(Debug, Clone)]
pub enum InitramfsFlavor {
    /// Boot initramfs (the production-shape one). Mounts the rootfs from
    /// `/dev/vda2`, switch_roots into it, hands off to `/sbin/init`. This
    /// is what lands on the ESP next to vmlinuz.
    Boot,
    /// Run initramfs (build-time only). Mounts the host's staging dir over
    /// 9p as `/sysroot`, reads the RUN command from `/sysroot/.umf-cmd`,
    /// chroots and executes it, persists the exit code to
    /// `/sysroot/.umf-run-exit`, powers off. Embeds 9p + networking
    /// modules in addition to the boot-side ones.
    Run,
}

/// Generate a minimal initramfs from `staging` + `kernel`. Returns the
/// gzipped CPIO image as bytes alongside a report describing what's in it.
///
/// Shorthand for [`generate_initramfs_with_flavor`] in [`InitramfsFlavor::Boot`].
pub fn generate_initramfs(
    staging: &BuildStaging,
    kernel: &KernelLayout,
) -> Result<(Vec<u8>, InitrdReport), InitrdError> {
    generate_initramfs_with_flavor(staging, kernel, InitramfsFlavor::Boot)
}

/// Variant of [`generate_initramfs`] that lets the caller pick a non-boot
/// flavor. [`InitramfsFlavor::Run`] is consumed by the VM RUN backend
/// — see [`crate::vm_runner::run_step_vm`].
pub fn generate_initramfs_with_flavor(
    staging: &BuildStaging,
    kernel: &KernelLayout,
    flavor: InitramfsFlavor,
) -> Result<(Vec<u8>, InitrdReport), InitrdError> {
    info!(
        release = %kernel.release,
        ?flavor,
        "initrd: generating initramfs",
    );

    let busybox_src = staging.path().join("bin").join("busybox");
    if !busybox_src.is_file() {
        return Err(InitrdError::MissingBusybox(busybox_src));
    }
    let busybox_bytes = std::fs::read(&busybox_src)?;
    let busybox_perms = std::fs::metadata(&busybox_src)?.permissions().mode();

    let modules_root = &kernel.modules;
    let embedded_modules = collect_modules_for(modules_root, &flavor)?;

    // Build the CPIO archive.
    let mut cpio = CpioWriter::new();
    cpio.push(top_level_dir("."));
    cpio.push(top_level_dir("bin"));
    cpio.push(top_level_dir("sbin"));
    cpio.push(top_level_dir("dev"));
    cpio.push(top_level_dir("proc"));
    cpio.push(top_level_dir("sys"));
    cpio.push(top_level_dir("sysroot"));
    cpio.push(top_level_dir("lib"));
    cpio.push(top_level_dir("lib/modules"));

    // busybox + symlinks.
    cpio.push(CpioEntry {
        path: PathBuf::from("bin/busybox"),
        kind: CpioKind::File(busybox_bytes),
        mode: busybox_perms & 0o7777,
        ..CpioEntry::default()
    });
    for applet in ["sh", "mount", "umount", "modprobe", "insmod", "switch_root"] {
        cpio.push(CpioEntry {
            path: PathBuf::from(format!("bin/{applet}")),
            kind: CpioKind::Symlink(PathBuf::from("busybox")),
            mode: 0o755,
            ..CpioEntry::default()
        });
    }

    // Modules: ship them under their original layout so insmod can find them
    // by absolute path.
    let modules_prefix = PathBuf::from("lib/modules").join(&kernel.release);
    cpio.push(top_level_dir_path(&modules_prefix));
    let mut ancestors_added: Vec<PathBuf> = vec![modules_prefix.clone()];
    for entry in &embedded_modules {
        let rel = entry.strip_prefix(modules_root).unwrap_or(entry);
        let dest = modules_prefix.join(rel);
        if let Some(parent) = dest.parent() {
            for ancestor in walk_ancestors(parent, &modules_prefix) {
                if !ancestors_added.contains(&ancestor) {
                    cpio.push(top_level_dir_path(&ancestor));
                    ancestors_added.push(ancestor);
                }
            }
        }
        let bytes = std::fs::read(entry)?;
        cpio.push(CpioEntry {
            path: dest,
            kind: CpioKind::File(bytes),
            mode: 0o644,
            ..CpioEntry::default()
        });
    }

    // /init shell script (flavor-specific).
    let init_body = match flavor {
        InitramfsFlavor::Boot => {
            build_boot_init_script(&kernel.release, &embedded_modules, modules_root)
        }
        InitramfsFlavor::Run => {
            build_run_init_script(&kernel.release, &embedded_modules, modules_root)
        }
    };
    cpio.push(CpioEntry {
        path: PathBuf::from("init"),
        kind: CpioKind::File(init_body.into_bytes()),
        mode: 0o755,
        ..CpioEntry::default()
    });

    let cpio_bytes = cpio.finish();
    let cpio_size_bytes = cpio_bytes.len();

    // gzip the archive — Linux's initramfs decoder accepts gzip, xz, lz4, …;
    // gzip is the universal default and we already depend on `flate2`.
    let mut gz = GzEncoder::new(Vec::new(), Compression::best());
    gz.write_all(&cpio_bytes)?;
    let compressed = gz.finish()?;

    let filename = format!("initramfs-{}.img", kernel.release);
    debug!(
        filename = %filename,
        cpio = cpio_size_bytes,
        compressed = compressed.len(),
        modules = embedded_modules.len(),
        "initrd: ready to copy onto ESP",
    );

    Ok((
        compressed.clone(),
        InitrdReport {
            filename,
            cpio_size_bytes,
            compressed_size_bytes: compressed.len(),
            modules_count: embedded_modules.len(),
        },
    ))
}

// ── Internals ───────────────────────────────────────────────────────────────

fn top_level_dir(name: &str) -> CpioEntry {
    CpioEntry {
        path: PathBuf::from(name),
        kind: CpioKind::Directory,
        mode: 0o755,
        ..CpioEntry::default()
    }
}

fn top_level_dir_path(path: &Path) -> CpioEntry {
    CpioEntry {
        path: path.to_path_buf(),
        kind: CpioKind::Directory,
        mode: 0o755,
        ..CpioEntry::default()
    }
}

/// Walk the ancestor chain from `path` (exclusive) up to (but not
/// including) `root_prefix`, yielding the ancestors in root-first order
/// so the CPIO archive carries parent directories before their children.
fn walk_ancestors(path: &Path, root_prefix: &Path) -> Vec<PathBuf> {
    let mut chain: Vec<PathBuf> = Vec::new();
    let mut cur = Some(path.to_path_buf());
    while let Some(p) = cur {
        if p == root_prefix {
            break;
        }
        chain.push(p.clone());
        cur = p.parent().map(Path::to_path_buf);
    }
    chain.reverse();
    chain
}

/// Pick kernel modules to embed in the initramfs, choosing the set based
/// on the flavor: boot needs virtio-blk + SquashFS; run additionally
/// needs 9p + virtio-net.
///
/// Walks `modules_root/kernel/` looking for module files (`*.ko` or
/// `*.ko.gz` or `*.ko.xz` or `*.ko.zst`) matching the flavor's allowlist.
fn collect_modules_for(
    modules_root: &Path,
    flavor: &InitramfsFlavor,
) -> Result<Vec<PathBuf>, InitrdError> {
    let allowlist: &[&str] = match flavor {
        InitramfsFlavor::Boot => &[
            "virtio",
            "virtio_ring",
            "virtio_pci",
            "virtio_pci_modern_dev",
            "virtio_blk",
            umf_core::boot::ROOTFS_FSTYPE,
        ],
        InitramfsFlavor::Run => &[
            // Boot-side basics — still needed even when the rootfs is on
            // 9p (the kernel uses virtio to talk to anything else).
            "virtio",
            "virtio_ring",
            "virtio_pci",
            "virtio_pci_modern_dev",
            "virtio_blk",
            umf_core::boot::ROOTFS_FSTYPE,
            // 9p filesystem (host-staging share).
            "9p",
            "9pnet",
            "9pnet_virtio",
            // virtio-net + DHCP-needed bits so RUN steps can pull from
            // package repos / git remotes / etc.
            "virtio_net",
        ],
    };

    let kernel_dir = modules_root.join("kernel");
    if !kernel_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut out: Vec<PathBuf> = Vec::new();
    for entry in walkdir::WalkDir::new(&kernel_dir).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let stem = module_stem(&name);
        if allowlist.contains(&stem.as_str()) {
            out.push(entry.path().to_path_buf());
        }
    }
    out.sort();
    Ok(out)
}

fn module_stem(filename: &str) -> String {
    // Strip a single `.ko` / `.ko.gz` / `.ko.xz` / `.ko.zst` suffix.
    let trimmed = filename
        .strip_suffix(".ko.zst")
        .or_else(|| filename.strip_suffix(".ko.xz"))
        .or_else(|| filename.strip_suffix(".ko.gz"))
        .or_else(|| filename.strip_suffix(".ko"))
        .unwrap_or(filename);
    trimmed.to_string()
}

fn build_boot_init_script(release: &str, modules: &[PathBuf], modules_root: &Path) -> String {
    let mut s = String::new();
    s.push_str("#!/bin/sh\n");
    s.push_str("# UMF initramfs init — minimal squashfs rootfs pivot.\n");
    s.push_str("set -eu\n");
    s.push_str("export PATH=/bin:/sbin\n");
    s.push('\n');
    s.push_str("# Early mounts.\n");
    s.push_str("mount -t proc proc /proc\n");
    s.push_str("mount -t sysfs sysfs /sys\n");
    s.push_str("mount -t devtmpfs devtmpfs /dev\n");
    s.push('\n');
    s.push_str("# Bring up the kernel modules we need to see the rootfs disk.\n");
    s.push_str("# `insmod` ignores missing files so a kernel that has these\n");
    s.push_str("# compiled-in (no .ko present) still boots.\n");
    for m in modules {
        let rel = m.strip_prefix(modules_root).unwrap_or(m);
        let in_initrd = PathBuf::from("/lib/modules").join(release).join(rel);
        s.push_str(&format!(
            "insmod {} 2>/dev/null || true\n",
            in_initrd.display()
        ));
    }
    s.push('\n');
    s.push_str("# Mount the squashfs rootfs (the second partition of the disk).\n");
    s.push_str("# `vda` for virtio-blk; `sda` if running on plain IDE/AHCI.\n");
    s.push_str("ROOT=/dev/vda2\n");
    s.push_str("[ -b /dev/sda2 ] && ROOT=/dev/sda2\n");
    s.push_str(&format!(
        "mount -t {} -o ro \"$ROOT\" /sysroot\n",
        umf_core::boot::ROOTFS_FSTYPE,
    ));
    s.push('\n');
    s.push_str("# Pivot.\n");
    s.push_str("exec switch_root /sysroot /sbin/init\n");
    s
}

/// Init script for the run-flavour initramfs. Loads 9p + virtio
/// modules, mounts the host's staging dir over 9p as `/sysroot`, reads
/// the RUN command from `/sysroot/.umf-cmd`, chroots and exec's it,
/// persists the exit code at `/sysroot/.umf-run-exit`, powers off.
///
/// Networking is brought up best-effort (DHCP via `udhcpc` if busybox
/// has it) so RUN steps can pull from package repos / git remotes / etc.
/// without explicit configuration.
fn build_run_init_script(release: &str, modules: &[PathBuf], modules_root: &Path) -> String {
    let mut s = String::new();
    s.push_str("#!/bin/sh\n");
    s.push_str(
        "# UMF run-initramfs init — executes a single RUN command \
                 against a 9p-shared staging dir.\n",
    );
    s.push_str(
        "# Failures are swallowed liberally — the build host \
                 ultimately reads the exit code from /sysroot/.umf-run-exit.\n",
    );
    s.push('\n');
    s.push_str("mount -t proc proc /proc 2>/dev/null || true\n");
    s.push_str("mount -t sysfs sysfs /sys 2>/dev/null || true\n");
    s.push_str("mount -t devtmpfs devtmpfs /dev 2>/dev/null || true\n");
    s.push('\n');
    s.push_str("# Load modules (best-effort — some may be compiled-in).\n");
    for m in modules {
        let rel = m.strip_prefix(modules_root).unwrap_or(m);
        let in_initrd = PathBuf::from("/lib/modules").join(release).join(rel);
        s.push_str(&format!(
            "insmod {} 2>/dev/null || true\n",
            in_initrd.display()
        ));
    }
    s.push('\n');
    s.push_str("# Mount the host's staging directory over 9p.\n");
    s.push_str("mkdir -p /sysroot\n");
    s.push_str(&format!(
        "mount -t 9p -o trans=virtio,version=9p2000.L,access=any \
         {} /sysroot\n",
        crate::bootable::MOUNT_TAG_STAGING,
    ));
    s.push('\n');
    s.push_str(
        "# Bring up loopback + DHCP on virtio-net for outbound \
                 access.\n",
    );
    s.push_str(
        "ip link set lo up 2>/dev/null || ifconfig lo up 2>/dev/null || \
                 true\n",
    );
    s.push_str(
        "ip link set eth0 up 2>/dev/null || ifconfig eth0 up \
                 2>/dev/null || true\n",
    );
    s.push_str("udhcpc -i eth0 -t 8 -T 2 -q 2>/dev/null || true\n");
    s.push('\n');
    s.push_str("# Bind kernel filesystems into the chroot target.\n");
    s.push_str("mkdir -p /sysroot/proc /sysroot/sys /sysroot/dev\n");
    s.push_str("mount -t proc proc /sysroot/proc 2>/dev/null || true\n");
    s.push_str("mount -t sysfs sysfs /sysroot/sys 2>/dev/null || true\n");
    s.push_str("mount -t devtmpfs devtmpfs /sysroot/dev 2>/dev/null || true\n");
    s.push('\n');
    s.push_str("# Source env (key=value lines) if present.\n");
    s.push_str("if [ -f /sysroot/.umf-env ]; then\n");
    s.push_str("    set -a\n");
    s.push_str("    . /sysroot/.umf-env\n");
    s.push_str("    set +a\n");
    s.push_str("fi\n");
    s.push('\n');
    s.push_str("# Run the RUN command inside the rootfs chroot.\n");
    s.push_str("EXIT=0\n");
    s.push_str("if [ -f /sysroot/.umf-cmd ]; then\n");
    s.push_str("    CMD=$(cat /sysroot/.umf-cmd)\n");
    s.push_str("    chroot /sysroot /bin/sh -c \"$CMD\" || EXIT=$?\n");
    s.push_str("else\n");
    s.push_str("    echo 'umf: /sysroot/.umf-cmd missing' >&2\n");
    s.push_str("    EXIT=127\n");
    s.push_str("fi\n");
    s.push('\n');
    s.push_str("# Persist exit code via 9p so the host picks it up.\n");
    s.push_str("echo \"$EXIT\" > /sysroot/.umf-run-exit\n");
    s.push_str("sync\n");
    s.push('\n');
    s.push_str("poweroff -f\n");
    s
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
