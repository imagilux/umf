//! Unit tests for OCI 1.1 artifact-manifest emission.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;

use tempfile::tempdir;

use super::*;
use crate::image::{ImageConfig, LayerSource, emit_image};

/// The spec-fixed digest of the canonical empty-JSON blob (`{}`).
const WELL_KNOWN_EMPTY_DIGEST: &str =
    "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";

/// Emit a minimal subject image for artifacts to refer to.
fn emit_subject_image(layout: &ImageLayout) -> ImageIndexEntry {
    let src = tempdir().expect("src tempdir");
    std::fs::write(src.path().join("hello"), b"hi\n").expect("write file");
    let layer = LayerSource::from_directory(src.path()).expect("layer");
    emit_image(
        layout,
        std::slice::from_ref(&layer),
        &ImageConfig::default(),
        "example.invalid/subject:1",
    )
    .expect("emit subject")
}

fn sbom_blob() -> ArtifactBlob {
    ArtifactBlob {
        media_type: "application/spdx+json".to_string(),
        data: Bytes::from_static(b"{\"spdxVersion\":\"SPDX-2.3\"}"),
        annotations: None,
    }
}

#[test]
fn emit_writes_subject_artifact_type_and_empty_config() {
    let dir = tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let subject = emit_subject_image(&layout);

    let blob = sbom_blob();
    let entry = emit_artifact_manifest(
        &layout,
        "application/spdx+json",
        Some(&subject_from_entry(&subject)),
        std::slice::from_ref(&blob),
        None,
        Some("example.invalid/subject-sbom:1"),
    )
    .expect("emit artifact");

    let manifest: OciImageManifest =
        serde_json::from_slice(&layout.read_blob(&entry.digest).expect("manifest blob"))
            .expect("parse manifest");

    assert_eq!(
        manifest.artifact_type.as_deref(),
        Some("application/spdx+json")
    );
    let subject_desc = manifest.subject.expect("subject present");
    assert_eq!(subject_desc.digest, subject.digest);
    assert_eq!(subject_desc.size, subject.size);

    assert_eq!(manifest.config.media_type, EMPTY_JSON_MEDIA_TYPE);
    assert_eq!(manifest.config.digest, WELL_KNOWN_EMPTY_DIGEST);
    assert_eq!(manifest.config.size, 2);
    assert_eq!(
        layout.read_blob(&manifest.config.digest).expect("config"),
        b"{}"
    );

    assert_eq!(manifest.layers.len(), 1);
    assert_eq!(manifest.layers[0].media_type, "application/spdx+json");
    assert_eq!(
        Bytes::from(layout.read_blob(&manifest.layers[0].digest).expect("blob")),
        blob.data,
        "artifact blob bytes round-trip verbatim",
    );
}

#[test]
fn content_less_artifact_uses_the_empty_descriptor_layer() {
    let dir = tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");

    let entry = emit_artifact_manifest(
        &layout,
        "application/vnd.example.marker.v1",
        None,
        &[],
        None,
        None,
    )
    .expect("emit content-less artifact");

    let manifest: OciImageManifest =
        serde_json::from_slice(&layout.read_blob(&entry.digest).expect("manifest blob"))
            .expect("parse manifest");
    assert_eq!(manifest.layers.len(), 1, "spec requires one layers entry");
    assert_eq!(manifest.layers[0].media_type, EMPTY_JSON_MEDIA_TYPE);
    assert_eq!(manifest.layers[0].digest, WELL_KNOWN_EMPTY_DIGEST);
    assert!(manifest.subject.is_none());
}

#[test]
fn emission_is_byte_reproducible_for_identical_inputs() {
    let annotations: BTreeMap<String, String> = [
        ("org.example.b".to_string(), "2".to_string()),
        ("org.example.a".to_string(), "1".to_string()),
    ]
    .into_iter()
    .collect();
    let blob = ArtifactBlob {
        annotations: Some(annotations.clone()),
        ..sbom_blob()
    };

    let emit_once = || {
        let dir = tempdir().expect("layout tempdir");
        let layout = ImageLayout::init(dir.path()).expect("init");
        let subject = emit_subject_image(&layout);
        emit_artifact_manifest(
            &layout,
            "application/spdx+json",
            Some(&subject_from_entry(&subject)),
            std::slice::from_ref(&blob),
            Some(&annotations),
            None,
        )
        .expect("emit artifact")
        .digest
    };

    assert_eq!(
        emit_once(),
        emit_once(),
        "artifact manifest digest must match across runs with identical inputs",
    );
}

#[test]
fn malformed_media_types_are_rejected() {
    let dir = tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");

    for bad in ["sbom", "", "application/", "/json", "a/b/c", "a b/json"] {
        let err = emit_artifact_manifest(&layout, bad, None, &[], None, None)
            .expect_err("bare artifactType must be rejected");
        assert!(
            matches!(err, RegistryError::InvalidMediaType(_)),
            "unexpected error for {bad:?}: {err}",
        );
    }

    let bad_blob = ArtifactBlob {
        media_type: "no-slash".to_string(),
        ..sbom_blob()
    };
    let err = emit_artifact_manifest(
        &layout,
        "application/spdx+json",
        None,
        std::slice::from_ref(&bad_blob),
        None,
        None,
    )
    .expect_err("blob media type must be validated");
    assert!(matches!(err, RegistryError::InvalidMediaType(_)));
}

#[test]
fn untagged_emission_registers_an_untagged_index_entry() {
    let dir = tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");

    let entry = emit_artifact_manifest(
        &layout,
        "application/vnd.example.marker.v1",
        None,
        &[],
        None,
        None,
    )
    .expect("emit untagged artifact");
    // Re-emitting the identical artifact must not duplicate the entry.
    emit_artifact_manifest(
        &layout,
        "application/vnd.example.marker.v1",
        None,
        &[],
        None,
        None,
    )
    .expect("re-emit untagged artifact");

    let index: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("index.json")).expect("read index.json"),
    )
    .expect("index.json is JSON");
    let manifests = index
        .get("manifests")
        .and_then(serde_json::Value::as_array)
        .expect("manifests array");
    assert_eq!(
        manifests.len(),
        1,
        "untagged emission registers exactly one entry (deduped by digest)",
    );
    assert_eq!(
        manifests[0]
            .get("digest")
            .and_then(serde_json::Value::as_str),
        Some(entry.digest.as_str()),
    );
    assert!(
        manifests[0]
            .pointer("/annotations/org.opencontainers.image.ref.name")
            .is_none(),
        "untagged entry carries no ref-name annotation",
    );
    assert!(
        layout.list_refs().expect("list refs").is_empty(),
        "untagged artifacts stay out of the ref table",
    );
}
