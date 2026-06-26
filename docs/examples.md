# Examples

End-to-end workflows showing how UMF builds compose. Each component (kernel, rootfs, bootloader) is itself an OCI artifact produced by a UMF build, so the same DSL and the same `registry → cache → source build` resolution chain apply at every level.

The workflows below follow that progression: build the components, then assemble them into final artifacts for each target.

---

## Component artifacts

### Building a base kernel

A kernel is the output of an ordinary container build that writes the compiled `vmlinuz` and modules to its filesystem and labels itself as a kernel artifact. There is no special build mode — the build runs on a container runtime, same as any container build. Downstream bootable builds consume the result via `FROM`, the same way they would any other OCI image.

The standard shape sits on top of a published `kernel-build-env` that supplies the toolchain:

```dockerfile
# Containerfile
FROM imagilux/kernel-build-env:1.0
ADD ./config/default /.config
RUN /usr/local/bin/build-linux 7.0

LABEL org.imagilux.umf.type=kernel
LABEL org.imagilux.umf.kernel.release="7.0"
LABEL org.imagilux.umf.kernel.config=default
```

`FROM imagilux/kernel-build-env:1.0` is a container-shaped UMF artifact carrying gcc/make and the kernel's required libs — a normal OCI image, resolved through the same `registry → cache → source` chain as everything else. `ADD ./config/default /.config` drops the kernel `.config` at the build's source-tree root for the script to pick up. `RUN /usr/local/bin/build-linux 7.0` fetches sources at the requested release, compiles, and writes `boot/vmlinuz-7.0` plus `lib/modules/7.0/` into the image.

The `org.imagilux.umf.type=kernel` label is what makes the result a kernel artifact — downstream bootable builds use it to validate `FROM` (see [L0 Introspection](specification.md#l0-introspection)).

Operators who don't want to trust a pre-published build env can inline every step on a minimal base instead (toolchain `ADD`, source fetch, compile, install) — the build is still an ordinary container build, just longer. Both shapes produce the same kernel artifact. (A fully self-contained `FROM scratch` producer also works — the engine accepts an empty base; remember the first `RUN` only has what you `ADD`ed before it.)

Build and publish:

```bash
umf build --tag registry.example.com/kernels/linux:7.0 --push .
```

Downstream bootable builds reference the result via `FROM registry.example.com/kernels/linux:7.0`.

### Building a kernel-build-env

The build env that the kernel build `FROM`s is itself a UMF artifact — no hidden builder magic, no implicit toolchain injection. It's a container target (filesystem + tools, no boot chain), built and published like any other component:

```dockerfile
# Containerfile
FROM debian:bookworm

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      bc bison cpio flex gcc kmod libelf-dev libssl-dev make perl rsync zstd && \
    apt-get clean && rm -rf /var/lib/apt/lists/*

LABEL org.imagilux.umf.type=kernel-build-env
LABEL org.imagilux.umf.kernel-build-env.version="1.0"
LABEL org.imagilux.umf.kernel-build-env.toolchain=gcc
```

Publish:

```bash
umf build --tag registry.example.com/kernel-build-env:1.0 --push .
```

Variants are just different artifacts: swap `gcc` for `clang lld llvm`, retag as `:1.0-llvm`, set `kernel-build-env.toolchain=llvm`, and a downstream kernel build `FROM`s `myorg/kernel-build-env:1.0-llvm` to compile a clang-built kernel. Custom patches, Rust-in-kernel, vendor toolchains — same shape, no new directives required.

### Building a curated rootfs

The same pattern produces a reusable rootfs — typically an org's hardened or pre-provisioned baseline. Layer your customisation on top of a vanilla distribution rootfs, then publish under your org's namespace.

```dockerfile
# Containerfile
FROM debian:bookworm

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      ca-certificates curl jq systemd-resolved && \
    apt-get clean && rm -rf /var/lib/apt/lists/*

ADD ./certs/myorg-ca.crt /usr/local/share/ca-certificates/myorg-ca.crt
RUN update-ca-certificates

LABEL org.imagilux.umf.type=rootfs
LABEL org.imagilux.umf.rootfs.org=myorg
LABEL org.imagilux.umf.rootfs.version="1.0"
```

Publish under your org's rootfs namespace:

```bash
umf build --tag registry.example.com/rootfs/myorg-base:1.0 --push .
```

Downstream builds add it as their userland with a bare `ADD`:

```dockerfile
ADD registry.example.com/rootfs/myorg-base:1.0 /
```

### Building a bootloader

Bootloaders follow the same shape: a UMF build whose output is the bootloader binary + assets, published under your registry. A downstream bootable build ships it inside its own rootfs (with `ADD <bootloader-ref> ...`), so that `umf compile` reads the bootloader for the classic flavor from inside the image. The classic-flavor bootloader is in-image only (there is no host fallback), so shipping it in the rootfs is how you pin a signed bootloader (Secure Boot, fleet-locked GRUB config) instead of relying on the userland's stock systemd-boot package.

```dockerfile
# Containerfile
FROM debian:bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
      grub-efi-amd64-bin grub-pc-bin grub2-common && \
    apt-get clean && rm -rf /var/lib/apt/lists/*
ADD ./grub.cfg /etc/default/grub
RUN grub-mkconfig -o /boot/grub/grub.cfg

LABEL org.imagilux.umf.type=bootloader
LABEL org.imagilux.umf.bootloader.flavor=grub
LABEL org.imagilux.umf.bootloader.version="2.12"
```

This is a container build that writes GRUB binaries and config into its filesystem, then labels itself a bootloader artifact. A downstream bootable build pulls those binaries into its own rootfs with `ADD <ref> ...`; `umf compile` then installs the in-image bootloader into the L0 boot partition when projecting the disk. (Note: `org.imagilux.umf.bootloader.flavor` here is an *artifact* label describing this bootloader build. It is a different key from the image-level `org.imagilux.umf.flavor` a bootable build sets to choose classic-vs-UKI packaging.)

The choice of `debian:bookworm` as the base here isn't preferential — it just happens to ship `grub-mkconfig`. The same build `FROM fedora:42` with `RUN dnf install -y grub2 && grub2-mkconfig` would publish an equally-valid GRUB artifact. UMF doesn't favour any distro; the base is just an OCI image the resolver looks up.

---

## Composing artifacts

### Full bootable image from custom components

Once your kernel and rootfs are published, a downstream bootable build composes them by reference. `FROM` and `ADD <oci-ref>` resolution both hit the registry — no source rebuild, no upstream pull.

```dockerfile
FROM registry.example.com/kernels/linux:7.0
ADD registry.example.com/rootfs/myorg-base:1.0 /
LABEL org.imagilux.umf.flavor=systemd-boot
ENTRYPOINT systemd

LABEL org.imagilux.umf.author="Gaël THEROND / Imagilux"
LABEL org.imagilux.umf.name="webserver"

RUN apt-get update && apt-get install -y nginx
ADD nginx.conf /etc/nginx/nginx.conf
EXPOSE 80/tcp
EXPOSE 443/tcp
RUN systemctl enable nginx.service
```

`FROM` resolving to a kernel artifact is what marks this as a **bootable** build. The userland comes from a bare `ADD <oci-ref>`, and `LABEL org.imagilux.umf.flavor=systemd-boot` selects classic-vs-UKI packaging (these replace the former `ROOTFS` / `BOOTLOADER` directives). `ENTRYPOINT systemd` selects the init system; the builder generates the initramfs implicitly at L3. Services are enabled by acting on the unit directly (`RUN systemctl enable …`, or letting the package's preset do it); host-specific settings like hostname, locale, and timezone are left to first-boot provisioning (cloud-init / ignition) so one image stays generic across deployments. `umf build` emits a plain OCI image (`type=bootable`); `umf compile` / `umf run` projects it to a disk later. This is the payoff of OCI-native distribution: the heavy artifacts (kernel, rootfs) are built once per release and pulled by every downstream build, and the bootable image is itself just another pushable OCI image.

---

## Target variations

The same component artifacts compose into different targets by varying the boot chain and ENTRYPOINT.

### Appliance — single binary as PID 1 (UKI)

A binary-path ENTRYPOINT runs the executable directly as PID 1 via the kernel's `init=`, with no init system and no initramfs. `LABEL org.imagilux.umf.flavor=uki` makes it a UKI: UMF wraps the kernel + command line in a `systemd-stub` `.efi` the firmware boots directly (assembled at compile time). `FROM` still references the kernel artifact; the same image boots as a VM (`umf run`, OVMF auto-supplied) or on bare metal (write the compiled disk to hardware). No `ADD --from` userland is needed here, the appliance binary is the only payload.

```dockerfile
FROM registry.example.com/kernels/linux:7.0
LABEL org.imagilux.umf.flavor=uki   # Unified Kernel Image, no separate bootloader
ENTRYPOINT /myapp                   # runs as PID 1 via init=/myapp

ADD myapp /myapp
```

### Container (degenerate case)

Drop the boot chain entirely: `FROM` a base image (not a kernel), no flavor label. The result is an ordinary OCI container image; either supply an explicit ENTRYPOINT path or set `ENTRYPOINT none` to let the runtime provide PID 1.

```dockerfile
FROM alpine:3.21
ENTRYPOINT ["/usr/sbin/nginx", "-g", "daemon off;"]

RUN apk add --no-cache nginx
ADD nginx.conf /etc/nginx/nginx.conf
EXPOSE 80/tcp
```

---

## Air-gapped operation

UMF builds resolve every `FROM` and `ADD <oci-ref>` reference through a uniform `registry → local cache → source build` chain. When the local cache is populated, no remote registry is contacted — which is the principle behind the [sovereignty-first pillar](index.md): an air-gapped node, given a pre-populated layout, can build new images from cached components alone.

### Pre-warming a layout from a connected node

On an internet-reachable machine, pull the base images the air-gapped builder will need:

```bash
# Pull each base into the same layout dir, then archive it for transport.
umf pull --layout-dir ./airgap-layout debian:bookworm
umf pull --layout-dir ./airgap-layout imagilux/kernel-build-env:1.0
umf save --layout-dir ./airgap-layout \
    --output ./airgap-layout.tar \
    debian:bookworm \
    imagilux/kernel-build-env:1.0
```

`umf save` produces a standard OCI Image Layout v1 tar archive, compatible with `skopeo copy oci-archive:` and `docker load`.

### Building on the air-gapped node

Transfer `airgap-layout.tar` to the air-gapped node and load it into the operator's layout:

```bash
umf load --layout-dir ~/.cache/umf/oci-layout --input ./airgap-layout.tar
```

From this point onward, any build whose `FROM` resolves to one of the pre-loaded refs runs entirely off the local cache — no registry, no DNS, no outbound TCP. The `umf build`'s internal `FROM` resolver short-circuits the pull step when the cached digest is already present.

```bash
# Build a derivative image; nothing reaches the network.
umf build -t local/derivative:1.0 .
```

To verify the layout contents at any time:

```bash
umf images           # list cached refs + types + sizes
umf inspect <ref>    # full target type / runtime config / layers / labels
```

The container-target air-gapped flow is locked in by `crates/umf-builder/tests/air_gapped_container_build.rs`, which proves a build whose inputs are pre-cached completes without any registry access. The VM-target equivalent (bootable disk emission from pre-cached kernel / rootfs / bootloader artifacts) goes through the same `registry → cache → source` resolution chain, so the same offline guarantee holds; the bootable path is validated end to end by the boot-smoke test, which builds a `type=bootable` image, `umf compile`s it, and boots it under QEMU/KVM.

## Supply chain

Once an image is built (and pushed), attach provenance to it as OCI 1.1 referrer artifacts, all cosign-/oras-compatible. The subject image must already be in the local layout, and `--push` (which needs the subject already pushed) uploads each referrer:

```bash
# Generate an SBOM by scanning the image's installed packages, then attach it.
umf sbom generate reg.example.com/app:1.0 --attach --push

# Or attach an SBOM produced elsewhere (format auto-detected).
umf sbom attach reg.example.com/app:1.0 --sbom ./app.spdx.json --push

# Sign the image (cosign-compatible signature referrer).
umf sign reg.example.com/app:1.0 --key id=signer,src=./cosign.key --push

# Wrap a predicate in a signed in-toto/SLSA DSSE attestation.
umf attest reg.example.com/app:1.0 --predicate ./provenance.json --type slsaprovenance --key id=signer,src=./cosign.key --push
```

List them back with any referrers-aware client (`oras discover <image>`, `cosign tree <image>`), and verify signatures / attestations with `cosign verify` / `cosign verify-attestation --key`. See the [CLI Reference](cli.md#supply-chain) for every flag.

---

For the full directive reference and resolution rules these workflows depend on, see the [Specification](specification.md).
