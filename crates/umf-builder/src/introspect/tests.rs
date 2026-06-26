//! Unit tests for the `introspect` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use oci_client::manifest::{
    IMAGE_LAYER_MEDIA_TYPE, ImageIndexEntry, OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MEDIA_TYPE,
    OciDescriptor, OciImageIndex,
};
use serde_json::json;
use std::fs;
use tempfile::tempdir;
use umf_core::l0::Payload;

use umf_oci::registry::layout::sha256_digest;

/// Stage a synthetic image with the given labels into `layout`, returning
/// the ref name it was upserted under.
fn stage_image(
    layout: &ImageLayout,
    ref_name: &str,
    labels: &[(&str, &str)],
    layers: usize,
    config_media_type: &str,
) -> String {
    let mut labels_json = serde_json::Map::new();
    for (k, v) in labels {
        labels_json.insert((*k).to_string(), json!(v));
    }
    let config_doc = json!({
        "architecture": "amd64",
        "os": "linux",
        "config": { "Labels": labels_json },
        "rootfs": { "type": "layers", "diff_ids": [] },
    });
    let config_bytes = serde_json::to_vec(&config_doc).expect("serialize config");
    let config_digest = layout.write_blob(&config_bytes).expect("write config");

    let mut layer_descriptors = Vec::with_capacity(layers);
    for i in 0..layers {
        let payload = format!("layer-{i}\n").into_bytes();
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
            media_type: config_media_type.to_string(),
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

    layout
        .upsert_ref(
            ref_name,
            ImageIndexEntry {
                media_type: OCI_IMAGE_MEDIA_TYPE.to_string(),
                digest: manifest_digest,
                size: manifest_bytes.len() as i64,
                platform: None,
                annotations: None,
            },
        )
        .expect("upsert ref");
    ref_name.to_string()
}

#[test]
fn scratch_sentinel_is_valid_from_for_container_only() {
    let p = L0Profile::scratch();
    assert_eq!(p.kind, L0Kind::Scratch);
    assert_eq!(p.source, L0Source::Label);
    assert!(p.manifest_digest.is_empty());
    assert!(p.labels.is_empty());
    // Container build: `FROM scratch` is valid.
    assert!(p.kind.is_valid_from(false));
    // Bootable build: `FROM scratch` is rejected (no kernel source).
    assert!(!p.kind.is_valid_from(true));
}

#[test]
fn introspect_label_container() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let r = stage_image(
        &layout,
        "example.invalid/example:latest",
        &[(label::TYPE, "container")],
        1,
        IMAGE_CONFIG_MEDIA_TYPE,
    );
    let profile = introspect(&layout, &r).expect("introspect");
    assert_eq!(profile.kind, L0Kind::Container);
    assert_eq!(profile.source, L0Source::Label);
    assert!(profile.kind.is_valid_from(false));
    // Container artifact is not valid as FROM in a VM build.
    assert!(!profile.kind.is_valid_from(true));
}

#[test]
fn introspect_label_bootable_and_retired_types() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    // `vm` / `bootc` / `unikernel` were retired as type values —
    // introspection now surfaces them as `Unknown`, not a recognised kind.
    for (label_value, expected) in [
        ("bootable", L0Kind::Bootable),
        ("vm", L0Kind::Unknown("vm".to_string())),
        ("bootc", L0Kind::Unknown("bootc".to_string())),
        ("unikernel", L0Kind::Unknown("unikernel".to_string())),
    ] {
        let r = stage_image(
            &layout,
            &format!("example.invalid/{label_value}:latest"),
            &[(label::TYPE, label_value)],
            1,
            IMAGE_CONFIG_MEDIA_TYPE,
        );
        let profile = introspect(&layout, &r).expect("introspect");
        assert_eq!(profile.kind, expected);
        assert_eq!(profile.source, L0Source::Label);
    }
}

#[test]
fn introspect_kernel_build_env_is_valid_from_for_container() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let r = stage_image(
        &layout,
        "example.invalid/build-env:latest",
        &[(label::TYPE, "kernel-build-env")],
        1,
        IMAGE_CONFIG_MEDIA_TYPE,
    );
    let profile = introspect(&layout, &r).expect("introspect");
    assert_eq!(profile.kind, L0Kind::KernelBuildEnv);
    assert!(profile.kind.is_valid_from(false));
    assert!(!profile.kind.is_valid_from(true));
    assert!(!profile.kind.is_payload());
}

#[test]
fn introspect_payload_kernel_is_valid_from_only_with_firmware() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let r = stage_image(
        &layout,
        "example.invalid/kernel:latest",
        &[(label::TYPE, "kernel")],
        1,
        IMAGE_CONFIG_MEDIA_TYPE,
    );
    let profile = introspect(&layout, &r).expect("introspect");
    assert_eq!(profile.kind, L0Kind::Payload(Payload::Kernel));
    assert!(profile.kind.is_payload());
    // Bootable build → kernel artifact is the valid FROM.
    assert!(profile.kind.is_valid_from(true));
    // Container build → kernel artifact is not usable as a base.
    assert!(!profile.kind.is_valid_from(false));
}

#[test]
fn introspect_other_payload_kinds_are_rejected_as_from() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    for (label_value, expected) in [
        ("rootfs", Payload::Rootfs),
        ("bootloader", Payload::Bootloader),
        ("firmware", Payload::Firmware),
    ] {
        let r = stage_image(
            &layout,
            &format!("example.invalid/{label_value}:latest"),
            &[(label::TYPE, label_value)],
            1,
            IMAGE_CONFIG_MEDIA_TYPE,
        );
        let profile = introspect(&layout, &r).expect("introspect");
        assert_eq!(profile.kind, L0Kind::Payload(expected));
        assert!(profile.kind.is_payload());
        assert!(!profile.kind.is_valid_from(true));
        assert!(!profile.kind.is_valid_from(false));
    }
}

#[test]
fn unrecognised_label_preserved_verbatim() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let r = stage_image(
        &layout,
        "example.invalid/future:latest",
        &[(label::TYPE, "future-shape")],
        1,
        IMAGE_CONFIG_MEDIA_TYPE,
    );
    let profile = introspect(&layout, &r).expect("introspect");
    assert_eq!(profile.kind, L0Kind::Unknown("future-shape".to_string()));
    assert_eq!(profile.source, L0Source::Label);
    assert!(!profile.kind.is_valid_from(true));
    assert!(!profile.kind.is_valid_from(false));
}

#[test]
fn no_label_with_layers_infers_container() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let r = stage_image(
        &layout,
        "example.invalid/unlabelled:latest",
        &[],
        2,
        IMAGE_CONFIG_MEDIA_TYPE,
    );
    let profile = introspect(&layout, &r).expect("introspect");
    assert_eq!(profile.kind, L0Kind::Container);
    assert_eq!(profile.source, L0Source::Inferred);
}

#[test]
fn no_label_no_layers_returns_unknown_empty() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let r = stage_image(
        &layout,
        "example.invalid/empty:latest",
        &[],
        0,
        IMAGE_CONFIG_MEDIA_TYPE,
    );
    let profile = introspect(&layout, &r).expect("introspect");
    assert_eq!(profile.kind, L0Kind::Unknown(String::new()));
    assert_eq!(profile.source, L0Source::Inferred);
}

#[test]
fn no_label_with_legacy_config_media_type_infers_container() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let r = stage_image(
        &layout,
        "example.invalid/legacy-config:latest",
        &[],
        1,
        IMAGE_DOCKER_CONFIG_MEDIA_TYPE,
    );
    let profile = introspect(&layout, &r).expect("introspect");
    assert_eq!(profile.kind, L0Kind::Container);
}

#[test]
fn unknown_ref_returns_not_found() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let err = introspect(&layout, "no-such-ref").expect_err("must fail");
    assert!(matches!(err, RegistryError::NotFound(_)), "got {err:?}");
}

#[test]
fn image_index_requires_platform_selection() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    // Build a minimal image-index manifest that points at no real children
    // (we only need introspect to recognise the index shape).
    let index = OciImageIndex {
        schema_version: 2,
        media_type: Some(OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
        manifests: vec![],
        artifact_type: None,
        annotations: None,
    };
    let bytes = serde_json::to_vec(&index).expect("serialize index");
    let digest = layout.write_blob(&bytes).expect("write index");
    layout
        .upsert_ref(
            "example.invalid/multi:latest",
            ImageIndexEntry {
                media_type: OCI_IMAGE_INDEX_MEDIA_TYPE.to_string(),
                digest,
                size: bytes.len() as i64,
                platform: None,
                annotations: None,
            },
        )
        .expect("upsert");

    let err = introspect(&layout, "example.invalid/multi:latest").expect_err("must fail");
    match err {
        RegistryError::InvalidLayout(msg) => {
            assert!(msg.contains("image index"), "message was {msg:?}")
        }
        other => panic!("expected InvalidLayout, got {other:?}"),
    }
}

#[test]
fn missing_config_blob_propagates_io_error() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let r = stage_image(
        &layout,
        "example.invalid/orphan:latest",
        &[(label::TYPE, "container")],
        1,
        IMAGE_CONFIG_MEDIA_TYPE,
    );
    // Yank the config blob out of the layout — manifest still references it.
    let entry = layout.lookup_ref(&r).expect("lookup").expect("present");
    let manifest_bytes = layout.read_blob(&entry.digest).expect("read manifest");
    let manifest: OciImageManifest =
        serde_json::from_slice(&manifest_bytes).expect("parse manifest");
    let config_path = layout.blob_path(&manifest.config.digest).expect("path");
    fs::remove_file(&config_path).expect("remove config blob");

    let err = introspect(&layout, &r).expect_err("must fail");
    assert!(matches!(err, RegistryError::Io(_)), "got {err:?}");
}

#[test]
fn labels_are_passed_through_for_downstream_inspection() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let r = stage_image(
        &layout,
        "example.invalid/labelled:latest",
        &[
            (label::TYPE, "container"),
            (label::SPEC_VERSION, "0.2"),
            (
                "org.opencontainers.image.source",
                "https://example.invalid/x",
            ),
        ],
        1,
        IMAGE_CONFIG_MEDIA_TYPE,
    );
    let profile = introspect(&layout, &r).expect("introspect");
    assert_eq!(
        profile.labels.get(label::SPEC_VERSION).map(String::as_str),
        Some("0.2"),
    );
    assert_eq!(
        profile
            .labels
            .get("org.opencontainers.image.source")
            .map(String::as_str),
        Some("https://example.invalid/x"),
    );
}
