//! End-to-end coupling between `umf_oci::image::emit_image` (the producer)
//! and `umf_builder::introspect` (the consumer).
//!
//! Originally lived in `umf_oci::image`'s in-tree test module but had to
//! move out when the OCI primitives were carved into their own crate
//! — `introspect` stays in `umf-builder` because it sits above the
//! OCI primitives (it pulls a manifest, reads a config, and produces a
//! UMF L0 profile, all in one go).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::path::Path;

use tempfile::tempdir;
use umf_builder::introspect::introspect;
use umf_core::l0::{L0Kind, Payload};
use umf_core::label;
use umf_oci::image::{ContainerConfig, ImageConfig, LayerSource, emit_image};
use umf_oci::registry::ImageLayout;

fn tiny_dir(root: &Path) {
    fs::create_dir_all(root.join("bin")).expect("mkdir bin");
    fs::write(root.join("bin/hello"), b"echo hi\n").expect("write hello");
    fs::write(root.join("README"), b"hello, umf\n").expect("write README");
}

#[test]
fn emit_then_introspect_round_trips_umf_type_and_spec_label() {
    let src = tempdir().expect("src tempdir");
    tiny_dir(src.path());
    let layer = LayerSource::from_directory(src.path()).expect("build layer");

    let layout_dir = tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init");

    let cfg = ImageConfig {
        container: ContainerConfig {
            entrypoint: Some(vec!["/bin/hello".to_string()]),
            ..ContainerConfig::default()
        },
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };

    let ref_name = "example.invalid/round-trip:v1";
    emit_image(&layout, &[layer], &cfg, ref_name).expect("emit");

    let profile = introspect(&layout, ref_name).expect("introspect");
    assert_eq!(profile.kind, L0Kind::Container);
    assert_eq!(
        profile.labels.get(label::TYPE).map(String::as_str),
        Some("container"),
    );
    assert_eq!(
        profile.labels.get(label::SPEC_VERSION).map(String::as_str),
        Some(label::CURRENT_SPEC_VERSION),
    );
}

#[test]
fn user_labels_are_overridden_by_umf_labels() {
    // Even if the caller sets `org.imagilux.umf.type` to something misleading,
    // the emitter overwrites it with the L0Kind-derived value.
    let layout_dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init");

    let mut cfg = ImageConfig::default();
    cfg.container
        .labels
        .insert(label::TYPE.to_string(), "kernel".to_string());
    cfg.umf_type = L0Kind::Container;

    emit_image(&layout, &[], &cfg, "example.invalid/y:1").expect("emit");
    let profile = introspect(&layout, "example.invalid/y:1").expect("introspect");
    assert_eq!(profile.kind, L0Kind::Container);
}

#[test]
fn bootable_umf_type_is_round_tripped() {
    let layout_dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init");

    let cfg = ImageConfig {
        umf_type: L0Kind::Bootable,
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &cfg, "example.invalid/bootable:1").expect("emit");

    let profile = introspect(&layout, "example.invalid/bootable:1").expect("introspect");
    assert_eq!(profile.kind, L0Kind::Bootable);
}

#[test]
fn payload_umf_type_is_round_tripped_and_flagged_invalid_as_from() {
    let layout_dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init");

    let cfg = ImageConfig {
        umf_type: L0Kind::Payload(Payload::Kernel),
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &cfg, "example.invalid/kernel:1").expect("emit");

    let profile = introspect(&layout, "example.invalid/kernel:1").expect("introspect");
    assert!(profile.kind.is_payload());
    // Kernel payload is a valid FROM only for a bootable build.
    assert!(profile.kind.is_valid_from(true));
    assert!(!profile.kind.is_valid_from(false));
}

#[test]
fn custom_spec_version_overrides_default() {
    let layout_dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init");

    let cfg = ImageConfig {
        umf_spec: Some("0.99".to_string()),
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &cfg, "example.invalid/future:1").expect("emit");

    let profile = introspect(&layout, "example.invalid/future:1").expect("introspect");
    assert_eq!(
        profile.labels.get(label::SPEC_VERSION).map(String::as_str),
        Some("0.99"),
    );
}
