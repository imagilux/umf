#!/usr/bin/env bash
# Guardrail: no `Command::new("<runtime>")` calls outside the one allowed
# helper for each VMM, and no calls to forbidden runtimes at all.
#
# Issue #107 locks this in as a CI step so the "single spawn helper per
# backend" contract from #101 can't quietly erode.

set -eu

fail=0

report() {
    if [ -n "$1" ]; then
        echo "==> $2"
        printf '%s\n' "$1"
        fail=1
    fi
}

# Forbidden runtimes — UMF must not shell out to any of these. The
# OCI container runtime is linked-in via libcontainer (umf-engine);
# the VMM control surfaces are `umf-vmm` backends. None of these
# binaries should ever be invoked from our source.
forbidden_runtimes=$(
    grep -rEn --include='*.rs' 'Command::new\("(runc|crun|youki|docker|podman)"\)' \
        crates/ src/ tests/ 2>/dev/null || true
)
report "$forbidden_runtimes" "forbidden runtime invocation"

# QEMU spawn is allowed only inside `crates/umf-vmm/src/backends/qemu/spawn.rs`.
forbidden_qemu=$(
    grep -rEn --include='*.rs' 'Command::new\("qemu-system-[A-Za-z0-9_]+"\)' \
        crates/ src/ tests/ 2>/dev/null \
        | grep -v 'crates/umf-vmm/src/backends/qemu/spawn.rs' || true
)
report "$forbidden_qemu" "qemu-system Command::new outside the umf-vmm qemu spawn helper"

# Cloud Hypervisor spawn is allowed only inside `crates/umf-vmm/src/backends/cloud_hypervisor/spawn.rs`.
forbidden_ch=$(
    grep -rEn --include='*.rs' 'Command::new\("cloud-hypervisor"\)' \
        crates/ src/ tests/ 2>/dev/null \
        | grep -v 'crates/umf-vmm/src/backends/cloud_hypervisor/spawn.rs' || true
)
report "$forbidden_ch" "cloud-hypervisor Command::new outside the umf-vmm cloud_hypervisor spawn helper"

if [ "$fail" -ne 0 ]; then
    cat >&2 <<'EOF'

The subprocess guardrail rejected the changes above.

UMF locks in two invariants:

  1. Container runtimes (runc, crun, youki, docker, podman) are
     never invoked as subprocess — the OCI runtime is linked-in
     via libcontainer (see umf-engine).
  2. The two supported VMM binaries (qemu-system-*, cloud-hypervisor)
     are spawned in exactly one place each — their per-backend
     `spawn.rs` helper. Everything after spawn goes through the
     typed Rust control surface (qapi for QEMU, cloud-hypervisor-client
     for CH).

If you need to invoke a VMM, route through the existing helpers in
`crates/umf-vmm/src/backends/`. If you need to add a new VMM backend,
create `backends/<name>/spawn.rs` and add it to the allowlist above.

EOF
    exit 1
fi

echo "subprocess-calls check: clean"
