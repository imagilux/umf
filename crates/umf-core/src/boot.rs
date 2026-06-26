//! The rootfs boot contract: the single source of truth for the root
//! filesystem type and GPT partition label that the two sides of a bootable
//! artifact must agree on. The builder side (the initramfs init script, its
//! kernel-module set, and the `org.imagilux.umf.rootfs.fs` label) and the
//! projector side (`umf compile`'s `root=PARTLABEL=… rootfstype=…` cmdline and
//! the GPT partition it creates) both source these constants, so the cmdline,
//! the partition name, and the initrd mount stay in lockstep. A mismatch
//! between any of them is an unbootable disk, which is why the strings live in
//! one place.

/// Root filesystem type: the `rootfstype=` kernel-cmdline token, the initramfs
/// `mount -t` type and the kernel module loaded for it, and the value written
/// to the [`crate::label::ROOTFS_FS`] label. squashfs is the only projected
/// rootfs today (erofs / ext4 are reserved).
pub const ROOTFS_FSTYPE: &str = "squashfs";

/// GPT partition label of the root partition. The projector names the
/// partition this and references it as `root=PARTLABEL=<this>`, so the disk
/// boots bus-agnostically with no `/dev/vda2` assumption (see issue 198).
pub const ROOTFS_PARTLABEL: &str = "ROOTFS";
