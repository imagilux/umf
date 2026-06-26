#!/usr/bin/env bash
# Build the UMF boot-smoke fixture (#271/#282): a `type=kernel` artifact tree and
# a minimal rootfs tree, extracted from a pinned Alpine image via docker, for the
# QEMU/KVM boot-smoke test. Reproducible and self-contained (no registry).
#
# Output layout (under $1, default /tmp/umf-boot-fix):
#   kernel/boot/vmlinuz-<release>        Alpine linux-virt kernel
#   kernel/lib/modules/<release>/...     modules, DECOMPRESSED (busybox insmod can't read .ko.gz)
#   rootfs/bin/busybox                   static busybox (no libc needed in the squashfs)
#   rootfs/sbin/init                     PID 1: prints the marker to ttyS0, then powers off
#   release                              the kernel release string
#
# Usage: scripts/make-boot-fixture.sh [OUTDIR]
#   env: ALPINE_TAG (default alpine:3.21), UMF_BOOT_MARKER (default UMF-BOOT-OK-<fixed>)
set -euo pipefail

OUT="${1:-/tmp/umf-boot-fix}"
ALPINE="${ALPINE_TAG:-alpine:3.21}"
MARKER="${UMF_BOOT_MARKER:-UMF-BOOT-OK-7f3a2c9d}"

rm -rf "$OUT"
mkdir -p "$OUT"

docker run --rm -i -v "$OUT:/out" -e MARKER="$MARKER" "$ALPINE" sh -s <<'INNER'
set -e
apk add --no-cache linux-virt busybox-static >/dev/null
rel="$(ls /lib/modules | head -1)"

# --- kernel artifact ---
mkdir -p /out/kernel/boot /out/kernel/lib/modules
cp "/boot/vmlinuz-virt" "/out/kernel/boot/vmlinuz-$rel"
cp -a "/lib/modules/$rel" /out/kernel/lib/modules/
# busybox insmod loads plain .ko only, so decompress every module
find /out/kernel/lib/modules -name '*.ko.gz' -exec gunzip -f {} +

# --- rootfs artifact ---
mkdir -p /out/rootfs/bin /out/rootfs/sbin
if [ -f /bin/busybox.static ]; then cp /bin/busybox.static /out/rootfs/bin/busybox
else cp "$(command -v busybox)" /out/rootfs/bin/busybox; fi
chmod 0755 /out/rootfs/bin/busybox
cat > /out/rootfs/sbin/init <<EOF
#!/bin/busybox sh
# UMF boot-smoke PID 1: announce success on the serial console, then halt.
/bin/busybox echo "$MARKER"
/bin/busybox sync
/bin/busybox poweroff -f
/bin/busybox sleep 30
EOF
chmod 0755 /out/rootfs/sbin/init

printf '%s' "$rel" > /out/release
INNER

# docker writes as root; hand ownership back to the caller
if [ "$(stat -c %u "$OUT/release")" != "$(id -u)" ]; then
  sudo chown -R "$(id -u):$(id -g)" "$OUT"
fi

echo "fixture: $OUT  release=$(cat "$OUT/release")"
echo "  kernel: $(du -sh "$OUT/kernel" | cut -f1)   rootfs: $(du -sh "$OUT/rootfs" | cut -f1)"
ls "$OUT/kernel/lib/modules/$(cat "$OUT/release")/kernel/drivers/block/" 2>/dev/null | grep -i virtio_blk || true
ls "$OUT/kernel/lib/modules/$(cat "$OUT/release")/kernel/fs/squashfs/" 2>/dev/null | grep -i squashfs || true
