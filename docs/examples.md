# Examples

End-to-end workflows showing how UMF builds compose. Each component (kernel, rootfs, bootloader) is itself an OCI artifact produced by a UMF build, so the same DSL and the same `registry → cache → source build` resolution chain apply at every level.

The workflows below follow that progression: build the components, then assemble them into final artifacts for each target.

---

## Component artifacts

### Building a base kernel

A kernel is a UMF build whose payload is the compiled kernel + modules. Downstream builds consume it via `KERNEL <name>:<release>`; the resolver pulls it from the registry, falling back to local cache, then to upstream source.

```dockerfile
# kernel-6.12.21.umf
FROM scratch
FIRMWARE uefi
BOOTLOADER none
ROOTFS alpine:3.21
KERNEL linux:6.12.21
INITRD none

LABEL org.imagilux.umf.type=kernel
LABEL org.imagilux.umf.kernel.version=6.12.21
LABEL org.imagilux.umf.kernel.config=default
```

Build and publish:

```bash
umf build -t registry.example.com/kernels/linux:6.12.21 .
umf push registry.example.com/kernels/linux:6.12.21
```

Downstream builds that reference `KERNEL linux:6.12.21` now resolve to this artifact instead of triggering an upstream source build.

### Building a curated rootfs

The same pattern produces a reusable rootfs — typically an org's hardened or pre-provisioned baseline. Layer your customisation on top of a vanilla distribution rootfs, then publish under your org's namespace.

```dockerfile
# myorg-base-1.0.umf
FROM scratch
ROOTFS debian:bookworm

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      ca-certificates curl jq systemd-resolved && \
    apt-get clean && rm -rf /var/lib/apt/lists/*

ADD ./certs/myorg-ca.crt /usr/local/share/ca-certificates/myorg-ca.crt
RUN update-ca-certificates

LABEL org.imagilux.umf.type=rootfs
LABEL org.imagilux.umf.rootfs.org=myorg
LABEL org.imagilux.umf.rootfs.version=1.0
```

Publish under your org's rootfs namespace:

```bash
umf build -t registry.example.com/rootfs/myorg-base:1.0 .
umf push registry.example.com/rootfs/myorg-base:1.0
```

Downstream builds reference it as a normal ROOTFS:

```dockerfile
ROOTFS myorg-base:1.0
```

### Building a bootloader

Bootloaders follow the same shape: a UMF build whose output is the bootloader binary + assets, published under your registry. Downstream builds reference it via `BOOTLOADER <name>`. Use this when you need a signed or pinned bootloader (Secure Boot, fleet-locked GRUB config) instead of the upstream-tracking default.

```dockerfile
# myorg-grub-2.12.umf
FROM scratch
ROOTFS alpine:3.21
BOOTLOADER grub

ADD ./grub.cfg /etc/default/grub
RUN grub-mkconfig -o /boot/grub/grub.cfg

LABEL org.imagilux.umf.type=bootloader
LABEL org.imagilux.umf.bootloader.flavor=grub
LABEL org.imagilux.umf.bootloader.version=2.12
```

---

## Composing artifacts

### Full VM image from custom components

Once your kernel and rootfs are published, a downstream VM build composes them by reference. The KERNEL and ROOTFS resolvers hit the registry — no source rebuild, no upstream pull.

```dockerfile
FROM scratch
FIRMWARE uefi
BOOTLOADER systemd-boot
ROOTFS myorg-base:1.0
KERNEL linux:6.12.21
INITRD auto

LABEL org.imagilux.umf.author="<author>"
LABEL org.imagilux.umf.name="webserver"

RUN apt-get update && apt-get install -y nginx
ADD nginx.conf /etc/nginx/nginx.conf
EXPOSE 80/tcp
EXPOSE 443/tcp
ENABLE nginx.service

HOSTNAME webserver
TIMEZONE Europe/Paris
```

This is the payoff of OCI-native distribution: the heavy artifacts (kernel, rootfs) are built once per release and pulled by every downstream build.

### Distro-base VM (simplified)

When you don't need a custom boot chain, `FROM <distro>:<release>` collapses the boot chain directives into a vanilla distro image. The boot chain is inherited from the base; you only add user-space layers.

```dockerfile
FROM ubuntu:24.04

RUN apt-get update && apt-get install -y nginx
ADD nginx.conf /etc/nginx/nginx.conf
EXPOSE 80/tcp
ENABLE nginx.service
```

---

## Target variations

The same component artifacts compose into different targets by varying the boot chain and ENTRYPOINT.

### Unikernel payload

`ENTRYPOINT binary` runs the executable directly as PID 1 — no init system, no userland. Pair with `ROOTFS none` and `INITRD none` for the minimal unikernel shape.

```dockerfile
FROM scratch
FIRMWARE uefi
BOOTLOADER none
KERNEL linux:6.12.21
INITRD none
ROOTFS none
ENTRYPOINT binary

ADD myapp /myapp
```

### Container (degenerate case)

Drop the boot chain entirely — no firmware, no bootloader, no kernel, no initrd. The result is an ordinary OCI container image; runtime supplies PID 1 (`ENTRYPOINT none`).

```dockerfile
FROM scratch
ROOTFS alpine:3.21
ENTRYPOINT none

RUN apk add --no-cache nginx
ADD nginx.conf /etc/nginx/nginx.conf
EXPOSE 80/tcp
```

---

For the full directive reference and resolution rules these workflows depend on, see the [Specification](specification.md).
