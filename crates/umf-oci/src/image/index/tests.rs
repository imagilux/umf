#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::image::{ContainerConfig, ImageConfig, emit_image};
use std::collections::BTreeMap;
use tempfile::tempdir;
use umf_core::l0::L0Kind;

/// Emit a synthetic single-arch image (zero layers, arch-stamped config)
/// under a per-arch ref and return its manifest digest.
fn emit_arch_image(layout: &ImageLayout, oci_arch: &str) -> String {
    let cfg = ImageConfig {
        architecture: oci_arch.to_string(),
        os: "linux".to_string(),
        container: ContainerConfig {
            // A per-arch marker in a label so a round-tripped child is
            // identifiable after selection.
            labels: BTreeMap::from([("arch.marker".to_string(), oci_arch.to_string())]),
            ..ContainerConfig::default()
        },
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };
    let entry = emit_image(
        layout,
        &[],
        &cfg,
        &format!("example.invalid/app:{oci_arch}"),
    )
    .expect("emit per-arch image");
    entry.digest
}

fn two_arch_children(layout: &ImageLayout) -> Vec<IndexChild> {
    let amd = emit_arch_image(layout, "amd64");
    let arm = emit_arch_image(layout, "arm64");
    vec![
        IndexChild {
            platform: platform_for("linux", "amd64"),
            manifest_digest: amd,
        },
        IndexChild {
            platform: platform_for("linux", "arm64"),
            manifest_digest: arm,
        },
    ]
}

#[test]
fn emit_index_writes_two_platform_tagged_descriptors() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let children = two_arch_children(&layout);
    let entry = emit_index(&layout, &children, "example.invalid/app:multi").expect("emit");

    assert_eq!(entry.media_type, OCI_IMAGE_INDEX_MEDIA_TYPE);
    assert!(layout.has_blob(&entry.digest));

    let index_bytes = layout.read_blob(&entry.digest).expect("index blob");
    let index: OciImageIndex = serde_json::from_slice(&index_bytes).expect("parse index");
    assert_eq!(index.schema_version, 2);
    assert_eq!(index.manifests.len(), 2);
    // Every child carries a platform with os/architecture populated.
    for m in &index.manifests {
        let p = m.platform.as_ref().expect("platform present");
        assert_eq!(p.os, Os::Linux);
        assert!(matches!(p.architecture, Arch::Amd64 | Arch::ARM64));
        assert_eq!(m.media_type, OCI_IMAGE_MEDIA_TYPE);
    }
}

#[test]
fn emit_index_is_byte_reproducible_regardless_of_child_order() {
    // Two fresh layouts get byte-identical child images (same inputs ⇒ same
    // digests); composing them in opposite input orders must still yield the
    // same index digest, because `emit_index` sorts deterministically.
    let dir_a = tempdir().expect("a");
    let layout_a = ImageLayout::init(dir_a.path()).expect("init a");
    let children_a = two_arch_children(&layout_a);
    let entry_a = emit_index(&layout_a, &children_a, "x:multi").expect("emit a");

    let dir_b = tempdir().expect("b");
    let layout_b = ImageLayout::init(dir_b.path()).expect("init b");
    let mut children_b = two_arch_children(&layout_b);
    children_b.reverse();
    let entry_b = emit_index(&layout_b, &children_b, "x:multi").expect("emit b");

    assert_eq!(
        entry_a.digest, entry_b.digest,
        "index digest must be independent of child input order",
    );
}

#[test]
fn emit_index_rejects_empty_children() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let err = emit_index(&layout, &[], "x:empty").expect_err("must reject");
    assert!(
        matches!(err, RegistryError::InvalidLayout(_)),
        "got {err:?}"
    );
}

#[test]
fn emit_index_errors_on_missing_child_blob() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let child = IndexChild {
        platform: platform_for("linux", "amd64"),
        manifest_digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
            .to_string(),
    };
    let err =
        emit_index(&layout, std::slice::from_ref(&child), "x:dangling").expect_err("absent child");
    // read_blob on a missing digest surfaces as an I/O error.
    assert!(matches!(err, RegistryError::Io(_)), "got {err:?}");
}

#[test]
fn select_manifest_picks_requested_arch() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let children = two_arch_children(&layout);
    let amd_digest = children[0].manifest_digest.clone();
    let arm_digest = children[1].manifest_digest.clone();
    let entry = emit_index(&layout, &children, "x:multi").expect("emit");

    let index_bytes = layout.read_blob(&entry.digest).expect("index blob");
    let index: OciImageIndex = serde_json::from_slice(&index_bytes).expect("parse");

    let amd = select_manifest_for_arch(&index, "amd64").expect("amd selected");
    assert_eq!(amd.digest, amd_digest);
    let arm = select_manifest_for_arch(&index, "arm64").expect("arm selected");
    assert_eq!(arm.digest, arm_digest);
    assert!(
        select_manifest_for_arch(&index, "riscv64").is_none(),
        "absent arch must not fall back",
    );
}

#[test]
fn selected_child_round_trips_to_its_arch_config() {
    // End-to-end at the library level: emit index → select per arch → read
    // the selected child's config → confirm it is the matching arch's image
    // (its arch-marker label + architecture field).
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let children = two_arch_children(&layout);
    let entry = emit_index(&layout, &children, "x:multi").expect("emit");
    let index: OciImageIndex =
        serde_json::from_slice(&layout.read_blob(&entry.digest).expect("blob")).expect("parse");

    for arch in ["amd64", "arm64"] {
        let chosen = select_manifest_for_arch(&index, arch).expect("selected");
        let m: oci_client::manifest::OciImageManifest =
            serde_json::from_slice(&layout.read_blob(&chosen.digest).expect("child manifest"))
                .expect("parse child");
        let cfg: serde_json::Value =
            serde_json::from_slice(&layout.read_blob(&m.config.digest).expect("config"))
                .expect("parse config");
        assert_eq!(cfg.get("architecture").and_then(|v| v.as_str()), Some(arch));
        assert_eq!(
            cfg.pointer("/config/Labels/arch.marker")
                .and_then(|v| v.as_str()),
            Some(arch),
        );
    }
}

/// Full distribution round-trip: emit an index in layout A, `push` it to the
/// in-process distribution server, `pull` it into a fresh layout B, and
/// confirm both arches survive with correct per-arch selection. Exercises
/// the index branch of `RegistryClient::{push,pull}` end to end.
#[cfg(feature = "test-server")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_round_trips_through_test_registry() {
    use crate::registry::RegistryClient;
    use crate::test_registry::TestRegistry;
    use oci_client::Reference;
    use oci_client::client::{ClientConfig, ClientProtocol};
    use oci_client::secrets::RegistryAuth;

    let registry = TestRegistry::start().await.expect("start test registry");
    let endpoint = registry.endpoint().to_string();

    // Producer layout: two per-arch images + the composed index.
    let dir_a = tempdir().expect("layout a");
    let layout_a = ImageLayout::init(dir_a.path()).expect("init a");
    let children = two_arch_children(&layout_a);
    let amd_digest = children[0].manifest_digest.clone();
    let arm_digest = children[1].manifest_digest.clone();
    let ref_name = format!("{endpoint}/app:multi");
    emit_index(&layout_a, &children, &ref_name).expect("emit index");

    // Plain-HTTP client (the test server speaks HTTP on 127.0.0.1).
    let client = RegistryClient::with_config(ClientConfig {
        protocol: ClientProtocol::Http,
        ..Default::default()
    });
    let reference: Reference = ref_name.parse().expect("parse reference");

    client
        .push(&reference, &ref_name, &layout_a, &RegistryAuth::Anonymous)
        .await
        .expect("push index");

    // Consumer layout: pull the index (and every child manifest tree) back.
    let dir_b = tempdir().expect("layout b");
    let layout_b = ImageLayout::init(dir_b.path()).expect("init b");
    let pulled = client
        .pull(&reference, &RegistryAuth::Anonymous, &layout_b)
        .await
        .expect("pull index");

    // Push/pull is done; everything asserted below reads only layout_b, so
    // shut the in-process registry down now, before the assertions, so a
    // failing assert can't skip cleanup and leave the server running.
    registry.shutdown().await;

    assert_eq!(pulled.media_type, OCI_IMAGE_INDEX_MEDIA_TYPE);

    // The pulled index resolves per-arch to the same child manifests, and
    // the child blobs are present in the fresh layout.
    let index: OciImageIndex =
        serde_json::from_slice(&layout_b.read_blob(&pulled.digest).expect("index blob"))
            .expect("parse pulled index");
    let amd = select_manifest_for_arch(&index, "amd64").expect("amd present");
    let arm = select_manifest_for_arch(&index, "arm64").expect("arm present");
    assert_eq!(amd.digest, amd_digest);
    assert_eq!(arm.digest, arm_digest);
    assert!(layout_b.has_blob(&amd.digest), "amd child manifest pulled");
    assert!(layout_b.has_blob(&arm.digest), "arm child manifest pulled");
}
