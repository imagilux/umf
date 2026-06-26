//! Unit tests for the `add` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use std::fs;
use tempfile::tempdir;

#[tokio::test]
async fn add_override_short_circuits_chain() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let tarball = dir.path().join("custom-rootfs.tar.gz");
    fs::write(&tarball, b"fake-tarball-bytes").expect("seed");

    let art = resolve_add(
        "alpine:3.21.0",
        Architecture::host(),
        None,
        &layout,
        Some(&tarball),
        None,
    )
    .await
    .expect("resolve");

    assert_eq!(art.layers, vec![tarball.clone()]);
    assert_eq!(art.provenance, AddProvenance::Override(tarball));
}

#[tokio::test]
async fn add_override_missing_file_errors() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let missing = dir.path().join("nope.tar.gz");

    let err = resolve_add(
        "alpine:3.21.0",
        Architecture::host(),
        None,
        &layout,
        Some(&missing),
        None,
    )
    .await
    .unwrap_err();
    match err {
        AddResolveError::Io(io_err) => {
            assert_eq!(io_err.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("expected Io NotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn add_malformed_oci_ref_rejected() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");

    // `alpine::3.21.0` has a double-colon → not a valid OCI ref.
    let err = resolve_add(
        "alpine::3.21.0",
        Architecture::host(),
        None,
        &layout,
        None,
        None,
    )
    .await
    .unwrap_err();
    match err {
        AddResolveError::MalformedRef { ref_name, .. } => {
            assert_eq!(ref_name, "alpine::3.21.0");
        }
        other => panic!("expected MalformedRef, got {other:?}"),
    }
}

#[tokio::test]
async fn add_chain_exhausted_when_only_cache_lookups_supplied() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");

    // Valid OCI ref, but no registry client, no override, no cache entry →
    // the chain runs out and returns NotFound naming what it tried.
    let err = resolve_add(
        "alpine:3.21.0",
        Architecture::host(),
        None,
        &layout,
        None,
        None,
    )
    .await
    .unwrap_err();
    match err {
        AddResolveError::NotFound { tried } => {
            assert!(tried.contains("cache"), "expected cache attempt: {tried:?}");
            assert!(
                tried.contains("registry"),
                "expected registry attempt: {tried:?}",
            );
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn add_registry_ref_overrides_directive_value_for_pull() {
    // When an explicit registry ref is supplied, the resolver pulls *that*
    // ref even though the directive value differs. Verified by checking the
    // canonical ref appears in the error message's `tried` list.
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");

    let err = resolve_add(
        "alpine:3.21.0",
        Architecture::host(),
        None,
        &layout,
        None,
        Some("registry.example.invalid/private/curated:1.0"),
    )
    .await
    .unwrap_err();
    match err {
        AddResolveError::NotFound { tried } => {
            assert!(
                tried.contains("registry.example.invalid/private/curated:1.0"),
                "tried should reference the explicit registry ref: {tried:?}",
            );
            assert!(
                !tried.contains("alpine:3.21.0"),
                "directive value should be replaced by the registry ref: {tried:?}",
            );
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}
