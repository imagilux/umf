# UMF — Universal Machine Format

## Overview

UMF is a declarative DSL that uses OCI image mechanics to produce bootable artifacts — from full VM disk images to unikernel payloads — through a Dockerfile-familiar syntax. The build pipeline leverages the same container/layer model (ephemeral execution environments, filesystem diffs, content-addressable layers) but extends it with VM-specific directives for firmware, bootloaders, kernels, and init systems.

## Core principles

- **One DSL, multiple targets**: the same directive set can produce VM images, bootc images, unikernel payloads, and standard OCI container images.
- **OCI-native distribution**: all artifacts — including intermediate components like pre-built kernels — are OCI images stored in standard registries.
- **Sovereignty-first**: every artifact can be built from source with zero network dependencies. Registries and caches accelerate but are never required.
- **Composable supply chain**: individual components (kernels, bootloaders, rootfs) are independently versioned OCI artifacts that the builder resolves through a uniform pull-or-build pipeline.


## Directives listing and compatibility

- For which directives apply to which target, see [Compatibility](compatibility.md).
- For the full directive reference, build order, and artifact resolution rules, see the [Specification](specification.md).
- For end-to-end workflows examples (building a base kernel, rootfs, composing a VM), see [Examples](examples.md).
