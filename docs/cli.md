# CLI Reference

Every `umf` subcommand in the **v0.0.1** reference implementation. This documents the tool; for the DSL it builds, see the [Specification](specification.md). `umf <command> --help` is always the authoritative, current flag list.

UMF is **daemonless** — no background service. Each invocation does its work in-process and records itself in a process registry (see [`umf ps`](#umf-ps)). The container build/run engine is **linked in** (youki's `libcontainer`); there is no `docker` / `podman` dependency on the host.

## Global flags

Available on every subcommand:

| Flag | Purpose |
|------|---------|
| `--layout-dir <PATH>` | Override the on-disk OCI layout. Default `$XDG_CACHE_HOME/umf/oci-layout` (`~/.cache/umf/oci-layout`). Used by build / run / images / push / pull / save / load / inspect / debug; `bench` uses it as the parent for its per-bench tempdir; `parse` and `doctor` ignore it. |
| `--rootless-net <none\|native\|pasta>` | Rootless RUN-step egress backend. `native` (default): in-process userspace TCP/IP stack (smoltcp, no external binary, works air-gapped). `pasta`: external `passt`/`pasta` helper (requires the `passt` package). `none`: loopback only, no outbound traffic. Env var: `UMF_ROOTLESS_NET` (flag wins). Only affects rootless builds; privileged builds use the veth + NAT path regardless. |
| `--rootless-net-allow <CATS>` | Re-allow host-internal address categories for the `native` backend's SSRF policy (comma or space-separated). Categories: `loopback`, `link-local`, `rfc1918`, `ula`, `cgnat`. Example: `--rootless-net-allow rfc1918` to reach an internal package mirror. A malformed value fails closed. Env var: `UMF_ROOTLESS_NET_ALLOW` (flag wins). Ignored by the `pasta` and `none` backends. |
| `--trace-format <text\|json\|pretty>` | Trace/log shape. `text` (default), `json` (one object per span event — pipe to `jq` / Loki / Honeycomb), `pretty` (tree-shaped). |
| `--trace-output <stderr\|stdout\|PATH>` | Where traces go. Default `stderr`. |
| `--trace-level <trace\|debug\|info\|warn\|error>` | Level filter; sugar over `RUST_LOG` (which wins when set). |

## Recipe input

`build`, `parse`, `debug build`, and `bench` resolve the recipe Docker-style (`doctor` does too, for scoping):

| Form | Behaviour |
|------|-----------|
| `umf build <dir>` / `umf build` (no path) | Discover `Containerfile`, then `Dockerfile`, inside the directory (default `.`); that directory is the build context. |
| `umf build <file>` | Use that file as the recipe — any name; the `.umf` extension is a convention, not a requirement. Context is the file's directory. |
| `umf build -f <file> [dir]` | `-f` / `--file` points at a recipe of any name anywhere, bypassing discovery; the positional then names the build-context directory (mirrors `docker build -f`). |

## Authoring

### `umf parse`

`umf parse [--format table|json|debug] [-f <PATH>] [PATH]`

Parse a recipe and print the AST without building (recipe resolution as in [Recipe input](#recipe-input)). `table` (default) is a per-directive summary; `json` includes spans, for tooling/IDEs; `debug` is the raw Rust representation.

```bash
umf parse .
umf parse --format json . | jq '.directives[].kind'
```

### `umf build`

`umf build [OPTIONS] [-f <PATH>] [PATH]`

Build a recipe into a **plain, layered OCI image** — container and bootable alike. The shape is **inferred** from `FROM`: a kernel artifact → a bootable image (`org.imagilux.umf.type=bootable`); a base image or `scratch` → a container. `build` never emits a disk — a bootable image is projected to one later by [`umf compile`](#umf-compile).

| Flag | Purpose |
|------|---------|
| `-f` / `--file <PATH>` | Recipe of any name, anywhere — bypasses `Containerfile`/`Dockerfile` discovery; the positional then names the context dir. See [Recipe input](#recipe-input). |
| `--tag <REF>` | Reference the image is registered under. **Required** — container and bootable both emit an OCI image. |
| `--push` | Push to the registry implied by `--tag` after building. Works for bootable images too — they are ordinary OCI. |
| `--insecure-registry` | Allow plain-HTTP push (local `registry:2`). |
| `--username <NAME>` / `--password-stdin` | Registry credentials (else env vars → `~/.docker/config.json`, including `credsStore`/`credHelpers` credential helpers). |
| `--secret id=<id>,src=<path>` *or* `id=<id>,env=<NAME>` | BuildKit-style build secret, bind-mounted at `/run/secrets/<id>` for RUN steps that opt in via `RUN --mount=type=secret`. Never enters a layer. Repeatable. (Container builds.) |
| `--platform <os/arch>` | Bootable-target preflight today (`qemu-system-<arch>` detection); cross-arch container builds are a follow-up. |
| `--compression <gzip\|zstd>` | Layer codec for everything this build packages. `gzip` (default) interoperates everywhere; `zstd` emits the OCI 1.1 `tar+zstd` media type — smaller and faster to decode, but the consumer must understand it. Step-cache entries are keyed per codec, so switching repackages layers instead of reusing the other codec's. |
| `--metrics <text\|json\|none>` / `--metrics-output <PATH>` | Per-build summary (timings, layer count, total bytes). `text` to stderr (default), `json` for CI. **Container builds only**: a bootable build emits no metrics report, and `--metrics-output` on one is rejected (*--metrics-output is only meaningful for container builds*). |

Bootable builds take the kernel from `FROM` and the rootfs from `ADD <oci-ref> /` in the recipe, both resolved through `registry → cache` (offline = already in the local OCI layout). There are no host-side override flags. One extra build flag is bootable-specific:

| Flag | Purpose |
|------|---------|
| `--staging-keep <PATH>` | Persist the unpacked staging tree after the build (debugging). |

> Disk geometry (`--disk-size`, `--esp-size`) and bootloader selection are **projection** concerns — they live on [`umf compile`](#umf-compile), not `build`.

```bash
umf build --tag local/app:1.0 .
umf build --tag reg.example.com/app:1.0 --push .
umf build --secret id=signkey,src=./key.pem --tag local/app:1.0 .
umf build -f recipes/web.umf --tag local/app:1.0 .   # explicit recipe of any name + context dir
umf build --tag local/appliance:1.0 .                # bootable (FROM a kernel) → type=bootable OCI image
```

## Compiling

### `umf compile`

`umf compile [OPTIONS] <REFERENCE>`

Project a bootable-OS image (`type=bootable`) into a bootable **disk**. `build` produces the relocatable object (the OCI image); `compile` links it into a target-specific executable (the disk) — reading the image's boot manifest (`org.imagilux.umf.*` labels) to lay down GPT + ESP, a classic bootloader entry or a UKI, and the squashfs rootfs. The image is the only input — no recipe, no second resolution — and must already be in the layout (`umf build` or `umf pull` it first).

The disk is **local-only**: never an OCI artifact, never pushed.

| Flag | Purpose |
|------|---------|
| `-o, --output <PATH>` | Write the raw disk here. Omit to write a content-addressed sidecar in the layout's block cache (a repeat compile is then a cache hit). |
| `--disk-size <BYTES>` | Total disk image size. Default 2 GiB (sparse). |
| `--esp-size <BYTES>` | EFI System Partition size. Default 500 MiB (per spec). |

For the classic flavor, the `systemd-boot` `.efi` is read from inside the image rootfs (`/usr/lib/systemd/boot/efi/<arch>.efi`): in-image only, no host fallback and no override flag. A classic-flavor image that ships no bootloader is an error (switch to `flavor=uki`, or install systemd-boot into the rootfs). `flavor=uki` needs no bootloader (ukify wraps the kernel).

```bash
umf compile local/appliance:1.0 -o ./disk.raw   # raw disk to a file
umf compile local/appliance:1.0                  # into the block cache (for `umf run`)
```

## Running

### `umf run`

`umf run [OPTIONS] <REFERENCE> [CMD]...`

Run a previously-built image. The target type is detected from the `org.imagilux.umf.type` label. Container images run via the linked-in `libcontainer` runtime. A **bootable image** (`type=bootable`) is auto-compiled to a disk (cached) and booted through `umf-vmm` — no `--disk` needed, with OVMF auto-discovered; the same path serves every bootable shape (classic or UKI, init-system or single-binary PID 1). A raw disk can still be booted directly with `--vmm` + `--disk`.

Container:

| Flag | Purpose |
|------|---------|
| `-i, --interactive` | Allocate a PTY (like `podman run -i`). |
| `-e, --env <KEY=VAL>` | Set/override an env var (merged over the image's; CLI wins). Repeatable. |
| `--entrypoint <CMD>` | Override ENTRYPOINT (drops the image's CMD). |
| `[CMD]...` | Override CMD/args — everything after the ref or a literal `--`. |
| `--keep-bundle` | Keep the prepared OCI bundle for inspection; path printed on exit. |
| `--insecure-registry` / `--username` / `--password-stdin` | Pull-on-miss TLS / credentials. |

Bootable / VM:

| Flag | Purpose |
|------|---------|
| `--vmm <qemu\|ch>` | Backend: `qemu` (default; universal, mature) or `ch` (Cloud Hypervisor: Rust-native, faster boot). For a raw `--disk` boot `ch` needs `--firmware` (see below); a bootable image discovers it automatically. |
| `--disk <PATH>` | Boot a raw disk directly (skips auto-compile). |
| `--firmware <PATH>` | UEFI firmware (OVMF / EDK II). For a bootable image it is auto-discovered on the host for both backends, so pass it only to override or when discovery fails (*no UEFI firmware for `<arch>` found…*). For a raw `--disk` boot there is no discovery: `qemu` falls back to its built-in firmware, `ch` requires this flag. |
| `--memory <MIB>` / `--cpus <N>` | Guest RAM (default 1024) / vCPUs (default 2). |
| `-p, --port-forward <HOST:GUEST[/udp]>` | Host port forward. Repeatable. QEMU uses user-mode networking (`hostfwd`); Cloud Hypervisor (`--vmm=ch`) has none, so UMF wires it host-side: a per-VM netns + tap + nft DNAT, pure-Rust (no `iproute2`), with a DHCP daemon in the namespace (`dnsmasq` by default; see `--dhcp-command`). Needs `nft` (and the DHCP daemon) on `PATH` (see `umf doctor`). |
| `--dhcp-command <ARGV>` | DHCP daemon run inside the VM netns for `--vmm=ch` port-forwarding. Default `dnsmasq`; `none` launches nothing (run your own DHCP there, or use a static guest IP); any other value is a whitespace-split command, e.g. `--dhcp-command "kea-dhcp4 -c /etc/kea.conf"`. The daemon starts with the bridge up at `10.70.x.1/29` and owns its own config. |
| `--graphic` | Graphical window instead of the default headless serial console. |

```bash
umf run local/app:1.0
umf run -i local/app:1.0 /bin/sh
umf run local/appliance:1.0 -p 8080:80 --memory 2048   # bootable: auto-compile + boot
umf run --vmm=qemu --disk ./disk.raw                    # boot a raw disk directly
```

## Image & layout management

### `umf images`

`umf images [--list] [--remove <REF>...] [--prune] [--format table|json]`

Manage the on-disk layout. With no flag (or `--list`) it lists cached refs (REF / TYPE / SIZE / DIGEST). `--remove` drops refs (blobs untouched unless `--prune` is also passed); `--prune` GCs everything unreachable from a surviving manifest — blobs, the erofs lower-layer cache, and the bootable **block cache** (a compiled disk whose source image is gone). (This replaces the old `rmi` subcommand — every `images` action operates on the same layout.)

```bash
umf images
umf images --remove local/app:1.0 --prune
umf images --format json | jq '.[].ref'
```

### `umf push` · `umf pull`

`umf push [OPTIONS] <REFERENCE>` — push an existing layout ref to its registry.
`umf pull [OPTIONS] <REFERENCE>` — pull a ref into the layout without building (pre-warm a `FROM` base for offline builds).

Both take `--insecure-registry`, `--username`, `--password-stdin`.

```bash
umf pull debian:bookworm
umf push reg.example.com/app:1.0
```

### `umf save` · `umf load`

`umf save -o <PATH> <REFERENCES>...` — export refs to an OCI Image Layout tarball.
`umf load -i <PATH> [--overwrite]` — import one back.

Round-trips with `skopeo copy oci-archive:` and `docker save` / `load`. Use `-` for stdout/stdin. The backbone of air-gapped transport (see [Examples → Air-gapped operation](examples.md#air-gapped-operation)). Without `--overwrite`, `load` errors fast on a colliding ref and leaves the layout untouched.

`umf save --type=block <REF> -o <PATH>` exports a different artifact: the **compiled disk** of a bootable image, extracted from the block cache as a raw, `dd`-able image (the image must have been `umf compile`d or `umf run` first — `save` extracts, it doesn't project). Default `--type=oci-archive` is the tarball form above. A block has no OCI reference, so it is never pushed — `save --type=block` is the only way off the machine.

```bash
umf save -o ./bundle.tar debian:bookworm local/app:1.0
umf load -i ./bundle.tar --overwrite
umf save --type=block local/appliance:1.0 -o ./disk.raw   # extract the compiled disk
```

### `umf index`

`umf index --tag <MULTI_REF> <CHILD_REF>...` composes a multi-arch OCI image index from per-arch images already in the local layout.

Build each architecture first (`umf build --platform=linux/<arch> --tag <child-ref>`, one per arch, each writing into the shared layout), then stitch the children into one `application/vnd.oci.image.index.v1+json` registered under `--tag` (**required**). At least one child ref is required, typically one per architecture; each must already resolve to a single-arch image manifest in the layout, and its `platform` descriptor is read from the child's own OCI config (`architecture` / `os`), so the index never misreports a child. The result pushes / pulls like any other ref, serves as a `FROM` base, and is consumed per-arch with [`umf inspect --platform`](#umf-inspect). `--push` (with the usual `--insecure-registry` / `--username` / `--password-stdin`) uploads the index and every child tree to the registry implied by `--tag`.

```bash
umf build --platform=linux/amd64 --tag reg.example.com/app:amd64 .
umf build --platform=linux/arm64 --tag reg.example.com/app:arm64 .
umf index --tag reg.example.com/app:1.0 reg.example.com/app:amd64 reg.example.com/app:arm64 --push
```

### umf registry

`umf registry add|remove|list` manages the registries UMF searches for **unqualified** references.

A bare reference like `alpine:3.23` (no registry host) resolves against Docker Hub by default. Configured registries are tried first, **in order**, before the `docker.io` fallback, when resolving an unqualified `FROM`, `ADD <oci-ref>`, or `umf pull`. A fully-qualified reference (one with an explicit host, e.g. `ghcr.io/owner/app`) is never rewritten. This mirrors Podman's unqualified-search-registries model. The list is stored at `$XDG_CONFIG_HOME/umf/registries.toml` (default `~/.config/umf/registries.toml`).

```bash
umf registry add registry.example.com    # try registry.example.com first for bare names
umf registry add ghcr.io                 # then ghcr.io, then docker.io
umf registry list                        # show the ordered search list (top = tried first)
umf registry remove ghcr.io
```

With that list, a recipe's `FROM alpine:3.23` resolves against `registry.example.com/alpine:3.23`, then `ghcr.io/alpine:3.23`, then `docker.io/library/alpine:3.23`, taking the first that exists. Per-registry credentials use the same resolution as `umf push` / `umf pull`.

## Supply chain

### `umf sbom`

`umf sbom attach <REFERENCE> --sbom <FILE> [--format spdx|cyclonedx] [--push]` attaches an SBOM document to an image as an OCI 1.1 *referrer* artifact: a manifest whose `subject` is the image and whose blob is the SBOM verbatim, with the document's media type as the `artifactType` (`application/spdx+json` or `application/vnd.cyclonedx+json`). The format is auto-detected from the document (SPDX `spdxVersion`, CycloneDX `bomFormat`); pass `--format` to force it.

This is the cosign-/oras-compatible attachment shape, so any referrers-aware client lists it back (`oras discover <image>`, `cosign tree <image>`). The subject image must already be in the local layout. `--push` (with the usual `--insecure-registry` / `--username` / `--password-stdin`) uploads the referrer to the image's registry, maintaining the OCI 1.1 referrers index or its `<algo>-<hex>` fallback tag; the image itself must already be pushed.

```bash
umf build --tag reg.example.com/app:1.0 . && umf push reg.example.com/app:1.0
umf sbom attach reg.example.com/app:1.0 --sbom app.spdx.json --push
oras discover reg.example.com/app:1.0          # the SBOM appears as a referrer
```

`umf sbom generate <REFERENCE> [--format spdx|cyclonedx] [-o <FILE>] [--attach] [--push]` *builds* an SBOM rather than taking one: it materializes the image's merged rootfs and reads its installed-package database (dpkg, apk, pacman, or the sqlite rpm database), then emits a deterministic SPDX 2.3 or CycloneDX 1.5 document (default SPDX). With neither `-o` nor `--attach` it prints to stdout; `-o <FILE>` (or `-o -`) writes it out; `--attach` stores it as a referrer, and `--push` uploads that referrer (same rules as `attach`).

```bash
umf sbom generate reg.example.com/app:1.0 -o app.spdx.json          # write SPDX
umf sbom generate reg.example.com/app:1.0 --format cyclonedx --attach --push
```

### `umf sign`

`umf sign <REFERENCE> --key <SPEC> [--key-type ecdsa-p256|ed25519] [--push]` signs an image with a static key and attaches a **cosign-compatible** signature as an OCI 1.1 referrer: the cosign "simple signing" payload (the image's manifest digest) signed and stored under artifactType `application/vnd.dev.cosign.artifact.sig.v1+json`, with the base64 signature in the `dev.cosignproject.cosign/signature` annotation. `cosign verify --key <pub>` reads it back.

The key is a PKCS#8 PEM private key passed through the same spec grammar as `umf build --secret`: `--key id=<id>,src=<key.pem>` or `--key id=<id>,env=<NAME>` (file or environment variable, never a layer). ECDSA P-256 (cosign's default) and ed25519 are supported; the algorithm is auto-detected from the key, or forced with `--key-type`. Sigstore keyless (Fulcio / Rekor) is out of scope for an air-gapped tool. `--push` uploads the signature referrer (the subject image must already be pushed).

```bash
umf sign reg.example.com/app:1.0 --key id=signing-key,src=cosign.key --push
cosign verify --key cosign.pub reg.example.com/app:1.0
```

### `umf attest`

`umf attest <REFERENCE> --predicate <FILE> [--type <TYPE>] --key <SPEC> [--push]` wraps a predicate document in an in-toto Statement (whose subject is the image's manifest digest), signs the DSSE Pre-Authentication Encoding with the same static-key channel as `umf sign`, and attaches the DSSE envelope as a referrer (blob media type `application/vnd.dsse.envelope.v1+json`, with a `predicateType` annotation on the manifest). `cosign verify-attestation --key <pub>` reads it back.

`--type` is the predicate type: a cosign shorthand (`slsaprovenance` (default), `spdx`, `cyclonedx`, `vuln`, `link`) or a full URI. The predicate JSON is supplied by you (UMF attaches it, it does not synthesize provenance). Keys, `--key-type`, and `--push` behave exactly as for `umf sign`.

```bash
umf attest reg.example.com/app:1.0 --predicate provenance.json --type slsaprovenance \
  --key id=signing-key,src=cosign.key --push
cosign verify-attestation --key cosign.pub --type slsaprovenance reg.example.com/app:1.0
```

## Introspection & operations

### `umf inspect`

`umf inspect [--format table|json] [--show-blobs] [--platform <os/arch>] [--insecure-registry] [--username <NAME>] [--password-stdin] <REFERENCE>`

Show an artifact's UMF labels, runtime config, layer composition, and history (the full L0 profile). Pulls from the registry on a layout miss; `--insecure-registry` / `--username` / `--password-stdin` apply to that pull. `--show-blobs` adds per-layer blob digests + sizes in the table view (`json` always includes them). When the reference resolves to a multi-arch image index, `--platform <os/arch>` (e.g. `linux/arm64`) selects which per-arch child to report (defaults to the host arch; ignored for a single-arch image). This is the consumer side of [`umf index`](#umf-index).

### `umf ps`

`umf ps [-o pretty|plain|json] [-s KEY[:DIR]] [-f KEY=VALUE,...] [--prune]`

List umf-managed processes — every build and run umf has launched, from `$XDG_STATE_HOME/umf/processes/`. Because umf is daemonless, finished processes remain as history (like `docker ps -a`). Sort keys: `id | name | process | type | status | release | started` (suffix `:asc` / `:desc`). Filter keys: `id, name, process, type, status, release` (all ANDed; `all` / `*` matches anything). `--prune` removes finished records, honouring `--filter`.

```bash
umf ps
umf ps -f status=failed,type=build
umf ps --prune -f status=exited
```

### `umf doctor`

`umf doctor [PATH]`

Report which host runtimes UMF needs and what's installed. With a recipe, scope the report to that build. The container engine is always available (linked in), so `doctor` surfaces VM-target prerequisites (`qemu-system-<arch>`, `/dev/kvm`) plus a **Container RUN-step network egress** section: whether `nft` is on `PATH`, whether `dnsmasq` is present (the default in-VM DHCP for `--vmm=ch` port-forwarding; not needed if you pass `--dhcp-command`), the `net.ipv4.ip_forward` state, and the netfilter `FORWARD` policy (UMF enables `ip_forward` itself but can't override a default-drop `FORWARD` policy). Run `sudo umf doctor` to let it read the ruleset for the FORWARD verdict. The section also reports the **rootless egress backend** selected by `--rootless-net` / `UMF_ROOTLESS_NET` (`native` by default), and whether `pasta` is available on `PATH` (relevant only when the `pasta` backend is selected).

## Developer tooling

### `umf debug build`

`umf debug build [-f <PATH>] [--tag <REF>] [--compression gzip|zstd] [--break-on <INDEX[,INDEX...]>] [PATH]`

Step through a container build directive-by-directive. The build pauses before each RUN / ADD / metadata directive and offers a small REPL (continue / step / inspect / breakpoint / quit), so you can walk a recipe and see what each directive does without rebuilding from scratch. `--break-on=<INDEX[,INDEX...]>` presets 1-based breakpoints so `c` (continue) stops at the next one instead of running to the end; the debugged image lands under `--tag` (default `umf-debug/local:latest`).

### `umf bench`

`umf bench [--runs N] [--warmup N] [--cold-only] [--format text|json] [--tag REF] [-f <PATH>] [PATH]`

Benchmark a recipe: one cold-cache build plus N warm-cache runs (default 5), reporting median / p99 / min / max wall-clock alongside cache-determinism flags (layer count + total bytes should be invariant across warm runs of a deterministic recipe). The registry is never contacted. `--format json` emits a structured `BenchReport` for CI regression tracking.

```bash
umf bench . --runs 10
umf bench . --cold-only --format json
```
