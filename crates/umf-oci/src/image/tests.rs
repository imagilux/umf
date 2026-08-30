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

/// Read every entry path out of a gzip-compressed layer blob.
fn layer_entry_paths(layer: &super::LayerSource) -> Vec<String> {
    let tar_bytes = {
        let mut out = Vec::new();
        let mut dec = flate2::read::GzDecoder::new(&layer.data[..]);
        std::io::Read::read_to_end(&mut dec, &mut out).expect("gunzip layer");
        out
    };
    let mut archive = tar::Archive::new(&tar_bytes[..]);
    archive
        .entries()
        .expect("entries")
        .map(|e| {
            e.expect("entry")
                .path()
                .expect("path")
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[test]
fn overlay_deletion_becomes_a_wh_marker_entry() {
    // overlayfs records a deletion as a 0/0 character device. Packing the
    // upper-dir must translate it to the OCI `.wh.<name>` spelling, or the
    // deletion is silently lost (a `RUN rm` that does nothing).
    //
    // Creating a character device needs CAP_MKNOD, so this asserts nothing
    // when unprivileged. The opaque-directory sibling below covers the same
    // translation path without privilege and always runs.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("kept.txt"), b"still here").expect("write");

    let whiteout = root.join("deleted.txt");
    let made = nix::sys::stat::mknod(
        &whiteout,
        nix::sys::stat::SFlag::S_IFCHR,
        nix::sys::stat::Mode::from_bits_truncate(0o600),
        0, // device 0/0 — the overlayfs deletion marker
    );
    if made.is_err() {
        eprintln!("skipping: mknod needs CAP_MKNOD (unprivileged environment)");
        return;
    }

    let layer = super::LayerSource::from_directory(root).expect("pack layer");
    let paths = layer_entry_paths(&layer);

    assert!(
        paths.iter().any(|p| p == ".wh.deleted.txt"),
        "deletion must be packed as `.wh.deleted.txt`, got {paths:?}",
    );
    assert!(
        !paths.iter().any(|p| p == "deleted.txt"),
        "the character device itself must not be packed, got {paths:?}",
    );
    assert!(
        paths.iter().any(|p| p == "kept.txt"),
        "ordinary files still pack, got {paths:?}",
    );
}

#[test]
fn opaque_directory_becomes_a_wh_opq_entry() {
    // An overlay marks "discard the lower layers' view of this directory"
    // with an xattr; OCI spells it as a `.wh..wh..opq` entry inside the dir.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let opaque = root.join("etc");
    std::fs::create_dir(&opaque).expect("mkdir");
    std::fs::write(opaque.join("fresh.conf"), b"new").expect("write");

    if xattr::set(&opaque, "user.overlay.opaque", b"y").is_err() {
        eprintln!("skipping: filesystem does not support user xattrs");
        return;
    }

    let layer = super::LayerSource::from_directory(root).expect("pack layer");
    let paths = layer_entry_paths(&layer);

    assert!(
        paths.iter().any(|p| p == "etc/.wh..wh..opq"),
        "opaque dir must carry `.wh..wh..opq`, got {paths:?}",
    );
    assert!(
        paths.iter().any(|p| p == "etc/fresh.conf"),
        "the directory's own entries still pack, got {paths:?}",
    );
}

#[test]
fn a_layer_with_no_whiteouts_is_unchanged() {
    // Regression guard: the whiteout translation must not perturb the
    // ordinary path, since diff_ids are content-addressed and any spurious
    // entry would invalidate every cached layer in the wild.
    let dir = tempfile::tempdir().expect("tempdir");
    tiny_dir(dir.path());
    let layer = super::LayerSource::from_directory(dir.path()).expect("pack");
    let paths = layer_entry_paths(&layer);
    assert!(
        !paths.iter().any(|p| p.contains(".wh.")),
        "no whiteout entries may appear for an ordinary tree, got {paths:?}",
    );
}

#[test]
fn a_deletion_round_trips_from_upper_dir_to_materialized_rootfs() {
    // The acceptance criterion for the whiteout work: a `RUN rm` (an
    // overlayfs 0/0 character device in the upper dir) must actually remove
    // the file when the resulting layers are applied. Before whiteout
    // emission this silently kept the file.
    let lower = tempfile::tempdir().expect("tempdir");
    std::fs::write(lower.path().join("keep.txt"), b"keep").expect("write");
    std::fs::write(lower.path().join("remove.txt"), b"doomed").expect("write");
    let base_layer = super::LayerSource::from_directory(lower.path()).expect("pack base");

    let upper = tempfile::tempdir().expect("tempdir");
    let made = nix::sys::stat::mknod(
        &upper.path().join("remove.txt"),
        nix::sys::stat::SFlag::S_IFCHR,
        nix::sys::stat::Mode::from_bits_truncate(0o600),
        0,
    );
    if made.is_err() {
        eprintln!("skipping: mknod needs CAP_MKNOD (unprivileged environment)");
        return;
    }
    let delete_layer = super::LayerSource::from_directory(upper.path()).expect("pack delete");

    let rootfs = tempfile::tempdir().expect("tempdir");
    for layer in [&base_layer, &delete_layer] {
        crate::materialize::apply_layer(&layer.data[..], rootfs.path()).expect("apply layer");
    }

    assert!(
        rootfs.path().join("keep.txt").exists(),
        "an untouched file survives",
    );
    assert!(
        !rootfs.path().join("remove.txt").exists(),
        "the whiteout must delete the lower-layer file",
    );
    assert!(
        !rootfs.path().join(".wh.remove.txt").exists(),
        "the marker itself must not land in the rootfs",
    );
}

/// A one-file tar, and the same bytes under each codec.
fn tar_and_encodings() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    use std::io::Write as _;
    let mut tar = Vec::new();
    {
        let mut b = tar::Builder::new(&mut tar);
        b.mode(tar::HeaderMode::Deterministic);
        let content = b"normalised\n";
        let mut h = tar::Header::new_gnu();
        h.set_size(content.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, "etc/marker", &content[..]).unwrap();
        b.finish().unwrap();
    }
    let xz = {
        let mut e = liblzma::write::XzEncoder::new(Vec::new(), 6);
        e.write_all(&tar).unwrap();
        e.finish().unwrap()
    };
    let bz = {
        let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        e.write_all(&tar).unwrap();
        e.finish().unwrap()
    };
    (tar, xz, bz)
}

#[test]
fn normalizing_preserves_diff_id_and_rewrites_the_media_type() {
    // The property that makes normalisation safe: `diff_id` is the digest of
    // the *uncompressed* tar, so re-compressing changes the blob but not the
    // layer's identity in `rootfs.diff_ids`. The image's content identity
    // survives; only the stored bytes and the descriptor change.
    let (tar, xz, bz) = tar_and_encodings();
    let expected_diff_id = super::sha256_digest(&tar);

    for (label, blob, mt) in [
        ("xz", xz, "application/vnd.oci.image.layer.v1.tar+xz"),
        ("bzip2", bz, "application/vnd.oci.image.layer.v1.tar+bzip2"),
    ] {
        let original = super::LayerSource {
            data: bytes::Bytes::from(blob),
            media_type: mt.to_string(),
            diff_id: expected_diff_id.clone(),
        };
        let normalized = original
            .normalized(super::LayerCompression::Gzip)
            .unwrap_or_else(|e| panic!("{label}: {e}"));

        assert_eq!(
            normalized.diff_id, expected_diff_id,
            "{label}: diff_id must survive re-compression",
        );
        assert_eq!(
            normalized.media_type,
            super::LayerCompression::Gzip.media_type(),
            "{label}: media type rewritten to the build's codec",
        );
        assert!(
            super::is_oci_layer_media_type(&normalized.media_type),
            "{label}: result must be a spec-defined layer type",
        );
        // And the bytes really are gzip now, not relabelled xz.
        assert_eq!(
            crate::format::detect(&normalized.data[..]),
            crate::format::Format::Gzip,
            "{label}: bytes must actually be re-encoded",
        );
    }
}

#[test]
fn normalizing_leaves_spec_defined_codecs_untouched() {
    // The common case must cost nothing: no decode, no re-encode, same blob.
    let dir = tempfile::tempdir().expect("tempdir");
    tiny_dir(dir.path());
    for compression in [super::LayerCompression::Gzip, super::LayerCompression::Zstd] {
        let layer = super::LayerSource::from_directory_with(dir.path(), compression).expect("pack");
        let before = (
            layer.data.clone(),
            layer.media_type.clone(),
            layer.diff_id.clone(),
        );
        let after = layer
            .normalized(super::LayerCompression::Gzip)
            .expect("normalize");
        assert_eq!(
            (after.data, after.media_type, after.diff_id),
            before,
            "a spec-defined codec must pass through byte-identical",
        );
    }
}

#[test]
fn normalizing_honours_the_builds_chosen_codec() {
    // Normalisation targets whatever the build emits, not a hardcoded gzip.
    let (_tar, xz, _bz) = tar_and_encodings();
    let layer = super::LayerSource {
        data: bytes::Bytes::from(xz),
        media_type: "application/vnd.oci.image.layer.v1.tar+xz".to_string(),
        diff_id: "sha256:placeholder".to_string(),
    };
    let normalized = layer
        .normalized(super::LayerCompression::Zstd)
        .expect("normalize to zstd");
    assert_eq!(
        normalized.media_type,
        super::LayerCompression::Zstd.media_type()
    );
    assert_eq!(
        crate::format::detect(&normalized.data[..]),
        crate::format::Format::Zstd,
    );
}

#[test]
fn normalizing_recomputes_a_wrong_diff_id() {
    // A base image whose config disagreed with its blobs must not have that
    // disagreement propagated into what we emit; the decoded bytes are
    // authoritative.
    let (tar, xz, _bz) = tar_and_encodings();
    let layer = super::LayerSource {
        data: bytes::Bytes::from(xz),
        media_type: "application/vnd.oci.image.layer.v1.tar+xz".to_string(),
        diff_id: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
            .to_string(),
    };
    let normalized = layer
        .normalized(super::LayerCompression::Gzip)
        .expect("normalize");
    assert_eq!(
        normalized.diff_id,
        super::sha256_digest(&tar),
        "diff_id is recomputed from the decoded bytes, not trusted",
    );
}
