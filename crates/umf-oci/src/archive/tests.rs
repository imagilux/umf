//! Unit tests for the `archive` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::image::{ContainerConfig, ImageConfig, emit_image};
use tempfile::TempDir;
use umf_core::l0::L0Kind;

fn stage_image(dir: &std::path::Path, ref_name: &str) -> ImageLayout {
    let layout = ImageLayout::init(dir).expect("init");
    let cfg = ImageConfig {
        container: ContainerConfig::default(),
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &cfg, ref_name).expect("emit");
    layout
}

#[test]
fn save_then_load_preserves_manifest_digest() {
    let src_dir = TempDir::new().expect("src");
    let src = stage_image(src_dir.path(), "example.invalid/round-trip:1");
    let original = src
        .lookup_ref("example.invalid/round-trip:1")
        .expect("lookup")
        .expect("ref present");

    let mut buf = Vec::new();
    save_to_writer(
        &src,
        &["example.invalid/round-trip:1".to_string()],
        &mut buf,
    )
    .expect("save");

    // Sanity: the buffer is a valid tar with the expected entries.
    let mut found_layout = false;
    let mut found_index = false;
    let mut blob_count = 0;
    {
        let mut archive = tar::Archive::new(buf.as_slice());
        for entry in archive.entries().expect("entries") {
            let entry = entry.expect("entry");
            let path = entry.path().expect("path").to_string_lossy().into_owned();
            if path == OCI_LAYOUT_FILE {
                found_layout = true;
            } else if path == INDEX_JSON_FILE {
                found_index = true;
            } else if path.starts_with(BLOBS_PREFIX) {
                blob_count += 1;
            }
        }
    }
    assert!(found_layout);
    assert!(found_index);
    assert!(blob_count >= 1);

    // Now load into a fresh layout and verify byte-for-byte
    // manifest digest survival.
    let dst_dir = TempDir::new().expect("dst");
    let dst = ImageLayout::init(dst_dir.path()).expect("init dst");
    let loaded = load_from_reader(&dst, buf.as_slice(), false).expect("load");
    assert_eq!(loaded, vec!["example.invalid/round-trip:1".to_string()]);
    let after = dst
        .lookup_ref("example.invalid/round-trip:1")
        .expect("lookup")
        .expect("ref present after load");
    assert_eq!(original.digest, after.digest);
}

#[test]
fn save_streams_blobs_byte_identically() {
    // The streaming save path (`write_file_entry`, feeding the tar body
    // straight from each blob's on-disk file) must produce blobs that are
    // byte-for-byte identical to the source layout — both as packed into
    // the archive and as re-materialised on the destination side. This is
    // the reproducibility / digest-integrity guard.
    let src_dir = TempDir::new().expect("src");
    let src = stage_image(src_dir.path(), "example.invalid/stream:1");

    // Enumerate every blob reachable from the saved ref in the source.
    let entry = src
        .lookup_ref("example.invalid/stream:1")
        .expect("lookup")
        .expect("ref present");
    let mut reachable: HashSet<String> = HashSet::new();
    src.collect_reachable(&entry.digest, &mut reachable)
        .expect("collect");
    assert!(!reachable.is_empty(), "image must have ≥1 blob");

    let mut buf = Vec::new();
    save_to_writer(&src, &["example.invalid/stream:1".to_string()], &mut buf).expect("save");

    // 1. Every blob the tar carries matches the source blob byte-for-byte,
    //    and its tar header size equals the real on-disk length.
    {
        let mut archive = tar::Archive::new(buf.as_slice());
        let mut seen = 0;
        for entry in archive.entries().expect("entries") {
            let mut entry = entry.expect("entry");
            let path = entry.path().expect("path").to_string_lossy().into_owned();
            let Some(hex) = path.strip_prefix(BLOBS_PREFIX) else {
                continue;
            };
            let digest = format!("sha256:{hex}");
            let declared = entry.header().size().expect("hdr size");
            let mut packed = Vec::new();
            entry.read_to_end(&mut packed).expect("read entry");
            assert_eq!(
                declared as usize,
                packed.len(),
                "tar header size must equal streamed body length for {digest}"
            );
            let on_disk = src.read_blob(&digest).expect("src blob");
            assert_eq!(packed, on_disk, "packed blob bytes diverged for {digest}");
            seen += 1;
        }
        assert_eq!(seen, reachable.len(), "every reachable blob must be packed");
    }

    // 2. Load into a fresh layout and assert each blob survives byte-for-
    //    byte (and its digest still verifies via the re-hashing reader).
    let dst_dir = TempDir::new().expect("dst");
    let dst = ImageLayout::init(dst_dir.path()).expect("init dst");
    load_from_reader(&dst, buf.as_slice(), false).expect("load");
    for digest in &reachable {
        let before = src.read_blob(digest).expect("src blob");
        let after = dst.read_blob(digest).expect("dst blob");
        assert_eq!(
            before, after,
            "blob bytes diverged across round-trip for {digest}"
        );
    }
}

#[test]
fn save_unknown_ref_errors() {
    let dir = TempDir::new().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let mut buf = Vec::new();
    let err = save_to_writer(&layout, &["nope".to_string()], &mut buf).expect_err("must error");
    assert!(matches!(err, ArchiveError::RefNotFound(name) if name == "nope"));
}

#[test]
fn load_rejects_non_oci_archive() {
    // A tar that doesn't contain `oci-layout` is not an OCI
    // archive even if it's otherwise well-formed.
    let mut buf = Vec::new();
    {
        let mut tar = tar::Builder::new(&mut buf);
        let mut header = tar::Header::new_gnu();
        header.set_path("random-file.txt").expect("path");
        header.set_size(4);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, b"hi!\n".as_slice()).expect("append");
        tar.finish().expect("finish");
    }
    let dir = TempDir::new().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init");
    let err = load_from_reader(&layout, buf.as_slice(), false).expect_err("must reject");
    assert!(matches!(err, ArchiveError::NotAnOciArchive));
}

#[test]
fn read_capped_rejects_oversized_entry() {
    // An entry larger than the cap is rejected without buffering past
    // the ceiling: feed `cap + 100` bytes against a tiny cap and assert
    // the overflow is caught. (`std::io::repeat` is an unbounded source,
    // but `read_capped` bounds the read to `cap + 1`, so this never
    // allocates more than a few bytes — the same property that protects
    // the real 8 GiB ceiling from an unbounded archive stream.)
    let cap = 16_u64;
    let mut src = std::io::repeat(0xAB).take(cap + 100);
    let err = read_capped(&mut src, "blobs/sha256/deadbeef", cap).expect_err("must reject");
    assert!(matches!(err, ArchiveError::Tar(msg) if msg.contains("ceiling")));
}

#[test]
fn read_capped_accepts_entry_at_ceiling() {
    // Exactly `cap` bytes is fine; the +1 in the implementation is the
    // overflow probe, not part of the allowance.
    let cap = 16_u64;
    let mut src = std::io::repeat(0xAB).take(cap);
    let buf = read_capped(&mut src, "blobs/sha256/deadbeef", cap).expect("at ceiling is ok");
    assert_eq!(buf.len() as u64, cap);
}

#[test]
fn load_ref_collision_without_overwrite_errors() {
    let src_dir = TempDir::new().expect("src");
    let src = stage_image(src_dir.path(), "example.invalid/dup:1");
    let mut buf = Vec::new();
    save_to_writer(&src, &["example.invalid/dup:1".to_string()], &mut buf).expect("save");

    let dst_dir = TempDir::new().expect("dst");
    let dst = stage_image(dst_dir.path(), "example.invalid/dup:1"); // pre-populated
    let err = load_from_reader(&dst, buf.as_slice(), false).expect_err("collision");
    assert!(matches!(err, ArchiveError::RefCollision(name) if name == "example.invalid/dup:1"));
}

#[test]
fn load_ref_collision_with_overwrite_replaces() {
    let src_dir = TempDir::new().expect("src");
    let src = stage_image(src_dir.path(), "example.invalid/dup:2");
    let src_digest = src
        .lookup_ref("example.invalid/dup:2")
        .expect("lookup")
        .expect("ref present")
        .digest;

    let mut buf = Vec::new();
    save_to_writer(&src, &["example.invalid/dup:2".to_string()], &mut buf).expect("save");

    let dst_dir = TempDir::new().expect("dst");
    let _ = stage_image(dst_dir.path(), "example.invalid/dup:2");
    let dst = ImageLayout::init(dst_dir.path()).expect("reopen");
    load_from_reader(&dst, buf.as_slice(), true).expect("overwrite load");
    let after = dst
        .lookup_ref("example.invalid/dup:2")
        .expect("lookup")
        .expect("ref present");
    assert_eq!(after.digest, src_digest);
}
