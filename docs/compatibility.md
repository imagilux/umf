# Target Compatibility

UMF produces a **container** image or a **bootable** image from a single DSL — the shape is **inferred** from the directives, never declared. Every build emits a plain OCI image; a bootable image is projected to a disk on demand by [`umf compile`](cli.md#umf-compile) (or [`umf run`](cli.md#umf-run)). Two independent axes shape the boot.

## Axis 1 — `FROM`: container vs bootable

|                              | container               | bootable                        |
|------------------------------|-------------------------|---------------------------------|
| `FROM`                       | base image \| `scratch` | kernel artifact (`type=kernel`) |
| `ADD <oci-ref> /`            | optional (userland)     | optional (userland)             |
| `LABEL org.imagilux.umf.flavor` | -                    | `systemd-boot` \| `uki`         |
| `ENTRYPOINT`                 | `<path>` \| `none`      | init \| `<path>`                |

**Legend:** ✓ applies — `-` not applicable — *italic* fixed condition. The boot chain has no dedicated directives: the userland is a stock `ADD <oci-ref> /` and boot packaging is a stock `LABEL org.imagilux.umf.flavor`.

What `FROM` resolves to *is* the target marker: a `type=kernel` artifact makes the build **bootable** (the kernel is installed at L2 and the result carries a boot manifest); a base image or `scratch` makes it a **container**. Firmware is a *boot-environment* fact supplied when the disk is run or deployed (OVMF for a VM, the hardware's own firmware on bare metal), not build content. A bootable disk is byte-identical regardless of where it will boot.

## Axis 2 — `org.imagilux.umf.flavor`: how the kernel is loaded (bootable only)

Boot packaging is a stock `LABEL` on the image, not a directive:

- `systemd-boot` (**classic**) — a bootloader is installed on the ESP and loads the kernel + initramfs via a loader entry. `grub` is reserved (not yet implemented by the projector).
- `uki` (**UKI**) — no bootloader; the kernel, initramfs, and command line are wrapped in a single `systemd-stub` `.efi` (a Unified Kernel Image) placed at the ESP fallback path, which the firmware boots directly. UMF assembles the UKI at **compile** time.

Classic-vs-UKI is purely this label's value, *not* a property of the kernel artifact. The same `FROM` kernel works either way. `umf compile` reads the label: absent defaults to `systemd-boot` with a warning; an unrecognised value is an error.

## Axis 3 — `ENTRYPOINT`: what runs as PID 1 (bootable only)

- `systemd` / `openrc` (or absent ⇒ `systemd`) — init system; the builder generates an initramfs implicitly at L3.
- `<path>` (shell form) or `["<path>", …]` (exec form) — the binary runs directly as PID 1 via the kernel's `init=`; **no init system, no initramfs**. This is the "appliance" shape (a normal Linux kernel running one process — *not* a library-OS unikernel).

The leading `/` (or the exec-array form) disambiguates a binary path from the `systemd`/`openrc`/`none` keywords.

## Targets at a glance

### container
The degenerate case: no boot chain. Standard OCI image with a rootfs; the runtime supplies PID 1 (`ENTRYPOINT none`) or runs the image's `ENTRYPOINT`.

### bootable
`FROM` is a kernel artifact; `ADD <oci-ref> /` supplies the userland; the `org.imagilux.umf.flavor` label picks classic-vs-UKI; `ENTRYPOINT` picks PID 1. `build` emits an OCI image (`type=bootable`) carrying the embedded kernel + a boot manifest. [`umf compile`](cli.md#umf-compile) projects it to a GPT/ESP disk; the same disk runs **as a VM** (the VMM supplies OVMF — `umf run` does this automatically) or **on bare metal** (written to a physical disk with `dd` / an installer / a cloud provisioner, where the hardware's firmware boots it). VM-vs-bare-metal is purely where the disk boots, not a build-time choice.

Multi-stage works the same as for a container build: the **final** stage's `FROM` selects the shape (a kernel artifact makes the whole build bootable), earlier stages build as ordinary container producers, and the final stage pulls files out of them with cross-stage `ADD --from=<stage>`. Only the final stage may be bootable; an earlier `FROM` that resolves to a kernel (nested-bootable) is rejected.

### Component artifacts
Component artifacts (kernel, kernel-build-env, rootfs, bootloader) are produced by ordinary container builds that write the appropriate payload and label themselves with the right `org.imagilux.umf.type`. They aren't booted directly; downstream builds consume a kernel or base via `FROM` and a rootfs via `ADD <oci-ref> /`. The kernel artifact stays a thin component (`vmlinuz` + modules) — UKI assembly happens in the *consuming* build's compile step, not at the kernel layer. See [L0 Introspection](specification.md#l0-introspection).

---

For the directive semantics behind each axis, see the [Specification](specification.md). For working configurations, see [Examples](examples.md).
