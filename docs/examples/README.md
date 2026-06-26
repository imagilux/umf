# Worked examples — the component supply chain

Three self-contained, buildable UMF recipes that compose into a bootable image,
each an ordinary **container** build that publishes an OCI artifact:

| Directory | Output | `org.imagilux.umf.type` |
|---|---|---|
| [`kernel-build-env/`](kernel-build-env/Containerfile) | gcc + kernel build deps | `kernel-build-env` |
| [`kernel/`](kernel/Containerfile) | `vmlinuz` + modules from torvalds/linux | `kernel` |
| [`rootfs/`](rootfs/Containerfile) | a neutral [FHS 3.0](https://refspecs.linuxfoundation.org/FHS_3.0/fhs/index.html) root filesystem | `rootfs` |

There is no special "kernel mode" or "rootfs mode": `umf build` infers the
container shape purely from what `FROM` resolves to (a base image or `scratch`,
not a kernel), and the `org.imagilux.umf.type` label is the only thing that
distinguishes the outputs. Each is a normal OCI image, resolved through the same
`registry → cache → source` chain as everything else.

## Build and publish

The chain is ordered — the kernel `FROM`s the build-env:

```bash
# 1. Toolchain (foundation for the kernel build)
umf build --tag registry.bitswalk.net/examples/kernel-build-env:1.0 --push docs/examples/kernel-build-env

# 2. Kernel — FROMs the build-env, clones + compiles Linux
umf build --tag registry.bitswalk.net/examples/linux:7.0 --push docs/examples/kernel

# 3. Neutral rootfs (independent of the other two)
umf build --tag registry.bitswalk.net/examples/rootfs-fhs:1.0 --push docs/examples/rootfs
```

## Composing them into a bootable image

A downstream bootable build references the kernel via `FROM` and the rootfs via
a bare `ADD <oci-ref> /`. `FROM` resolving to a `type=kernel` artifact is what
marks the build bootable; boot packaging is chosen with a stock
`LABEL org.imagilux.umf.flavor` (`systemd-boot` or `uki`):

```dockerfile
FROM registry.bitswalk.net/examples/linux:7.0
ADD registry.bitswalk.net/examples/rootfs-fhs:1.0 /
LABEL org.imagilux.umf.flavor=systemd-boot
ENTRYPOINT systemd
```

`umf build` emits a plain `type=bootable` OCI image; `umf compile` / `umf run`
projects it to a disk and boots it (QEMU/Cloud Hypervisor or bare metal).

## Reproducing the bootable flow locally

The `registry.bitswalk.net/examples/*` tags above are **illustrative**: they show
the canonical publish-and-compose shape, but they are not guaranteed pullable
from a public registry, and the `kernel/` recipe needs a multi-hour upstream
compile. For a reproducible, minimal end-to-end bootable run that needs neither,
the repository ships a fixture builder:

```bash
scripts/make-boot-fixture.sh /tmp/umf-boot-fixture   # Alpine linux-virt kernel + static-busybox rootfs
```

It writes a `kernel/` + `rootfs/` pair (a real `vmlinuz` plus modules and a
busybox `/sbin/init`). The boot-smoke test (`tests/boot_smoke.rs`) seeds that pair as OCI artifacts, runs `umf build` into a
`type=bootable` image, `umf compile`s it to a UKI disk, and boots it under
QEMU/KVM to a userspace marker:

```bash
UMF_BOOT_SMOKE=1 UMF_BOOT_FIXTURE=/tmp/umf-boot-fixture cargo test --release --test boot_smoke
```

This is the minimal, reproducible demo set; the `kernel-build-env` → `kernel` →
`rootfs` chain above is the production-shaped equivalent (real toolchain, real
compile).

## `FROM scratch` vs. a base image

The recipes here build on real base images (`debian:bookworm-slim`,
`busybox:1.36-musl`) because each wants what the base ships (a toolchain, or
busybox binaries). `FROM scratch` is equally supported: a producer build can
start from nothing and pull its userland with a bare `ADD <oci-ref> /`. Reach
for a base image when you want its contents; reach for `scratch` when you are
assembling the filesystem yourself.

## Network during `RUN`

The `kernel-build-env` and `kernel` recipes fetch over the network during `RUN`
(`apt-get`, `git clone`), the conventional shape and the same one the
`apt`-based recipes in [`../examples.md`](../examples.md) use. `umf build` runs
each `RUN` step in an isolated network namespace with compartmentalized NAT'd
egress (a veth pair plus a host `nft` masquerade rule, programmed in-process by
`umf-networking`), so those fetches work on a build that has the privileges
egress needs (`CAP_NET_ADMIN`, the rootful path CI's NAT-egress lane exercises).
Where that egress is unavailable (no host forwarding, or insufficient
privileges), pre-vendor the toolchain and kernel source into the build context
and `ADD` them instead of fetching.

## Validation status

- **All three** pass `umf parse` (lexing + grammar + structural validation).
- **`rootfs/`** builds end-to-end with no `RUN` network access, so it is the
  quickest to reproduce.
- **`kernel-build-env/`** and **`kernel/`** build on a host that provides `RUN`
  egress (see above). `kernel/` additionally needs an existing upstream tag (the
  `v7.0` placeholder isn't on torvalds/linux yet, so substitute e.g. `v6.12` /
  release `6.12.0`) and a multi-hour compile.
