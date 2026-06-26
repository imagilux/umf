#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use tempfile::TempDir;
use umf_oci::image::{ImageConfig, LayerSource, emit_image};

fn pkg(name: &str, version: &str, purl: &str) -> Package {
    Package {
        name: name.to_string(),
        version: version.to_string(),
        arch: Some("amd64".to_string()),
        purl: Some(purl.to_string()),
    }
}

#[test]
fn spdx_document_is_well_formed_and_deterministic() {
    let pkgs = vec![pkg("bash", "5.1", "pkg:deb/debian/bash@5.1")];
    let a = to_spdx(&pkgs, "img:1", "sha256:abc", "1970-01-01T00:00:00Z");
    let b = to_spdx(&pkgs, "img:1", "sha256:abc", "1970-01-01T00:00:00Z");
    assert_eq!(a, b, "same inputs produce a byte-identical document");
    assert_eq!(a["spdxVersion"], "SPDX-2.3");
    assert_eq!(a["packages"][0]["name"], "bash");
    assert_eq!(
        a["packages"][0]["externalRefs"][0]["referenceLocator"],
        "pkg:deb/debian/bash@5.1",
    );
    assert_eq!(a["relationships"][0]["relationshipType"], "DESCRIBES");
}

#[test]
fn cyclonedx_document_is_well_formed_and_deterministic() {
    let pkgs = vec![pkg("musl", "1.2", "pkg:apk/alpine/musl@1.2")];
    let a = to_cyclonedx(&pkgs, "img:1", "sha256:abc");
    let b = to_cyclonedx(&pkgs, "img:1", "sha256:abc");
    assert_eq!(a, b);
    assert_eq!(a["bomFormat"], "CycloneDX");
    assert_eq!(a["specVersion"], "1.5");
    assert_eq!(a["components"][0]["name"], "musl");
    assert_eq!(a["components"][0]["purl"], "pkg:apk/alpine/musl@1.2");
}

/// Seed a layout with an image whose single layer carries a minimal dpkg
/// status database, so `generate` has a real rootfs to materialize + scan.
fn seed_image_with_dpkg(layout: &ImageLayout, reference: &str) {
    let src = TempDir::new().unwrap();
    std::fs::create_dir_all(src.path().join("var/lib/dpkg")).unwrap();
    std::fs::write(
        src.path().join("var/lib/dpkg/status"),
        "Package: bash\nStatus: install ok installed\nVersion: 5.1-2\nArchitecture: amd64\n\n\
         Package: coreutils\nStatus: install ok installed\nVersion: 8.32-4\nArchitecture: amd64\n",
    )
    .unwrap();
    let layer = LayerSource::from_directory(src.path()).unwrap();
    emit_image(
        layout,
        std::slice::from_ref(&layer),
        &ImageConfig::default(),
        reference,
    )
    .unwrap();
}

#[test]
fn generate_scans_a_built_image_and_writes_spdx() {
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    seed_image_with_dpkg(&layout, "example.invalid/app:1");

    let out = TempDir::new().unwrap();
    let out_path = out.path().join("sbom.spdx.json");
    run_generate(GenerateArgs {
        reference: "example.invalid/app:1",
        format: SbomFormat::Spdx,
        output: Some(&out_path),
        attach: false,
        push: false,
        layout_dir_override: Some(layout_dir.path()),
        insecure_registry: false,
        username: None,
        password_stdin: false,
    })
    .expect("generate succeeds");

    let doc: Value = serde_json::from_slice(&std::fs::read(&out_path).unwrap()).unwrap();
    assert_eq!(doc["spdxVersion"], "SPDX-2.3");
    let names: Vec<&str> = doc["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["bash", "coreutils"]);
}

#[test]
fn generate_attaches_a_referrer() {
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    seed_image_with_dpkg(&layout, "example.invalid/app:2");
    let subject = layout.lookup_ref("example.invalid/app:2").unwrap().unwrap();

    run_generate(GenerateArgs {
        reference: "example.invalid/app:2",
        format: SbomFormat::Cyclonedx,
        output: None,
        attach: true,
        push: false,
        layout_dir_override: Some(layout_dir.path()),
        insecure_registry: false,
        username: None,
        password_stdin: false,
    })
    .expect("generate + attach succeeds");

    let referrers = layout.list_referrers(&subject.digest, None).unwrap();
    assert_eq!(referrers.len(), 1);
    assert_eq!(
        referrers[0].artifact_type.as_deref(),
        Some(CYCLONEDX_MEDIA_TYPE),
    );
}
