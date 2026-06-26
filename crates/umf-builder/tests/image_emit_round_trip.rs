//! End-to-end: build a layer from a directory tree, emit an image via
//! the producer path (`emit_image`), push it through an in-process OCI
//! distribution v2 server, pull it back into a fresh layout, and
//! verify the manifest digest + UMF labels survive the round trip.
//!
//! Runs unconditionally — the in-process server is provided by
//! `umf_oci::test_registry`. No `podman` or `docker` on the host
//! required.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;

use oci_client::Reference;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::manifest::OciImageManifest;
use oci_client::secrets::RegistryAuth;
use tempfile::tempdir;
use umf_builder::introspect::introspect;
use umf_core::l0::L0Kind;
use umf_core::label;
use umf_oci::image::{ContainerConfig, ImageConfig, LayerSource, emit_image};
use umf_oci::registry::{ImageLayout, RegistryClient};
use umf_oci::test_registry::TestRegistry;

fn http_client_for(endpoint: &str) -> RegistryClient {
    let cfg = ClientConfig {
        protocol: ClientProtocol::HttpsExcept(vec![endpoint.to_string()]),
        ..Default::default()
    };
    RegistryClient::with_config(cfg)
}

fn populate_tree(root: &std::path::Path) {
    fs::create_dir_all(root.join("usr/local/bin")).expect("mkdir bin");
    fs::write(
        root.join("usr/local/bin/hello"),
        b"#!/bin/sh\necho hello from umf\n",
    )
    .expect("write hello");
    fs::write(root.join("etc/hello.conf"), b"greeting=hi\n")
        .or_else(|_| {
            fs::create_dir_all(root.join("etc")).expect("mkdir etc");
            fs::write(root.join("etc/hello.conf"), b"greeting=hi\n")
        })
        .expect("write hello.conf");
}

#[tokio::test]
async fn emit_then_push_then_pull_preserves_manifest_digest() {
    let registry = TestRegistry::start().await.expect("start test registry");
    let endpoint = registry.endpoint().to_string();

    // ── Build a layer from a tree ─────────────────────────────────────────
    let tree = tempdir().expect("tree tempdir");
    populate_tree(tree.path());
    let layer = LayerSource::from_directory(tree.path()).expect("build layer");

    // ── Emit an image into a producer-side layout ─────────────────────────
    let producer_dir = tempdir().expect("producer tempdir");
    let producer = ImageLayout::init(producer_dir.path()).expect("init producer");

    let ref_name = format!("{endpoint}/umf-it/emit-round-trip:latest");

    let cfg = ImageConfig {
        container: ContainerConfig {
            entrypoint: Some(vec!["/usr/local/bin/hello".to_string()]),
            env: vec!["PATH=/usr/local/bin:/usr/bin".to_string()],
            ..ContainerConfig::default()
        },
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };
    let emitted = emit_image(&producer, &[layer], &cfg, &ref_name).expect("emit");
    let source_manifest_digest = emitted.digest.clone();

    // ── Push to registry, then pull back into a fresh consumer layout ─────
    let client = http_client_for(&endpoint);
    let registry_ref: Reference = ref_name.parse().expect("parse reference");
    client
        .push(
            &registry_ref,
            &ref_name,
            &producer,
            &RegistryAuth::Anonymous,
        )
        .await
        .expect("push");

    let consumer_dir = tempdir().expect("consumer tempdir");
    let consumer = ImageLayout::init(consumer_dir.path()).expect("init consumer");
    let pulled = client
        .pull(&registry_ref, &RegistryAuth::Anonymous, &consumer)
        .await
        .expect("pull");

    assert_eq!(
        pulled.digest, source_manifest_digest,
        "manifest digest must survive emit → push → pull",
    );

    // ── Verify the pulled image still introspects as UMF Container ────────
    let profile = introspect(&consumer, &ref_name).expect("introspect");
    assert_eq!(profile.kind, L0Kind::Container);
    assert_eq!(
        profile.labels.get(label::TYPE).map(String::as_str),
        Some("container"),
    );
    assert_eq!(
        profile.labels.get(label::SPEC_VERSION).map(String::as_str),
        Some(label::CURRENT_SPEC_VERSION),
    );

    // ── Verify the manifest in the consumer layout references real blobs ──
    let manifest_bytes = consumer.read_blob(&pulled.digest).expect("manifest");
    let manifest: OciImageManifest =
        serde_json::from_slice(&manifest_bytes).expect("parse manifest");
    let _ = consumer
        .read_blob(&manifest.config.digest)
        .expect("config blob present");
    for layer in &manifest.layers {
        let _ = consumer
            .read_blob(&layer.digest)
            .expect("layer blob present");
    }

    registry.shutdown().await;
}
