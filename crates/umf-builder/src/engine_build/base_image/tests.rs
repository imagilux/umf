//! Unit tests for the `base_image` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use oci_client::manifest::{
    IMAGE_LAYER_MEDIA_TYPE, ImageIndexEntry, OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MEDIA_TYPE,
    OciDescriptor, OciImageIndex, Platform,
};
use oci_spec::image::{Arch, Os};
use serde_json::json;
use tempfile::tempdir;
use umf_oci::registry::layout::sha256_digest;

const IMAGE_CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.image.config.v1+json";

/// Stage a single-arch child image (config + `layers` real layer blobs +
/// manifest) into `layout` and return the index entry pointing at it, with
/// `platform` set to `arch`. The arch is recorded in the OCI config's
/// `architecture` field and `gen` makes layer payloads unique per child so
/// no two children share a blob digest.
fn stage_child(layout: &ImageLayout, arch: &str, layers: usize, generation: u8) -> ImageIndexEntry {
    let diff_ids: Vec<String> = (0..layers)
        .map(|i| format!("sha256:diff-{arch}-{generation}-{i}"))
        .collect();
    let config_doc = json!({
        "architecture": arch,
        "os": "linux",
        "config": {},
        "rootfs": { "type": "layers", "diff_ids": diff_ids },
    });
    let config_bytes = serde_json::to_vec(&config_doc).expect("serialize config");
    let config_digest = layout.write_blob(&config_bytes).expect("write config");

    let mut layer_descriptors = Vec::with_capacity(layers);
    for i in 0..layers {
        let payload = format!("layer-{arch}-{generation}-{i}\n").into_bytes();
        let digest = layout.write_blob(&payload).expect("write layer");
        layer_descriptors.push(OciDescriptor {
            media_type: IMAGE_LAYER_MEDIA_TYPE.to_string(),
            digest,
            size: payload.len() as i64,
            urls: None,
            annotations: None,
        });
    }

    let manifest = OciImageManifest {
        schema_version: 2,
        media_type: Some(OCI_IMAGE_MEDIA_TYPE.to_string()),
        config: OciDescriptor {
            media_type: IMAGE_CONFIG_MEDIA_TYPE.to_string(),
            digest: config_digest,
            size: config_bytes.len() as i64,
            urls: None,
            annotations: None,
        },
        layers: layer_descriptors,
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let manifest_bytes = serde_json::to_vec(&manifest).expect("serialize manifest");
    let manifest_digest = sha256_digest(&manifest_bytes);
    layout
        .write_blob_with_digest(&manifest_bytes, &manifest_digest)
        .expect("write manifest");

    ImageIndexEntry {
        media_type: OCI_IMAGE_MEDIA_TYPE.to_string(),
        digest: manifest_digest,
        size: manifest_bytes.len() as i64,
        platform: Some(Platform {
            architecture: Arch::from(arch),
            os: Os::Linux,
            os_version: None,
            os_features: None,
            variant: None,
            features: None,
        }),
        annotations: None,
    }
}

/// Stage a multi-arch index (amd64 + arm64 children, with *different* layer
/// counts so the selected child is identifiable) under `ref_name`.
fn stage_multiarch_index(layout: &ImageLayout, ref_name: &str) {
    // amd64 child has 1 layer; arm64 child has 2, so the layer count tells
    // us which manifest resolution picked.
    let amd64 = stage_child(layout, "amd64", 1, 0);
    let arm64 = stage_child(layout, "arm64", 2, 1);
    let index = OciImageIndex {
        schema_version: 2,
        media_type: Some(OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
        manifests: vec![amd64, arm64],
        artifact_type: None,
        annotations: None,
    };
    let bytes = serde_json::to_vec(&index).expect("serialize index");
    let digest = layout.write_blob(&bytes).expect("write index");
    layout
        .upsert_ref(
            ref_name,
            ImageIndexEntry {
                media_type: OCI_IMAGE_INDEX_MEDIA_TYPE.to_string(),
                digest,
                size: bytes.len() as i64,
                platform: None,
                annotations: None,
            },
        )
        .expect("upsert ref");
}

#[test]
fn index_selection_picks_requested_arch() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let ref_name = "example.invalid/multi:latest";
    stage_multiarch_index(&layout, ref_name);

    // amd64 → the 1-layer child, config arch amd64.
    let amd = resolve_base_image(&layout, ref_name, Architecture::X86_64).expect("resolve amd64");
    assert_eq!(amd.layers.len(), 1, "expected the amd64 (1-layer) manifest");
    assert_eq!(amd.config.architecture, "amd64");

    // arm64 → the 2-layer child, config arch arm64 (the target arch is
    // stamped on the emitted image even though the base agreed).
    let arm = resolve_base_image(&layout, ref_name, Architecture::Aarch64).expect("resolve arm64");
    assert_eq!(arm.layers.len(), 2, "expected the arm64 (2-layer) manifest");
    assert_eq!(arm.config.architecture, "arm64");
}

#[test]
fn index_selection_errors_when_arch_absent() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let ref_name = "example.invalid/amd64only:latest";
    // Index with only an amd64 child.
    let amd64 = stage_child(&layout, "amd64", 1, 0);
    let index = OciImageIndex {
        schema_version: 2,
        media_type: Some(OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
        manifests: vec![amd64],
        artifact_type: None,
        annotations: None,
    };
    let bytes = serde_json::to_vec(&index).expect("serialize index");
    let digest = layout.write_blob(&bytes).expect("write index");
    layout
        .upsert_ref(
            ref_name,
            ImageIndexEntry {
                media_type: OCI_IMAGE_INDEX_MEDIA_TYPE.to_string(),
                digest,
                size: bytes.len() as i64,
                platform: None,
                annotations: None,
            },
        )
        .expect("upsert ref");

    // Requesting arm64 must error, not silently fall back to amd64.
    // (`BaseImage` isn't `Debug`, so match the Result directly rather
    // than `expect_err`.)
    match resolve_base_image(&layout, ref_name, Architecture::Aarch64) {
        Err(EngineBuildError::NoManifestForPlatform { arch }) => assert_eq!(arch, "arm64"),
        Err(other) => panic!("expected NoManifestForPlatform, got {other:?}"),
        Ok(_) => panic!("expected an error for an unpublished arch, got Ok"),
    }
}
