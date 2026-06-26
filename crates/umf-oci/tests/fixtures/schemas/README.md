# Vendored OCI image-spec JSON schemas

These files are an unmodified copy of the JSON-schema documents from the
[OCI image-spec][image-spec] `schema/` directory. They are vendored (rather
than fetched at test time) so the conformance test in
`crates/umf-oci/tests/conformance_image_spec.rs` runs unconditionally and
offline: byte-reproducibility and air-gapped buildability are both house
invariants, and a network dependency in the test gate would violate them.

## Pin

| Field | Value |
|-------|-------|
| Upstream | <https://github.com/opencontainers/image-spec> |
| Source path | `schema/` |
| Tag | `v1.1.1` |
| Commit | `147f9c13cedb47a0c4d9a11a222961073d585877` |
| Fetched | 2026-05-31 |

The schemas target [JSON Schema draft-04][draft4] (declared via their
`$schema` / `id` fields).

## Files

| File | OCI document it validates |
|------|---------------------------|
| `image-manifest-schema.json` | image manifest (`application/vnd.oci.image.manifest.v1+json`) |
| `config-schema.json` | image config (`application/vnd.oci.image.config.v1+json`) |
| `image-index-schema.json` | image index (`application/vnd.oci.image.index.v1+json`) — also the on-disk `index.json` |
| `content-descriptor.json` | a content descriptor (referenced by the three above) |
| `defs.json` | shared primitive definitions (`int64`, `mapStringString`, …) |
| `defs-descriptor.json` | descriptor-specific definitions (`mediaType`, `digest`, …) |
| `image-layout-schema.json` | the `oci-layout` marker file |

`content-descriptor.json`, `defs.json`, and `defs-descriptor.json` are not
validated directly; the manifest / config / index schemas `$ref` into them by
relative filename. The test resolves those relative references against the
vendored copies via a basename-keyed `jsonschema::Retrieve` retriever, so no
network fetch happens during validation.

## Refreshing the pin

Re-fetch every file in this directory from the same `schema/` path at the new
tag, update the **Pin** table above, then run
`cargo test -p umf-oci --test conformance_image_spec` and confirm it stays
green. Treat any schema tightening that the emitter then fails as a real
finding, not a fixture to relax.

[image-spec]: https://github.com/opencontainers/image-spec
[draft4]: https://json-schema.org/specification-links#draft-4
