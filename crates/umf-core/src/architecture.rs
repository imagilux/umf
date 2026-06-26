//! CPU architecture the build targets.
//!
//! UMF supports x86_64 (Intel / AMD) and aarch64 (Apple Silicon, ARM
//! servers, Raspberry Pi). Every architecture-tied string the builder
//! emits — the QEMU binary name, the UEFI fallback path
//! (`BOOTX64.EFI` vs `BOOTAA64.EFI`), the systemd-boot binary name, the
//! Alpine CDN URL's arch component, the OCI image-config `architecture`
//! field — comes out of [`Architecture`] so adding a new architecture
//! later is a single-enum-variant change.
//!
//! [`Architecture::host`] picks the architecture matching the *build host*
//! at compile time. The CLI's `--platform` flag (Buildx convention,
//! `os/arch`) overrides this when cross-arch builds are requested.

/// One of the CPU architectures UMF currently supports.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Architecture {
    /// 64-bit x86 (Intel / AMD).
    X86_64,
    /// 64-bit ARM (Apple Silicon, AWS Graviton, Raspberry Pi 4/5).
    Aarch64,
}

/// Error returned when a `<os>/<arch>` platform string can't be parsed
/// into an [`Architecture`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformParseError {
    /// The input string.
    pub input: String,
    /// Why it didn't parse.
    pub reason: PlatformParseReason,
}

/// Specific failure mode for [`Architecture::from_platform_str`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformParseReason {
    /// Expected `os/arch`, found something without a `/`.
    MissingSeparator,
    /// The OS isn't `linux`. UMF is Linux-only — host kernels other
    /// than Linux can be VM hypervisors but not build targets.
    UnsupportedOs(String),
    /// The architecture name isn't recognised.
    UnsupportedArchitecture(String),
}

impl std::fmt::Display for PlatformParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.reason {
            PlatformParseReason::MissingSeparator => {
                write!(
                    f,
                    "platform {:?} missing `/` separator (expected `os/arch`)",
                    self.input
                )
            }
            PlatformParseReason::UnsupportedOs(os) => {
                write!(
                    f,
                    "platform {:?} OS {:?} unsupported (only `linux`)",
                    self.input, os
                )
            }
            PlatformParseReason::UnsupportedArchitecture(arch) => {
                write!(
                    f,
                    "platform {:?} architecture {:?} unsupported (only `amd64`/`x86_64` or `arm64`/`aarch64`)",
                    self.input, arch,
                )
            }
        }
    }
}

impl std::error::Error for PlatformParseError {}

impl Architecture {
    /// Architecture matching the running build host. Resolved at compile
    /// time via [`cfg`]; build hosts on unsupported arches default to
    /// [`Architecture::X86_64`] (the loudest failure surface).
    #[must_use]
    pub const fn host() -> Self {
        if cfg!(target_arch = "x86_64") {
            Self::X86_64
        } else if cfg!(target_arch = "aarch64") {
            Self::Aarch64
        } else {
            // Best-effort default — downstream calls (QEMU detection,
            // bootloader paths) will fail clearly when run on an arch we
            // don't know about, rather than silently producing the wrong
            // disk image.
            Self::X86_64
        }
    }

    /// Parse a Buildx-shaped platform string. Accepts:
    ///
    /// * `linux/amd64` (OCI convention) → [`Architecture::X86_64`]
    /// * `linux/x86_64` (Linux convention) → same
    /// * `linux/arm64` (OCI) → [`Architecture::Aarch64`]
    /// * `linux/aarch64` (Linux) → same
    pub fn from_platform_str(s: &str) -> Result<Self, PlatformParseError> {
        let (os, arch) = s.split_once('/').ok_or_else(|| PlatformParseError {
            input: s.into(),
            reason: PlatformParseReason::MissingSeparator,
        })?;
        if os != "linux" {
            return Err(PlatformParseError {
                input: s.into(),
                reason: PlatformParseReason::UnsupportedOs(os.into()),
            });
        }
        Self::from_arch_str(arch).ok_or_else(|| PlatformParseError {
            input: s.into(),
            reason: PlatformParseReason::UnsupportedArchitecture(arch.into()),
        })
    }

    /// Parse just the architecture part of a platform string, accepting
    /// either the OCI shorthand (`amd64`, `arm64`) or the Linux name
    /// (`x86_64`, `aarch64`).
    #[must_use]
    pub fn from_arch_str(s: &str) -> Option<Self> {
        match s {
            "amd64" | "x86_64" => Some(Self::X86_64),
            "arm64" | "aarch64" => Some(Self::Aarch64),
            _ => None,
        }
    }

    /// Name of the `qemu-system-<arch>` binary for this architecture.
    #[must_use]
    pub const fn qemu_binary_name(self) -> &'static str {
        match self {
            Self::X86_64 => "qemu-system-x86_64",
            Self::Aarch64 => "qemu-system-aarch64",
        }
    }

    /// UEFI removable-media fallback filename, mounted at
    /// `/EFI/BOOT/<filename>` on the ESP. Firmware loads this when no
    /// NVRAM boot variable matches.
    #[must_use]
    pub const fn uefi_fallback_filename(self) -> &'static str {
        match self {
            Self::X86_64 => "BOOTX64.EFI",
            Self::Aarch64 => "BOOTAA64.EFI",
        }
    }

    /// systemd-boot binary name as shipped by the distros' `systemd`
    /// (or `systemd-boot-unsigned`) packages.
    #[must_use]
    pub const fn systemd_boot_filename(self) -> &'static str {
        match self {
            Self::X86_64 => "systemd-bootx64.efi",
            Self::Aarch64 => "systemd-bootaa64.efi",
        }
    }

    /// Linux `uname -m`-style architecture string. Used in Alpine CDN
    /// URLs and similar distro endpoints.
    #[must_use]
    pub const fn linux_arch_string(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }

    /// OCI image-config-shaped architecture string. Used in the
    /// `architecture` field of `config.json` and similar manifests.
    #[must_use]
    pub const fn oci_arch_string(self) -> &'static str {
        match self {
            Self::X86_64 => "amd64",
            Self::Aarch64 => "arm64",
        }
    }

    /// Linux serial-console device for this architecture's primary UART,
    /// used in the kernel `console=` parameter. x86 exposes a 16550
    /// (`ttyS0`); aarch64 under QEMU's `virt` board exposes a PL011
    /// (`ttyAMA0`). A disk or micro-VM told the wrong device boots with no
    /// serial console output.
    #[must_use]
    pub const fn serial_console(self) -> &'static str {
        match self {
            Self::X86_64 => "ttyS0",
            Self::Aarch64 => "ttyAMA0",
        }
    }
}

impl Default for Architecture {
    fn default() -> Self {
        Self::host()
    }
}

impl std::fmt::Display for Architecture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.linux_arch_string())
    }
}

#[cfg(test)]
mod tests;
