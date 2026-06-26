//! Unit tests for the `from_kernel` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use std::fs;
use tempfile::tempdir;

#[tokio::test]
async fn from_kernel_override_short_circuits_chain() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let tarball = dir.path().join("custom-kernel.tar");
    fs::write(&tarball, b"fake-kernel-tarball").expect("seed");

    let art = resolve_from_kernel(
        "imagilux/kernel-linux:7.0",
        Architecture::host(),
        None,
        &layout,
        Some(&tarball),
        None,
    )
    .await
    .expect("resolve");

    assert_eq!(art.layers, vec![tarball.clone()]);
    assert_eq!(art.provenance, FromKernelProvenance::Override(tarball));
}

#[tokio::test]
async fn from_kernel_override_missing_file_errors() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let missing = dir.path().join("nope.tar");
    let err = resolve_from_kernel(
        "imagilux/kernel-linux:7.0",
        Architecture::host(),
        None,
        &layout,
        Some(&missing),
        None,
    )
    .await
    .unwrap_err();
    match err {
        FromKernelResolveError::Io(io_err) => {
            assert_eq!(io_err.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("expected Io NotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn from_kernel_malformed_oci_ref_rejected() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let err = resolve_from_kernel(
        "kernel::7.0",
        Architecture::host(),
        None,
        &layout,
        None,
        None,
    )
    .await
    .unwrap_err();
    match err {
        FromKernelResolveError::MalformedRef { ref_name, .. } => {
            assert_eq!(ref_name, "kernel::7.0");
        }
        other => panic!("expected MalformedRef, got {other:?}"),
    }
}

#[tokio::test]
async fn from_kernel_chain_exhausted_when_only_cache_lookups_supplied() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let err = resolve_from_kernel(
        "imagilux/kernel-linux:7.0",
        Architecture::host(),
        None,
        &layout,
        None,
        None,
    )
    .await
    .unwrap_err();
    match err {
        FromKernelResolveError::NotFound { tried } => {
            assert!(tried.contains("cache"), "expected cache attempt: {tried:?}");
            assert!(
                tried.contains("registry"),
                "expected registry attempt: {tried:?}",
            );
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

// (`rootfs_*_real_fetch` integration tests for live OCI pulls
// against a public registry live in `crates/umf-builder/tests/`
// rather than here; this `mod tests` is for offline-deterministic
// unit tests that don't require network or credentials.)
