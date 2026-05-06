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

## L0 Introspection

Every UMF artifact self-describes its target type via the `org.imagilux.umf.type` label set at build time. When a downstream build's `FROM` resolves to an OCI artifact, the builder reads that label — and, when absent, infers from manifest structure (presence of boot-chain layers, ENTRYPOINT value) — to determine the legal directive set, the RUN execution environment, and the active mode of any multi-mode directive.

The rule is single-sourced from the L0 type rather than per-directive exclusion tables:

- **`FROM scratch`** — blank L0. The full boot chain (FIRMWARE, BOOTLOADER, ROOTFS, KERNEL, INITRD) is unlocked; this is how a new bootable artifact is constructed from zero.
- **L0 is container-shaped** (rootfs only, no boot chain) — userspace directives (RUN, ADD, ENV, …) layer on top. RUN executes in a container. KERNEL in this context operates in *producer mode*: it fetches Linux sources at the requested release, applies the `.config` supplied via `ADD ./<path> /.config`, compiles against the toolchain present in L0, and emits a kernel artifact.
- **L0 is VM- or bootc-shaped** (boot chain present) — userspace directives layer on top, but re-declaring boot-chain directives is an error (the chain is already baked in). RUN executes in a micro-VM booted from the current layer state.
- **L0 is a component payload** (a published kernel, bootloader, or standalone rootfs) — not a valid `FROM` by itself; these are payloads to be installed by a parent build, not starting points. The builder rejects with a clear error.

This is what makes UMF recursive. A kernel build `FROM`'s a container-shaped kernel-build-env. That env was itself built `FROM debian:bookworm`, also container-shaped. Walking the chain requires no special cases — the same introspection rule applies at every level.

---

## Directives Reference

### FROM

| Property | Value |
|----------|-------|
| Status   | Required |
| Values   | `image_name:release` \| `scratch` |
| Default  | `scratch` implied when not provided |
| Multiple | No |
| Excludes | Determined by L0 introspection — see [L0 Introspection](#l0-introspection) |

Creates the first layer (L0) from `scratch` (blank, full boot chain unlocked) or from an existing OCI artifact, which the builder introspects to determine the legal directive set for the layers that follow.

When `image_name:release` resolves to a UMF artifact, the builder reads `org.imagilux.umf.type` and applies the corresponding rules (see [L0 Introspection](#l0-introspection)). Non-UMF OCI images are treated as container-shaped L0 by default.

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

KERNEL has two modes, selected by L0 introspection (see [L0 Introspection](#l0-introspection)):

- **Consumer mode** — `FROM scratch` with a boot chain. Resolves a pre-built kernel artifact and installs it as L2: the kernel image into L1's `/boot`, modules into `/lib/modules/<release>`. Resolution: **registry → local cache → upstream source build**.
- **Producer mode** — `FROM` a container-shaped build env. Fetches Linux sources at the requested release, applies the kernel `.config` supplied via `ADD ./<path> /.config` in the build, compiles against the toolchain present in L0, and emits a kernel artifact. The result is what consumer-mode KERNEL elsewhere will resolve.

The release string maps 1:1 to `torvalds/linux` git tags — `KERNEL linux:v7.0` resolves to upstream `v7.0`.

#### Example

Consumer (in a VM build):

```dockerfile
KERNEL linux:v7.0
```

Producer (in a kernel-build artifact):

```dockerfile
FROM imagilux/kernel-build-env:v1
KERNEL linux:v7.0
ADD ./config/default /.config
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
