//! OCI image-spec conformance gate (OCI-1).
//!
//! Turns the "≈95% of the image surface" *estimate* in the OCI-compliance
//! brief into a *measured*, CI-enforced check: every JSON document the
//! [`umf_oci`] emission API produces — image manifest, image config, and image
//! index (the on-disk `index.json`) — is validated against the official OCI
//! image-spec JSON schemas.
//!
//! The schemas are vendored under `tests/fixtures/schemas/` (pinned to
//! image-spec `v1.1.1`; see the README there). They are draft-04 and `$ref`
//! each other by relative filename (`defs-descriptor.json#/...`,
//! `content-descriptor.json`, …). A basename-keyed [`Retrieve`] retriever
//! resolves those references against the vendored copies, so validation needs
//! no network and runs unconditionally — matching UMF's offline / air-gapped
//! invariant.
//!
//! ## Two tracks (per the brief)
//!
//! 1. **Schema validation** — [`emitted_manifest_config_and_index_match_image_spec`]
//!    and friends. Run unconditionally; this is the always-on CI gate
//!    (`cargo test -p umf-oci --test conformance_image_spec`).
//! 2. **External round-trip** — [`emitted_layout_round_trips_through_skopeo`].
//!    Runs `skopeo copy oci:<layout>:<tag> dir:…` over a real emitted layout,
//!    **gated on the tool being present**: it skips with a one-line `eprintln!`
//!    when `skopeo` is not on `PATH` (mirroring the `UMF_ENGINE_SMOKE` gating
//!    used by the engine smoke tests). CI installs skopeo so the round-trip
//!    actually exercises there.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use bytes::Bytes;
use jsonschema::{Retrieve, Uri, Validator};
use oci_client::manifest::{ImageIndexEntry, OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MEDIA_TYPE};
use serde_json::{Value, json};
use tempfile::TempDir;
use umf_core::l0::L0Kind;
use umf_oci::image::{
    ArtifactBlob, ContainerConfig, ImageConfig, LayerSource, emit_artifact_manifest, emit_image,
    subject_from_entry,
};
use umf_oci::registry::{ImageLayout, ReferrersIndex};

// ── Vendored schemas ─────────────────────────────────────────────────────────

const MANIFEST_SCHEMA: &str = include_str!("fixtures/schemas/image-manifest-schema.json");
const CONFIG_SCHEMA: &str = include_str!("fixtures/schemas/config-schema.json");
const INDEX_SCHEMA: &str = include_str!("fixtures/schemas/image-index-schema.json");
const CONTENT_DESCRIPTOR_SCHEMA: &str = include_str!("fixtures/schemas/content-descriptor.json");
const DEFS_SCHEMA: &str = include_str!("fixtures/schemas/defs.json");
const DEFS_DESCRIPTOR_SCHEMA: &str = include_str!("fixtures/schemas/defs-descriptor.json");

/// Resolves the image-spec schemas' relative `$ref`s (e.g.
/// `defs-descriptor.json#/definitions/mediaType`) against the vendored copies,
/// keyed by basename.
///
/// The schemas declare draft-04 `id`s like
/// `https://opencontainers.org/schema/image/manifest`, so a relative reference
/// resolves to an absolute `https://opencontainers.org/schema/<file>.json`
/// URI. Rather than reproduce RFC-3986 base resolution per schema, the
/// retriever keys on the final path segment — every cross-file reference in
/// these schemas is by bare filename, so the basename uniquely identifies the
/// target document. Returning the vendored bytes keeps validation fully
/// offline.
struct VendoredSchemas {
    by_basename: HashMap<&'static str, Value>,
}

impl VendoredSchemas {
    fn new() -> Self {
        let mut by_basename = HashMap::new();
        for (name, body) in [
            ("image-manifest-schema.json", MANIFEST_SCHEMA),
            ("config-schema.json", CONFIG_SCHEMA),
            ("image-index-schema.json", INDEX_SCHEMA),
            ("content-descriptor.json", CONTENT_DESCRIPTOR_SCHEMA),
            ("defs.json", DEFS_SCHEMA),
            ("defs-descriptor.json", DEFS_DESCRIPTOR_SCHEMA),
        ] {
            let value: Value = serde_json::from_str(body)
                .unwrap_or_else(|e| panic!("vendored {name} is invalid JSON: {e}"));
            by_basename.insert(name, value);
        }
        Self { by_basename }
    }
}

impl Retrieve for VendoredSchemas {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let full = uri.as_str();
        let basename = full.rsplit('/').next().unwrap_or(full);
        self.by_basename.get(basename).cloned().ok_or_else(|| {
            format!("no vendored schema for reference {full:?} (basename {basename:?})").into()
        })
    }
}

/// Compile a draft-04 [`Validator`] for `schema_src`, wiring the vendored
/// retriever so cross-file `$ref`s resolve offline.
fn validator_for(schema_src: &str) -> Validator {
    let schema: Value = serde_json::from_str(schema_src).expect("schema is valid JSON");
    jsonschema::draft4::options()
        .with_retriever(VendoredSchemas::new())
        .build(&schema)
        .expect("compile draft-04 schema")
}

/// Assert `instance` validates against `validator`, surfacing every error.
fn assert_valid(validator: &Validator, instance: &Value, what: &str) {
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("  at {}: {e}", e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{what} does not satisfy its OCI image-spec schema:\n{}\n--- instance ---\n{}",
        errors.join("\n"),
        serde_json::to_string_pretty(instance).unwrap_or_default(),
    );
}

// ── Emission helpers ─────────────────────────────────────────────────────────

/// Build a tiny layer from a throwaway directory (mirrors the unit tests).
fn tiny_layer() -> (TempDir, LayerSource) {
    let src = TempDir::new().expect("src tempdir");
    std::fs::create_dir_all(src.path().join("bin")).expect("mkdir bin");
    std::fs::write(src.path().join("bin/hello"), b"echo hi\n").expect("write hello");
    std::fs::write(src.path().join("README"), b"hello, umf\n").expect("write README");
    let layer = LayerSource::from_directory(src.path()).expect("build layer");
    (src, layer)
}

/// Emit a container image carrying a representative `config` sub-object
/// (entrypoint / env / exposed ports / labels) so the config schema's `config`
/// branch is exercised, not just the bare required fields.
fn emit_container(layout: &ImageLayout, ref_name: &str) -> ImageIndexEntry {
    let (_src, layer) = tiny_layer();
    let cfg = ImageConfig {
        container: ContainerConfig {
            user: Some("nobody".to_string()),
            env: vec!["PATH=/usr/local/bin:/usr/bin".to_string()],
            entrypoint: Some(vec!["/bin/hello".to_string()]),
            cmd: Some(vec!["--help".to_string()]),
            working_dir: Some("/srv".to_string()),
            exposed_ports: vec!["80/tcp".to_string(), "443/tcp".to_string()],
            volumes: vec!["/data".to_string()],
            stop_signal: Some("SIGTERM".to_string()),
            labels: [(
                "org.opencontainers.image.title".to_string(),
                "umf-conformance".to_string(),
            )]
            .into_iter()
            .collect(),
        },
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };
    emit_image(layout, std::slice::from_ref(&layer), &cfg, ref_name).expect("emit container")
}

/// Emit a `type=bootable` image (a kernel-backed UMF artifact) so the
/// conformance gate covers UMF's additive shape, which still has to be a
/// spec-valid OCI image.
fn emit_bootable(layout: &ImageLayout, ref_name: &str) -> ImageIndexEntry {
    let (_src, layer) = tiny_layer();
    let cfg = ImageConfig {
        umf_type: L0Kind::Bootable,
        ..ImageConfig::default()
    };
    emit_image(layout, std::slice::from_ref(&layer), &cfg, ref_name).expect("emit bootable")
}

/// Read a manifest descriptor's blob and parse it as JSON.
fn read_json_blob(layout: &ImageLayout, digest: &str) -> Value {
    let bytes = layout.read_blob(digest).expect("read blob");
    serde_json::from_slice(&bytes).expect("blob is JSON")
}

// ── Track 1: schema validation (unconditional CI gate) ───────────────────────

#[test]
fn emitted_manifest_config_and_index_match_image_spec() {
    let manifest_v = validator_for(MANIFEST_SCHEMA);
    let config_v = validator_for(CONFIG_SCHEMA);
    let index_v = validator_for(INDEX_SCHEMA);

    let dir = TempDir::new().expect("layout tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init layout");

    // Emit two shapes into one layout.
    let container = emit_container(&layout, "example.invalid/app:1");
    let bootable = emit_bootable(&layout, "example.invalid/os:1");

    for (entry, what) in [(&container, "container"), (&bootable, "bootable")] {
        // Manifest.
        let manifest = read_json_blob(&layout, &entry.digest);
        assert_eq!(
            manifest.get("schemaVersion").and_then(Value::as_u64),
            Some(2),
            "{what} manifest schemaVersion"
        );
        assert_valid(&manifest_v, &manifest, &format!("{what} manifest"));

        // Config blob the manifest points at.
        let config_digest = manifest
            .get("config")
            .and_then(|c| c.get("digest"))
            .and_then(Value::as_str)
            .expect("manifest.config.digest");
        let config = read_json_blob(&layout, config_digest);
        assert_valid(&config_v, &config, &format!("{what} config"));
    }

    // The on-disk index.json is itself an OCI image index.
    let index_bytes = std::fs::read(dir.path().join("index.json")).expect("read index.json");
    let index: Value = serde_json::from_slice(&index_bytes).expect("index.json is JSON");
    assert_valid(&index_v, &index, "on-disk index.json");
}

#[test]
fn authored_multi_arch_index_matches_image_spec() {
    // Beyond the tag-index that `emit_image` writes, validate a hand-authored
    // multi-arch image index carrying `platform` descriptors — the shape OCI-3
    // will emit. This exercises the index schema's per-entry descriptor +
    // platform branches, which the (single, untagged-platform) on-disk index
    // does not.
    let index_v = validator_for(INDEX_SCHEMA);

    let dir = TempDir::new().expect("layout tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init layout");
    let amd64 = emit_container(&layout, "example.invalid/multi:amd64");
    let arm64 = emit_container(&layout, "example.invalid/multi:arm64");

    // Authored as raw JSON (not via the typed `OciImageIndex`) so the test
    // asserts on the exact wire shape and stays free of `oci-spec`'s
    // `Platform` / `Arch` / `Os` types, which `umf-oci` does not depend on.
    let index = json!({
        "schemaVersion": 2,
        "mediaType": OCI_IMAGE_INDEX_MEDIA_TYPE,
        "manifests": [
            {
                "mediaType": OCI_IMAGE_MEDIA_TYPE,
                "digest": amd64.digest,
                "size": amd64.size,
                "platform": { "architecture": "amd64", "os": "linux" }
            },
            {
                "mediaType": OCI_IMAGE_MEDIA_TYPE,
                "digest": arm64.digest,
                "size": arm64.size,
                "platform": { "architecture": "arm64", "os": "linux", "variant": "v8" }
            }
        ]
    });
    assert_valid(&index_v, &index, "authored multi-arch index");
}

#[test]
fn emitted_artifact_manifest_matches_image_spec() {
    // OCI-4a: an artifact manifest (empty-JSON config + `artifactType` +
    // `subject`) is still an image manifest and must satisfy the same schema —
    // including the OCI 1.1 fields none of the other emitted shapes set.
    let manifest_v = validator_for(MANIFEST_SCHEMA);
    let index_v = validator_for(INDEX_SCHEMA);

    let dir = TempDir::new().expect("layout tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init layout");
    let subject = emit_container(&layout, "example.invalid/app:1");

    let blob = ArtifactBlob {
        media_type: "application/spdx+json".to_string(),
        data: Bytes::from_static(b"{\"spdxVersion\":\"SPDX-2.3\"}"),
        annotations: None,
    };
    let artifact = emit_artifact_manifest(
        &layout,
        "application/spdx+json",
        Some(&subject_from_entry(&subject)),
        std::slice::from_ref(&blob),
        None,
        Some("example.invalid/app-sbom:1"),
    )
    .expect("emit artifact");

    let manifest = read_json_blob(&layout, &artifact.digest);
    assert_valid(&manifest_v, &manifest, "artifact manifest");
    assert_eq!(
        manifest.get("artifactType").and_then(Value::as_str),
        Some("application/spdx+json"),
        "artifactType is on the wire"
    );
    assert_eq!(
        manifest.pointer("/subject/digest").and_then(Value::as_str),
        Some(subject.digest.as_str()),
        "subject points at the referred manifest"
    );
    assert_eq!(
        manifest
            .pointer("/config/mediaType")
            .and_then(Value::as_str),
        Some("application/vnd.oci.empty.v1+json"),
        "config is the empty descriptor"
    );

    // The ref'd artifact lands in index.json, which must stay schema-valid.
    let index_bytes = std::fs::read(dir.path().join("index.json")).expect("read index.json");
    let index: Value = serde_json::from_slice(&index_bytes).expect("index.json is JSON");
    assert_valid(&index_v, &index, "index.json with an artifact ref");
}

#[test]
fn referrers_fallback_index_matches_image_index_schema() {
    // OCI-4b: the `<algo>-<hex>` fallback tag holds an ordinary image index
    // whose descriptors additionally carry `artifactType`. Build one from the
    // layout's local referrers listing — the same descriptors the client
    // pushes — and hold it to the index schema.
    let index_v = validator_for(INDEX_SCHEMA);

    let dir = TempDir::new().expect("layout tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init layout");
    let subject = emit_container(&layout, "example.invalid/app:1");
    let artifact = emit_artifact_manifest(
        &layout,
        "application/spdx+json",
        Some(&subject_from_entry(&subject)),
        &[],
        None,
        None,
    )
    .expect("emit artifact");

    let mut fallback = ReferrersIndex::empty();
    for descriptor in layout
        .list_referrers(&subject.digest, None)
        .expect("local referrers listing")
    {
        fallback.upsert(descriptor);
    }
    assert_eq!(fallback.manifests.len(), 1);
    assert_eq!(fallback.manifests[0].digest, artifact.digest);

    let doc = serde_json::to_value(&fallback).expect("serialize fallback index");
    assert_valid(&index_v, &doc, "referrers fallback index");
    assert_eq!(
        doc.pointer("/manifests/0/artifactType")
            .and_then(Value::as_str),
        Some("application/spdx+json"),
        "fallback descriptors carry artifactType on the wire",
    );
}

#[test]
fn schema_rejects_a_malformed_manifest() {
    // Guard against a vacuous validator: a manifest missing the required
    // `layers` array (and with a bad schemaVersion) must fail. If this ever
    // passes, the retriever or draft selection is silently a no-op.
    let manifest_v = validator_for(MANIFEST_SCHEMA);
    let bad = json!({
        "schemaVersion": 1,
        "config": { "mediaType": "application/vnd.oci.image.config.v1+json" }
    });
    assert!(
        manifest_v.validate(&bad).is_err(),
        "a manifest with schemaVersion 1, a malformed config descriptor, and no layers must be rejected"
    );
}

// ── Track 2: external round-trip (gated on tool presence) ────────────────────

/// Path to a tool on `PATH`, or `None`.
fn which(tool: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|candidate| candidate.is_file())
}

#[test]
fn emitted_layout_round_trips_through_skopeo() {
    // `skopeo copy oci:<layout>:<tag> dir:<out>` is the canonical external read
    // of an OCI image layout: it parses index.json, resolves the tag to a
    // manifest, fetches the config and every layer blob, and verifies each
    // descriptor's digest + size as it copies. A clean exit is a strong
    // structural-conformance signal from a tool that is not UMF.
    //
    // Gated on `skopeo` being on `PATH` (mirroring the `UMF_ENGINE_SMOKE`
    // gating): the CI workflow installs it (apt) so it runs there; locally it
    // skips with one line when absent.
    let Some(skopeo) = which("skopeo") else {
        eprintln!(
            "skipping OCI round-trip: `skopeo` not on PATH \
             (install it to exercise an external read of the emitted layout)"
        );
        return;
    };

    let dir = TempDir::new().expect("layout tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init layout");
    let _ = emit_container(&layout, "example.invalid/roundtrip:1");
    let layout_path = dir.path().to_str().expect("utf-8 layout path");

    let out = TempDir::new().expect("skopeo out tempdir");
    let status = run(
        &skopeo,
        &[
            "copy",
            &format!("oci:{layout_path}:example.invalid/roundtrip:1"),
            &format!("dir:{}", out.path().display()),
        ],
        "skopeo copy",
    );
    assert!(status, "skopeo copy must succeed over the emitted layout");
}

/// Run `bin args…`, streaming stdout/stderr into the test log, returning
/// whether it exited 0.
fn run(bin: &Path, args: &[&str], label: &str) -> bool {
    match Command::new(bin).args(args).output() {
        Ok(output) => {
            if !output.stdout.is_empty() {
                eprintln!(
                    "[{label} stdout]\n{}",
                    String::from_utf8_lossy(&output.stdout)
                );
            }
            if !output.stderr.is_empty() {
                eprintln!(
                    "[{label} stderr]\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            output.status.success()
        }
        Err(e) => {
            eprintln!("[{label}] failed to spawn {}: {e}", bin.display());
            false
        }
    }
}

#[test]
fn cross_file_ref_resolution_enforces_digest_pattern() {
    // The subtle part of this gate is resolving the schemas' relative,
    // cross-file `$ref`s (digest/mediaType patterns live in
    // `defs-descriptor.json`, not in the manifest schema) through the vendored
    // retriever. Feed a manifest whose config-descriptor `digest` violates that
    // pattern (no `algo:` prefix): if the `$ref` silently didn't resolve, the
    // constraint would be absent and this would wrongly pass. Keeping it as a
    // standing test means a future jsonschema/retriever regression that turns
    // the gate vacuous fails loudly here.
    let manifest_v = validator_for(MANIFEST_SCHEMA);
    let bad = json!({
        "schemaVersion": 2,
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "size": 1,
            "digest": "not-a-valid-digest"
        },
        "layers": [
            { "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "size": 1, "digest": "sha256:abc" }
        ]
    });
    assert!(
        manifest_v.validate(&bad).is_err(),
        "digest pattern from defs-descriptor.json must be enforced via the cross-file $ref"
    );
}
