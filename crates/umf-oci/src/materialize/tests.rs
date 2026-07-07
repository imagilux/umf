//! Unit tests for the `materialize` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::Write as _;

/// Build a tar from `(path, contents)` entries; a `None` content marks a
/// directory entry.
fn tar(entries: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut b = tar::Builder::new(&mut out);
        b.mode(tar::HeaderMode::Deterministic);
        for (path, content) in entries {
            let mut h = tar::Header::new_gnu();
            match content {
                Some(data) => {
                    h.set_entry_type(tar::EntryType::Regular);
                    h.set_size(data.len() as u64);
                    h.set_mode(0o644);
                    h.set_cksum();
                    b.append_data(&mut h, path, *data).unwrap();
                }
                None => {
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_size(0);
                    h.set_mode(0o755);
                    h.set_cksum();
                    b.append_data(&mut h, path, std::io::empty()).unwrap();
                }
            }
        }
        b.finish().unwrap();
    }
    out
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut e = GzEncoder::new(&mut out, Compression::fast());
    e.write_all(bytes).unwrap();
    e.finish().unwrap();
    out
}

fn zstd(bytes: &[u8]) -> Vec<u8> {
    zstd::stream::encode_all(bytes, 3).unwrap()
}

#[test]
fn applies_plain_layer() {
    let dir = tempfile::tempdir().unwrap();
    apply_layer(
        tar(&[("etc/hostname", Some(b"umf"))]).as_slice(),
        dir.path(),
    )
    .expect("apply");
    assert_eq!(fs::read(dir.path().join("etc/hostname")).unwrap(), b"umf");
}

#[test]
fn applies_gzipped_layer() {
    let dir = tempfile::tempdir().unwrap();
    let layer = gzip(&tar(&[("hello", Some(b"world"))]));
    apply_layer(layer.as_slice(), dir.path()).expect("apply gzip");
    assert_eq!(fs::read(dir.path().join("hello")).unwrap(), b"world");
}

#[test]
fn applies_zstd_layer() {
    let dir = tempfile::tempdir().unwrap();
    let layer = zstd(&tar(&[("hello", Some(b"world"))]));
    apply_layer(layer.as_slice(), dir.path()).expect("apply zstd");
    assert_eq!(fs::read(dir.path().join("hello")).unwrap(), b"world");
}

#[test]
fn zstd_whiteout_removes_lower_layer_file() {
    // Whiteouts must be honoured the same way through the zstd decode path.
    let dir = tempfile::tempdir().unwrap();
    apply_layer(
        zstd(&tar(&[("a", Some(b"1")), ("b", Some(b"2"))])).as_slice(),
        dir.path(),
    )
    .unwrap();
    apply_layer(zstd(&tar(&[(".wh.a", Some(b""))])).as_slice(), dir.path()).unwrap();
    assert!(!dir.path().join("a").exists(), "a should be whited out");
    assert!(dir.path().join("b").is_file(), "b should remain");
}

/// End-to-end: emit a zstd layer via the producer path
/// ([`crate::image::LayerSource::from_directory_with`]) and materialize it
/// back, asserting the rootfs is byte-identical to the source tree. This is
/// the OCI-2 acceptance round-trip (emit `+zstd` → pull/materialize).
#[test]
fn emit_zstd_layer_materializes_to_identical_rootfs() {
    use crate::image::{LayerCompression, LayerSource};

    let src = tempfile::tempdir().unwrap();
    fs::create_dir_all(src.path().join("bin")).unwrap();
    fs::write(src.path().join("bin/hello"), b"echo hi\n").unwrap();
    fs::write(src.path().join("README"), b"hello, umf\n").unwrap();

    let layer =
        LayerSource::from_directory_with(src.path(), LayerCompression::Zstd).expect("emit zstd");
    // Sanity: the emitted blob really is zstd, not gzip.
    assert_eq!(
        crate::format::detect(&layer.data),
        crate::format::Format::Zstd
    );

    let out = tempfile::tempdir().unwrap();
    apply_layer(layer.data.as_ref(), out.path()).expect("materialize zstd");

    assert_eq!(
        fs::read(out.path().join("bin/hello")).unwrap(),
        b"echo hi\n"
    );
    assert_eq!(
        fs::read(out.path().join("README")).unwrap(),
        b"hello, umf\n"
    );
}

#[test]
fn whiteout_removes_lower_layer_file() {
    let dir = tempfile::tempdir().unwrap();
    apply_layer(
        tar(&[("a", Some(b"1")), ("b", Some(b"2"))]).as_slice(),
        dir.path(),
    )
    .unwrap();
    // Upper layer whites out `a`.
    apply_layer(tar(&[(".wh.a", Some(b""))]).as_slice(), dir.path()).unwrap();
    assert!(!dir.path().join("a").exists(), "a should be whited out");
    assert!(dir.path().join("b").is_file(), "b should remain");
}

#[test]
fn opaque_marker_clears_directory() {
    let dir = tempfile::tempdir().unwrap();
    apply_layer(
        tar(&[("d/", None), ("d/x", Some(b"x")), ("d/y", Some(b"y"))]).as_slice(),
        dir.path(),
    )
    .unwrap();
    // Opaque `d` + a fresh entry: only the new entry survives.
    apply_layer(
        tar(&[("d/.wh..wh..opq", Some(b"")), ("d/z", Some(b"z"))]).as_slice(),
        dir.path(),
    )
    .unwrap();
    assert!(!dir.path().join("d/x").exists(), "x cleared by opaque");
    assert!(!dir.path().join("d/y").exists(), "y cleared by opaque");
    assert_eq!(fs::read(dir.path().join("d/z")).unwrap(), b"z");
}

#[test]
fn opaque_marker_preserves_same_layer_siblings_written_before_it() {
    // Regression: the opaque clear must not depend on intra-layer
    // tar ordering. Here `d/keep` is emitted BEFORE `d/.wh..wh..opq` in the
    // SAME layer — opaque hides only lower-layer content, so `d/keep` (a
    // same-layer sibling) must survive while the lower-layer `d/old` is gone.
    let dir = tempfile::tempdir().unwrap();
    apply_layer(
        tar(&[("d/", None), ("d/old", Some(b"old"))]).as_slice(),
        dir.path(),
    )
    .unwrap();
    // Marker placed AFTER the sibling it must not delete.
    apply_layer(
        tar(&[
            ("d/", None),
            ("d/keep", Some(b"keep")),
            ("d/.wh..wh..opq", Some(b"")),
        ])
        .as_slice(),
        dir.path(),
    )
    .unwrap();
    assert!(
        !dir.path().join("d/old").exists(),
        "lower-layer d/old must be cleared by the opaque marker"
    );
    assert_eq!(
        fs::read(dir.path().join("d/keep")).unwrap(),
        b"keep",
        "same-layer d/keep (written before the marker) must survive"
    );
}

#[test]
fn safe_descend_rejects_traversal_and_symlinks() {
    let base = tempfile::tempdir().unwrap();
    // `..` / absolute components are refused outright.
    assert!(safe_descend(base.path(), Path::new("../escape"), false).is_err());
    assert!(safe_descend(base.path(), Path::new("a/../../escape"), false).is_err());
    assert!(safe_descend(base.path(), Path::new("/etc/passwd"), false).is_err());

    // A clean, existing relative path resolves.
    fs::create_dir_all(base.path().join("a")).unwrap();
    fs::write(base.path().join("a/b"), b"x").unwrap();
    assert_eq!(
        safe_descend(base.path(), Path::new("a/b"), true).unwrap(),
        Some(base.path().join("a").join("b")),
    );

    // An absent path → None (nothing to delete).
    assert_eq!(
        safe_descend(base.path(), Path::new("a/missing"), true).unwrap(),
        None
    );

    // An intermediate symlink → None (refuse to traverse it).
    std::os::unix::fs::symlink("/tmp", base.path().join("link")).unwrap();
    assert_eq!(
        safe_descend(base.path(), Path::new("link/x"), true).unwrap(),
        None
    );
    // A final symlink is allowed only when the caller will unlink it directly.
    assert_eq!(
        safe_descend(base.path(), Path::new("link"), true).unwrap(),
        Some(base.path().join("link")),
    );
    assert_eq!(
        safe_descend(base.path(), Path::new("link"), false).unwrap(),
        None
    );
}

/// Build a malicious layer: a symlink `evil -> <link_target>` followed by an
/// opaque marker under `evil/` (so `clear_dir` follows the symlink) — plus
/// a `.wh.<victim>` under `evil/` (so `remove_path` follows it).
fn symlink_whiteout_layer(link_target: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut b = tar::Builder::new(&mut out);
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        b.append_link(&mut h, "evil", link_target).unwrap();

        for marker in [".wh..wh..opq", ".wh.victim"] {
            let mut m = tar::Header::new_gnu();
            m.set_entry_type(tar::EntryType::Regular);
            m.set_size(0);
            m.set_mode(0o644);
            m.set_cksum();
            b.append_data(&mut m, format!("evil/{marker}"), std::io::empty())
                .unwrap();
        }
        b.finish().unwrap();
    }
    out
}

/// SECURITY: a whiteout whose path traverses a layer-planted symlink must
/// NOT delete files outside the target rootfs. A `.wh.` / opaque marker
/// under a symlinked directory would otherwise make `clear_dir`/`remove_path`
/// follow the symlink and delete host files.
#[test]
fn whiteout_through_symlink_does_not_escape_target() {
    let sentinel = tempfile::tempdir().unwrap();
    fs::write(sentinel.path().join("victim"), b"host data").unwrap();
    fs::write(sentinel.path().join("bystander"), b"host data").unwrap();

    let target = tempfile::tempdir().unwrap();
    let layer = symlink_whiteout_layer(sentinel.path());
    // Applying must not delete anything under the (out-of-tree) sentinel.
    let _ = apply_layer(layer.as_slice(), target.path());

    assert!(
        sentinel.path().join("victim").exists(),
        "whiteout escaped: host file under a symlinked dir was deleted"
    );
    assert!(
        sentinel.path().join("bystander").exists(),
        "opaque marker escaped: host dir contents were cleared"
    );
}

#[test]
fn capped_reader_allows_up_to_cap_then_errors() {
    use std::io::Read as _;
    // A reader of 100 bytes behind a 64-byte cap: reads succeed until the cap,
    // then error (a decompression bomb is aborted mid-stream).
    let data = [0u8; 100];
    let mut r = CappedReader::new(&data[..], 64);
    let mut sink = Vec::new();
    let err = r.read_to_end(&mut sink).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("decompression bomb"));
}

#[test]
fn capped_reader_passes_a_stream_at_exactly_the_cap() {
    use std::io::Read as _;
    let data = [7u8; 64];
    let mut r = CappedReader::new(&data[..], 64);
    let mut sink = Vec::new();
    // Exactly `cap` bytes read cleanly to EOF, no false bomb error.
    assert_eq!(r.read_to_end(&mut sink).unwrap(), 64);
    assert_eq!(sink, &data[..]);
}
