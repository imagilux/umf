# UMF — Universal Machine Format

Dockerfile-inspired DSL that uses OCI image mechanics (layered, content-addressable, registry-distributed) to produce bootable artifacts — VM and bare-metal disk images, classic-boot or UKI-packaged — and OCI containers as the degenerate case.

This repository hosts two things, tightly coupled:

1. The **UMF specification** — normative source under `docs/`, published via MkDocs Material at <https://umf.imagilux.org/>.
2. The **UMF reference implementation** — Rust Cargo workspace: the `umf` CLI at the root (`src/main.rs`) and the library crates under `crates/` (`umf-core`, `umf-parser`, `umf-oci`, `umf-networking`, `umf-engine`, `umf-vmm`, `umf-builder`, `umf-compile`).

The spec is normative; the implementation is canonical. Both live in the same git history so that spec changes and the implementation that matches them ship together. They version independently: the spec is at v0.0.1, the binary at v0.0.1.

- **Status**: spec v0.0.1 (draft); reference implementation v0.0.1 — container target feature-complete (build / run / registry / OCI archives, all in-process). The unified **bootable** path is two steps: `umf build` emits an OCI image (`org.imagilux.umf.type=bootable`) and `umf compile` projects that image to a VM or bare-metal disk image (classic systemd-boot or UKI), with init-system or appliance (binary-`ENTRYPOINT`) PID 1. Container vs bootable is inferred purely from what `FROM` resolves to (see Design pillars); the boot chain has no dedicated directives. Real-kernel boot is validated end to end in CI: the boot-smoke lane builds a minimal kernel + busybox image, `umf compile`s it, and boots it under QEMU/KVM to a userspace marker. See `docs/known-limitations.md` for the remaining not-yet-supported paths.
- **Author**: Gaël THEROND / Imagilux

## Naming

UMF — *Universal* Machine Format. Matches the repo directory name `umf/` and is the canonical name throughout the spec. The label namespace is `org.imagilux.umf.*` — Imagilux is the operational entity that owns the spec and reference artifacts. Migrate any legacy `org.umf.*` (unscoped) or `org.vmf.*` / VMF reference encountered to `org.imagilux.umf.*` / UMF.

## Design pillars

1. **One DSL, two shapes — container and bootable** — inferred from what `FROM` resolves to, never declared. A *bootable* build is a plain OCI build whose `FROM` resolves to a kernel artifact (`org.imagilux.umf.type=kernel`); a base image or `scratch` makes it a container. The boot chain has **no** dedicated directives — it is expressed in stock OCI primitives: the userland is a bare `ADD <oci-ref> /`, and boot packaging is a normal `LABEL org.imagilux.umf.flavor` (`systemd-boot` = classic, `uki` = Unified Kernel Image; `grub` reserved). `ENTRYPOINT` then picks PID 1: an init system (`systemd`/`openrc`) ⇒ `/sbin/init` + generated initramfs; a binary path ⇒ *appliance* via `init=`. VM vs bare metal is not a build choice — one byte-identical UEFI disk boots either way. Container is the degenerate case: no kernel, no boot chain.
2. **OCI-native** — every component, including kernels, bootloaders, and build environments, ships as an OCI artifact in a standard registry. A kernel artifact is itself a UMF build that `FROM`'s a kernel-build-env (which is itself a UMF build); the system self-hosts at every level.
3. **Sovereignty-first** — any artifact is buildable from source on an air-gapped node. Registries and caches accelerate but are never required.
4. **Composable supply chain** — components are independently versioned; the same `registry → local cache → source build` resolution applies to every OCI reference: `FROM` and every `ADD <oci-ref>` (the userland base, formerly the `ROOTFS` directive).

## Build order (strict, boot-driven — not author-driven)

`umf build` lays down the image; `umf compile` later projects it to a disk. The boot chain is expressed in stock OCI primitives — no `FIRMWARE` / `BOOTLOADER` / `ROOTFS` directives exist.

```
L1  ADD <oci-ref> /           distro-specific base userland on the root tree (replaces the old ROOTFS directive)
L2  kernel install from FROM  FROM artifact (kernel OCI image) supplies vmlinuz + modules, installed onto L1
L3  initramfs (implicit)      generated when ENTRYPOINT is systemd/openrc; skipped for binary-path ENTRYPOINT (appliance)
L4+ RUN / ADD / ENV / ...     user-space, Docker-equivalent caching semantics
```

Disk projection (the old L0 — GPT/ESP boot partition, UEFI, classic systemd-boot or UKI) is no longer a build phase: it is performed by `umf compile`, which reads the `org.imagilux.umf.flavor` label plus the image's ordinary layers. Container builds skip L1–L3 entirely: `FROM <image>` (or `scratch`) supplies the starting filesystem and user-space layers stack on top.

## Directive groups

- **Boot chain (no dedicated directives)**: a bootable build `FROM`s a kernel artifact, lays down userland with `ADD <oci-ref> /`, and selects packaging with a `LABEL org.imagilux.umf.flavor`. The kernel comes from `FROM` (polymorphic — see L0 introspection); the initramfs is generated implicitly from `ENTRYPOINT` and the userland — there are no `FIRMWARE`, `BOOTLOADER`, `ROOTFS`, `KERNEL`, or `INITRD` directives. What `FROM` resolves to (a `type=kernel` artifact) is what marks a build bootable.
- **Metadata**: FROM, LABEL, ENV, ARG
- **Build steps**: SHELL, USER, WORKDIR, RUN, ADD
- **Runtime config**: ENTRYPOINT, EXPOSE, ENABLE/DISABLE, HOSTNAME, LOCALE, TIMEZONE

## Storage model

Flat layout: a single root-filesystem partition for the system. Data volumes are mounted at runtime, never declared in the DSL. This preserves a clean SYS / DATA segregation and pushes any disk shaping (resize, additional volumes, encryption) to runtime tooling — cloud-init, ignition, or whatever the operator runs. LVM/RAID-era complexity is intentionally rejected; modern filesystems plus runtime provisioning cover the legitimate cases.

Build output is always a sparse image. There is no build-time disk-size declaration — consumers (hypervisors, Redfish endpoints) provision according to runtime configuration, not an in-image hint.

## Don't import Docker assumptions

These directives diverge from Docker semantics — relying on Docker intuition will mislead:

- **EXPOSE** emits actual nftables rules with **default-deny**. Not metadata. Only explicitly-exposed ports are reachable.
- **ENTRYPOINT** is polymorphic: `systemd` / `openrc` selects an init system (bootable build — VM or bare metal) and triggers implicit L3 initramfs generation; a binary path (`/myapp` or exec form `["/usr/sbin/nginx", "-g", "daemon off;"]`) runs the executable directly as PID 1 (appliance in a bootable build — `init=<path>`, no initramfs; plain entrypoint in a container otherwise); `none` skips PID 1 entirely (container where the runtime supplies init). Bare-string keywords without a leading `/` are reserved for the init-system values.
- **FROM** is polymorphic and structurally introspected — and it is the *single source of truth* for the build's shape. The builder resolves `FROM` (registry → cache), reads `org.imagilux.umf.type` (or infers from manifest structure), and shapes the build accordingly: resolves to `type=kernel` ⇒ bootable (vmlinuz + modules installed at L2, RUN in a micro-VM); resolves to a container base or `scratch` ⇒ container (RUN in a container). A `type=bootable` image **is** a valid FROM — extend it like any base and the result stays bootable. The old `type=vm` no longer exists: a projected disk is never an OCI artifact. Mismatches (e.g. a bootable-only `flavor` label on a container base) are rejected at build start. There is no per-directive "mode" — every polymorphism lives on FROM.
- **RUN** execution depends on the resolved shape: container build (FROM a base / `scratch`) → container, bootable build (FROM a kernel) → micro-VM booted from the current layer state. Not a per-target switch — derived from what `FROM` resolves to.

## Adopted Docker conventions (no surprises here)

These are intentionally Docker-shaped — reach for Docker intuition first.

- **Multi-stage**: `FROM <image> AS <name>` + `ADD --from=<name> <src> <dst>`. Each stage is an independent OCI image; the final stage's `FROM` selects the shape. (`--from` is stage-only; an external OCI image is laid down with a *bare* `ADD <oci-ref> /`, not `--from`.)
- **Cross-architecture**: `--platform=linux/<arch>` runtime flag (Buildx convention), not a DSL directive. Drives every component-resolution decision (the kernel and userland images pulled via `FROM` / `ADD` must match the target arch); `binfmt_misc` + qemu-user-static handle RUN steps in container mode.
- **Build secrets**: `RUN --mount=type=secret,id=<id>,target=<path> ...` for sensitive material (Secure Boot signing keys, pull credentials, etc.). Never via ARG. Secret is scoped to a single RUN, never enters a layer, never contaminates the cache key.

## Reference implementation (v0.0.1)

The `umf` CLI is daemonless and OCI-native end to end. Subcommands, grouped:

- **Authoring / build**: `parse`, `build` — always emits an OCI image (`--tag`, `--push`, `--secret`, `--metrics`); the shape is inferred from what `FROM` resolves to (a `type=kernel` base produces a `type=bootable` image, otherwise a container image). There is no `--disk-out`.
- **Project**: `compile` — projects a `type=bootable` OCI image to a VM or bare-metal disk image (GPT/ESP, UEFI; classic systemd-boot or UKI), reading the `org.imagilux.umf.flavor` label plus the image's layers. This is the former bootable `--disk-out` path, now a separate step.
- **Run**: `run` — container via linked-in libcontainer (`-i`, `-e`, `--entrypoint`, CMD override); VM via `--vmm=qemu|ch --disk <img>`.
- **Layout / distribution**: `images` (list / `--remove` / `--prune` — `rmi` was folded in here), `index` (compose a multi-arch image index from already-built per-arch images), `push`, `pull`, `save`, `load` (OCI Image Layout archives, skopeo/`docker save`-compatible).
- **Introspection / ops**: `inspect`, `ps` (process registry under `$XDG_STATE_HOME/umf/processes/`), `doctor` (host-runtime check).
- **Dev tooling**: `debug build` (directive-by-directive step-through), `bench` (cold + N warm, cache-determinism flags).

Global flags on every subcommand: `--layout-dir` (OCI layout cache, default `$XDG_CACHE_HOME/umf/oci-layout`) and `--trace-format` / `--trace-output` / `--trace-level` (structured tracing, sugar over `RUST_LOG`). Container builds and runs are fully in-process — no host `docker` / `podman`. Full reference in `docs/cli.md`; internals in `docs/architecture.md`.

## Repo layout

```
umf/
├── docs/                        # MkDocs source — spec + reference-implementation docs
│   ├── index.md                 # overview / pitch + impl status
│   ├── quickstart.md            # install → build → run (operator getting-started)
│   ├── compatibility.md         # per-target directive matrix
│   ├── specification.md         # normative reference — build order, resolution, directives
│   ├── examples.md              # workflow recipes
│   ├── cli.md                   # CLI reference — every umf command + flag
│   ├── architecture.md          # reference-implementation internals (crate tree, pipelines)
│   ├── prerequisites.md         # host-setup checklist (kernel features, binaries, KVM)
│   ├── troubleshooting.md       # failure messages → causes → fixes
│   ├── known-limitations.md     # not-yet-supported paths + spec-vs-impl gaps
│   ├── examples/                # recipes for the tutorial artifacts (kernel, kernel-build-env, rootfs)
│   ├── assets/
│   │   ├── logo-light.svg
│   │   └── logo-dark.svg
│   └── stylesheets/
│       └── imagilux.css         # IDS overlay for mkdocs-material
├── overrides/
│   └── partials/
│       └── logo.html            # mkdocs-material partial override
├── mkdocs.yml                   # MkDocs Material config
├── pyproject.toml               # Python deps (uv-managed, MkDocs only)
├── uv.lock
├── src/                         # Rust — `umf` CLI binary
│   ├── main.rs                  # thin entrypoint → cli::run
│   ├── cli/                     # clap parsing + per-subcommand dispatch (build, compile, run, images, index, push/pull, save/load, inspect, ps, doctor, debug, bench)
│   └── render.rs                # `umf parse` table renderer
├── crates/                      # Rust — library crates (umf- prefix: avoids std::core shadow + ready for crates.io publication)
│   ├── umf-core/                # shared types, errors, AST, OCI label namespace
│   │   ├── Cargo.toml
│   │   └── src/
│   ├── umf-parser/              # UMF source → AST
│   │   ├── Cargo.toml
│   │   └── src/
│   ├── umf-oci/                 # OCI primitives — manifest/config/layer emission, registry client, layout cache, materialize, staging, archive import
│   │   ├── Cargo.toml
│   │   └── src/
│   ├── umf-networking/          # NAT'd egress for container RUN steps — veth pair (rtnetlink) + host nft masquerade
│   │   ├── Cargo.toml
│   │   └── src/
│   ├── umf-engine/              # container build engine — libcontainer RUN executor, OCI bundle prep, overlayfs lower/upper capture, RUN-step egress (umf-networking); also powers umf run (container target)
│   │   ├── Cargo.toml
│   │   └── src/
│   ├── umf-vmm/                 # VMM control layer — VmRuntime trait + QEMU/QMP & Cloud Hypervisor/REST backends; powers umf run (VM) + per-RUN micro-VMs
│   │   ├── Cargo.toml
│   │   └── src/
│   ├── umf-builder/             # AST → OCI image — FROM resolution + introspection, RUN backends (container / micro-VM), secrets, EXPOSE→nftables, boot-manifest labels
│   │   ├── Cargo.toml
│   │   └── src/
│   └── umf-compile/             # disk projection — type=bootable OCI image → GPT/ESP/UEFI disk (squashfs root, systemd-boot or UKI); powers umf compile
│       ├── Cargo.toml
│       └── src/
├── releasenotes/                # reno release notes (notes/*.yaml, aggregated by tag)
├── scripts/                     # installer + maintenance scripts (install.sh, make-boot-fixture.sh, regen-ch-client.sh, …)
├── bench/                       # benchmark fixtures / recipes for `umf bench`
├── tests/                       # CLI integration tests
├── Cargo.toml                   # workspace manifest + root binary package
├── Cargo.lock                   # committed (this repo ships a binary)
├── Cross.toml                   # cross-rs config for the musl/aarch64 release builds
├── Containerfile                # container image wrapping the released umf binary
├── deny.toml                    # cargo-deny policy (licenses / advisories / bans)
├── rust-toolchain.toml          # pins stable + rustfmt + clippy
├── rustfmt.toml
├── README.md
├── CHANGELOG.md                 # pointer to the reno-generated notes — don't hand-edit
├── CONTRIBUTING.md
├── SECURITY.md
├── .gitignore
├── .github/
│   └── workflows/
│       ├── rust.yml             # CI — build / test / clippy / fmt on push + PR to main
│       ├── boot-smoke.yml       # CI — real-kernel boot-smoke: build → compile → QEMU/KVM boot → assert userspace
│       ├── oci-conformance.yml  # CI — OCI image-spec conformance gate (JSON schema + skopeo/crane)
│       ├── privileged.yml       # CI — privileged lane: rootful libcontainer RUN, NAT egress, VM boot
│       ├── release.yml          # binary GitHub Release on `vX.Y.Z` tags
│       └── deploy-docs.yml      # MkDocs publish on `spec-vX.Y[.Z]` tags
└── .claude/
    └── CLAUDE.md                # this file
```

Edit `docs/specification.md` for normative spec revisions; `docs/index.md` for pitch/overview changes; `docs/compatibility.md` for matrix or per-target notes; `docs/examples.md` for workflow recipes; `docs/quickstart.md` / `docs/cli.md` / `docs/architecture.md` / `docs/prerequisites.md` / `docs/troubleshooting.md` / `docs/known-limitations.md` for the reference-implementation (tool) docs. The `crates/` libraries and `src/` hold the reference implementation — touch those for behavior, not spec wording.

## Python tooling — uv-native (for the spec site)

The MkDocs site uses **`uv`** for all Python work. Never reach for `pip`, `python -m venv`, `pip-tools`, `requirements.txt`, or `apt install python3-*` — uv covers venv creation, dependency resolution, locking, and command execution in one tool.

```bash
uv sync                # creates .venv if missing, installs from pyproject.toml + uv.lock
uv run mkdocs serve    # live preview at http://127.0.0.1:8000
uv run mkdocs build    # static site → site/
uv add <package>       # adds to pyproject.toml, updates uv.lock and .venv
uv remove <package>    # inverse of uv add
```

`pyproject.toml` is the single source of truth for declared Python dependencies. `uv.lock` is the resolved lockfile and **is committed**. `.venv/` and `site/` are gitignored.

Don't pre-emptively check for system Python packages — assume Python and uv are present on the host.

## Release notes — reno

Binary-release notes are managed with **[`reno`](https://docs.openstack.org/reno/)**, a `uv`-managed dev dependency. Each change worth a line in the next release adds one YAML note under `releasenotes/notes/`; reno aggregates them by git tag.

```bash
uv run reno new <slug>     # create releasenotes/notes/<slug>-<hash>.yaml
uv run reno report         # render accumulated notes, grouped by tag
```

A note is RST-bodied YAML using reno's standard sections (`prelude`, `security`, `features`, `fixes`, `upgrade`, `deprecations`, `other`). Notes committed after the last `vX.Y.Z` tag belong to the next release; tagging freezes them into that version. Add a note **with** the change that warrants it rather than batching at release time.

## Rust tooling — cargo-native (for the reference implementation)

The implementation is a **Cargo workspace** in hybrid layout: the `umf` CLI sits at the repo root (`src/main.rs`) and the library crates live under `crates/` as `umf-core`, `umf-parser`, `umf-oci`, `umf-networking`, `umf-engine`, `umf-vmm`, `umf-builder`, and `umf-compile`. Each library has its own dependency footprint and is consumed by the CLI through workspace deps.

```bash
cargo check --workspace                                     # quick type-check across all crates
cargo build --workspace                                     # debug build
cargo build --release                                       # optimized binary at target/release/umf
cargo test --workspace                                      # all tests
cargo fmt --all                                             # format
cargo clippy --workspace --all-targets -- -D warnings       # lint (deny warnings)
cargo run -- <args>                                         # run the CLI
```

**Edition**: 2024. **Toolchain**: `stable` pinned via `rust-toolchain.toml` with `rustfmt` and `clippy` — rustup auto-installs on first invocation. **License**: Apache-2.0 (single license; the full text lives in `LICENSE` at the repo root with the standard appendix and a `Copyright 2026 Gaël THEROND / Imagilux` notice). **Cargo.lock**: committed (this is a binary, not a published library — reproducibility wins).

**Shared dependency policy**: declare common deps once under `[workspace.dependencies]` in the root `Cargo.toml`, then opt-in per crate via `dep = { workspace = true }`. Same for `[workspace.lints]` — lint policy is workspace-wide. Don't pin a dep in two places.

**Library boundaries** (the dependency graph is a strict tree — no cycles):

- `umf-core` — shared types, errors, AST node definitions, the `org.imagilux.umf.*` label namespace constants. No external IO. Depended on by everything else; depends on nothing.
- `umf-parser` — `&str` → AST. Lexer + grammar + diagnostics. Depends only on `umf-core`. Future LSP / linting tools depend on this without pulling in builder dependencies.
- `umf-oci` — OCI primitives. Image emission (manifest / config / layer blobs into a layout), on-disk image-layout cache, registry client (pull/push, distribution protocol), layer materialization (whiteouts, traversal containment), build staging directory, OCI Image Layout archive import/export. Depends only on `umf-core`.
- `umf-networking` — NAT'd egress for container RUN steps. Builds a veth pair + addresses + routes in-process via `rtnetlink` (NETLINK_ROUTE) and applies a host-side NAT masquerade rule via the `nft` binary. No internal umf deps. (`rustables` is deliberately avoided — it SIGABRTs on send under `nix` ≥ 0.27.)
- `umf-engine` — UMF-native container build + run engine. youki/`libcontainer`-backed RUN executor, OCI bundle preparation from a pulled image, overlayfs setup with stackable lowers + captured upper-dir, per-RUN egress wired through `umf-networking`. Powers both container builds and `umf run`'s container path. Depends on `umf-core` + `umf-oci` + `umf-networking`. Runs in-process — no `docker build`, no host docker/podman dependency.
- `umf-vmm` — VMM control layer. The `VmRuntime` trait plus QEMU (QMP) and Cloud Hypervisor (REST) backends. Powers both `umf run`'s VM boot path and the builder's per-RUN micro-VMs (bootable build). A pure control surface — no internal umf deps.
- `umf-builder` — AST → OCI image. FROM resolution + introspection (the container-vs-bootable decision), container-target lowering through `umf-engine`, micro-VM RUN execution through `umf-vmm` (bootable build), boot-manifest labels, EXPOSE→nftables generation, secrets handling. Emits the OCI image (container or `type=bootable`); disk projection lives in `umf-compile`. Depends on `umf-core` + `umf-oci` + `umf-engine` + `umf-vmm`; deliberately does **not** depend on `umf-parser`.
- `umf-compile` — disk projection. Takes a `type=bootable` OCI image and emits a bootable disk: GPT partition table + ESP/FAT32 (`gpt`, `fatfs`), squashfs root (`backhand`), and the boot chain (UKI or systemd-boot per the `flavor` label), with symlink-contained reads of in-image boot files. Powers `umf compile`. Depends on `umf-core` + `umf-oci`.
- `umf` (binary, in `src/`) — CLI wiring. Depends on `umf-core`, `umf-parser`, `umf-oci`, `umf-engine`, `umf-vmm`, `umf-builder`, `umf-compile` (and `umf-networking` transitively through `umf-engine`).

The AST lives in `umf-core`, not in `umf-parser`, deliberately: it keeps `umf-builder` / `umf-oci` / `umf-engine` / `umf-vmm` / `umf-networking` / `umf-compile` free of any parser dependency and lets the layers evolve independently.

**Naming**: workspace crates use the **`umf-`** prefix (`umf-core`, `umf-parser`, `umf-oci`, `umf-networking`, `umf-engine`, `umf-vmm`, `umf-builder`, `umf-compile`). The prefix avoids shadowing `std::core` in clap's derive macro and is a precondition for publishing the libraries independently under the `imagilux` umbrella on crates.io.

## Versioned docs — mike

Versions are managed by **mike** and surfaced in the header via mkdocs-material's built-in version selector (`extra.version.provider: mike` in `mkdocs.yml`). Each published version lives in its own subdirectory of the `gh-pages` branch (`gh-pages/0.5/`, `gh-pages/latest/`, …), so they are independently browsable and frozen at publish time.

```bash
uv run mike serve                                      # local preview WITH version switcher
uv run mike deploy --push --update-aliases 0.0.1 latest  # publish spec 0.0.1, alias /latest/ to it
uv run mike set-default --push latest                  # make /latest/ the root redirect
uv run mike list                                       # show published versions
uv run mike delete 0.0.1                               # remove a published version
```

**Live site**: https://umf.imagilux.org/ (GitHub Pages, custom domain, HTTPS-enforced; serves the `gh-pages` branch of `imagilux/umf`).

**CI-driven publish (tag-triggered)**: `.github/workflows/deploy-docs.yml` deploys on git-tag push matching `spec-vX.Y` or `spec-vX.Y.Z`. The tag name (minus the leading `spec-v`) becomes the mike version slot and `latest` is repointed to it. The binary is released independently by `release.yml` on bare `vX.Y.Z` tags — the two release tracks are intentionally decoupled. `main` is a continuous development branch — pushes to main do *not* publish; cut a tag to release. Manual override (re-publish, backport, off-schedule) is available via the *Run workflow* button with explicit version/alias inputs. Prefer CI over local `mike deploy` to keep `gh-pages` linear.

**Cutting a release**:

```bash
# spec docs (mkdocs/mike) — from main, after the spec content is ready
git tag spec-v0.0.1 && git push origin spec-v0.0.1   # publishes spec 0.0.1 + repoints latest
# binary release (GitHub Release, gnu/musl × aarch64/x86_64) — independent track
git tag v0.0.1 && git push origin v0.0.1             # builds + publishes the binary
```

**Convention**: tag spec revisions `spec-vX.Y[.Z]` (the mike slot is the version minus the `spec-v` prefix) plus the `latest` alias on whichever is current; tag binary releases `vX.Y.Z`. Pre-release work-in-progress goes under `dev`. Once a version is published, treat it as immutable; corrections go into the next version, not amendments to a published one.
