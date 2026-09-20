//! The rootfs boot contract: the single source of truth for the root
//! filesystem types and GPT partition label that the two sides of a bootable
//! artifact must agree on.
//!
//! A bootable disk only boots when three things agree: the bytes actually
//! written into the ROOTFS partition, the `rootfstype=` token on the kernel
//! command line, and whatever the initramfs passes to `mount`. Historically
//! all three were the same hard-coded `squashfs` constant, which made the
//! agreement trivially true and the choice unavailable.
//!
//! They are now kept in lockstep by *ownership* instead: `umf compile` is the
//! single writer of both the partition contents and the cmdline, so the two
//! cannot disagree. The initramfs no longer names a filesystem at all — it
//! reads `rootfstype=` back from `/proc/cmdline`, exactly as it already reads
//! `root=` to find the device, and carries the drivers for every supported
//! filesystem so whichever one the projector chose can be mounted. That is
//! what lets one built image project to any of these filesystems.

/// Root filesystem written into the ROOTFS partition by `umf compile`.
///
/// Selected at **projection** time (`umf compile --fs`), not at build time:
/// the filesystem is a property of the disk being projected, not of the OCI
/// image, whose layers are identical either way. An image's
/// [`crate::label::ROOTFS_FS`] label records the default to use when `--fs`
/// is not given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RootfsFs {
    /// Read-only, compressed. Written in-process by `umf-compile` via
    /// `backhand` — no host tooling, which is why it is the default.
    #[default]
    Squashfs,
    /// Read-write, uncompressed. Written by the host's `mkfs.ext4`.
    /// The choice for a root meant to be remounted writable in place.
    Ext4,
    /// Read-only, compressed; faster random access than squashfs.
    /// Written by the host's `mkfs.erofs`.
    Erofs,
}

impl RootfsFs {
    /// Every supported filesystem, in the order the CLI lists them.
    pub const ALL: [Self; 3] = [Self::Squashfs, Self::Ext4, Self::Erofs];

    /// The canonical token: the `rootfstype=` cmdline value, the
    /// [`crate::label::ROOTFS_FS`] label value, the `mount -t` type, and the
    /// kernel module name — all four are the same string for all three of
    /// these filesystems, which is why one accessor serves every caller.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Squashfs => "squashfs",
            Self::Ext4 => "ext4",
            Self::Erofs => "erofs",
        }
    }

    /// Parse a label or flag value. Returns `None` for anything unrecognised;
    /// callers report the supported set rather than guessing a default, since
    /// silently falling back would project a filesystem the operator did not
    /// ask for.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|fs| fs.as_str() == token)
    }

    /// The host `mkfs` binary this filesystem needs, or `None` when it is
    /// written in-process.
    ///
    /// This is the honest shape of the dependency: squashfs keeps `umf
    /// compile` pure-Rust and air-gapped-safe, while ext4 and erofs are a
    /// deliberate exception — those formats have no mature pure-Rust writer,
    /// and the tools are standard on any distribution. Unlike the erofs
    /// *layer cache* in `umf-oci`, which falls back to a pure-Rust unpack
    /// when `mkfs.erofs` is missing, there is no fallback here: a root
    /// partition the operator asked to be ext4 cannot be silently written as
    /// something else, so a missing tool is a hard error.
    #[must_use]
    pub const fn host_mkfs(self) -> Option<&'static str> {
        match self {
            Self::Squashfs => None,
            Self::Ext4 => Some("mkfs.ext4"),
            Self::Erofs => Some("mkfs.erofs"),
        }
    }

    /// Whether the filesystem is mounted read-only by the kernel regardless
    /// of mount options. Both compressed formats are; ext4 is not, which is
    /// the entire reason to pick it.
    #[must_use]
    pub const fn is_read_only(self) -> bool {
        matches!(self, Self::Squashfs | Self::Erofs)
    }

    /// The supported tokens, comma-separated — for error messages and help
    /// text, so the accepted set is never written out by hand twice.
    #[must_use]
    pub fn supported_tokens() -> String {
        Self::ALL
            .iter()
            .map(|fs| fs.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl std::fmt::Display for RootfsFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RootfsFs {
    type Err = UnsupportedRootfsFs;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_token(s).ok_or_else(|| UnsupportedRootfsFs(s.to_string()))
    }
}

/// A `rootfs.fs` token that names no filesystem UMF can write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedRootfsFs(pub String);

impl std::fmt::Display for UnsupportedRootfsFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unsupported root filesystem `{}` (supported: {})",
            self.0,
            RootfsFs::supported_tokens(),
        )
    }
}

impl std::error::Error for UnsupportedRootfsFs {}

/// GPT partition label of the root partition. The projector names the
/// partition this and references it as `root=PARTLABEL=<this>`, so the disk
/// boots bus-agnostically with no `/dev/vda2` assumption (see issue 198).
pub const ROOTFS_PARTLABEL: &str = "ROOTFS";

#[cfg(test)]
mod tests;
