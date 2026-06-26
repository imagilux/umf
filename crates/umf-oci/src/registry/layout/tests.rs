//! Unit tests for the `layout` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use tempfile::tempdir;

#[test]
fn sha256_matches_known_vector() {
    // "" → sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
    assert_eq!(
        sha256_digest(b""),
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn init_creates_marker_and_index() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init layout");
    assert_eq!(layout.root(), dir.path());
    assert!(dir.path().join("oci-layout").is_file());
    assert!(dir.path().join("index.json").is_file());
    assert!(dir.path().join("blobs/sha256").is_dir());
}

#[test]
fn init_is_idempotent() {
    let dir = tempdir().expect("tempdir");
    let _ = ImageLayout::init(dir.path()).expect("init 1");
    let _ = ImageLayout::init(dir.path()).expect("init 2");
    let marker = fs::read_to_string(dir.path().join("oci-layout")).expect("read marker");
    let parsed: LayoutMarker = serde_json::from_str(&marker).expect("parse marker");
    assert_eq!(parsed.image_layout_version, IMAGE_LAYOUT_VERSION);
}

#[test]
fn open_rejects_missing_marker() {
    let dir = tempdir().expect("tempdir");
    let err = ImageLayout::open(dir.path()).expect_err("must fail");
    assert!(matches!(err, RegistryError::InvalidLayout(_)));
}

#[test]
fn open_rejects_bad_version() {
    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("oci-layout"),
        br#"{"imageLayoutVersion":"9.9.9"}"#,
    )
    .expect("write marker");
    let err = ImageLayout::open(dir.path()).expect_err("must fail");
    assert!(matches!(err, RegistryError::InvalidLayout(_)));
}

#[test]
fn write_blob_round_trips() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let payload = b"hello, umf";
    let digest = layout.write_blob(payload).expect("write blob");
    assert!(layout.has_blob(&digest));
    let read = layout.read_blob(&digest).expect("read blob");
    assert_eq!(read, payload);
}

#[test]
fn write_blob_with_digest_rejects_mismatch() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let err = layout
        .write_blob_with_digest(
            b"hello",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        )
        .expect_err("must fail");
    assert!(matches!(err, RegistryError::DigestMismatch { .. }));
}

#[test]
fn block_cache_path_form_and_validation() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let hex = "ab".repeat(32);
    let digest = format!("sha256:{hex}");
    let p = layout
        .block_cache_path(&digest, "00ff")
        .expect("valid path");
    assert_eq!(
        p.file_name().expect("file name"),
        std::ffi::OsStr::new("00ff.img")
    );
    assert_eq!(
        p.parent().expect("parent").file_name().expect("dir name"),
        std::ffi::OsStr::new(hex.as_str()),
    );
    // Reuses `split_digest`, so a traversal-laced digest is rejected.
    assert!(
        layout
            .block_cache_path("sha256:../../etc/x", "00ff")
            .is_err()
    );
    // `variant` is joined into the path → must be non-empty hex.
    assert!(layout.block_cache_path(&digest, "../x").is_err());
    assert!(layout.block_cache_path(&digest, "").is_err());
}

#[test]
fn prune_block_cache_keeps_referenced_removes_orphans() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let live_hex = "11".repeat(32);
    layout
        .upsert_ref(
            "os:latest",
            ImageIndexEntry {
                media_type: oci_client::manifest::OCI_IMAGE_MEDIA_TYPE.to_string(),
                digest: format!("sha256:{live_hex}"),
                size: 7,
                platform: None,
                annotations: None,
            },
        )
        .expect("upsert");

    let orphan_hex = "22".repeat(32);
    let blocks = dir.path().join(CACHE_DIR).join(BLOCK_CACHE_DIR);
    for hex in [&live_hex, &orphan_hex] {
        let d = blocks.join(hex);
        fs::create_dir_all(&d).expect("mkdir block dir");
        fs::write(d.join("deadbeef.img"), b"DISK").expect("write block");
    }

    let (removed, bytes_freed) = layout.prune_block_cache().expect("prune");
    assert_eq!(removed, 1, "only the orphan subtree should be removed");
    assert_eq!(bytes_freed, 4, "freed the orphan's 4-byte block");
    assert!(blocks.join(&live_hex).is_dir(), "referenced block kept");
    assert!(!blocks.join(&orphan_hex).exists(), "orphan block removed");
}

#[test]
fn write_blob_dedups_by_digest() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let a = layout.write_blob(b"identical").expect("first write");
    let b = layout.write_blob(b"identical").expect("second write");
    assert_eq!(a, b);
}

#[test]
fn upsert_and_lookup_ref() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let entry = ImageIndexEntry {
        media_type: oci_client::manifest::OCI_IMAGE_MEDIA_TYPE.to_string(),
        digest: "sha256:11111111111111111111111111111111111111111111111111111111111111aa".into(),
        size: 7,
        platform: None,
        annotations: None,
    };
    layout
        .upsert_ref("alpine:latest", entry.clone())
        .expect("upsert");
    let found = layout
        .lookup_ref("alpine:latest")
        .expect("lookup")
        .expect("present");
    assert_eq!(found.digest, entry.digest);
    // Re-inserting under the same ref name must not duplicate.
    layout
        .upsert_ref("alpine:latest", entry)
        .expect("re-upsert");
    let index = layout.read_index().expect("index");
    assert_eq!(index.manifests.len(), 1);
}

#[test]
fn blob_path_rejects_non_sha256() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let err = layout.blob_path("sha512:abc").expect_err("must reject");
    assert!(matches!(err, RegistryError::MalformedDigest(_)));
}

#[test]
fn blob_path_rejects_malformed_digest() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    for bad in ["", ":abc", "sha256:", "no-colon"] {
        assert!(matches!(
            layout.blob_path(bad).expect_err("must reject"),
            RegistryError::MalformedDigest(_)
        ));
    }
}

/// A non-hex `hex` half (path separators, `..`, absolute path) must be
/// rejected before it can be joined into a filesystem path — otherwise
/// a malicious image's digest / diff_id escapes the layout.
#[test]
fn digest_paths_reject_traversal_hex() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let payloads = [
        "sha256:../../etc/passwd",
        "sha256:/etc/cron.d/pwn",
        "sha256:..",
        "sha256:abc/def",
        "sha256:a.b",
    ];
    for bad in payloads {
        assert!(
            matches!(
                layout.blob_path(bad).expect_err("blob_path must reject"),
                RegistryError::MalformedDigest(_)
            ),
            "blob_path accepted traversal digest {bad:?}"
        );
        assert!(
            matches!(
                layout
                    .erofs_cache_path(bad)
                    .expect_err("erofs_cache_path must reject"),
                RegistryError::MalformedDigest(_)
            ),
            "erofs_cache_path accepted traversal diff_id {bad:?}"
        );
    }
    // A well-formed sha256 still resolves to a path inside the layout.
    let good = format!("sha256:{}", "ab".repeat(32));
    assert!(layout.erofs_cache_path(&good).is_ok());
    assert!(layout.blob_path(&good).is_ok());
}

#[test]
fn prune_erofs_cache_removes_orphans_keeps_referenced() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");

    // A config whose rootfs.diff_ids references one layer.
    let referenced = format!("{SHA256_ALGO}:{}", "11".repeat(32));
    let cfg = serde_json::json!({
        "architecture": "amd64", "os": "linux", "config": {},
        "rootfs": { "type": "layers", "diff_ids": [referenced] }
    });
    let cfg_bytes = serde_json::to_vec(&cfg).expect("cfg json");
    let cfg_digest = layout.write_blob(&cfg_bytes).expect("write cfg");
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": { "mediaType": "application/vnd.oci.image.config.v1+json",
                    "digest": cfg_digest, "size": cfg_bytes.len() },
        "layers": []
    });
    let m_bytes = serde_json::to_vec(&manifest).expect("manifest json");
    let m_digest = layout.write_blob(&m_bytes).expect("write manifest");
    layout
        .upsert_ref(
            "example.invalid/erofs-gc:1",
            ImageIndexEntry {
                media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
                digest: m_digest,
                size: m_bytes.len() as i64,
                platform: None,
                annotations: None,
            },
        )
        .expect("upsert ref");

    // One erofs file for the referenced diff_id, one orphan.
    let referenced_erofs = layout.erofs_cache_path(&referenced).expect("ref path");
    fs::write(&referenced_erofs, b"erofs-referenced").expect("write ref erofs");
    let orphan = format!("{SHA256_ALGO}:{}", "22".repeat(32));
    let orphan_erofs = layout.erofs_cache_path(&orphan).expect("orphan path");
    fs::write(&orphan_erofs, b"erofs-orphan").expect("write orphan erofs");

    let (removed, bytes) = layout.prune_erofs_cache().expect("prune erofs");
    assert_eq!(removed, 1, "only the orphan should be removed");
    assert_eq!(bytes, b"erofs-orphan".len() as u64);
    assert!(referenced_erofs.exists(), "referenced erofs must survive");
    assert!(!orphan_erofs.exists(), "orphan erofs must be gone");
}

/// Regression: a manifest that is still referenced by `index.json`
/// but whose on-disk blob is corrupt (digest no longer matches) must not be
/// treated as a leaf. If it were, its config + layers would be deemed
/// unreachable and `prune_blobs` would delete them. Instead the prune must
/// abort with a digest mismatch and leave every blob untouched.
#[test]
fn prune_blobs_keeps_subtree_of_referenced_corrupt_manifest() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");

    // A config + a single layer, both written as real blobs.
    let cfg = serde_json::json!({
        "architecture": "amd64", "os": "linux", "config": {},
        "rootfs": { "type": "layers", "diff_ids": [] }
    });
    let cfg_bytes = serde_json::to_vec(&cfg).expect("cfg json");
    let cfg_digest = layout.write_blob(&cfg_bytes).expect("write cfg");
    let layer_bytes = b"a layer blob".to_vec();
    let layer_digest = layout.write_blob(&layer_bytes).expect("write layer");

    // A manifest tying them together.
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": { "mediaType": "application/vnd.oci.image.config.v1+json",
                    "digest": cfg_digest, "size": cfg_bytes.len() },
        "layers": [ { "mediaType": "application/vnd.oci.image.layer.v1.tar",
                      "digest": layer_digest, "size": layer_bytes.len() } ]
    });
    let m_bytes = serde_json::to_vec(&manifest).expect("manifest json");
    let m_digest = layout.write_blob(&m_bytes).expect("write manifest");

    // The index references the manifest by its (correct) digest.
    layout
        .upsert_ref(
            "example.invalid/corrupt-gc:1",
            ImageIndexEntry {
                media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
                digest: m_digest.clone(),
                size: m_bytes.len() as i64,
                platform: None,
                annotations: None,
            },
        )
        .expect("upsert ref");

    // Corrupt the manifest blob in place: overwrite its bytes so the file
    // still exists at `blobs/sha256/<hex>` but no longer hashes to its
    // digest. read_blob() will now return DigestMismatch.
    let manifest_path = layout.blob_path(&m_digest).expect("manifest path");
    fs::write(&manifest_path, b"corrupted-not-json").expect("corrupt manifest");
    assert!(
        matches!(
            layout.read_blob(&m_digest),
            Err(RegistryError::DigestMismatch { .. })
        ),
        "precondition: corrupt manifest must read as a digest mismatch"
    );

    // Prune must refuse to proceed rather than GC the referenced subtree.
    let err = layout
        .prune_blobs()
        .expect_err("prune must abort on a referenced corrupt manifest");
    assert!(
        matches!(err, RegistryError::DigestMismatch { .. }),
        "expected DigestMismatch, got {err:?}"
    );

    // Crucially, the config + layer blobs are still on disk.
    assert!(layout.has_blob(&cfg_digest), "config blob must survive");
    assert!(layout.has_blob(&layer_digest), "layer blob must survive");
}

/// Counterpart to the corrupt-manifest case: a manifest that is referenced
/// by the index but genuinely *absent* on disk is a safe leaf. The walk
/// treats it as childless and the prune completes (it cannot reach, and so
/// will not retain, any config/layers — but it also doesn't have any other
/// referenced blob to drop here). This guards the NotFound vs DigestMismatch
/// split from regressing back to "swallow every error".
#[test]
fn prune_blobs_tolerates_referenced_absent_manifest() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");

    // Reference a manifest digest for which no blob was ever written.
    let absent = format!("{SHA256_ALGO}:{}", "33".repeat(32));
    layout
        .upsert_ref(
            "example.invalid/absent-gc:1",
            ImageIndexEntry {
                media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
                digest: absent,
                size: 0,
                platform: None,
                annotations: None,
            },
        )
        .expect("upsert ref");

    // No blob on disk → nothing to remove, and no abort.
    let (removed, bytes) = layout
        .prune_blobs()
        .expect("prune must tolerate a genuinely-absent referenced manifest");
    assert_eq!(removed, 0);
    assert_eq!(bytes, 0);
}

#[test]
fn upsert_untagged_dedupes_by_digest_and_spares_tagged_entries() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init layout");

    let entry = |digest: &str| ImageIndexEntry {
        media_type: OCI_IMAGE_MEDIA_TYPE.to_string(),
        digest: digest.to_string(),
        size: 7,
        platform: None,
        annotations: None,
    };

    // A tagged entry and an untagged entry may share a digest.
    layout
        .upsert_ref("example.invalid/app:1", entry("sha256:aaaa"))
        .expect("upsert tagged");
    layout
        .upsert_untagged(entry("sha256:aaaa"))
        .expect("upsert untagged");
    // Re-upserting the same untagged digest replaces, never duplicates.
    layout
        .upsert_untagged(entry("sha256:aaaa"))
        .expect("re-upsert untagged");
    layout
        .upsert_untagged(entry("sha256:bbbb"))
        .expect("upsert second untagged");

    let index = layout.read_index().expect("read index");
    assert_eq!(index.manifests.len(), 3, "tagged + two distinct untagged");
    assert_eq!(
        layout.list_refs().expect("list refs").len(),
        1,
        "untagged entries stay out of the ref listing",
    );
}

#[test]
fn list_referrers_matches_subject_and_filters_by_artifact_type() {
    use bytes::Bytes;

    use crate::image::{
        ArtifactBlob, ImageConfig, emit_artifact_manifest, emit_image, subject_from_entry,
    };

    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init layout");

    let subject = emit_image(
        &layout,
        &[],
        &ImageConfig::default(),
        "example.invalid/app:1",
    )
    .expect("emit subject");
    let other = emit_image(
        &layout,
        &[],
        &ImageConfig {
            architecture: "arm64".to_string(),
            ..ImageConfig::default()
        },
        "example.invalid/other:1",
    )
    .expect("emit unrelated image");

    let blob = ArtifactBlob {
        media_type: "application/spdx+json".to_string(),
        data: Bytes::from_static(b"{}"),
        annotations: None,
    };
    let sbom = emit_artifact_manifest(
        &layout,
        "application/spdx+json",
        Some(&subject_from_entry(&subject)),
        std::slice::from_ref(&blob),
        None,
        None,
    )
    .expect("emit sbom artifact");
    let signature = emit_artifact_manifest(
        &layout,
        "application/vnd.example.signature.v1",
        Some(&subject_from_entry(&subject)),
        &[],
        None,
        None,
    )
    .expect("emit signature artifact");
    // A referrer of a *different* subject must never show up.
    emit_artifact_manifest(
        &layout,
        "application/spdx+json",
        Some(&subject_from_entry(&other)),
        &[],
        None,
        None,
    )
    .expect("emit unrelated artifact");

    let all = layout
        .list_referrers(&subject.digest, None)
        .expect("list referrers");
    let mut digests: Vec<&str> = all.iter().map(|d| d.digest.as_str()).collect();
    digests.sort_unstable();
    let mut expected = [sbom.digest.as_str(), signature.digest.as_str()];
    expected.sort_unstable();
    assert_eq!(digests, expected);

    let filtered = layout
        .list_referrers(&subject.digest, Some("application/spdx+json"))
        .expect("filtered listing");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].digest, sbom.digest);
    assert_eq!(
        filtered[0].artifact_type.as_deref(),
        Some("application/spdx+json"),
        "descriptors carry the referrer's artifactType",
    );

    assert!(
        layout
            .list_referrers(&subject.digest, Some("application/never"))
            .expect("empty filtered listing")
            .is_empty()
    );
}

/// `image_disk_size` reports an image's real on-disk footprint — the summed
/// size of every unique blob (the manifest, its config and layers, and for an
/// index every child manifest's subtree), de-duped by digest — not the
/// top-level manifest's own byte length. Regression: `umf images` previously
/// showed a multi-arch index's tiny (~9 KiB) manifest size
/// instead of its real tens-of-MiB layer payload.
#[test]
fn image_disk_size_sums_unique_blobs_across_index_children() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");

    // Two real layer blobs of distinct, non-trivial sizes. `shared` is
    // referenced by BOTH child manifests (must be counted once); `only_a` by
    // the first child only. write_blob stores raw bytes, so a blob file's size
    // equals its byte length.
    let shared_layer = vec![0xab_u8; 6000];
    let only_a_layer = vec![0xcd_u8; 3000];
    let shared_digest = layout
        .write_blob(&shared_layer)
        .expect("write shared layer");
    let only_a_digest = layout
        .write_blob(&only_a_layer)
        .expect("write only-a layer");

    let write_child = |arch: &str, layers: &[(&str, usize)]| -> (String, usize, usize) {
        let cfg_bytes = serde_json::to_vec(&serde_json::json!({
            "architecture": arch, "os": "linux", "config": {},
            "rootfs": { "type": "layers", "diff_ids": [] }
        }))
        .expect("cfg json");
        let cfg_digest = layout.write_blob(&cfg_bytes).expect("write cfg");
        let layer_descs: Vec<_> = layers
            .iter()
            .map(|(d, n)| {
                serde_json::json!({
                    "mediaType": "application/vnd.oci.image.layer.v1.tar",
                    "digest": d, "size": n
                })
            })
            .collect();
        let m_bytes = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": { "mediaType": "application/vnd.oci.image.config.v1+json",
                        "digest": cfg_digest, "size": cfg_bytes.len() },
            "layers": layer_descs
        }))
        .expect("manifest json");
        let m_digest = layout.write_blob(&m_bytes).expect("write manifest");
        (m_digest, m_bytes.len(), cfg_bytes.len())
    };

    let (manifest_a, m_a_len, cfg_a_len) = write_child(
        "amd64",
        &[
            (&shared_digest, shared_layer.len()),
            (&only_a_digest, only_a_layer.len()),
        ],
    );
    let (manifest_b, m_b_len, cfg_b_len) =
        write_child("arm64", &[(&shared_digest, shared_layer.len())]);

    // The multi-arch index referencing both children.
    let index_bytes = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [
            { "mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": manifest_a,
              "size": m_a_len, "platform": { "architecture": "amd64", "os": "linux" } },
            { "mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": manifest_b,
              "size": m_b_len, "platform": { "architecture": "arm64", "os": "linux" } }
        ]
    }))
    .expect("index json");
    let index_digest = layout.write_blob(&index_bytes).expect("write index");
    layout
        .upsert_ref(
            "example.invalid/multiarch:1",
            ImageIndexEntry {
                media_type: "application/vnd.oci.image.index.v1+json".to_string(),
                digest: index_digest.clone(),
                size: index_bytes.len() as i64,
                platform: None,
                annotations: None,
            },
        )
        .expect("upsert ref");

    // The footprint is every unique blob, with the shared 6000-byte layer
    // counted exactly ONCE.
    let expected = (index_bytes.len()
        + m_a_len
        + m_b_len
        + cfg_a_len
        + cfg_b_len
        + shared_layer.len()
        + only_a_layer.len()) as u64;
    let got = layout.image_disk_size(&index_digest).expect("size walk");
    assert_eq!(got, expected, "sums every unique blob once");
    assert_ne!(
        got,
        expected + shared_layer.len() as u64,
        "the shared layer must be de-duped, not double-counted",
    );
    // Far larger than the index manifest's own size — the bug reported that.
    assert!(
        got > (index_bytes.len() as u64) * 4,
        "footprint ({got}) must dwarf the index manifest size ({})",
        index_bytes.len(),
    );

    // Single-image footprint: child A = its manifest + config + its two layers.
    let expected_a = (m_a_len + cfg_a_len + shared_layer.len() + only_a_layer.len()) as u64;
    assert_eq!(
        layout.image_disk_size(&manifest_a).expect("size a"),
        expected_a,
        "single-image footprint sums that image's own blobs",
    );
}
