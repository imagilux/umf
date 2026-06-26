//! Unit tests for the `image` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use std::fs;
use tempfile::tempdir;

fn tiny_dir(root: &Path) {
    fs::create_dir_all(root.join("bin")).expect("mkdir bin");
    fs::write(root.join("bin/hello"), b"echo hi\n").expect("write hello");
    fs::write(root.join("README"), b"hello, umf\n").expect("write README");
}

#[test]
fn emit_image_with_zero_layers_writes_empty_rootfs() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let entry = emit_image(
        &layout,
        &[],
        &ImageConfig::default(),
        "example.invalid/zero:latest",
    )
    .expect("emit");
    assert!(layout.has_blob(&entry.digest));

    let manifest_bytes = layout.read_blob(&entry.digest).expect("manifest");
    let manifest: OciImageManifest = serde_json::from_slice(&manifest_bytes).expect("parse");
    assert!(manifest.layers.is_empty());

    let config_bytes = layout
        .read_blob(&manifest.config.digest)
        .expect("config blob");
    let parsed: serde_json::Value = serde_json::from_slice(&config_bytes).expect("config json");
    let diff_ids = parsed
        .get("rootfs")
        .and_then(|r| r.get("diff_ids"))
        .and_then(|d| d.as_array())
        .expect("rootfs.diff_ids array");
    assert!(diff_ids.is_empty());
}

#[test]
fn layer_from_directory_diff_id_differs_from_blob_digest() {
    let src = tempdir().expect("src tempdir");
    tiny_dir(src.path());

    let layer = LayerSource::from_directory(src.path()).expect("build layer");
    let blob_digest = sha256_digest(&layer.data);

    // diff_id is the uncompressed-tar digest; blob digest is the gzipped
    // blob's digest. They must differ for any non-trivial layer.
    assert_ne!(layer.diff_id, blob_digest);
    assert!(layer.diff_id.starts_with("sha256:"));
    assert!(blob_digest.starts_with("sha256:"));
    assert_eq!(layer.media_type, IMAGE_LAYER_GZIP_MEDIA_TYPE);
}

#[test]
fn from_directory_defaults_to_gzip() {
    // The default codec stays gzip so existing byte-for-byte expectations
    // hold; `from_directory` must equal `from_directory_with(.., Gzip)`.
    let src = tempdir().expect("src tempdir");
    tiny_dir(src.path());
    let default = LayerSource::from_directory(src.path()).expect("default");
    let explicit_gzip =
        LayerSource::from_directory_with(src.path(), LayerCompression::Gzip).expect("gzip");
    assert_eq!(default.media_type, IMAGE_LAYER_GZIP_MEDIA_TYPE);
    assert_eq!(default.media_type, explicit_gzip.media_type);
    assert_eq!(default.diff_id, explicit_gzip.diff_id);
    assert_eq!(default.data, explicit_gzip.data);
}

#[test]
fn zstd_layer_shares_diff_id_with_gzip_but_differs_in_blob() {
    // The codec changes only the blob bytes + media type; the diff_id is
    // the uncompressed-tar sha256 and so is identical to the gzip layer's.
    let src = tempdir().expect("src tempdir");
    tiny_dir(src.path());
    let gzip =
        LayerSource::from_directory_with(src.path(), LayerCompression::Gzip).expect("gzip layer");
    let zstd =
        LayerSource::from_directory_with(src.path(), LayerCompression::Zstd).expect("zstd layer");

    assert_eq!(zstd.media_type, IMAGE_LAYER_ZSTD_MEDIA_TYPE);
    assert_eq!(
        zstd.diff_id, gzip.diff_id,
        "diff_id is the uncompressed-tar digest; codec-independent",
    );
    assert_ne!(zstd.data, gzip.data, "compressed blobs differ by codec");
    // The blob really is a zstd stream (magic `28 b5 2f fd`).
    assert_eq!(
        crate::format::detect(&zstd.data),
        crate::format::Format::Zstd
    );
}

#[test]
fn zstd_layer_is_byte_reproducible() {
    // Two zstd layers from the same directory must be byte-identical so the
    // blob digest (and thus the manifest digest) is reproducible.
    let src = tempdir().expect("src tempdir");
    tiny_dir(src.path());
    let a = LayerSource::from_directory_with(src.path(), LayerCompression::Zstd).expect("a");
    let b = LayerSource::from_directory_with(src.path(), LayerCompression::Zstd).expect("b");
    assert_eq!(a.diff_id, b.diff_id);
    assert_eq!(a.data, b.data, "zstd blob must be deterministic");

    // And the emitted manifest digest is stable across layouts.
    let cfg = ImageConfig::default();
    let layout_a_dir = tempdir().expect("layout a");
    let layout_a = ImageLayout::init(layout_a_dir.path()).expect("init a");
    let entry_a = emit_image(&layout_a, &[a], &cfg, "example.invalid/z:1").expect("emit a");
    let layout_b_dir = tempdir().expect("layout b");
    let layout_b = ImageLayout::init(layout_b_dir.path()).expect("init b");
    let entry_b = emit_image(&layout_b, &[b], &cfg, "example.invalid/z:1").expect("emit b");
    assert_eq!(entry_a.digest, entry_b.digest);
}

#[test]
fn zstd_layer_descriptor_carries_zstd_media_type() {
    // The manifest descriptor for a zstd layer must advertise `+zstd`.
    let src = tempdir().expect("src tempdir");
    tiny_dir(src.path());
    let layer =
        LayerSource::from_directory_with(src.path(), LayerCompression::Zstd).expect("zstd layer");
    let layout_dir = tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init");
    let entry = emit_image(
        &layout,
        std::slice::from_ref(&layer),
        &ImageConfig::default(),
        "example.invalid/zm:1",
    )
    .expect("emit");
    let manifest_bytes = layout.read_blob(&entry.digest).expect("manifest");
    let manifest: OciImageManifest = serde_json::from_slice(&manifest_bytes).expect("parse");
    assert_eq!(manifest.layers.len(), 1);
    assert_eq!(manifest.layers[0].media_type, IMAGE_LAYER_ZSTD_MEDIA_TYPE);
}

#[test]
fn emit_is_byte_reproducible_for_identical_inputs() {
    let src = tempdir().expect("src tempdir");
    tiny_dir(src.path());
    // Two layers from the same directory → identical bytes.
    let layer_a = LayerSource::from_directory(src.path()).expect("layer a");
    let layer_b = LayerSource::from_directory(src.path()).expect("layer b");
    assert_eq!(layer_a.diff_id, layer_b.diff_id);
    assert_eq!(layer_a.data, layer_b.data);

    let cfg = ImageConfig {
        container: ContainerConfig {
            entrypoint: Some(vec!["/bin/hello".to_string()]),
            env: vec!["PATH=/usr/local/bin:/usr/bin".to_string()],
            ..ContainerConfig::default()
        },
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };

    let layout_a_dir = tempdir().expect("layout a");
    let layout_a = ImageLayout::init(layout_a_dir.path()).expect("init a");
    let entry_a = emit_image(&layout_a, &[layer_a], &cfg, "example.invalid/x:1").expect("emit a");

    let layout_b_dir = tempdir().expect("layout b");
    let layout_b = ImageLayout::init(layout_b_dir.path()).expect("init b");
    let entry_b = emit_image(&layout_b, &[layer_b], &cfg, "example.invalid/x:1").expect("emit b");

    assert_eq!(
        entry_a.digest, entry_b.digest,
        "manifest digest must match across runs with identical inputs",
    );
}

#[test]
fn container_config_fields_appear_in_emitted_blob() {
    let layout_dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init");

    let cfg = ImageConfig {
        container: ContainerConfig {
            user: Some("nobody".to_string()),
            env: vec!["PATH=/usr/local/bin".to_string()],
            entrypoint: Some(vec!["/bin/hello".to_string()]),
            cmd: Some(vec!["--help".to_string()]),
            working_dir: Some("/srv".to_string()),
            exposed_ports: vec!["80/tcp".to_string(), "443/tcp".to_string()],
            volumes: vec!["/data".to_string()],
            stop_signal: Some("SIGTERM".to_string()),
            labels: BTreeMap::new(),
        },
        ..ImageConfig::default()
    };
    let entry = emit_image(&layout, &[], &cfg, "example.invalid/k:1").expect("emit");

    let manifest_bytes = layout.read_blob(&entry.digest).expect("manifest");
    let manifest: OciImageManifest = serde_json::from_slice(&manifest_bytes).expect("parse");
    let config_bytes = layout
        .read_blob(&manifest.config.digest)
        .expect("config blob");
    let v: serde_json::Value = serde_json::from_slice(&config_bytes).expect("config json");
    let cfg_obj = v.get("config").expect("config sub-object");
    assert_eq!(cfg_obj.get("User"), Some(&serde_json::json!("nobody")));
    assert_eq!(
        cfg_obj.get("Env"),
        Some(&serde_json::json!(["PATH=/usr/local/bin"])),
    );
    assert_eq!(
        cfg_obj.get("Entrypoint"),
        Some(&serde_json::json!(["/bin/hello"])),
    );
    assert_eq!(cfg_obj.get("Cmd"), Some(&serde_json::json!(["--help"])));
    assert_eq!(cfg_obj.get("WorkingDir"), Some(&serde_json::json!("/srv")));
    assert_eq!(
        cfg_obj.get("ExposedPorts"),
        Some(&serde_json::json!({"80/tcp": {}, "443/tcp": {}})),
    );
    assert_eq!(
        cfg_obj.get("Volumes"),
        Some(&serde_json::json!({"/data": {}})),
    );
    assert_eq!(
        cfg_obj.get("StopSignal"),
        Some(&serde_json::json!("SIGTERM")),
    );
}

#[test]
fn layer_blob_digest_matches_descriptor() {
    let src = tempdir().expect("src tempdir");
    tiny_dir(src.path());
    let layer = LayerSource::from_directory(src.path()).expect("build layer");

    let layout_dir = tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init");
    let entry = emit_image(
        &layout,
        std::slice::from_ref(&layer),
        &ImageConfig::default(),
        "example.invalid/l:1",
    )
    .expect("emit");
    let manifest_bytes = layout.read_blob(&entry.digest).expect("manifest");
    let manifest: OciImageManifest = serde_json::from_slice(&manifest_bytes).expect("parse");
    assert_eq!(manifest.layers.len(), 1);
    let descriptor = &manifest.layers[0];
    assert_eq!(descriptor.digest, sha256_digest(&layer.data));
    assert_eq!(descriptor.size, layer.data.len() as i64);

    // The diff_id lives in the config, not on the manifest descriptor.
    let config_bytes = layout
        .read_blob(&manifest.config.digest)
        .expect("config blob");
    let parsed: serde_json::Value = serde_json::from_slice(&config_bytes).expect("config json");
    let diff_ids = parsed
        .get("rootfs")
        .and_then(|r| r.get("diff_ids"))
        .and_then(|d| d.as_array())
        .expect("diff_ids");
    assert_eq!(diff_ids.len(), 1);
    assert_eq!(diff_ids[0], serde_json::Value::String(layer.diff_id));
}

#[test]
fn history_field_omitted_when_empty() {
    // Default ImageConfig carries an empty `history` Vec; the serialised
    // config blob shouldn't even contain the key (conventional producer
    // behaviour for trivial scratch images).
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let entry = emit_image(
        &layout,
        &[],
        &ImageConfig::default(),
        "example.invalid/empty:1",
    )
    .expect("emit");
    let manifest_bytes = layout.read_blob(&entry.digest).expect("manifest");
    let manifest: OciImageManifest = serde_json::from_slice(&manifest_bytes).expect("parse");
    let config_bytes = layout
        .read_blob(&manifest.config.digest)
        .expect("config blob");
    let parsed: serde_json::Value = serde_json::from_slice(&config_bytes).expect("config json");
    assert!(
        parsed.get("history").is_none(),
        "empty history should be omitted; got: {parsed}"
    );
}

#[test]
fn history_round_trips_through_emit_image() {
    // Supply a history with one populated step + one empty_layer step;
    // confirm both shapes (with/without empty_layer) come back intact.
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let config = ImageConfig {
        history: vec![
            HistoryEntry {
                created: Some("2024-01-01T00:00:00Z".to_string()),
                created_by: Some("/bin/sh -c apk add curl".to_string()),
                author: None,
                comment: None,
                empty_layer: false,
            },
            HistoryEntry {
                created: Some("2024-01-01T00:00:01Z".to_string()),
                created_by: Some("LABEL maintainer=Imagilux".to_string()),
                author: None,
                comment: None,
                empty_layer: true,
            },
        ],
        ..ImageConfig::default()
    };
    let entry = emit_image(&layout, &[], &config, "example.invalid/hist:1").expect("emit");
    let manifest_bytes = layout.read_blob(&entry.digest).expect("manifest");
    let manifest: OciImageManifest = serde_json::from_slice(&manifest_bytes).expect("parse");
    let config_bytes = layout
        .read_blob(&manifest.config.digest)
        .expect("config blob");
    let parsed: serde_json::Value = serde_json::from_slice(&config_bytes).expect("config json");
    let history = parsed
        .get("history")
        .and_then(|v| v.as_array())
        .expect("history array present");
    assert_eq!(history.len(), 2);

    // First entry: filesystem-affecting step. `empty_layer` should be
    // omitted (since `false` is the spec default).
    assert_eq!(
        history[0].get("created").and_then(|v| v.as_str()),
        Some("2024-01-01T00:00:00Z")
    );
    assert_eq!(
        history[0].get("created_by").and_then(|v| v.as_str()),
        Some("/bin/sh -c apk add curl")
    );
    assert!(
        history[0].get("empty_layer").is_none(),
        "empty_layer=false should be omitted",
    );

    // Second entry: metadata-only step. `empty_layer: true` must serialise.
    assert_eq!(
        history[1]
            .get("empty_layer")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        history[1].get("created_by").and_then(|v| v.as_str()),
        Some("LABEL maintainer=Imagilux")
    );
}
