# UMF — Universal Machine Format

UMF is a declarative DSL that uses OCI image mechanics to produce **bootable** artifacts — bare-metal and VM disk images, classic or UKI, with an init system or a single binary as PID 1 — through a Dockerfile-familiar syntax. It reuses the container/layer model (ephemeral execution environments, filesystem diffs, content-addressable layers) and treats `FROM` polymorphically as the kernel source in bootable builds, with the userland added via `ADD <oci-ref> /` and boot packaging selected by a stock `LABEL`. The shape — a plain OCI container or a bootable image — is **inferred** from the directives, never declared. Every `build` emits a plain, layered OCI image; a bootable image is **projected** to a disk on demand by `umf compile` (or `umf run`), never published as an artifact.

## Core principles

- **One DSL, multiple targets**: the same directive set produces standard OCI container images and bootable disk images (bare-metal or VM; classic-bootloader or UKI; init-system or single-binary PID 1).
- **OCI-native distribution**: all artifacts — including intermediate components like pre-built kernels — are OCI images stored in standard registries.
- **Sovereignty-first**: every artifact can be built from source with zero network dependencies. Registries and caches accelerate but are never required.
- **Composable supply chain**: individual components (kernels, bootloaders, rootfs) are independently versioned OCI artifacts the builder resolves through a uniform pull-or-build pipeline.

## Reference implementation

The `umf` CLI (**v0.0.1**) is a single Rust binary that builds, compiles, and runs these artifacts. The container target is feature-complete — build, run, registry push/pull, and OCI archive import/export, all **in-process** (no `docker` / `podman` daemon). Bootable builds emit plain OCI images that `umf compile` projects to disks (GPT/ESP, classic or UKI) and `umf run` boots via QEMU or Cloud Hypervisor. The tool is daemonless and OCI-native end to end. Start at the [Quickstart](quickstart.md), or see the [CLI Reference](cli.md) and [Architecture](architecture.md).

The spec versions independently from the binary: these normative pages describe the DSL (currently v0.0.1); the implementation is tagged and released on its own cadence.

## Find your way around

- [Quickstart](quickstart.md) — install the CLI and build + run your first artifact.
- [Specification](specification.md) — the normative directive reference, build order, and artifact resolution rules.
- [Compatibility](compatibility.md) — which directives apply to which target.
- [Examples](examples.md) — end-to-end workflows: building a base kernel, a curated rootfs, composing a VM, air-gapped operation.
- [CLI Reference](cli.md) — every `umf` command and flag.
- [Architecture](architecture.md) — how the reference implementation fits together.
