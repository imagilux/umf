# UMF — Universal Machine Format

Dockerfile-inspired DSL that uses OCI image mechanics (layered, content-addressable, registry-distributed) to produce bootable artifacts: VM disk images, bootc images, unikernels — and OCI containers as the degenerate case.

**This repository is documentation-only — by design, not by stage.** It hosts the UMF specification and the MkDocs site that publishes it. Nothing else lives here: no runtime, no parser, no builder, no reference implementation, no test fixtures, no example UMF source files outside the spec's illustrative snippets. Tooling and implementations live in separate repositories. Don't propose adding any of the above here, even as scaffolding.

- **Status**: draft spec v0.1 — published via MkDocs Material. `docs/index.md` is the overview/pitch landing page; `docs/compatibility.md` holds the per-target directive matrix; `docs/specification.md` holds the normative reference (build order, artifact resolution, directives); `docs/examples.md` holds workflow-oriented examples (building a kernel, rootfs, composing a VM, target variations).
- **Author**: Gaël THEROND / Imagilux

## Naming

UMF — *Universal* Machine Format. Matches the repo directory name `umf/` and is the canonical name throughout the spec. The label namespace is `org.imagilux.umf.*` — Imagilux is the operational entity that owns the spec and reference artifacts.

**Historical notes**:

- The project was originally framed as VMF (*Virtual* Machine Format) before the broader "Universal" framing took over. References to VMF or `org.vmf.*` predate the rename and should be migrated wherever encountered (old discussions, external mentions, downstream tooling).
- The label namespace was briefly `org.umf.*` (unscoped) before settling on `org.imagilux.umf.*` once the operational org was confirmed (Bitswalk is the holding parent; Imagilux is the operational entity for product work). Migrate any `org.umf.*` reference encountered to `org.imagilux.umf.*`.

## Design pillars

1. **One DSL, four targets** (VM / bootc / unikernel / container) — target is inferred from the combination of directives, not declared explicitly.
2. **OCI-native** — every component, including kernels and bootloaders, ships as an OCI artifact in a standard registry. A kernel artifact is itself a UMF build (`FROM scratch` + `KERNEL linux:x.y.z`); the system self-hosts.
3. **Sovereignty-first** — any artifact is buildable from source on an air-gapped node. Registries and caches accelerate but are never required.
4. **Composable supply chain** — components are independently versioned; the same `registry → local cache → source build` resolution applies to every component directive (KERNEL, BOOTLOADER, ROOTFS, …).

## Build order (strict, boot-driven — not author-driven)

```
L0  FIRMWARE + BOOTLOADER     boot partition (GPT/ESP)
L1  ROOTFS                    distro-specific base userland on root partition
L2  KERNEL                    pulled or built from kernel.org, installed to L1's /boot and /lib/modules
L3  INITRD                    always emitted after L1+L2 so it sees correct modules and userland
L4+ RUN / ADD / ENV / ...     user-space, Docker-equivalent caching semantics
```

## Directive groups

- **Boot chain** (only valid with `FROM scratch`): FIRMWARE, BOOTLOADER, ROOTFS, KERNEL, INITRD
- **Metadata**: FROM, LABEL, ENV, ARG
- **Build steps**: SHELL, USER, WORKDIR, RUN, ADD
- **Runtime config**: ENTRYPOINT, EXPOSE, ENABLE/DISABLE, HOSTNAME, LOCALE, TIMEZONE

## Storage model

Flat layout: a single ROOTFS partition for the system. Data volumes are mounted at runtime, never declared in the DSL. This preserves a clean SYS / DATA segregation and pushes any disk shaping (resize, additional volumes, encryption) to runtime tooling — cloud-init, ignition, or whatever the operator runs. LVM/RAID-era complexity is intentionally rejected; modern filesystems plus runtime provisioning cover the legitimate cases.

Build output is always a sparse image. There is no build-time disk-size declaration — consumers (hypervisors, Redfish endpoints) provision according to runtime configuration, not an in-image hint.

## Don't import Docker assumptions

These directives diverge from Docker semantics — relying on Docker intuition will mislead:

- **EXPOSE** emits actual nftables rules with **default-deny**. Not metadata. Only explicitly-exposed ports are reachable.
- **ENTRYPOINT** selects the init system / PID 1 (`systemd` / `openrc` / `binary` / `none`), not just an exec command. This is what makes the unikernel target work — `ENTRYPOINT binary` runs the executable directly, no init.
- **FROM image:tag** mutually excludes FIRMWARE, BOOTLOADER, KERNEL, INITRD, ROOTFS. Only `FROM scratch` unlocks the boot chain.
- **RUN** in VM-target mode executes inside a micro-VM booted from the current layer state, not in a container.

## Adopted Docker conventions (no surprises here)

These are intentionally Docker-shaped — reach for Docker intuition first.

- **Multi-stage**: `FROM <image> AS <name>` + `ADD --from=<name> <src> <dst>`. Each stage is an independent OCI image; the final stage's `FROM scratch` unlocks the boot chain when needed.
- **Cross-architecture**: `--platform=linux/<arch>` runtime flag (Buildx convention), not a DSL directive. Drives KERNEL / BOOTLOADER / INITRD resolution; `binfmt_misc` + qemu-user-static handle RUN steps.
- **Build secrets**: `RUN --mount=type=secret,id=<id>,target=<path> ...` for sensitive material (Secure Boot signing keys, pull credentials, etc.). Never via ARG. Secret is scoped to a single RUN, never enters a layer, never contaminates the cache key.

## Resolved design decisions

The spec's original "Open Questions" section was resolved and removed from the spec — resolutions below are authoritative. They don't alter any directive's semantics, so v0.1 stands as the finalized first iteration; no version bump. Future spec revisions should not re-introduce these as open.

- **No DISK directive.** Builds emit sparse images; runtime tooling handles disk-side shaping. No build-time size declaration, no LABEL hint either.
- **No PARTITION directive.** Flat ROOTFS for the system, additional volumes mounted at runtime for data. LVM and similar are intentionally out of scope — operators who need them get them via runtime provisioning, not the DSL.
- **Multi-stage builds**: Docker model verbatim (`FROM ... AS <name>` + `ADD --from=<name>`).
- **Cross-architecture**: runtime `--platform` flag, not a directive.
- **Cloud-init / Ignition**: no directive. Documented pattern using existing primitives — `ADD ./user-data /var/lib/cloud/seed/nocloud/user-data` + `ENABLE cloud-init.service`. Same shape for ignition. DSL stays agnostic of first-boot tooling.
- **Secure Boot key delivery**: BuildKit-style mounted secrets (see "Adopted Docker conventions" above). Documentation must cover both the sovereign air-gapped flow (operator-supplied local file) and the CI flow (secret manager mount).
- **UEFI-only.** BIOS / MBR is dropped. `FIRMWARE` accepts `uefi` and `uefi-secure` only; the boot partition is always GPT/ESP. Future spec revisions should not re-introduce a `bios` value.

## Repo layout

```
umf/
├── docs/
│   ├── index.md           # overview / pitch — what UMF is, principles
│   ├── compatibility.md   # per-target directive matrix + brief notes per target
│   ├── specification.md   # normative reference — build order, artifact resolution, directives
│   ├── examples.md        # workflow-oriented examples — kernel/rootfs/bootloader artifacts, VM composition, target variations
│   ├── assets/
│   │   ├── logo-light.svg # Imagilux wordmark for light scheme (ink-black IMAGILU + amber X)
│   │   └── logo-dark.svg  # Imagilux wordmark for dark scheme (white-smoke IMAGILU + amber X)
│   └── stylesheets/
│       └── imagilux.css   # Imagilux Design System overlay for mkdocs-material (tokens + Material variable bridge)
├── overrides/
│   └── partials/
│       └── logo.html      # mkdocs-material partial override — renders both logos, scheme-swapped via CSS
├── mkdocs.yml             # MkDocs Material config
├── pyproject.toml         # project metadata + Python deps (uv-managed)
├── uv.lock                # uv lockfile, committed for reproducibility
├── README.md              # repo-facing intro
├── .gitignore
└── .claude/
    └── CLAUDE.md          # this file
```

Docs-only repository (see scope statement at top). The Python deps in `pyproject.toml` exist solely to build the MkDocs site — they are not UMF runtime dependencies. Edit `docs/specification.md` for normative spec revisions; `docs/index.md` for pitch/explanation changes; `docs/compatibility.md` for matrix or per-target notes; `docs/examples.md` for workflow recipes.

## Python tooling — uv-native

This project uses **`uv`** for all Python work. Never reach for `pip`, `python -m venv`, `pip-tools`, `requirements.txt`, or `apt install python3-*` — uv covers venv creation, dependency resolution, locking, and command execution in one tool.

```bash
uv sync                # creates .venv if missing, installs from pyproject.toml + uv.lock
uv run mkdocs serve    # live preview at http://127.0.0.1:8000
uv run mkdocs build    # static site → site/
uv add <package>       # adds to pyproject.toml, updates uv.lock and .venv
uv remove <package>    # inverse of uv add
```

`pyproject.toml` is the single source of truth for declared dependencies. `uv.lock` is the resolved lockfile and **is committed**. `.venv/` and `site/` are gitignored.

Don't pre-emptively check for system Python packages — assume Python and uv are present on the host.

## Versioned docs — mike

Versions are managed by **mike** and surfaced in the header via mkdocs-material's built-in version selector (`extra.version.provider: mike` in `mkdocs.yml`). Each published version lives in its own subdirectory of the `gh-pages` branch (`gh-pages/0.1/`, `gh-pages/latest/`, …), so they are independently browsable and frozen at publish time.

```bash
uv run mike serve                                    # local preview WITH version switcher
uv run mike deploy --push --update-aliases 0.1 latest  # publish v0.1, alias /latest/ to it
uv run mike set-default --push latest                # make /latest/ the root redirect
uv run mike list                                     # show published versions
uv run mike delete 0.1                               # remove a published version
```

**Live site**: https://umf.imagilux.org/ (GitHub Pages, custom domain, HTTPS-enforced; serves the `gh-pages` branch of `imagilux/umf`).

**CI-driven publish**: `.github/workflows/deploy-docs.yml` runs `mike deploy` automatically on every push to `main` (excluding `.claude/`, `.github/`, `.gitignore`, `README.md`). Defaults to publishing `0.1 latest`; bump the workflow's `inputs.version.default` when cutting v0.2. Manual override available via the *Run workflow* button on the Actions tab. Prefer CI over local `mike deploy` to keep `gh-pages` linear.

**Convention**: tag major spec revisions with the spec version (`0.1`, `0.2`, `1.0`) plus the `latest` alias on whichever is current. Pre-release work-in-progress goes under `dev`. Once a version is published, treat it as immutable; corrections go into the next version, not amendments to a published one.
