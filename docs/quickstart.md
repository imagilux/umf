# Quickstart

Zero to a built-and-running artifact. This page is about the `umf` reference implementation (**v0.0.1**); for the DSL it builds, see the [Specification](specification.md).

## Install

UMF is a single Linux binary (x86_64 / aarch64, glibc or musl). Pick whichever path fits your host; the from-source build at the end always works offline.

### One-line installer

```bash
curl -fsSL https://raw.githubusercontent.com/imagilux/umf/main/scripts/install.sh | sh
```

The script detects your OS and CPU, downloads the matching release tarball from GitHub Releases, verifies its SHA-256 against the published `SHA256SUMS`, and installs `umf` to `/usr/local/bin` (root) or `~/.local/bin` (non-root). On a musl host (Alpine) it picks the static musl build automatically. It is idempotent and quiet on success, printing only the installed path. Override the defaults with environment variables:

| Variable | Default | Effect |
| --- | --- | --- |
| `UMF_VERSION` | latest release | install a specific tag, e.g. `v0.0.1` |
| `UMF_LIBC` | auto (`gnu`, or `musl` on Alpine) | force the libc flavor |
| `UMF_INSTALL_DIR` | `/usr/local/bin` or `~/.local/bin` | install somewhere else |

### Cargo

```bash
cargo install --git https://github.com/imagilux/umf umf
```

Builds the CLI from the git source with your stable toolchain. Needs the same `libseccomp` dev package as a from-source build (see below).

### Container image

`umf` runs fine inside a container, with one caveat: it drives micro-VMs (QEMU / Cloud Hypervisor) and sets up container `RUN` namespaces, so it needs `--privileged` and `/dev/kvm`. Build the image from the [`Containerfile`](https://github.com/imagilux/umf/blob/main/Containerfile) at the repo root (its header shows how to fetch the release binary into the build context), then:

```bash
docker run --rm --privileged --device /dev/kvm \
  -v "$PWD:/work" -w /work umf:latest build --tag local/app:1.0 .
```

There is no pre-published image yet; build it locally for now.

### From source

```bash
git clone https://github.com/imagilux/umf
cd umf
cargo build --release                                # → target/release/umf
install -m755 target/release/umf ~/.local/bin/umf    # or anywhere on $PATH
```

Edition 2024, stable toolchain (pinned in `rust-toolchain.toml`, auto-installed by rustup on first build).

One system build dependency: the **libseccomp** library and headers. The build engine links `libcontainer` with its `libseccomp` feature so that every container `RUN` step gets the default seccomp syscall filter. Install the dev package before building, for example `sudo apt-get install -y libseccomp-dev` on Debian/Ubuntu, or `sudo dnf install -y libseccomp-devel` on Fedora.

### Verify a release by hand

The installer does this for you, but you can also verify and place a binary manually. Each release publishes per-target tarballs plus a single `SHA256SUMS`:

```bash
VERSION=v0.0.1
TARGET=x86_64-unknown-linux-gnu                                  # or -musl, aarch64-…
base="https://github.com/imagilux/umf/releases/download/$VERSION"
curl -fsSLO "$base/umf-${VERSION#v}-$TARGET.tar.gz"
curl -fsSLO "$base/SHA256SUMS"
sha256sum -c SHA256SUMS --ignore-missing                          # must report: OK
tar -xzf "umf-${VERSION#v}-$TARGET.tar.gz"
sudo install -m755 "umf-${VERSION#v}-$TARGET/umf" /usr/local/bin/umf
umf --version
```

## Check your host

`umf doctor` reports which host runtimes UMF can use. The container target needs nothing external — the build/run engine is linked in:

```console
$ umf doctor
Detected runtimes on this host:
  qemu-system-x86_64: /usr/bin/qemu-system-x86_64
  /dev/kvm: accessible
  container runtime: linked-in (umf-engine + libcontainer) — always available; powers both `umf build` and `umf run` without any external container CLI
  seccomp: default profile active (deny-by-default, N syscalls allowed) on RUN steps

Container RUN-step network egress (NAT out through the host):
  nft: /usr/sbin/nft
  dnsmasq: /usr/sbin/dnsmasq
  net.ipv4.ip_forward: enabled
  FORWARD policy: unknown — re-run as root (`sudo umf doctor`) to inspect the nftables ruleset
```

Pass a recipe — or a directory holding a `Containerfile`/`Dockerfile` — to scope the check to one build: `umf doctor .`. For the full host checklist and fixes when one of these reads MISSING, see [Prerequisites](prerequisites.md) and [Troubleshooting](troubleshooting.md).

## Build a container

Write a recipe — UMF reuses Dockerfile-familiar syntax. A `FROM` that resolves to a base image (not a kernel) makes this a [container target](compatibility.md#container):

```dockerfile
# Containerfile
FROM alpine:3.21
RUN apk add --no-cache nginx
ADD nginx.conf /etc/nginx/nginx.conf
EXPOSE 80/tcp
ENTRYPOINT ["/usr/sbin/nginx", "-g", "daemon off;"]
```

```bash
umf build --tag local/hello:1.0 .
```

`umf build .` discovers a `Containerfile` (then `Dockerfile`) in the given directory, Docker-style, and uses that directory as the build context. Pass an explicit recipe file of any name, or point `-f`/`--file` at one anywhere — the `.umf` extension is a convention, not a requirement.

The build runs entirely **in-process** — `libcontainer` + overlayfs, no `docker build`, no host container daemon. RUN steps execute in a sealed container assembled from the layers; each directive that writes to the filesystem becomes one content-addressed layer, and re-runs reuse cached layers whose input hash is unchanged.

## Run it

```bash
umf run local/hello:1.0
umf run -i local/hello:1.0 /bin/sh          # interactive shell (allocates a PTY)
umf run local/hello:1.0 -- nginx -t         # override CMD after the ref
```

`umf run` detects the target from the image's `org.imagilux.umf.type` label and runs container artifacts through the same linked-in runtime — again, no external CLI.

## See what you built

```bash
umf images                  # cached refs: REF / TYPE / SIZE / DIGEST
umf inspect local/hello:1.0 # labels, runtime config, layers, history
umf ps                      # every build/run umf has launched (umf is daemonless)
```

## Share it

```bash
umf build --tag registry.example.com/hello:1.0 --push .   # build + push
umf push registry.example.com/hello:1.0                   # push an existing ref
umf pull debian:bookworm                                  # pre-warm a FROM base
```

Credentials resolve in order: `--username` + `--password-stdin`, then `UMF_REGISTRY_USERNAME` / `UMF_REGISTRY_PASSWORD`, then `~/.docker/config.json`. Add `--insecure-registry` for a plain-HTTP local `registry:2`.

## Build a bootable image

Point `FROM` at a kernel artifact (and add a userland with `ADD <oci-ref> /`) to switch to the [bootable target](compatibility.md#bootable). `build` still emits a **plain OCI image** — bootable is just additive metadata (an embedded kernel + a boot manifest). Boot packaging is chosen with a `LABEL org.imagilux.umf.flavor` (`systemd-boot` or `uki`). The bootable *disk* is a projection, produced on demand:

```bash
umf build --tag local/appliance:1.0 .            # → OCI image (type=bootable), pushable like any image
umf run local/appliance:1.0                       # auto-compile to a disk + boot under QEMU (auto-OVMF)
```

`umf run` compiles the image to a disk on first use (cached) and boots it through `umf-vmm` — QEMU via QMP, or Cloud Hypervisor with `--vmm=ch`. To get the disk as a file instead of booting it:

```bash
umf compile local/appliance:1.0 -o ./disk.raw    # project to a sparse raw disk
umf save local/appliance:1.0 --type=block -o ./disk.raw   # or extract one already compiled
```

See [Examples](examples.md) for composing the kernel + rootfs + bootloader artifacts a bootable build consumes.

## Next steps

- [Prerequisites](prerequisites.md) — host packages and permissions (qemu, nftables, KVM group, OVMF, libseccomp).
- [CLI Reference](cli.md) — every command and flag.
- [Specification](specification.md) — the normative directive reference.
- [Examples](examples.md) — kernels, curated rootfs, full VM composition, air-gapped operation.
- [Architecture](architecture.md) — how the reference implementation fits together.
- [Troubleshooting](troubleshooting.md) — common failures (RUN-step egress, rootless uid maps, missing OVMF).
- [Known limitations](known-limitations.md) — the not-yet-supported paths.
