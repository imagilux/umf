# Specification

The normative reference for UMF directive behavior, build order, and artifact resolution.

For the project pitch, design rationale, and target matrix, see [Overview](index.md). For end-to-end workflows (building a kernel, rootfs, composing a VM), see [Examples](examples.md).

---

## Build Order

The build pipeline processes directives in a strict dependency order; each phase contributes to the OCI image. For bootable builds the kernel binaries come from the artifact referenced by `FROM`.

```
ADD <oci-ref> / userland (L1) → kernel install from FROM (L2) → initramfs (L3, init-system builds only) → RUN/ADD/... (L4+)
        └─ umf build emits a plain OCI image (type=bootable);  L0 (boot partition) is laid down later by umf compile
```

- **L1** — Root filesystem: the base userland is added with `ADD <oci-ref> /`, which unpacks the referenced OCI image's filesystem onto the build's staging tree (the base image is resolved through the same chain `FROM` uses).
- **L2** — Kernel: the `FROM` artifact (a kernel OCI image) is pulled and its `boot/vmlinuz-*` and `lib/modules/*` payload is installed onto L1.
- **L3** — Initramfs: generated implicitly from L2's modules and L1's filesystem context when `ENTRYPOINT` is an init system (`systemd` or `openrc`), and shipped inside the image rootfs. Skipped when `ENTRYPOINT` is a binary path (appliance shape) — the kernel jumps straight to the binary via `init=`. The builder picks the initramfs generator (dracut, mkinitramfs, mkinitfs, …) based on `ENTRYPOINT`.
- **L4+** — User-space layers: RUN, ADD, ENV, etc. Each directive that produces a filesystem diff becomes one content-addressed layer; subsequent builds reuse layers whose input hash (the directive text plus the input context) matches a previously-emitted blob.

`umf build` emits all of the above as a **plain OCI image** (`org.imagilux.umf.type=bootable`) carrying a [boot manifest](#boot-manifest-labels). **L0 — the boot partition** (GPT/ESP, plus a classic bootloader entry or a UKI) is *not* a build layer: it is laid down by `umf compile`, which projects the image into a disk on demand. Firmware (OVMF for a VM, the hardware's own on bare metal) is supplied at run/deploy time, never built in. UMF is UEFI-only (GPT/ESP, FAT32); BIOS / MBR is not supported.

Container builds skip L1–L3 (and L0) entirely: `FROM <image>` (or `FROM scratch`) supplies the starting filesystem, then user-space layers stack on top.

## Artifact Resolution

Every reference to an OCI artifact — `FROM`, `ADD <oci-ref>` — follows the same resolution chain:

```
Registry lookup → Local cache → Build from source
```

This means any UMF file can bootstrap on a fully air-gapped, single-node setup — you pay the build-time penalty once, then artifacts are cached locally. Stand up a local registry and other nodes pull from it. Federate registries across sites for a full supply chain. Entry at any point in the chain works.

## L0 Introspection

Every UMF artifact self-describes its target type via the `org.imagilux.umf.type` label set at build time. When a downstream build's `FROM` resolves to an OCI artifact, the builder reads that label — and, when absent, infers from manifest structure (presence of boot-chain layers, ENTRYPOINT value) — to determine the legal directive set and the RUN execution environment.

`FROM` is polymorphic, and **what it resolves to** is the single source of truth for the build's shape: a base image (or `scratch`) makes it a container build; a kernel artifact makes it a bootable build. The builder reads the resolved artifact's label and shapes the build accordingly. Mismatches (a directive that doesn't fit the resolved shape) are rejected at build start.

The rule is single-sourced from the FROM type, rather than per-directive exclusion tables:

- **`FROM scratch`** — blank starting point. A container build: user-space layers (RUN, ADD, ENV, …) stack on top to construct the artifact from nothing. It has no kernel, so it is never bootable.
- **`FROM` resolves to a container-shaped artifact** (no boot chain — `org.imagilux.umf.type=container` or label absent) — a container build. RUN executes in a container; user-space directives layer on top.
- **`FROM` resolves to a kernel artifact** (`org.imagilux.umf.type=kernel`) — a bootable build: using a kernel as the base is exactly what makes the build bootable. Its `boot/vmlinuz-*` and `lib/modules/*` are installed at L2 (see [Build Order](#build-order)), the userland is added with `ADD <oci-ref> /` and boot packaging is selected with the `org.imagilux.umf.flavor` label, and RUN executes in a micro-VM. The result is `type=bootable`.
- **`FROM` resolves to a bootable image** (`org.imagilux.umf.type=bootable`) — **valid**: a bootable image is a regular OCI image, so it can be extended like any base, and the result stays bootable. (It can also be projected straight to a disk with `umf compile`.) The old `type=vm` no longer exists — a projected disk is never an OCI artifact.

Kernel and rootfs publishing are themselves ordinary container builds. A kernel build `FROM`s either `scratch` (and fetches, compiles, and installs the kernel through RUN steps) or a pre-built kernel-build-env (and runs a build script over the supplied `.config`). Either way, it writes `boot/vmlinuz-<release>` and `lib/modules/<release>/` to its filesystem and labels itself `org.imagilux.umf.type=kernel`. The output is a regular OCI image — what makes it a "kernel artifact" is the label plus the file layout, not a special build mode. Downstream bootable builds reference it via `FROM` exactly like any other OCI image. The same shape applies to rootfs and bootloader publishing.

This is what makes UMF recursive. A kernel build `FROM`s a container-shaped kernel-build-env. That env was itself built `FROM debian:bookworm`, also container-shaped. Walking the chain requires no special cases — the same introspection rule applies at every level.

## Boot-manifest labels

A **bootable OS image** — one built `FROM` a `type=kernel` artifact — carries a small set of `org.imagilux.umf.*` labels (alongside `type`) that make it *self-describing for projection*. The projection step (`umf compile`) reads them to assemble a bootable disk from the image **alone** — no recipe, no out-of-band parameters. They are emitted by the builder, not authored directly.

| Label | Meaning | Values |
|-------|---------|--------|
| `org.imagilux.umf.entrypoint` | PID-1 mode | `systemd` / `openrc` (init system + generated initramfs) · `appliance` (binary as PID 1 via `init=`, no initramfs) · `none` |
| `org.imagilux.umf.kernel.release` | Embedded kernel release | e.g. `7.0.0`; modules at `/usr/lib/modules/<release>/` |
| `org.imagilux.umf.kernel.vmlinuz` | Path to the kernel image within the rootfs | e.g. `/boot/vmlinuz-7.0.0` |
| `org.imagilux.umf.kernel.cmdline` | Extra kernel cmdline appended at projection | the appliance `init=<path> [-- args]` fragment; empty for init systems |
| `org.imagilux.umf.initramfs` | Path to the generated initramfs within the rootfs | absent ⇒ appliance (no initramfs) |
| `org.imagilux.umf.rootfs.fs` | Root-partition filesystem the projector formats | `squashfs` · `erofs` · `ext4` |
| `org.imagilux.umf.flavor` | Boot packaging the projector applies | `systemd-boot` (classic) · `uki` (Unified Kernel Image); `grub` reserved |

Unlike the other rows, which the builder emits, `org.imagilux.umf.flavor` is authored directly as a normal `LABEL` (see the [LABEL](#label) directive). `umf compile` reads it: **absent ⇒ `systemd-boot` (classic), with a warning**; an unrecognised value is an error. The `root=` / `rootfstype=` / console portions of the kernel command line are derived by the projector from the partition layout and `rootfs.fs`; only `kernel.cmdline` is carried on the image. Because the disk is derived entirely from these labels plus the image's ordinary layers, the same OCI image is both a `podman`-runnable container **and** a projectable bootable artifact — "bootable" is additive metadata, not a distinct artifact kind.

## Multi-Stage Builds

A single UMF source file can declare multiple build stages, each with its own `FROM` and its own directive sequence. Stages are named with `FROM <ref> AS <name>` and the final stage determines the build shape (its `FROM` selects container or bootable). Earlier stages produce intermediate artifacts whose files can be copied into the final stage with `ADD --from=<name> <src> <dst>`.

Each stage is an independent OCI image internally; only the final stage's filesystem ends up in the published artifact. Intermediate stages serve as throwaway compile / build environments — toolchains, source trees, and build caches stay out of the shipped image.

#### Example

```dockerfile
FROM rust:1.85 AS build
ADD ./src /src
WORKDIR /src
RUN cargo build --release

FROM scratch
ADD alpine:3.21 /
ADD --from=build /src/target/release/myapp /usr/local/bin/myapp
ENTRYPOINT ["/usr/local/bin/myapp"]
```

## Cross-Architecture Builds

The target architecture is selected by a CLI flag (`--platform=<os>/<arch>`), not a DSL directive — the same source can produce builds for multiple architectures without duplicating definitions.

```bash
umf build --platform=linux/arm64 -t myapp:1.0 .
umf build --platform=linux/amd64 -t myapp:1.0 .
```

`--platform` drives every component-resolution decision: the kernel artifact pulled via `FROM` must match the target architecture, the bootloader binaries must match, base images for container builds are pulled at the matching arch. RUN steps in container mode use `binfmt_misc` + qemu-user-static for cross-arch execution on the host. The default when `--platform` is omitted is the host's native architecture.

## Build Secrets

Sensitive material (signing keys, registry credentials, private-repo tokens) must never enter a layer — anything written to a `RUN` step's filesystem becomes a content-addressed blob and remains recoverable from the published image. UMF supports BuildKit-style mounted secrets so the secret is available to one specific `RUN` step and never persisted:

```dockerfile
RUN --mount=type=secret,id=signing-key,target=/run/secrets/key sbsign --key /run/secrets/key ...
```

The CLI supplies the secret value at build time, either from a file or from an environment variable:

```bash
umf build --secret id=signing-key,src=./key.pem -t myimage:1.0 .
umf build --secret id=signing-key,env=SIGNING_KEY -t myimage:1.0 .
```

The secret is mounted as a tmpfs file inside the `RUN` step's container or VM, scoped to that step. It does not enter any layer, does not contribute to the cache key (so a rotated key doesn't invalidate downstream caches), and is unavailable to subsequent steps. `RUN --mount=type=secret` is the only sanctioned channel for sensitive build inputs — never pass secrets through `ARG` (which is recorded in the image history) or `ADD` (which writes to a layer).

## RUN-step Network Egress

A `RUN` step may reach the network during the build (to fetch packages, clone sources, pull dependencies). How that egress is provided depends on privilege, and like `--platform` it is governed by a CLI flag rather than a directive:

- **Rootful builds** route RUN-step traffic through a host veth pair plus a NAT masquerade rule: the step egresses through the host while staying in its own network namespace.
- **Rootless builds** (an unprivileged `umf build` / `umf run`) cannot use the host veth path, which needs real root. Egress is provided by a userspace backend selected with `--rootless-net` (or the `UMF_ROOTLESS_NET` environment variable; the flag wins):
    - `native` (the default): an in-process userspace network stack that re-originates the container's connections from ordinary host sockets, with no external binary, working on an air-gapped node.
    - `pasta`: the same userspace crossing via the external `passt` / `pasta` helper (opt-in; requires the package installed).
    - `none`: loopback only, no egress.

**Default-deny SSRF policy.** Rootless egress refuses connections to host-internal destinations by default, checked at connect time on the literal destination address: loopback (and the unspecified address), link-local (including the `169.254.169.254` cloud-metadata IP), RFC1918, IPv6 unique-local, and CGNAT shared space. This is enforcement, not advice, and mirrors the block-all posture of [EXPOSE](#expose): a build's RUN steps cannot reach the host's own services, the cloud-metadata endpoint, or the local network unless an operator opts in. Re-allow categories with `--rootless-net-allow` (or `UMF_ROOTLESS_NET_ALLOW`), for instance to reach an internal package mirror:

```bash
umf build --rootless-net-allow rfc1918 -t myapp:1.0 .
```

The backends, the address categories, and the operator workflow are detailed in the reference-implementation docs ([CLI](cli.md), [Prerequisites](prerequisites.md), [Troubleshooting](troubleshooting.md)). The normative point is the security posture: outbound egress from a RUN step is namespaced, and host-internal destinations are denied by default.

---

## Directives Reference

### FROM

| Property  | Value |
|-----------|-------|
| Status    | Required (defaults to `scratch` for container builds; must be explicit — a kernel artifact — for bootable builds) |
| Values    | `image_name:release` \| `scratch` |
| Default   | `scratch` (container builds only) |
| Multiple  | One per stage (multi-stage builds use `FROM ... AS <name>`) |
| Semantics | Polymorphic — see [L0 Introspection](#l0-introspection) |

References the starting OCI artifact for the build. What `FROM` resolves to **is** the build's shape:

- **Container build**: `FROM` is a base image; `scratch` means no base — user-space layers stack on top of an empty filesystem.
- **Bootable build**: `FROM` resolves to a kernel artifact (`org.imagilux.umf.type=kernel`) — that is what makes the build bootable. The builder installs `boot/vmlinuz-*` and `lib/modules/*` from the resolved artifact onto L1 as part of the L2 kernel install phase. A `type=bootable` image is itself a valid `FROM` — extend it like any base; the result stays bootable.

Mismatches are rejected at build start: e.g. a bootable-only `org.imagilux.umf.flavor` label on a `FROM scratch` or a container base (no kernel source).

`FROM` must be the first directive of a stage. The **only** directive permitted before it is [ARG](#arg) (a global build arg usable in the `FROM` line, matching Docker); every other directive, `LABEL` included, follows `FROM`.

#### Example

Container build:

```dockerfile
FROM ubuntu:24.04
```

```dockerfile
FROM scratch
```

Bootable build (userland added with `ADD <oci-ref>`, packaging chosen by a `LABEL`):

```dockerfile
FROM imagilux/kernel-linux:7.0
ADD debian:bookworm /
LABEL org.imagilux.umf.flavor=systemd-boot
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

One `LABEL` may carry multiple `key=value` pairs on a single line (regular Docker style); it is exactly equivalent to writing one `LABEL` per pair.

#### Example

```dockerfile
LABEL <key>=<value>
LABEL org.opencontainers.image.title="app" org.opencontainers.image.version="1.0"
```

### ENV

| Property | Value |
|----------|-------|
| Status   | Optional |
| Multiple | Yes |

Sets environment variables that are embedded in the image and persist at runtime.

One `ENV` may set multiple `key=value` pairs on a single line (regular Docker style), applied left to right.

#### Example

```dockerfile
ENV <key>=<value>
ENV APP_HOME=/opt/app APP_PORT=8080
```

### ARG

| Property | Value |
|----------|-------|
| Status   | Optional |
| Multiple | Yes |
| Position | The only directive permitted **before** `FROM` |

Declares a build-time variable, optionally with a default. References to it as `${NAME}` or `$NAME` in later directive operands are substituted at build time; values come from `--build-arg NAME=value` on the CLI, falling back to the declared default.

`ARG` is the only directive allowed before the first `FROM`. A pre-`FROM` `ARG` is **global to the build**: it may be referenced in the `FROM` line (for example, to parametrize the base-image tag) and in a stage's other directives, with no need to re-declare it inside the stage. This is a deliberate, friendlier divergence from Docker, which requires re-declaring a global `ARG` within each stage before using it there; accepting the un-re-declared reference is strictly more compatible (a recipe that does re-declare still works). An in-stage `ARG` adds or overrides a value for the directives that follow it. Every other directive, including `LABEL`, follows `FROM`.

Two deliberate departures from Docker, both tightening security:

- **Values never enter the image.** An `ARG` value is not recorded in the image history or config (Docker records it, which is a classic build-arg leak). It affects only the build and the layer-cache key (so a changed value rebuilds correctly), and leaves no trace in the published artifact.
- **Secret-shaped names are flagged.** An `ARG` whose name looks like a secret (it contains `PASSWORD`, `TOKEN`, `SECRET`, or `KEY`) emits a warning that points at `RUN --mount=type=secret`, the only sanctioned channel for sensitive material (see [Build Secrets](#build-secrets)). The warning is a nudge, never a refusal.

#### Example

```dockerfile
ARG VERSION=1.0
FROM myapp:${VERSION}
RUN ./configure --release ${VERSION}
```

```bash
umf build --build-arg VERSION=2.0 -t myapp:2.0 .
```

### Boot chain (no dedicated directives)

The boot chain has zero custom directives. A bootable build is a plain OCI build that `FROM`s a kernel artifact and expresses everything else in stock OCI primitives:

- **Userland** is added with `ADD <oci-ref> /` (see [ADD](#add)). The referenced image is resolved through the same chain `FROM` uses; its filesystem becomes the root userland at L1.
- **Boot packaging** is selected with a normal `LABEL org.imagilux.umf.flavor` on the image (see [Boot-manifest labels](#boot-manifest-labels)): `systemd-boot` for a classic bootloader, `uki` for a Unified Kernel Image. `grub` is reserved (not yet implemented by the projector). `umf compile` reads the label and assembles the boot partition accordingly: absent defaults to `systemd-boot` with a warning; an unrecognised value is an error.

For the classic flavor, `umf compile` reads the bootloader `.efi` from inside the image rootfs (`/usr/lib/systemd/boot/efi/<arch>.efi`, e.g. from the userland's own systemd-boot package). There is no host fallback and no override flag: a classic-flavor image that ships no bootloader is an error (switch to `flavor=uki`, or install systemd-boot into the rootfs). For `uki`, the kernel, initramfs, and command line are wrapped in a single `systemd-stub` `.efi` placed at the ESP fallback path, which the firmware boots directly; UMF assembles the UKI at compile time (no bootloader needed).

#### Example

```dockerfile
FROM imagilux/kernel-linux:7.0
ADD debian:bookworm /                 # userland (was the ROOTFS directive)
LABEL org.imagilux.umf.flavor=systemd-boot   # classic; use uki for a Unified Kernel Image
```

### SHELL

| Property | Value |
|----------|-------|
| Status   | Optional |
| Values   | keyword `bash` \| `sh` \| `powershell` \| `none`, **or** an exec array |
| Default  | `sh` |

Sets the interpreter used to run shell-form `RUN` / `CMD` / `ENTRYPOINT` instructions. Two additive forms are accepted:

- **Keyword** (umf shorthand): `bash`, `sh`, `powershell`, or `none`. Each expands to its conventional argv (`bash` → `["/bin/bash", "-c"]`).
- **Exec array** (regular Docker style): an explicit interpreter argv, preserved verbatim — so a strict-mode `SHELL ["/bin/bash", "-euo", "pipefail", "-c"]` works as written.

#### Example

```dockerfile
SHELL sh
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
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

Sets the working directory for subsequent instructions. Creates the directory if it does not exist. A relative path is resolved against the previous `WORKDIR` (the default is `/`), so `WORKDIR /opt/app` followed by `WORKDIR sub` is `/opt/app/sub`.

#### Example

```dockerfile
WORKDIR <path>
WORKDIR /opt/app
```

### RUN

| Property | Value |
|----------|-------|
| Status   | Optional |
| Multiple | Yes |

Executes a command inside the build environment. Each RUN produces a new layer (filesystem diff).

The execution environment depends on the build target: bootable builds (bare metal / VM) run the command in a micro-VM booted from the current layer state, container builds run it in a sealed container assembled from the same layers. The DSL surface is identical either way — only the underlying runner differs.

Supports both shell form and exec form, plus `--mount=type=secret` for sensitive build inputs (see [Build Secrets](#build-secrets)). A RUN step's network egress is namespaced, and when the build is rootless it is governed by `--rootless-net` under a default-deny SSRF policy (see [RUN-step Network Egress](#run-step-network-egress)).

#### Example

```dockerfile
RUN <command>
RUN ["<command>", "<arg>"]
RUN --mount=type=secret,id=<id>,target=<path> <command>
```

### ADD

| Property | Value |
|----------|-------|
| Status   | Optional |
| Multiple | Yes |

Single source-to-destination copy primitive. The source **type** is auto-detected from its form (in this order):

- an explicit scheme: `oci://<ref>` or `https+oci://<ref>` is an **OCI image**, resolved through the same `registry → cache → source` chain `FROM` uses;
- `http://` or `https://` is a **remote blob** (a URL); the resource is fetched and, if it's a recognised archive, extracted to the destination (the implementation unpacks `.tar` and `.tar.gz` today; `.tar.xz` and `.zip` are specified but not yet extracted, see [Known limitations](known-limitations.md#add-url-archive-coverage));
- a `./`, `../`, `/`, or `~/`-prefixed source is a **local path** (file or directory in the build context);
- a bare source carrying a `:tag` or `@digest` (e.g. `debian:bookworm`, `registry.example.com/rootfs:1.0`, `name@sha256:…`) is an **OCI image** by shape heuristic. A leading `/` alone is **not** a signal here, local paths have one too; for a bare-name image with no tag, use the explicit `oci://<name>` form.

After fetch, the content is confirmed by magic-number fingerprinting (a Rust-native `file(1)`-style sniffer), so a mislabeled source (e.g. a URL that turns out not to be the expected archive) is caught.

`ADD <oci-ref> /` is the canonical way to lay down a base userland in a bootable build (it replaces the former `ROOTFS` directive): an external OCI image is a **bare `ADD` source**, never a `--from`.

`--from=<stage>` is **stage-only**: it copies from an earlier **build stage** in this file (a bare stage name, e.g. `build`), exactly like Docker's `COPY --from=<stage>` (see [Multi-Stage Builds](#multi-stage-builds)). It is never used for an external image.

Trailing `/` on the destination forces directory semantics — `ADD foo /target/` always treats `/target` as a directory. Relative destinations are resolved against the current `WORKDIR`.

#### Example

```dockerfile
ADD <source> <destination>
ADD --from=<stage> <source> <destination>
ADD <oci-ref> /
```

### COPY

| Property | Value |
|----------|-------|
| Status   | Optional |
| Multiple | Yes |

Docker-compatible plain copy of a local build-context path (or a `--from=<stage>` path) to the destination. `COPY` routes to [ADD](#add) restricted to **local-copy semantics**: a URL or `<oci-ref>` source is **rejected**, since fetching remote blobs and pulling OCI images is `ADD`'s job. For the local-context and `--from` sources it accepts, `COPY` behaves exactly like `ADD`. Note that UMF's `ADD` does not auto-extract a *local* archive either (per the [ADD](#add) rules, extraction applies only to a URL source), so `COPY ./bundle.tar /` lands the tarball verbatim, exactly as `ADD ./bundle.tar /` would. Reach for `ADD` when you want a URL fetch, archive extraction, or an `<oci-ref>` source; `COPY` exists so an existing Dockerfile builds unchanged.

#### Example

```dockerfile
COPY ./src /app/src
COPY --from=build /out/bin /usr/local/bin/
```

### ENTRYPOINT

| Property | Value |
|----------|-------|
| Status   | Required in bootable builds; optional in container (inherits from FROM otherwise) |
| Values   | `systemd` \| `openrc` \| `<path>` \| `["<path>", "<arg>", …]` \| `none` |
| Default  | Inherited from FROM in container builds; must be explicit in bootable builds |

Defines PID 1 for the artifact. The value is interpreted polymorphically:

- `systemd` — full systemd init. Typical for bootable targets; the builder generates a systemd-shaped initramfs at L3 automatically.
- `openrc` — OpenRC init (Alpine and similar). The builder generates an openrc-shaped initramfs at L3 automatically.
- `<path>` (shell form) or `["<path>", "<arg>", …]` (exec form) — direct binary execution as PID 1. In bootable builds, this is the **appliance** shape: the kernel boots straight into the binary via `init=<path>`, no init system and no initramfs (it remains a normal Linux kernel running a single process — not a library-OS unikernel). In container builds, the value is written to the image config's `ENTRYPOINT` field and runs under the consuming runtime's process model.
- `none` — no init declared. Container target only; the consuming runtime supplies PID 1.

The leading `/` (or the JSON-array exec form) disambiguates binary paths from the `systemd` / `openrc` / `none` keywords — bare strings without a leading `/` are reserved for the keyword values.

#### Example

VM with systemd:

```dockerfile
ENTRYPOINT systemd
```

Appliance — binary boots as PID 1 (no init system):

```dockerfile
ENTRYPOINT /myapp
```

Container with explicit command:

```dockerfile
ENTRYPOINT ["/usr/sbin/nginx", "-g", "daemon off;"]
```

Container with runtime-supplied init:

```dockerfile
ENTRYPOINT none
```

### CMD

| Property | Value |
|----------|-------|
| Status   | Optional |
| Multiple | Last wins |
| Target   | Container only |

Sets the image's default command (the OCI image-config `Cmd` field), exactly as Docker does. With a binary or `none` `ENTRYPOINT`, `CMD` supplies the default arguments appended to the entrypoint (overridable when the container is run); with no `ENTRYPOINT`, `CMD` is the default command itself.

`CMD` is meaningful only for the **container** target. A bootable build whose `ENTRYPOINT` is an init system (`systemd` / `openrc`) has no command to default, so a `CMD` there is **rejected** at build start. This is the one place UMF keeps Docker's `ENTRYPOINT` + `CMD` split: it falls out of the OCI config having both fields, and it costs nothing for the container case while staying out of the way of the polymorphic, init-aware `ENTRYPOINT`.

#### Example

```dockerfile
ENTRYPOINT ["/usr/bin/myapp"]
CMD ["--help"]
```

### EXPOSE

| Property | Value |
|----------|-------|
| Status   | Optional |
| Multiple | Yes |
| Default  | Block all |

Emits actual nftables firewall rules in the image. EXPOSE has enforcement semantics, not metadata-only intent — a port that isn't EXPOSEd is not reachable.

Default policy is **block all** — only explicitly exposed ports are reachable.

In the current implementation this enforcement is realized for **init-system bootable images** (`ENTRYPOINT systemd` / `openrc`), where the generated `nftables` service loads the ruleset at boot. Container builds record the exposed ports as OCI image-config metadata, and appliance images (a binary-path `ENTRYPOINT`) write the ruleset without an init system to auto-load it (see [Known limitations](known-limitations.md#expose-firewall-enforcement)).

The protocol is optional and defaults to `tcp` (`EXPOSE 8080` == `EXPOSE 8080/tcp`, regular Docker style), and one `EXPOSE` may list several ports on a line.

#### Example

```dockerfile
EXPOSE <port>[/<protocol>]
EXPOSE 80 443/tcp 53/udp
```

### VOLUME

| Property | Value |
|----------|-------|
| Status   | Optional |
| Multiple | Yes |
| Target   | Container only |

Records a mount point as a volume in the OCI image-config `Volumes` field, the advisory metadata Docker's `VOLUME` writes. A consuming container runtime honours it; UMF itself shapes no disk for it (volumes are provisioned at runtime, never baked, per the [storage model](index.md)). Inert for the bootable target, whose root filesystem is flat and whose data volumes are mounted by runtime tooling.

#### Example

```dockerfile
VOLUME /var/lib/myapp
```

### STOPSIGNAL

| Property | Value |
|----------|-------|
| Status   | Optional |
| Multiple | Last wins |
| Target   | Container only |

Sets the signal the runtime sends to stop the container, written to the OCI image-config `StopSignal` field. Advisory metadata honoured by the consuming runtime; like `VOLUME`, it is a container-config field, not something baked into a bootable image.

#### Example

```dockerfile
STOPSIGNAL SIGTERM
```

---

## Stability & versioning

The UMF DSL is **pre-1.0 and draft**. Until v1.0 the directive surface is not frozen: directives, defaults, and semantics may change between minor versions. A directive can be **removed** in a minor when the same outcome is better expressed in stock OCI primitives, or **added** to track an OCI image-config field; removals always ship with a migration note.

The bias is deliberate: prefer reusing an OCI primitive over minting a UMF-specific directive, and remove anything that earns its keep only by habit. A directive survives to 1.0 only if it expresses something OCI cannot.

**What v1.0 gates on.** v1.0 freezes the directive set and its semantics under a stability guarantee (removals would then require a major bump). The gate is: the directive surface has settled across a few real bootable builds, and the reference implementation validates the full bootable path end to end, including real-kernel boot under QEMU/KVM (now in place via the boot-smoke test). The currently not-yet-supported paths are catalogued in [Known limitations](known-limitations.md).

**Why the spec and the binary version independently.** The spec (this document, currently **0.0.1**) and the reference implementation (the `umf` binary, currently **0.0.1**) carry separate version numbers and separate release tags (`spec-vX.Y[.Z]` for the docs, `vX.Y.Z` for the binary). They ship from one git history, so a spec change and the implementation that matches it land in the same commits, but they move at different cadences: the spec changes only when the DSL's normative meaning does, while the binary also advances on bug fixes, performance, audits, and CLI ergonomics that touch no directive. Decoupling the numbers keeps "what the language means" legible without dragging it forward on every implementation patch. A given binary states which spec version it implements; mixing a binary with a newer spec than it was built against is unsupported.

---

*Spec version 0.0.1 (draft). The reference implementation lives in the same repository as this document; spec changes and the implementation that matches them ship together. Directives, defaults, and semantics are still subject to revision until v1.0.*
