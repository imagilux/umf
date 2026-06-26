//! Unit tests for the `erofs` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::registry::layout::sha256_digest;
use flate2::Compression;
use flate2::write::GzEncoder;

/// Build a gzipped tar carrying one file, returning `(gzip_bytes,
/// blob_digest, diff_id)`. diff_id is the digest of the *uncompressed*
/// tar, matching OCI's `rootfs.diff_ids`.
fn synth_layer(path: &str, contents: &[u8]) -> (Vec<u8>, String, String) {
    use std::io::Write as _;
    let mut tar_bytes = Vec::new();
    {
        let mut b = tar::Builder::new(&mut tar_bytes);
        let mut h = tar::Header::new_gnu();
        h.set_path(path).expect("set path");
        h.set_size(contents.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append(&h, contents).expect("append");
        b.finish().expect("finish");
    }
    let diff_id = sha256_digest(&tar_bytes);
    let mut gz = Vec::new();
    let mut enc = GzEncoder::new(&mut gz, Compression::default());
    enc.write_all(&tar_bytes).expect("gz write");
    enc.finish().expect("gz finish");
    let blob_digest = sha256_digest(&gz);
    (gz, blob_digest, diff_id)
}

#[test]
fn cache_path_is_content_addressed_on_diff_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let p = layout
        .erofs_cache_path("sha256:abc123")
        .expect("erofs path");
    assert!(
        p.ends_with("cache/erofs/abc123.erofs"),
        "got {}",
        p.display()
    );
}

#[test]
fn encode_is_idempotent_and_reuses_cache() {
    if !encoder_available() {
        eprintln!("skipping encode_is_idempotent: mkfs.erofs not available");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let (gz, blob_digest, diff_id) = synth_layer("etc/motd", b"hi\n");
    layout
        .write_blob_with_digest(&gz, &blob_digest)
        .expect("write blob");

    let first = ensure_layer_erofs(&layout, &blob_digest, &diff_id).expect("encode");
    assert!(
        first.metadata().expect("stat").len() > 0,
        "erofs should be non-empty"
    );
    let mtime1 = std::fs::metadata(&first)
        .expect("stat")
        .modified()
        .expect("mtime");

    // Second call must be a no-op cache hit (same path, file untouched).
    let second = ensure_layer_erofs(&layout, &blob_digest, &diff_id).expect("encode 2");
    assert_eq!(first, second);
    let mtime2 = std::fs::metadata(&second)
        .expect("stat")
        .modified()
        .expect("mtime");
    assert_eq!(mtime1, mtime2, "cache hit should not rewrite the erofs");
}
