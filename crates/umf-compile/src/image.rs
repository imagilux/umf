//! Image → block orchestration: project a bootable-OS OCI image resident in a
//! layout into a disk, reading only the image — its layers plus the
//! `org.imagilux.umf.*` boot manifest. No recipe, no second resolution.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tempfile::TempDir;
use tracing::{info, warn};
use umf_core::architecture::Architecture;
use umf_core::l0::L0Kind;
use umf_core::label;
use umf_oci::materialize::materialize_layers;
use umf_oci::registry::{ImageLayout, RegistryError};

use crate::error::CompileError;
use crate::partition::{DiskGeometry, DiskInputs, DiskProjection, project_disk};

/// Outcome of compiling a bootable-OS image into a disk.
#[derive(Debug, Clone)]
pub struct CompileReport {
    /// Manifest digest of the source image — the block-cache key.
    pub source_digest: String,
    /// Geometry + partition offsets of the produced disk.
    pub projection: DiskProjection,
    /// Entrypoint mode from the boot manifest (`systemd` / `openrc` /
    /// `appliance` / `none`).
    pub entrypoint: String,
    /// Boot-packaging flavor applied (`systemd-boot` / `uki`).
    pub flavor: String,
}

/// Project the bootable-OS image `reference` (resident in `layout`) into a disk
/// at `out`. Reads the boot manifest + the image's layers; gated on
/// `org.imagilux.umf.type=bootable`.
///
/// `bootloader_override` supplies an explicit bootloader `.efi`; when `None` and
/// the manifest names a classic bootloader, the host install is probed.
pub fn compile_image(
    layout: &ImageLayout,
    reference: &str,
    out: &Path,
    geometry: DiskGeometry,
    bootloader_override: Option<&Path>,
) -> Result<CompileReport, CompileError> {
    let entry = layout
        .lookup_ref(reference)?
        .ok_or_else(|| RegistryError::NotFound(reference.to_string()))?;

    let manifest: Manifest = serde_json::from_slice(&layout.read_blob(&entry.digest)?)?;
    if manifest.is_index() {
        return Err(CompileError::ImageIndex(reference.to_string()));
    }
    let config: ConfigDoc = serde_json::from_slice(&layout.read_blob(&manifest.config.digest)?)?;
    let labels = &config.config.labels;

    // Gate: only `type=bootable` images project to a disk.
    let kind = labels
        .get(label::TYPE)
        .map(String::as_str)
        .unwrap_or("unknown");
    if L0Kind::from_label(kind) != L0Kind::Bootable {
        return Err(CompileError::NotBootable {
            reference: reference.to_string(),
            kind: kind.to_string(),
        });
    }

    // Materialize the rootfs layer(s) (whiteout-aware) into a scratch dir.
    let rootfs = TempDir::new()?;
    let layer_digests: Vec<&str> = manifest.layers.iter().map(|d| d.digest.as_str()).collect();
    materialize_layers(layout, &layer_digests, rootfs.path())?;

    // Read the boot manifest.
    let vmlinuz_rel = require_label(labels, label::KERNEL_VMLINUZ)?;
    let kernel_release = require_label(labels, label::KERNEL_RELEASE)?;
    // Boot packaging the projector applies (the flavor LABEL). Absent ⇒ default
    // to classic systemd-boot with a warning. Older images may carry the legacy
    // `org.imagilux.umf.bootloader` label instead; fall back to it, mapping
    // `none` ⇒ `uki`, so those images still project.
    let flavor = match labels.get(label::FLAVOR) {
        Some(f) => f.as_str(),
        None => match labels.get("org.imagilux.umf.bootloader") {
            Some(legacy) if legacy == "none" => "uki",
            Some(legacy) => legacy.as_str(),
            None => {
                warn!(
                    reference,
                    "no `{}` label on the image; defaulting to `systemd-boot` (classic) — \
                     rebuild with `LABEL org.imagilux.umf.flavor=systemd-boot|uki` to be explicit",
                    label::FLAVOR,
                );
                "systemd-boot"
            }
        },
    };
    let entrypoint = labels
        .get(label::ENTRYPOINT)
        .map(String::as_str)
        .unwrap_or("systemd");
    let extra_cmdline = labels
        .get(label::KERNEL_CMDLINE)
        .map(String::as_str)
        .unwrap_or("");

    // The projector only writes a squashfs ROOTFS partition (see
    // `filesystem::write_squashfs_from_dir`). An image declaring any other
    // `rootfs.fs` would otherwise silently compile to squashfs — and boot with
    // a cmdline claiming `rootfstype=squashfs` regardless — so reject it rather
    // than misrepresent the filesystem. Absent ⇒ squashfs (the historical
    // default, kept for images emitted before the label was read).
    if let Some(rootfs_fs) = labels.get(label::ROOTFS_FS) {
        if rootfs_fs != "squashfs" {
            return Err(CompileError::Io(std::io::Error::other(format!(
                "boot-manifest label `{}` is `{rootfs_fs}`, but the projector only writes a \
                 squashfs ROOTFS partition (ext4 / erofs are not implemented); rebuild with \
                 the default squashfs rootfs",
                label::ROOTFS_FS,
            ))));
        }
    }

    // `kernel_release` and `extra_cmdline` are interpolated into the loader
    // entry / UKI cmdline; a newline would inject extra bootloader directives.
    reject_control_chars(label::KERNEL_RELEASE, kernel_release)?;
    reject_control_chars(label::KERNEL_CMDLINE, extra_cmdline)?;

    // `kernel.vmlinuz` / `initramfs` are untrusted image labels used as
    // filesystem paths — contain them to the materialized rootfs so a malicious
    // image can't read a host file (`..`, absolute, or a symlink pointing out).
    let vmlinuz = rootfs_subpath(rootfs.path(), label::KERNEL_VMLINUZ, vmlinuz_rel)?;
    // The resolved *basename* is interpolated verbatim into the systemd-boot
    // loader entry (`linux /<name>`). `rootfs_subpath` confines the path inside
    // the rootfs but places no constraint on the filename charset, and a Linux
    // filename may legally carry a newline — so a malicious image could plant
    // `vmlinuz\noptions …` and inject an extra loader directive. Guard it like
    // the cmdline values above.
    reject_filename_control_chars(label::KERNEL_VMLINUZ, &vmlinuz)?;

    // The initramfs ships inside the image rootfs (init-system builds); read it
    // back so the projector can copy it onto the ESP. Appliance builds omit the
    // label and boot directly.
    let initrd_bytes;
    let initrd_name;
    let initrd = match labels.get(label::INITRAMFS) {
        Some(path) => {
            initrd_bytes = std::fs::read(rootfs_subpath(rootfs.path(), label::INITRAMFS, path)?)?;
            initrd_name = Path::new(path).file_name().map_or_else(
                || "initramfs.img".to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            // Same guard as vmlinuz: this basename goes into `initrd /<name>`.
            reject_control_chars(label::INITRAMFS, &initrd_name)?;
            Some((initrd_bytes.as_slice(), initrd_name.as_str()))
        }
        None => None,
    };

    let architecture =
        Architecture::from_arch_str(&config.architecture).unwrap_or_else(Architecture::host);

    // Resolve the bootloader binary per the image's flavor.
    let bootloader_efi_bytes;
    let bootloader_efi = match flavor {
        "uki" => None,
        "systemd-boot" => {
            bootloader_efi_bytes =
                resolve_bootloader(architecture, bootloader_override, rootfs.path())?;
            Some(bootloader_efi_bytes.as_slice())
        }
        other => return Err(CompileError::UnsupportedBootloader(other.to_string())),
    };

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    info!(
        reference,
        out = %out.display(),
        flavor,
        entrypoint,
        "compiling bootable image to disk"
    );
    let projection = project_disk(
        out,
        &DiskInputs {
            geometry,
            rootfs_dir: rootfs.path(),
            vmlinuz: &vmlinuz,
            kernel_release,
            bootloader_efi,
            initrd,
            architecture,
            extra_cmdline,
        },
    )?;

    Ok(CompileReport {
        source_digest: entry.digest,
        projection,
        entrypoint: entrypoint.to_string(),
        flavor: flavor.to_string(),
    })
}

fn require_label<'a>(
    labels: &'a BTreeMap<String, String>,
    key: &'static str,
) -> Result<&'a str, CompileError> {
    labels
        .get(key)
        .map(String::as_str)
        .ok_or(CompileError::MissingLabel(key))
}

/// Resolve a boot-manifest *path* label (`kernel.vmlinuz` / `initramfs`) to a
/// file **inside** the materialized rootfs, refusing any escape. The value is
/// attacker-controlled (it comes from a pulled image), so this is the
/// containment boundary that stops a malicious image from making the projector
/// read a host file onto the disk it produces:
///
/// 1. Reject non-`Normal` path components (`..`, absolute / prefix) — blocks the
///    `../../etc/shadow` traversal.
/// 2. Canonicalize the result and require it stays under the (canonicalized)
///    rootfs — blocks a layer that planted a symlink whose target points out,
///    and incidentally rejects a label naming a file the image doesn't contain.
fn rootfs_subpath(
    rootfs: &Path,
    label: &'static str,
    value: &str,
) -> Result<PathBuf, CompileError> {
    let unsafe_path = || CompileError::UnsafeLabelPath {
        label,
        value: value.to_string(),
    };

    let mut candidate = rootfs.to_path_buf();
    for comp in Path::new(value.trim_start_matches('/')).components() {
        match comp {
            std::path::Component::Normal(c) => candidate.push(c),
            std::path::Component::CurDir => {}
            _ => return Err(unsafe_path()),
        }
    }

    // `canonicalize` resolves symlinks (and requires the target to exist), so a
    // planted symlink pointing outside the rootfs canonicalizes outside it.
    let canonical = candidate.canonicalize().map_err(|_| unsafe_path())?;
    let root_canonical = rootfs.canonicalize().map_err(CompileError::Io)?;
    if !canonical.starts_with(&root_canonical) {
        return Err(unsafe_path());
    }
    Ok(canonical)
}

/// Reject a boot-manifest value that gets interpolated into the loader
/// entry / UKI cmdline if it carries a control character (newline, CR, …) —
/// otherwise a crafted value could inject extra bootloader directives.
fn reject_control_chars(label: &'static str, value: &str) -> Result<(), CompileError> {
    if value.chars().any(char::is_control) {
        return Err(CompileError::UnsafeLabelValue { label });
    }
    Ok(())
}

/// [`reject_control_chars`] applied to a resolved path's *basename* — the
/// kernel/initramfs filenames are interpolated into the loader entry
/// (`linux /<name>`, `initrd /<name>`). `rootfs_subpath` confines the path
/// inside the rootfs but says nothing about the filename's character set.
fn reject_filename_control_chars(label: &'static str, path: &Path) -> Result<(), CompileError> {
    if let Some(name) = path.file_name() {
        reject_control_chars(label, &name.to_string_lossy())?;
    }
    Ok(())
}

/// Resolve the systemd-boot `.efi`, in precedence order:
///
/// 1. the explicit `bootloader_override` argument (a library test seam; there
///    is no CLI flag for it);
/// 2. a bootloader shipped *inside the image rootfs*
///    (`/usr/lib/systemd/boot/efi/<arch>.efi`, e.g. from the userland's own
///    `systemd-boot` package), confined to the rootfs via [`rootfs_subpath`] so
///    a planted symlink can't read a host file out onto the disk.
///
/// There is **no host fallback**: the disk must be reproducible from the image
/// alone, so a classic-flavor image that ships no bootloader is an error (use
/// `flavor=uki`, or install systemd-boot into the rootfs).
fn resolve_bootloader(
    architecture: Architecture,
    override_path: Option<&Path>,
    rootfs: &Path,
) -> Result<Vec<u8>, CompileError> {
    // 1. Explicit override wins.
    if let Some(p) = override_path {
        return std::fs::read(p).map_err(|_| CompileError::BootloaderUnavailable {
            kind: "systemd-boot".to_string(),
            tried: p.display().to_string(),
        });
    }

    let filename = architecture.systemd_boot_filename();
    let in_image_rel = format!("usr/lib/systemd/boot/efi/{filename}");

    // 2. A bootloader the rootfs provides. `rootfs_subpath` canonicalizes and
    //    confines the result inside the rootfs, so a symlink escaping to a host
    //    file (or an absent path) is refused. There is no host fallback: the
    //    disk must be reproducible from the image alone.
    if let Ok(p) = rootfs_subpath(rootfs, "in-image bootloader", &in_image_rel)
        && let Ok(bytes) = std::fs::read(&p)
    {
        return Ok(bytes);
    }

    Err(CompileError::BootloaderUnavailable {
        kind: "systemd-boot".to_string(),
        tried: format!("{in_image_rel} (in image)"),
    })
}

// ── Minimal OCI manifest / config shapes (label + layer extraction only) ─────

#[derive(Debug, Default, Deserialize)]
struct Manifest {
    #[serde(rename = "mediaType", default)]
    media_type: String,
    #[serde(default)]
    config: Descriptor,
    #[serde(default)]
    layers: Vec<Descriptor>,
}

impl Manifest {
    fn is_index(&self) -> bool {
        self.media_type.contains("image.index") || self.media_type.contains("manifest.list")
    }
}

#[derive(Debug, Default, Deserialize)]
struct Descriptor {
    #[serde(default)]
    digest: String,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigDoc {
    #[serde(default)]
    architecture: String,
    #[serde(default)]
    config: ConfigSection,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigSection {
    #[serde(default, rename = "Labels")]
    labels: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests;
