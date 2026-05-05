# Target Compatibility

UMF produces four kinds of artifact from a single DSL. The target is **inferred** from the combination of directives — never declared explicitly. The matrix below shows which directives apply to each target.

|              | VM | bootc | unikernel | container |
|--------------|----|-------|-----------|-----------|
| FIRMWARE     | ✓  | -     | -         | -         |
| BOOTLOADER   | ✓  | -     | -         | -         |
| KERNEL       | ✓  | ✓     | ✓         | -         |
| INITRD       | ✓  | ✓     | -         | -         |
| ROOTFS       | ✓  | ✓     | -         | ✓         |
| ENTRYPOINT   | ✓  | ✓     | binary    | ✓         |

**Legend:** ✓ applies — `-` not applicable to this target — *value* fixed value required.

## Targets at a glance

### VM

Bootable disk image with a full boot chain — firmware, bootloader, kernel, initramfs, rootfs, and init system. Suitable for hypervisors (qemu/KVM, VMware, Hyper-V) and bare-metal install.

Boot chain order: FIRMWARE → BOOTLOADER → ROOTFS → KERNEL → INITRD. Init system is selected by ENTRYPOINT (typically `systemd`).

### bootc

Bootable container image — kernel, initramfs, rootfs, init system. Host firmware and bootloader are supplied by the platform (e.g. an existing OS partition you `bootc switch` into), so FIRMWARE and BOOTLOADER are skipped.

### unikernel

Single-binary payload that runs as PID 1 directly on the kernel — no userland, no init system. KERNEL is required; ENTRYPOINT must be `binary`. ROOTFS and INITRD are omitted.

### container

The degenerate case: no boot chain at all. Standard OCI container image with a rootfs; runtime supplies PID 1 (`ENTRYPOINT none`).

---

For the directive semantics behind each row, see the [Specification](specification.md). For working configurations of each target, see [Examples](examples.md).
