# Specification

The normative reference for UMF directive behavior, build order, and artifact resolution.

For the project pitch, design rationale, and target matrix, see [Overview](index.md). For end-to-end workflows (building a kernel, rootfs, composing a VM), see [Examples](examples.md).

---

## Build Order

The build pipeline processes directives in a strict dependency order. Each major phase produces one or more OCI layers.

```
FIRMWARE (L0) → BOOTLOADER (L0) → ROOTFS (L1) → KERNEL (L2) → INITRD (L3) → RUN/ADD/... (L4+)
```

- **L0** — Boot partition: FIRMWARE creates the partition layout (GPT/ESP), BOOTLOADER installs into it. Single layer.
- **L1** — Root filesystem: distro-specific base rootfs unpacked onto the root partition.
- **L2** — Kernel: built from source (kernel.org) or pulled as a pre-built OCI artifact, installed into L1's `/boot` and `/lib/modules`.
- **L3** — Initramfs: generated from L2's modules and L1's filesystem context. Always produced after KERNEL + ROOTFS so it includes the correct drivers.
- **L4+** — User-space layers: RUN, ADD, ENV, etc. Standard layer stacking with caching semantics identical to Docker.

## Artifact Resolution

Every component directive (KERNEL, BOOTLOADER, ROOTFS, etc.) follows the same resolution chain:

```
Registry lookup → Local cache → Build from source
```

This means any UMF file can bootstrap on a fully air-gapped, single-node setup — you pay the build-time penalty once, then artifacts are cached locally. Stand up a local registry and other nodes pull from it. Federate registries across sites for a full supply chain. Entry at any point in the chain works.

---

## Directives Reference

### FROM

| Property | Value |
|----------|-------|
| Status   | Required |
| Values   | `image_name:release` \| `scratch` |
| Default  | `scratch` implied when not provided |
| Multiple | No |
| Excludes | FIRMWARE, BOOTLOADER, KERNEL, INITRD, ROOTFS when using `image_name:release` |

Creates the first layer (L0) from an existing image when provided with an `image_name:release` reference.

Delegates L0 creation to the FIRMWARE instruction when using `scratch`.

Using `scratch` removes the exclusion list, enabling all low-level directives for subsequent build steps.

#### Example

```dockerfile
FROM <image>:<release>
FROM scratch
```

### LABEL

| Property | Value |
|----------|-------|
| Status   | Optional |
| Multiple | Yes |

Key/value metadata written to the OCI manifest. Used for structural information:

- Organization, author, name, release/version
- Description, tags
- Artifact type hints (e.g. `org.imagilux.umf.type=kernel`)
- Arbitrary metadata

Labels are inherited from previous layers when building on top of an existing image.

#### Example

```dockerfile
LABEL <key>=<value>
```

### ENV

| Property | Value |
|----------|-------|
| Status   | Optional |
| Multiple | Yes |

Sets environment variables that are embedded in the image and persist at runtime.

#### Example

```dockerfile
ENV <key>=<value>
```

### ARG

| Property | Value |
|----------|-------|
| Status   | Optional |
| Multiple | Yes |

Build-time only variables. Not kept in the final image.

#### Example

```dockerfile
ARG <name>=<default>
```

### FIRMWARE

| Property | Value |
|----------|-------|
| Status   | Optional (only when `FROM scratch`) |
| Values   | `uefi` \| `uefi-secure` |
| Default  | `uefi` implied when not provided |

Creates L0 of the artifact when `FROM` is `scratch` or absent. Defines the boot chain type and creates the boot partition. UMF is UEFI-only; BIOS / MBR is not supported.

Partition layout:

- Format: GPT
- Flags: ESP, BOOT
- Filesystem: FAT32
- Size: 500MB

#### Example

```dockerfile
FIRMWARE uefi
```

### BOOTLOADER

| Property | Value |
|----------|-------|
| Status   | Optional |
| Values   | `grub` \| `systemd-boot` \| `none` |
| Default  | `systemd-boot` |

Installs the chosen bootloader onto the L0 boot partition.

#### Example

```dockerfile
BOOTLOADER systemd-boot
```

### ROOTFS

| Property | Value |
|----------|-------|
| Status   | Optional |
| Values   | `distribution:release` \| `none` |
| Default  | `none` |

Unpacks a distro-specific root filesystem onto the root partition (L1). This provides the base userland (package manager, libc, core utilities) without a kernel.

#### Example

```dockerfile
ROOTFS <distribution>:<release>
```

### KERNEL

| Property | Value |
|----------|-------|
| Status   | Optional |
| Values   | `linux:release` \| `none` |
| Default  | Upstream LTS |

Installs a Linux kernel (L2). Pulls from kernel.org and builds locally when no pre-built OCI artifact is found in the registry or local cache.

Resolution: **registry → local cache → upstream source build**.

#### Example

```dockerfile
KERNEL linux:<release>
```

### INITRD

| Property | Value |
|----------|-------|
| Status   | Optional |
| Values   | `auto` \| `dracut` \| `mkinitramfs` \| `none` |
| Default  | `auto` |

Generates an initramfs (L3). Always processed after KERNEL + ROOTFS to ensure correct module and driver inclusion.

`auto` selects the appropriate generator based on the ROOTFS distribution.

#### Example

```dockerfile
INITRD auto
```

### SHELL

| Property | Value |
|----------|-------|
| Status   | Optional |
| Values   | `bash` \| `sh` \| `powershell` \| `none` |
| Default  | `sh` |

Sets which shell is used to execute RUN instructions.

#### Example

```dockerfile
SHELL sh
```

### USER

| Property | Value |
|----------|-------|
| Status   | Optional |
| Default  | `root` |

Switches the execution context to the specified user. Creates the user if it does not exist.

#### Example

```dockerfile
USER <username>
```

### WORKDIR

| Property | Value |
|----------|-------|
| Status   | Optional |
| Default  | `/` |

Sets the working directory for subsequent instructions. Creates the directory if it does not exist.

#### Example

```dockerfile
WORKDIR <path>
```

### RUN

| Property | Value |
|----------|-------|
| Status   | Optional |
| Multiple | Yes |

Executes a command inside the build environment. Each RUN produces a new layer (filesystem diff).

In VM mode, the build environment is a micro-VM booted from the current layer state. In container-target mode, behaves identically to Docker's RUN.

Supports both shell form and exec form (shown below).

#### Example

```dockerfile
RUN <command>
RUN ["<command>", "<arg>"]
```

### ADD

| Property | Value |
|----------|-------|
| Status   | Optional |
| Multiple | Yes |

Copies files and directories from the build context into the image. Merges Docker's COPY and ADD into a single directive. Supports local paths, URLs, and archive auto-extraction.

#### Example

```dockerfile
ADD <source> <destination>
```

### ENTRYPOINT

| Property | Value |
|----------|-------|
| Status   | Optional |
| Values   | `systemd` \| `openrc` \| `binary` \| `none` |
| Default  | `systemd` |

Defines the init system / PID 1 process.

- `systemd` — full systemd init (typical for VM and bootc targets)
- `openrc` — OpenRC init (Alpine and similar)
- `binary` — direct binary execution, no init system (unikernel target)
- `none` — no init, useful for container targets where the runtime provides it

#### Example

```dockerfile
ENTRYPOINT systemd
```

### EXPOSE

| Property | Value |
|----------|-------|
| Status   | Optional |
| Multiple | Yes |
| Default  | Block all |

Creates actual nftables firewall rules in the image. Unlike Docker's EXPOSE (which is purely metadata), UMF's EXPOSE has enforcement semantics.

Default policy is **block all** — only explicitly exposed ports are reachable.

#### Example

```dockerfile
EXPOSE <port>/<protocol>
```

### ENABLE / DISABLE

| Property | Value |
|----------|-------|
| Status   | Optional |
| Multiple | Yes |

Manages init system service units. Works with both systemd and OpenRC depending on the ENTRYPOINT selection.

#### Example

```dockerfile
ENABLE <service>
DISABLE <service>
```

### HOSTNAME

| Property | Value |
|----------|-------|
| Status   | Optional |

Sets the system hostname in the image.

#### Example

```dockerfile
HOSTNAME <hostname>
```

### LOCALE

| Property | Value |
|----------|-------|
| Status   | Optional |
| Default  | `en_US.UTF-8` |

Configures the system locale.

#### Example

```dockerfile
LOCALE <locale>
```

### TIMEZONE

| Property | Value |
|----------|-------|
| Status   | Optional |
| Default  | `UTC` |

Sets the system timezone.

#### Example

```dockerfile
TIMEZONE <timezone>
```

---

*This document captures the current state of the UMF design as of initial brainstorming. Directives, defaults, and semantics are subject to revision.*
