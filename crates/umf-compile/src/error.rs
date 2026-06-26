//! Errors produced while projecting a bootable-OS OCI image into a block.

use std::io;

use thiserror::Error;
use umf_core::architecture::Architecture;

use crate::filesystem::FilesystemError;

/// Errors from the disk projector.
#[derive(Debug, Error)]
pub enum CompileError {
    /// `flavor=uki` builds a UKI, which requires `ukify`.
    #[error(
        "flavor `uki` builds a UKI, which requires `ukify` (systemd-ukify) on PATH; install systemd-ukify"
    )]
    UkifyUnavailable,

    /// A `ukify` invocation failed.
    #[error("ukify: {0}")]
    UkifyFailed(String),

    /// A UKI (`flavor=uki`) projection was requested for a target architecture
    /// that differs from the build host's. `ukify` embeds the host's systemd
    /// EFI stub (there is no flag to source the target-arch stub), so a
    /// cross-arch UKI would be unbootable. Build on the target arch, or use
    /// `flavor=systemd-boot`.
    #[error(
        "cannot build a UKI for {target} on a {host} host: ukify embeds the host's \
         EFI stub, so a cross-arch UKI is unbootable; build on a {target} host, or use \
         `LABEL org.imagilux.umf.flavor=systemd-boot`"
    )]
    CrossArchUki {
        /// The recipe's target architecture.
        target: Architecture,
        /// The build host's architecture.
        host: Architecture,
    },

    /// The configured disk size can't hold the ESP plus GPT overhead.
    #[error("disk size {disk} bytes too small; need at least {required} bytes for the ESP + GPT")]
    DiskTooSmall {
        /// Requested size.
        disk: u64,
        /// Minimum needed.
        required: u64,
    },

    /// The image isn't a bootable-OS image. Only `org.imagilux.umf.type=
    /// bootable` images project to a disk; containers and bare kernels don't.
    #[error(
        "{reference} is `type={kind}` — only `type=bootable` images can be compiled to a disk \
         (`umf build` a recipe whose FROM resolves to a kernel to produce one)"
    )]
    NotBootable {
        /// The image reference that was rejected.
        reference: String,
        /// The `org.imagilux.umf.type` value found (or `unknown`).
        kind: String,
    },

    /// The image is missing a boot-manifest label the projector needs.
    #[error("bootable image is missing the `{0}` boot-manifest label")]
    MissingLabel(&'static str),

    /// A boot-manifest *path* label (`kernel.vmlinuz` / `initramfs`) resolves
    /// outside the materialized rootfs — via `..`, an absolute path, or a
    /// symlink pointing out. Refused so a malicious image cannot make the
    /// projector read a host file onto the disk it produces.
    #[error(
        "boot-manifest label `{label}` value {value:?} escapes the image rootfs \
         (`..`, absolute, or a symlink out) — refusing to read outside the image"
    )]
    UnsafeLabelPath {
        /// The label whose value was rejected.
        label: &'static str,
        /// The offending value.
        value: String,
    },

    /// A boot-manifest value interpolated into the boot configuration (kernel
    /// release / cmdline) contains a control character (e.g. a newline) that
    /// could inject additional bootloader directives. Refused.
    #[error(
        "boot-manifest label `{label}` contains a disallowed control character — \
         refusing to inject it into the boot configuration"
    )]
    UnsafeLabelValue {
        /// The label whose value was rejected.
        label: &'static str,
    },

    /// The reference resolves to a multi-arch image index, not a single image.
    #[error("{0} resolves to an image index — pull a single-platform image first")]
    ImageIndex(String),

    /// The image's flavor label names a packaging the projector can't apply.
    #[error("flavor `{0}` is not supported (expected `systemd-boot` or `uki`; `grub` is reserved)")]
    UnsupportedBootloader(String),

    /// A classic bootloader is required but no binary could be found (not in the
    /// image, not on the host, no override).
    #[error(
        "flavor `{kind}` needs a bootloader binary; none found ({tried}) — ship one in the \
         image, install systemd-boot on the host, or pass --bootloader-path"
    )]
    BootloaderUnavailable {
        /// The bootloader kind from the manifest.
        kind: String,
        /// The host path that was probed.
        tried: String,
    },

    /// Decoding the image manifest or config JSON failed.
    #[error("decoding image json: {0}")]
    Json(#[from] serde_json::Error),

    /// Materializing the image's layers into a rootfs failed.
    #[error("materialize rootfs: {0}")]
    Materialize(#[from] umf_oci::materialize::MaterializeError),

    /// Reading the image manifest / config / layers from the layout failed.
    #[error("oci layout: {0}")]
    Oci(#[from] umf_oci::registry::RegistryError),

    /// ROOTFS partition (squashfs) emit error.
    #[error("rootfs filesystem: {0}")]
    Filesystem(#[from] FilesystemError),

    /// GPT table emission error.
    #[error("gpt: {0}")]
    Gpt(#[from] gpt::GptError),

    /// Protective-MBR write error.
    #[error("mbr: {0}")]
    Mbr(#[from] gpt::mbr::MBRError),

    /// I/O error during projection (includes FAT/ESP errors, which `fatfs`
    /// surfaces as [`io::Error`]).
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
}
