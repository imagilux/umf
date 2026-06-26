#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use tempfile::TempDir;
use umf_oci::image::{ImageConfig, emit_image};

#[test]
fn detect_format_recognizes_spdx_cyclonedx_and_rejects_others() {
    assert_eq!(
        detect_format(br#"{"spdxVersion":"SPDX-2.3","name":"x"}"#),
        Some(SPDX_MEDIA_TYPE),
    );
    assert_eq!(
        detect_format(br#"{"bomFormat":"CycloneDX","specVersion":"1.5"}"#),
        Some(CYCLONEDX_MEDIA_TYPE),
    );
    assert_eq!(detect_format(br#"{"unrelated":true}"#), None);
    assert_eq!(detect_format(b"\xff\xfe not utf-8"), None);
}

fn write_file(dir: &TempDir, name: &str, body: &[u8]) -> std::path::PathBuf {
    let p = dir.path().join(name);
    std::fs::write(&p, body).unwrap();
    p
}

#[test]
fn attach_emits_a_referrer_with_the_detected_media_type() {
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    let subject = emit_image(
        &layout,
        &[],
        &ImageConfig::default(),
        "example.invalid/app:1",
    )
    .unwrap();

    let tmp = TempDir::new().unwrap();
    let sbom = write_file(&tmp, "app.spdx.json", br#"{"spdxVersion":"SPDX-2.3"}"#);

    run_attach(AttachArgs {
        reference: "example.invalid/app:1",
        sbom: &sbom,
        format: None,
        push: false,
        layout_dir_override: Some(layout_dir.path()),
        insecure_registry: false,
        username: None,
        password_stdin: false,
    })
    .expect("attach succeeds");

    let referrers = layout.list_referrers(&subject.digest, None).unwrap();
    assert_eq!(referrers.len(), 1, "exactly one SBOM referrer");
    assert_eq!(referrers[0].artifact_type.as_deref(), Some(SPDX_MEDIA_TYPE));
}

#[test]
fn explicit_format_overrides_detection() {
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    let subject = emit_image(
        &layout,
        &[],
        &ImageConfig::default(),
        "example.invalid/app:2",
    )
    .unwrap();

    let tmp = TempDir::new().unwrap();
    // No recognizable marker; --format forces the media type.
    let sbom = write_file(&tmp, "opaque.json", b"{}");

    run_attach(AttachArgs {
        reference: "example.invalid/app:2",
        sbom: &sbom,
        format: Some(SbomFormat::Cyclonedx),
        push: false,
        layout_dir_override: Some(layout_dir.path()),
        insecure_registry: false,
        username: None,
        password_stdin: false,
    })
    .expect("attach with explicit format succeeds");

    let referrers = layout.list_referrers(&subject.digest, None).unwrap();
    assert_eq!(
        referrers[0].artifact_type.as_deref(),
        Some(CYCLONEDX_MEDIA_TYPE),
    );
}

#[test]
fn attach_to_a_missing_image_is_a_clear_error() {
    let layout_dir = TempDir::new().unwrap();
    ImageLayout::init(layout_dir.path()).unwrap();
    let tmp = TempDir::new().unwrap();
    let sbom = write_file(&tmp, "s.json", br#"{"spdxVersion":"SPDX-2.3"}"#);

    let err = run_attach(AttachArgs {
        reference: "example.invalid/absent:1",
        sbom: &sbom,
        format: None,
        push: false,
        layout_dir_override: Some(layout_dir.path()),
        insecure_registry: false,
        username: None,
        password_stdin: false,
    })
    .expect_err("a missing subject image must error");
    assert!(matches!(err, CliSbomError::ImageNotFound(_)), "got {err:?}");
}

#[test]
fn undetectable_without_format_is_a_clear_error() {
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    emit_image(
        &layout,
        &[],
        &ImageConfig::default(),
        "example.invalid/app:3",
    )
    .unwrap();
    let tmp = TempDir::new().unwrap();
    let sbom = write_file(&tmp, "opaque.json", b"{}");

    let err = run_attach(AttachArgs {
        reference: "example.invalid/app:3",
        sbom: &sbom,
        format: None,
        push: false,
        layout_dir_override: Some(layout_dir.path()),
        insecure_registry: false,
        username: None,
        password_stdin: false,
    })
    .expect_err("an unrecognizable SBOM without --format must error");
    assert!(
        matches!(err, CliSbomError::UndetectableFormat(_)),
        "got {err:?}",
    );
}
