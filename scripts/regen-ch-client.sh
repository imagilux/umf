#!/usr/bin/env bash
# Fallback for when the upstream `cloud-hypervisor-client` crate lags
# behind cloud-hypervisor's OpenAPI spec.
#
# Pulls the latest YAML from cloud-hypervisor/main and regenerates a
# typed Rust client via OpenAPI Generator. The output replaces the
# crate's `apis` + `models` modules; review the diff and bump the
# vendored copy in this repo (or, if upstream catches up, just bump
# the Cargo.toml dependency instead).
#
# Requires:
#   - openapi-generator-cli (npm i -g @openapitools/openapi-generator-cli)
#   - curl
#   - jq (for sanity-checking the YAML)
#
# Usage:
#   scripts/regen-ch-client.sh <output-dir>
#
# The default output dir is `target/ch-client-regen/`. Run from the
# repo root.

set -euo pipefail

YAML_URL="https://raw.githubusercontent.com/cloud-hypervisor/cloud-hypervisor/main/vmm/src/api/openapi/cloud-hypervisor.yaml"
OUT_DIR="${1:-target/ch-client-regen}"

if ! command -v openapi-generator-cli >/dev/null 2>&1; then
    echo "openapi-generator-cli not on PATH" >&2
    echo "  npm install -g @openapitools/openapi-generator-cli" >&2
    exit 1
fi

mkdir -p "$OUT_DIR"
spec="$OUT_DIR/cloud-hypervisor.yaml"

echo "==> Fetching upstream OpenAPI spec"
curl -fsSL "$YAML_URL" -o "$spec"
echo "    saved to $spec"

echo "==> Regenerating Rust client"
openapi-generator-cli generate \
    -g rust \
    -i "$spec" \
    -o "$OUT_DIR/client" \
    --additional-properties=preferUnsignedInt=true,supportAsync=true,bestFitInt=true

echo
echo "==> Done. Generated client at: $OUT_DIR/client"
echo
echo "Next steps:"
echo "  1. Diff against the published cloud-hypervisor-client crate to"
echo "     see what's new in upstream."
echo "  2. If upstream picked up the spec changes, just bump the"
echo "     dependency in Cargo.toml."
echo "  3. If we need to ship faster than upstream's release cadence,"
echo "     vendor the generated client into the workspace (e.g."
echo "     crates/cloud-hypervisor-client-vendored/) and switch the"
echo "     dep to a path = ... reference."
