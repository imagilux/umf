# Architecture

How the UMF reference implementation (**v0.0.1**) is built — for contributors, and for operators who want to know what runs where. The [Specification](specification.md) defines the DSL itself and versions independently from the binary; this page tracks the implementation.

## Workspace

A single `umf` binary on a strict, acyclic tree of library crates (the `umf-` prefix keeps them independently publishable and avoids shadowing `std::core` in clap's derive macro):

```text
umf-core            shared types, errors, AST, org.imagilux.umf.* label constants — no IO
 ├─ umf-parser      &str → AST: lexer, grammar, diagnostics (depends only on core)
 ├─ umf-oci         OCI primitives: manifest/config/layer emission, registry client
 │   │              (pull/push), on-disk layout cache, build staging, archive import/export
 │   └─ umf-engine  in-process container build + run: libcontainer RUN executor,
 │                  OCI bundle prep, overlayfs lower/upper capture, RUN-step egress
 ├─ umf-vmm         VMM control layer: VmRuntime trait + QEMU (QMP) & Cloud Hypervisor
 │                  (REST) backends — pure control surface, no internal umf deps
 ├─ umf-networking  per-container NAT'd egress for RUN net namespaces (veth over
 │                  rtnetlink + nft masquerade), plus host-side cloud-hypervisor
 │                  VM port-forwarding (netns + tap + nft DNAT + pluggable DHCP);
 │                  no internal umf deps; used by umf-engine (egress) + umf run (VM net)
 ├─ umf-compile     disk projector: a type=bootable OCI image → a GPT/ESP/UKI/squashfs
 │                  disk, all userspace (gpt/fatfs/backhand), driven by the boot
 │                  manifest (depends on core + oci; no parser, no engine)
 └─ umf-builder     AST → OCI images: L0 introspection, FROM resolution, container
                    lowering (via umf-engine), bootable-image assembly + micro-VM RUN
                    (via umf-vmm), nftables rules, secrets
umf (src/)          CLI: clap parsing + per-subcommand dispatch + output renderers
```

Two deliberate choices keep the tree clean:

- **The AST lives in `umf-core`, not `umf-parser`.** So `umf-builder`, `umf-oci`, and `umf-engine` carry no parser dependency, and `umf-builder` consumes an already-parsed AST handed to it by the CLI.
- **`umf-vmm` has no internal UMF dependencies.** It's a pure VMM control surface — callers feed it a spec and drive the resulting handle through the `VmRuntime` trait. Both the build path (per-RUN micro-VMs) and the run path (booting a finished image) reuse it instead of scattering `qemu-system-*` invocations.

## Build → image, compile → disk

The build shape is inferred from the recipe — whether `FROM` resolves to a `type=kernel` artifact is the switch — and `umf-builder` lowers the AST to a **plain OCI image** either way. A bootable image is projected to a disk afterward by `umf-compile`.

**Container build** (`FROM` a base image / `scratch`): the starting filesystem plus RUN / ADD / ENV / … stacked as user-space layers, all in-process through `umf-engine`.

**Bootable build** (`FROM` a `type=kernel` artifact): the boot-driven build order assembles the rootfs, plus an `org.imagilux.umf.*` boot manifest describing it —

```text
L1  ADD <oci-ref> /           base userland from a stock OCI image
L2  kernel install from FROM  FROM's boot/vmlinuz-* + lib/modules/* land on L1
L3  initramfs (implicit)      generated when ENTRYPOINT is systemd/openrc;
                              skipped for a binary-path ENTRYPOINT (init=, appliance)
L4+ RUN / ADD / ENV / …       user-space; RUN runs in a micro-VM booted from
                              the current layer state
```

The result carries `type=bootable`: runnable as a container, pushable like any image, and a valid `FROM` to extend. **No disk yet** — `umf-compile` reads the boot manifest, materializes the rootfs, and shapes the GPT/ESP boot partition (the L0 step, deferred to compile time) — a classic bootloader entry or a UKI, plus the squashfs root — writing a local block (never an OCI artifact). `umf run` runs this projection automatically before booting.

Each filesystem-modifying directive becomes one content-addressed layer; re-builds reuse layers whose input hash (directive text + input context) is unchanged — Docker-equivalent caching semantics. Compiled blocks are likewise content-addressed (image digest + geometry), so a repeat compile / run is a cache hit.

## The container engine (in-process)

`umf-engine` is what lets container builds and runs need **no host container CLI** — no `docker`, no `podman`, no daemon. It embeds youki's `libcontainer`:

- **Bundle prep** turns a pulled OCI image into a runnable bundle (rootfs directory + `config.json`), bind-mounting the host's DNS config (read-only, never captured) so name resolution works inside the step.
- **overlayfs** stacks the image layers as lowers and captures a RUN step's writes in an upper-dir, which `umf-builder` packs into the next layer.
- **`LibcontainerRuntime`** executes the step — rootless by default, with uid/gid mapping derived from the caller. A `NoopRuntime` backs dry-runs and tests.
- **Network egress** — each RUN step runs in its own network namespace (never the host's). `umf-networking` wires it out through the host between container create and start: a veth pair (a per-container `/30`) plus an `nft` masquerade rule, so `apt` / `git` / `curl` resolve and reach the network. It's best-effort and torn down with the step; a build whose RUN steps don't touch the network is unaffected. `umf doctor` reports the host's `ip_forward` and FORWARD-policy state, which UMF can't override.

`umf run` reuses the same engine: it translates an image's ENTRYPOINT + CMD (plus CLI overrides) into a run spec and drives libcontainer end-to-end.

## The VMM layer (dual backend)

`umf-vmm` exposes one `VmRuntime` trait with two backends behind it:

- **QEMU** — `qemu-system-<arch>`, controlled post-spawn over [QMP](https://qemu.readthedocs.io/en/latest/interop/qmp-intro.html) via the typed `qapi` crate (boot-ready detection, status queries, graceful shutdown).
- **Cloud Hypervisor** — controlled over its OpenAPI REST socket via the generated `cloud-hypervisor-client`. Rust-native, faster boot; needs an explicit firmware path.

`umf run` boots a bootable image by auto-compiling it to a disk (via `umf-compile`) and handing that to a backend — or `--disk <img>` boots a raw disk directly; the same crate backs the builder's per-RUN micro-VMs for bootable-build RUN steps.

## OCI everywhere

Every component — base images, kernels, kernel-build-envs, rootfs, bootloaders — is an ordinary OCI artifact, and every reference (`FROM`, `ADD <oci-ref>`) resolves through one chain:

```text
registry  →  local layout cache  →  build from source
```

`umf-oci` owns this: a v2 registry client (pull / push), an on-disk **image-layout cache** (default `$XDG_CACHE_HOME/umf/oci-layout`, overridable with `--layout-dir`), and OCI Image Layout archive import/export (`umf save` / `load`, round-tripping with skopeo and `docker save`). When the cache already holds a digest, the resolver short-circuits the pull — which is what lets a pre-warmed, air-gapped node build with no network at all (see [Examples → Air-gapped operation](examples.md#air-gapped-operation)).

An optional **erofs lower-layer cache** sits alongside the layout: `umf-oci` encodes cached lower layers as erofs images (content-addressed on their `diff_id`) and `umf-engine` mounts them read-only as overlayfs lowers, which speeds warm rebuilds. It shells out to `mkfs.erofs` when that binary is present and falls back to a pure-Rust unpack when it isn't, so it is pure acceleration and never a requirement (`umf images --prune` garbage-collects it).

Artifacts self-describe via the `org.imagilux.umf.type` label (`container` / `kernel` / `rootfs` / `bootloader` / `bootable`); the builder reads it during **L0 introspection** to validate `FROM` against the companion directives (see [L0 Introspection](specification.md#l0-introspection)).

On top of OCI 1.1 referrers, UMF also carries a supply-chain surface: `umf sbom` (attach or generate an SPDX / CycloneDX document), `umf sign` (cosign-compatible signatures), and `umf attest` (in-toto / SLSA predicates in a signed DSSE envelope) all attach as referrer artifacts of an image. See the [CLI Reference](cli.md#supply-chain) for the commands.

## Daemonless, with a process registry

There is no UMF daemon. Each `umf build` / `umf run` does its work in-process and writes a record under `$XDG_STATE_HOME/umf/processes/`. `umf ps` reads that registry, so finished builds and runs stay visible as history — `docker ps -a` ergonomics without a background service. `umf ps --prune` trims finished records.

## Observability

- **Tracing** — structured spans across the whole pipeline (`--trace-format text|json|pretty`, `--trace-level`, sugar over `RUST_LOG`). `json` is pipeable to `jq` / Loki / Honeycomb.
- **Per-build metrics** — `umf build --metrics text|json` reports timings, layer count, and total bytes; `json` is CI-friendly.
- **Benchmarking** — `umf bench` runs a recipe cold + N warm and reports median / p99 / min / max plus cache-determinism invariants.

## Spec vs. implementation versioning

The **spec** (this site's normative pages) and the **binary** version independently: the spec is at `v0.0.1`, the reference implementation at `v0.0.1`. The docs site is mike-versioned by spec version; binary releases are tagged separately and carry their own reno-managed release notes. Both ship from the same git history, so a spec change and the implementation that matches it land together.
