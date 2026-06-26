//! Regression test: resolving a base whose ref is a multi-arch OCI
//! **image index** (e.g. `alpine:3.21`) must select the platform-matching child
//! manifest instead of trying to parse the index as a single-image manifest
//! (which fails with serde's `missing field \`config\``).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;

use tempfile::tempdir;
use umf_builder::resolver::{AddProvenance, AddResolveError, resolve_add};
use umf_core::architecture::Architecture;
use umf_core::l0::L0Kind;
use umf_oci::image::{
    ImageConfig, IndexChild, LayerCompression, LayerSource, emit_image, emit_index, platform_for,
};
use umf_oci::registry::ImageLayout;

/// Emit a single-arch (host) container image with one layer into `layout`,
/// returning its manifest digest (the child an index entry points at).
fn emit_host_child(layout: &ImageLayout, ref_name: &str) -> String {
    let host = Architecture::host();
    let holder = tempdir().expect("rootfs tempdir");
    let root = holder.path().join("root");
    fs::create_dir_all(root.join("etc")).expect("mkdir");
    fs::write(root.join("etc/os-release"), b"ID=multiarch-test\n").expect("seed file");

    let layer = LayerSource::from_directory_with(&root, LayerCompression::Gzip).expect("layer");
    let config = ImageConfig {
        architecture: host.oci_arch_string().to_string(),
        os: "linux".to_string(),
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };
    emit_image(layout, &[layer], &config, ref_name)
        .expect("emit child image")
        .digest
}

#[tokio::test]
async fn resolve_add_selects_the_host_child_of_a_multi_arch_index() {
    let host = Architecture::host();
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init layout");

    // A single-arch child image, then a multi-arch index pointing at it. The
    // index ref is fully qualified so `Reference::whole()` round-trips it
    // unchanged (the cache lookup keys on the canonical form).
    let child_digest = emit_host_child(&layout, "registry.test/multiarch/child:1.0");
    let index_ref = "registry.test/multiarch/base:1.0";
    emit_index(
        &layout,
        &[IndexChild {
            platform: platform_for("linux", host.oci_arch_string()),
            manifest_digest: child_digest,
        }],
        index_ref,
    )
    .expect("emit index");

    // Previously this failed with `missing field \`config\`` because the index
    // bytes were parsed as a single-image manifest. Now it descends to the
    // host-arch child and returns that child's layers.
    let art = resolve_add(index_ref, host, None, &layout, None, None)
        .await
        .expect("multi-arch base index should resolve to its host-arch child");

    assert_eq!(art.layers.len(), 1, "host child carries exactly one layer");
    assert!(
        matches!(art.provenance, AddProvenance::Cache(ref r) if r == index_ref),
        "resolved from the on-disk layout (cache), got {:?}",
        art.provenance,
    );
}

#[tokio::test]
async fn resolve_add_index_without_a_matching_arch_errors_clearly() {
    let host = Architecture::host();
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init layout");

    // Index whose only child targets a different arch than the host. The index
    // is parsed correctly (no `missing field \`config\`` crash); selection finds
    // no host-arch child, so the cache rung reports a miss and, with no registry
    // client, the chain is exhausted into a clean `NotFound` (the precise
    // no-matching-arch detail is logged as a warning by the ladder).
    let child_digest = emit_host_child(&layout, "registry.test/multiarch/child:1.0");
    let other_arch = if host.oci_arch_string() == "amd64" {
        "arm64"
    } else {
        "amd64"
    };
    let index_ref = "registry.test/multiarch/otherarch:1.0";
    emit_index(
        &layout,
        &[IndexChild {
            platform: platform_for("linux", other_arch),
            manifest_digest: child_digest,
        }],
        index_ref,
    )
    .expect("emit index");

    let err = resolve_add(index_ref, host, None, &layout, None, None)
        .await
        .expect_err("an index with no host-arch child must not resolve");
    assert!(
        matches!(err, AddResolveError::NotFound { .. }),
        "expected a graceful chain-exhausted NotFound, got {err:?}",
    );
    assert!(
        !err.to_string().contains("missing field"),
        "the index must be parsed, not crash with a serde missing-field error: {err}",
    );
}
