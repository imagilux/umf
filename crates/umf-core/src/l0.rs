//! L0 (FROM) artifact kinds and the target shape they imply.
//!
//! "L0" is the image at the bottom of the FROM chain — the image a build
//! starts from. Its shape determines the legal directive set, the RUN
//! execution backend, and the semantic role of FROM itself: a base image for
//! container builds, or a kernel artifact for bootable builds (a build is
//! bootable precisely when its FROM resolves to a `type=kernel` artifact).
//!
//! This module is **pure types** — no IO. The matching IO path lives in
//! `umf_builder::introspect`, which reads a manifest's config blob and maps
//! the [`crate::label::TYPE`] value to an [`L0Kind`] via
//! [`L0Kind::from_label`].

use core::fmt;

/// Identified shape of an L0 artifact.
///
/// Maps the documented `org.imagilux.umf.type` label values to typed variants,
/// plus [`Scratch`](Self::Scratch) for `FROM scratch` and
/// [`Unknown`](Self::Unknown) for forward-compatibility with future label
/// values we haven't taught the builder about yet.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum L0Kind {
    /// `FROM scratch` — no L0 image. Full boot chain unlocked.
    ///
    /// Never returned by introspection — there is no image to introspect.
    /// Constructed directly via [`Self::Scratch`] when the AST's `FROM`
    /// resolves to the `scratch` sentinel.
    Scratch,

    /// `container` — rootfs-only OCI image, the degenerate case.
    ///
    /// Valid as `FROM` for container builds. `RUN` is routed to a container
    /// backend rather than a micro-VM.
    Container,

    /// `bootable` — an assembled bootable-OS image (kernel + rootfs + boot
    /// manifest). Needs `umf compile` to project into a disk, but **is a valid
    /// `FROM`**: you can build on top of it to extend the OS, and the result is
    /// still bootable. (Replaces the old `vm` shape — a projected disk is never
    /// an OCI artifact, so there is no `type=vm`.)
    Bootable,

    /// `kernel-build-env` — container-shaped build environment carrying
    /// gcc / make / kernel headers.
    ///
    /// Valid as `FROM` for container builds (typically: a build that compiles
    /// the kernel and emits a `Payload::Kernel` artifact).
    KernelBuildEnv,

    /// A component payload — kernel / rootfs / bootloader / firmware.
    ///
    /// Only [`Payload::Kernel`] is a valid `FROM` — and using it as the base is
    /// exactly what makes a build bootable. The other payloads (rootfs,
    /// bootloader, firmware) are consumed by their respective directives, not
    /// used as starting points.
    Payload(Payload),

    /// Label present but value unrecognised.
    ///
    /// Preserved verbatim so callers can emit accurate diagnostics or apply
    /// policy without losing the source string.
    Unknown(String),
}

/// The kind of component payload a non-startable L0 represents.
///
/// Each variant corresponds to a downstream consumer:
/// - [`Self::Kernel`] is consumed by `FROM` in bootable builds.
/// - [`Self::Rootfs`] is consumed via `ADD --from=<ref>` (the base userland).
/// - [`Self::Bootloader`] is consumed by `umf compile` (a bootloader `.efi` the
///   projector installs onto the ESP per the flavor label).
/// - [`Self::Firmware`] is reserved for future use (the builder currently ships
///   firmware as part of host runtime requirements, not as an OCI artifact).
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Payload {
    /// `kernel` — kernel artifact (vmlinuz + modules) emitted by an ordinary
    /// container build that writes those binaries and labels itself accordingly.
    /// Used as `FROM` in VM builds.
    Kernel,
    /// `rootfs` — distro-specific base userland. Consumed via
    /// `ADD --from=<ref>` in a bootable build.
    Rootfs,
    /// `bootloader` — bootloader artifact (shim, GRUB, systemd-boot, …).
    /// Consumed by `umf compile` per the flavor label.
    Bootloader,
    /// `firmware` — UEFI firmware blob (typically OVMF / EDK II). Reserved
    /// for future use.
    Firmware,
}

/// Where an [`L0Kind`] was determined from.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum L0Source {
    /// The `org.imagilux.umf.type` label was present and recognised.
    Label,
    /// No label was present; kind derived from manifest structure.
    Inferred,
}

impl L0Kind {
    /// Parse an `org.imagilux.umf.type` label value.
    ///
    /// Unrecognised values are preserved as [`Self::Unknown`] rather than
    /// rejected — the builder may still apply policy, and surfacing the raw
    /// value yields better diagnostics.
    pub fn from_label(value: &str) -> Self {
        match value {
            "container" => Self::Container,
            "bootable" => Self::Bootable,
            // `vm`, `bootc`, and `unikernel` were retired as type values; any
            // such label on a legacy artifact falls through to `Unknown` below.
            "kernel-build-env" => Self::KernelBuildEnv,
            "kernel" => Self::Payload(Payload::Kernel),
            "rootfs" => Self::Payload(Payload::Rootfs),
            "bootloader" => Self::Payload(Payload::Bootloader),
            "firmware" => Self::Payload(Payload::Firmware),
            other => Self::Unknown(other.to_string()),
        }
    }

    /// `true` if this kind is a component payload.
    pub fn is_payload(&self) -> bool {
        matches!(self, Self::Payload(_))
    }

    /// `true` if this kind is a kernel artifact (`org.imagilux.umf.type=kernel`).
    ///
    /// Kernel artifacts are the one payload kind valid as `FROM`; using one as
    /// the base is what makes a build bootable.
    pub fn is_kernel(&self) -> bool {
        matches!(self, Self::Payload(Payload::Kernel))
    }

    /// `true` if this kind is a legal `FROM` for the given build intent.
    ///
    /// A `bootable` OS image is always a legal base — you can extend it and the
    /// result stays bootable. For a bootable build, the base is a kernel
    /// artifact. For a container build, `scratch`, container-shaped artifacts,
    /// and `kernel-build-env` are legal. Other payloads (rootfs / bootloader /
    /// firmware) and unknown labels are rejected.
    pub fn is_valid_from(&self, bootable_build: bool) -> bool {
        if matches!(self, Self::Bootable) {
            return true;
        }
        if bootable_build {
            self.is_kernel()
        } else {
            matches!(self, Self::Scratch | Self::Container | Self::KernelBuildEnv)
        }
    }
}

impl fmt::Display for L0Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scratch => f.write_str("scratch"),
            Self::Container => f.write_str("container"),
            Self::Bootable => f.write_str("bootable"),
            Self::KernelBuildEnv => f.write_str("kernel-build-env"),
            Self::Payload(p) => p.fmt(f),
            Self::Unknown(s) => f.write_str(s),
        }
    }
}

impl fmt::Display for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Kernel => "kernel",
            Self::Rootfs => "rootfs",
            Self::Bootloader => "bootloader",
            Self::Firmware => "firmware",
        })
    }
}

#[cfg(test)]
mod tests;
