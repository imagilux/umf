//! Unit tests for the `platform` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use oci_client::manifest::{OciImageIndex, Platform};

fn entry(arch: &str, variant: Option<&str>, digest: &str) -> ImageIndexEntry {
    ImageIndexEntry {
        digest: digest.to_string(),
        media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
        size: 0,
        platform: Some(Platform {
            architecture: Arch::from(arch),
            os: Os::Linux,
            variant: variant.map(str::to_string),
            os_version: None,
            os_features: None,
            features: None,
        }),
        annotations: None,
    }
}

fn index(entries: Vec<ImageIndexEntry>) -> OciImageIndex {
    OciImageIndex {
        schema_version: 2,
        media_type: Some("application/vnd.oci.image.index.v1+json".to_string()),
        manifests: entries,
        annotations: None,
        artifact_type: None,
    }
}

#[test]
fn picks_the_matching_architecture() {
    let idx = index(vec![
        entry("amd64", None, "sha256:amd"),
        entry("arm64", None, "sha256:arm"),
    ]);
    assert_eq!(
        select_for_arch(&idx, Architecture::Aarch64).map(|m| m.digest.as_str()),
        Some("sha256:arm"),
    );
    assert_eq!(
        select_for_arch(&idx, Architecture::X86_64).map(|m| m.digest.as_str()),
        Some("sha256:amd"),
    );
}

#[test]
fn prefers_the_variantless_entry_but_accepts_a_variant() {
    // Both present: the plain entry wins.
    let both = index(vec![
        entry("arm64", Some("v8"), "sha256:v8"),
        entry("arm64", None, "sha256:plain"),
    ]);
    assert_eq!(
        select_for_arch(&both, Architecture::Aarch64).map(|m| m.digest.as_str()),
        Some("sha256:plain"),
    );

    // Only a variant-qualified entry: still resolves rather than failing.
    let only_variant = index(vec![entry("arm64", Some("v8"), "sha256:v8")]);
    assert_eq!(
        select_for_arch(&only_variant, Architecture::Aarch64).map(|m| m.digest.as_str()),
        Some("sha256:v8"),
    );
}

#[test]
fn never_falls_back_to_a_different_architecture() {
    // The property that matters: returning the wrong arch would produce an
    // image whose layers contradict its declared platform.
    let idx = index(vec![entry("amd64", None, "sha256:amd")]);
    assert!(select_for_arch(&idx, Architecture::Aarch64).is_none());
}

#[test]
fn ignores_non_linux_manifests() {
    let mut e = entry("arm64", None, "sha256:win");
    if let Some(p) = e.platform.as_mut() {
        p.os = Os::Windows;
    }
    let idx = index(vec![e]);
    assert!(select_for_arch(&idx, Architecture::Aarch64).is_none());
}

#[test]
fn an_entry_without_platform_metadata_never_matches() {
    let mut e = entry("arm64", None, "sha256:bare");
    e.platform = None;
    let idx = index(vec![e]);
    assert!(select_for_arch(&idx, Architecture::Aarch64).is_none());
}
