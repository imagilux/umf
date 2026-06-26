# UMF — Universal Machine Format

[![Rust CI](https://github.com/imagilux/umf/actions/workflows/rust.yml/badge.svg?branch=main)](https://github.com/imagilux/umf/actions/workflows/rust.yml)
[![Docs](https://github.com/imagilux/umf/actions/workflows/deploy-docs.yml/badge.svg)](https://github.com/imagilux/umf/actions/workflows/deploy-docs.yml)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

A Dockerfile-inspired DSL that uses OCI image mechanics — layered, content-addressable, registry-distributed — to build **bootable** artifacts: bare-metal and VM disk images (classic-boot or UKI-packaged), with OCI containers as the degenerate case. One declarative recipe; the shape is inferred from the directives — no target is ever declared.

📖 **Docs & specification**: <https://umf.imagilux.org/>

**Status** — spec **v0.0.1** (draft) · reference implementation **v0.0.1**. The container target is feature-complete (build + run + registry + OCI archives, all in-process). A bootable build (`FROM` a kernel artifact) emits a plain OCI image that `umf compile` / `umf run` then projects to a VM or bare-metal disk: classic boot or UKI, init-system or single-binary appliance; real-kernel boot is validated end to end under QEMU/KVM. The spec and the binary version independently and ship from the same git history. See [Known limitations](docs/known-limitations.md) for the not-yet-supported paths.

This repository holds both the **specification** (MkDocs site under `docs/`) and the **reference implementation** (Rust workspace — `umf` CLI in `src/`, libraries in `crates/`).

## What it does

```dockerfile
# hello.umf: FROM a base image (not a kernel) ⇒ container target
FROM alpine:3.21
RUN apk add --no-cache nginx
ENTRYPOINT ["/usr/sbin/nginx", "-g", "daemon off;"]
```

```bash
umf build --tag local/hello:1.0 ./hello.umf   # in-process — no docker daemon
umf run local/hello:1.0                        # linked-in libcontainer runtime
umf images                                     # inspect the local OCI layout
```

Point `FROM` at a kernel artifact and the same tool emits a bootable OCI image (boot packaging is picked with a `LABEL org.imagilux.umf.flavor`: classic systemd-boot or UKI), which `umf compile` / `umf run` projects to a VM or bare-metal disk. See the [Quickstart](docs/quickstart.md) and [CLI Reference](docs/cli.md).

## Install

UMF is a single Linux binary (x86_64 / aarch64, glibc or musl).

```bash
# One-line installer: detects OS + arch, downloads the matching release
# tarball, verifies its SHA-256, installs to /usr/local/bin (root) or
# ~/.local/bin. Override with UMF_VERSION / UMF_LIBC / UMF_INSTALL_DIR.
curl -fsSL https://raw.githubusercontent.com/imagilux/umf/main/scripts/install.sh | sh
```

```bash
# Build + install from the git source with cargo (needs the stable
# toolchain + libseccomp-dev; see "from source" below).
cargo install --git https://github.com/imagilux/umf umf
```

```bash
# Container image: build it from the Containerfile at the repo root,
# then run the CLI without a local toolchain. umf drives micro-VMs and
# container namespaces, so it needs --privileged + /dev/kvm.
docker build -t umf:latest .                              # see Containerfile header for the binary it expects
docker run --rm --privileged --device /dev/kvm \
  -v "$PWD:/work" -w /work umf:latest build --tag local/app:1.0 .
```

Or the from-source `cargo build --release` path below. There is no pre-published image yet; the [Containerfile](Containerfile) header shows how to drop the release binary into the build context. Released binaries are checked against a published `SHA256SUMS`; see the [Quickstart](docs/quickstart.md#install) for the manual verify-and-install steps.

## Reference implementation

A Cargo workspace (edition 2024, stable toolchain pinned in `rust-toolchain.toml`):

- `umf` (`src/`) — the CLI (clap).
- `crates/umf-core` — shared types, errors, AST, the `org.imagilux.umf.*` label namespace.
- `crates/umf-parser` — UMF source → AST.
- `crates/umf-oci` — OCI primitives: manifest / config / layer emission, registry client, on-disk layout cache, archive import/export.
- `crates/umf-engine` — in-process container build + run (youki `libcontainer` + overlayfs), incl. NAT'd egress for RUN steps. No `docker build`, no host container daemon.
- `crates/umf-vmm` — VMM control layer: a `VmRuntime` trait with QEMU (QMP) and Cloud Hypervisor (REST) backends.
- `crates/umf-networking` — per-container NAT'd network egress for RUN net namespaces (veth over netlink + `nft` masquerade).
- `crates/umf-builder` — AST → OCI images: L0 introspection, FROM resolution, container lowering, bootable-image assembly.
- `crates/umf-compile` — projects a bootable OCI image into a GPT/ESP/UKI/squashfs disk (all userspace).

See [Architecture](docs/architecture.md) for how they fit together.

```bash
cargo build --release                                  # → target/release/umf
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

## Spec site — local preview

```bash
uv sync
uv run mkdocs serve      # http://127.0.0.1:8000
```

## License

Apache-2.0 — see [`LICENSE`](LICENSE). © 2026 Gaël THEROND / Imagilux.
