//! Round-trip test: synthesise a minimal OCI image in a local layout,
//! push it to an in-process OCI distribution v2 server (no external
//! container runtime), pull it back into a fresh layout, and verify
//! the manifest digest survives byte-for-byte.
//!
//! Runs unconditionally — the in-process server is provided by
//! `umf_oci::test_registry` behind the `test-server` cargo feature
//! (enabled here via the `umf-oci` dev-dep). No `podman` or `docker`
//! on the host required.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;

use oci_client::Reference;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::manifest::{
    IMAGE_LAYER_MEDIA_TYPE, ImageIndexEntry, OCI_IMAGE_MEDIA_TYPE, OciDescriptor, OciImageManifest,
};
use oci_client::secrets::RegistryAuth;
use tempfile::tempdir;
use umf_oci::registry::{ImageLayout, RegistryClient, sha256_digest};
use umf_oci::test_registry::TestRegistry;

/// Synthesise a tiny OCI image directly in the layout and return its ref entry.
///
/// Returns `(reference_name, manifest_digest)`. The reference name is what we
/// pass to `RegistryClient::push` and what we recover via `lookup_ref`.
fn stage_synthetic_image(layout: &ImageLayout, ref_name: &str) -> (String, String) {
    let layer = b"hello from umf integration test\n";
    let layer_digest = layout.write_blob(layer).expect("write layer");

    let config_doc = serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "config": {},
        "rootfs": {
            "type": "layers",
            "diff_ids": [layer_digest],
        },
    });
    let config_bytes = serde_json::to_vec(&config_doc).expect("serialize config");
    let config_digest = layout.write_blob(&config_bytes).expect("write config");

    let manifest = OciImageManifest {
        schema_version: 2,
        media_type: Some(OCI_IMAGE_MEDIA_TYPE.to_string()),
        config: OciDescriptor {
            media_type: oci_client::manifest::IMAGE_CONFIG_MEDIA_TYPE.to_string(),
            digest: config_digest,
            size: config_bytes.len() as i64,
            urls: None,
            annotations: None,
        },
        layers: vec![OciDescriptor {
            media_type: IMAGE_LAYER_MEDIA_TYPE.to_string(),
            digest: layer_digest,
            size: layer.len() as i64,
            urls: None,
            annotations: None,
        }],
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let manifest_bytes = serde_json::to_vec(&manifest).expect("serialize manifest");
    let manifest_digest = sha256_digest(&manifest_bytes);
    layout
        .write_blob_with_digest(&manifest_bytes, &manifest_digest)
        .expect("write manifest blob");

    let mut annotations = BTreeMap::new();
    annotations.insert("org.imagilux.umf.test".to_string(), "true".to_string());
    layout
        .upsert_ref(
            ref_name,
            ImageIndexEntry {
                media_type: OCI_IMAGE_MEDIA_TYPE.to_string(),
                digest: manifest_digest.clone(),
                size: manifest_bytes.len() as i64,
                platform: None,
                annotations: Some(annotations),
            },
        )
        .expect("upsert ref");

    (ref_name.to_string(), manifest_digest)
}

fn http_client_for(endpoint: &str) -> RegistryClient {
    let cfg = ClientConfig {
        protocol: ClientProtocol::HttpsExcept(vec![endpoint.to_string()]),
        ..Default::default()
    };
    RegistryClient::with_config(cfg)
}

#[tokio::test]
async fn synthetic_image_round_trips_through_in_process_registry() {
    let registry = TestRegistry::start().await.expect("start test registry");
    let endpoint = registry.endpoint().to_string();
    let client = http_client_for(&endpoint);

    // ── Stage a synthetic image in a source layout ────────────────────────
    let src_dir = tempdir().expect("src tempdir");
    let src = ImageLayout::init(src_dir.path()).expect("init src");
    let local_ref = format!("{endpoint}/umf-it/round-trip:latest");
    let (_, source_manifest_digest) = stage_synthetic_image(&src, &local_ref);

    // ── Push to the local registry ────────────────────────────────────────
    let registry_ref: Reference = local_ref.parse().expect("parse reference");
    client
        .push(&registry_ref, &local_ref, &src, &RegistryAuth::Anonymous)
        .await
        .expect("push");

    // ── Pull back into a fresh layout ─────────────────────────────────────
    let dst_dir = tempdir().expect("dst tempdir");
    let dst = ImageLayout::init(dst_dir.path()).expect("init dst");
    let pulled = client
        .pull(&registry_ref, &RegistryAuth::Anonymous, &dst)
        .await
        .expect("pull");

    // ── Verify manifest digest survives byte-for-byte ─────────────────────
    assert_eq!(
        pulled.digest, source_manifest_digest,
        "round-trip should preserve the manifest digest"
    );

    // ── Verify every referenced blob is present and digest-valid in dst ──
    let manifest_bytes = dst.read_blob(&pulled.digest).expect("read pulled manifest");
    let manifest: OciImageManifest =
        serde_json::from_slice(&manifest_bytes).expect("parse pulled manifest");
    let _ = dst
        .read_blob(&manifest.config.digest)
        .expect("config blob present in dst");
    for layer in &manifest.layers {
        let _ = dst
            .read_blob(&layer.digest)
            .expect("layer blob present in dst");
    }

    // ── Verify the source layout still resolves the ref ──────────────────
    let src_entry = src.lookup_ref(&local_ref).expect("lookup").expect("found");
    assert_eq!(src_entry.digest, source_manifest_digest);

    registry.shutdown().await;
}
