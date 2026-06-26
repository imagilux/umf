//! Constants for the `org.imagilux.umf.*` OCI label namespace.

/// Root namespace for all UMF-owned OCI labels.
pub const NAMESPACE: &str = "org.imagilux.umf";

/// Label key identifying the UMF artifact shape (`container`, `bootable`,
/// `kernel`, `kernel-build-env`, `rootfs`, `bootloader`, `firmware`).
pub const TYPE: &str = "org.imagilux.umf.type";

/// Label key for the UMF spec version the artifact was produced against.
pub const SPEC_VERSION: &str = "org.imagilux.umf.spec";

/// The UMF spec version this build of the implementation targets.
///
/// Written as the value of the [`SPEC_VERSION`] label when emitting artifacts.
/// Updated whenever the normative spec under `docs/` revises in a way that
/// changes artifact format or directive semantics.
pub const CURRENT_SPEC_VERSION: &str = "0.0.1";

// ----- Boot-manifest labels -----
//
// Present only on **bootable OS images** (those built `FROM` a `type=kernel`
// artifact). They make the image self-describing for projection: `umf compile`
// reads them to assemble a bootable disk from the image *alone* — no recipe, no
// out-of-band parameters.

/// PID-1 mode baked into a bootable OS image: `systemd` / `openrc` (init
/// systems — the kernel boots `/sbin/init` with a generated initramfs) |
/// `appliance` (a binary runs as PID 1 via `init=`, no initramfs) | `none`.
pub const ENTRYPOINT: &str = "org.imagilux.umf.entrypoint";

/// Kernel release string of the embedded kernel (e.g. `7.0.0`). Modules live at
/// `/usr/lib/modules/<release>/` within the image rootfs.
pub const KERNEL_RELEASE: &str = "org.imagilux.umf.kernel.release";

/// Path, within the image rootfs, to the kernel image (vmlinuz) the projector
/// copies onto the ESP (e.g. `/boot/vmlinuz-7.0.0`).
pub const KERNEL_VMLINUZ: &str = "org.imagilux.umf.kernel.vmlinuz";

/// Extra kernel command-line tokens the projector appends — carries the
/// appliance `init=<path> [-- args]` fragment; empty for init-system modes.
/// (`root=` / `rootfstype=` / `ro` / console are derived by the projector from
/// the partition layout and [`ROOTFS_FS`].)
pub const KERNEL_CMDLINE: &str = "org.imagilux.umf.kernel.cmdline";

/// Path, within the image rootfs, to the generated initramfs the projector
/// copies onto the ESP. Absent ⇒ appliance build (no initramfs).
pub const INITRAMFS: &str = "org.imagilux.umf.initramfs";

/// Root-partition filesystem the projector formats: `squashfs` | `erofs` |
/// `ext4`.
pub const ROOTFS_FS: &str = "org.imagilux.umf.rootfs.fs";

/// Boot packaging `umf compile` applies, set as a `LABEL` on the bootable
/// image: `systemd-boot` / `grub` (classic — loose kernel + initrd behind a
/// loader entry) | `uki` (kernel, initrd, and cmdline wrapped in a single
/// `systemd-stub` `.efi`). Absent ⇒ compile defaults to `systemd-boot` and
/// warns; an unrecognised value is an error.
///
/// Replaces the former `org.imagilux.umf.bootloader` label; `BOOTLOADER` is
/// not a UMF directive. (Older images carrying the old label still
/// project: compile falls back to it, mapping `none` ⇒ `uki`.)
pub const FLAVOR: &str = "org.imagilux.umf.flavor";

#[cfg(test)]
mod tests;
