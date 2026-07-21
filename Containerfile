# Minimal OCI image that ships the `umf` CLI.
#
# UMF is daemonless and OCI-native: there is no host docker/podman or
# container-runtime dependency baked in. This image just carries the
# statically-linked release binary on a small base (Alpine for a shell +
# CA certificates so registry HTTPS works out of the box).
#
# IMPORTANT (running umf in a container):
#   umf drives micro-VMs (QEMU / Cloud Hypervisor) and sets up container
#   RUN namespaces. Both the BUILD of a UMF artifact and RUNning one need
#   elevated host access. Run this image with:
#
#       podman run --rm --privileged --device /dev/kvm \
#         -v "$PWD:/work" -w /work umf:latest build --tag local/app:1.0 .
#
#   (docker run ... --privileged --device /dev/kvm ... works the same way.)
#   --privileged + /dev/kvm are required; without KVM the VM paths fall
#   back to slow emulation or fail, and the container build paths need the
#   namespace/mount privileges. /work is just a convention for bind-mounting
#   your recipe + context.
#
# Build context: place the release `umf` binary next to this file, or pass
# its path via the UMF_BIN build arg. Grab one from a GitHub Release
# (musl target, statically linked):
#
#   v=v0.0.1; t=x86_64-unknown-linux-musl
#   curl -fsSL "https://github.com/imagilux/umf/releases/download/$v/umf-${v#v}-$t.tar.gz" \
#     | tar -xz --strip-components=1 "umf-${v#v}-$t/umf"
#   podman build -t umf:latest .
#
# A musl binary is statically linked, so it runs unmodified on this Alpine
# base. To ship a glibc (gnu) binary instead, swap the base for a glibc
# image (e.g. debian:stable-slim) and drop in the gnu tarball's `umf`.

FROM alpine:3.24

# CA roots for pulling/pushing OCI artifacts over HTTPS.
RUN apk add --no-cache ca-certificates

# Path to the release binary in the build context (override with
# --build-arg UMF_BIN=...). Default matches the layout you get from
# `tar -xz --strip-components=1 .../umf` shown above.
ARG UMF_BIN=umf

COPY ${UMF_BIN} /usr/local/bin/umf

# Fail the build early if the copied artifact isn't an executable umf.
RUN chmod 0755 /usr/local/bin/umf && umf --version

ENTRYPOINT ["umf"]
