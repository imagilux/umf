//! Unit tests for the `staging` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::Write as _;

fn build_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        builder.mode(tar::HeaderMode::Deterministic);
        for (path, content) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *content).unwrap();
        }
        builder.finish().unwrap();
    }
    tar_bytes
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = GzEncoder::new(&mut out, Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap();
    out
}

#[test]
fn unpack_plain_tar() {
    let tar_bytes = build_tar(&[
        ("etc/os-release", b"NAME=\"Alpine Linux\"\n"),
        ("bin/sh", b"#!shebang\n"),
    ]);
    let mut staging = BuildStaging::new().expect("new");
    staging.unpack_tar_bytes(&tar_bytes).expect("unpack");

    let os_release = staging.path().join("etc/os-release");
    let content = std::fs::read(os_release).expect("read");
    assert!(content.starts_with(b"NAME=\"Alpine"));
    assert!(staging.path().join("bin/sh").is_file());
}

#[test]
fn unpack_gzipped_tar() {
    let tar_bytes = build_tar(&[("hello", b"world")]);
    let gz_bytes = gzip(&tar_bytes);

    let mut staging = BuildStaging::new().expect("new");
    staging.unpack_tar_bytes(&gz_bytes).expect("unpack gzip");

    let content = std::fs::read(staging.path().join("hello")).expect("read");
    assert_eq!(content, b"world");
}

#[test]
fn unpack_zstd_compressed_tar() {
    let tar_bytes = build_tar(&[("hello", b"zstd world")]);
    let zst_bytes = zstd::stream::encode_all(&tar_bytes[..], 3).expect("zstd encode");

    let mut staging = BuildStaging::new().expect("new");
    staging.unpack_tar_bytes(&zst_bytes).expect("unpack zstd");

    let content = std::fs::read(staging.path().join("hello")).expect("read");
    assert_eq!(content, b"zstd world");
}

#[test]
fn unpack_xz_compressed_tar() {
    let tar_bytes = build_tar(&[("hello", b"xz world")]);
    let mut enc = liblzma::write::XzEncoder::new(Vec::new(), 6);
    enc.write_all(&tar_bytes).expect("xz write");
    let xz_bytes = enc.finish().expect("xz finish");

    let mut staging = BuildStaging::new().expect("new");
    staging.unpack_tar_bytes(&xz_bytes).expect("unpack xz");

    let content = std::fs::read(staging.path().join("hello")).expect("read");
    assert_eq!(content, b"xz world");
}

#[test]
fn unpack_bzip2_compressed_tar() {
    let tar_bytes = build_tar(&[("hello", b"bzip2 world")]);
    let mut enc = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    enc.write_all(&tar_bytes).expect("bz write");
    let bz_bytes = enc.finish().expect("bz finish");

    let mut staging = BuildStaging::new().expect("new");
    staging.unpack_tar_bytes(&bz_bytes).expect("unpack bzip2");

    let content = std::fs::read(staging.path().join("hello")).expect("read");
    assert_eq!(content, b"bzip2 world");
}

#[test]
fn a_compressed_stream_that_wraps_no_tar_fails_clearly() {
    // The zstd magic routes the stream to the decoder; the decoded bytes are
    // not a tar, so the unpack must fail rather than land garbage.
    let zst_bytes = zstd::stream::encode_all(&b"just a text file"[..], 3).expect("zstd encode");
    let mut staging = BuildStaging::new().expect("new");
    let err = staging
        .unpack_tar_bytes(&zst_bytes)
        .expect_err("a compressed non-tar must fail");
    assert!(
        matches!(err, StagingError::Unpack(_)),
        "expected Unpack, got {err:?}"
    );
}

#[test]
fn a_zip_fed_to_the_tar_unpacker_is_refused() {
    let zip_bytes = build_zip(|w, opts| {
        w.start_file("a.txt", opts).unwrap();
        w.write_all(b"hi").unwrap();
    });
    let mut staging = BuildStaging::new().expect("new");
    let err = staging
        .unpack_tar_bytes(&zip_bytes)
        .expect_err("zip is not a tar stream");
    assert!(
        matches!(err, StagingError::NotATar("zip")),
        "expected NotATar(zip), got {err:?}"
    );
}

/// Build an in-memory zip via a caller-supplied recording closure.
fn build_zip(
    record: impl FnOnce(&mut zip::ZipWriter<std::io::Cursor<Vec<u8>>>, zip::write::SimpleFileOptions),
) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    record(&mut writer, zip::write::SimpleFileOptions::default());
    writer.finish().expect("zip finish").into_inner()
}

fn write_zip_file(dir: &Path, bytes: &[u8]) -> PathBuf {
    let path = dir.join("payload.zip");
    std::fs::write(&path, bytes).expect("seed zip");
    path
}

#[test]
fn unpack_zip_extracts_dirs_files_permissions_and_symlinks() {
    use std::os::unix::fs::PermissionsExt as _;

    let zip_bytes = build_zip(|w, opts| {
        w.add_directory("etc", opts).unwrap();
        w.start_file("etc/app.conf", opts).unwrap();
        w.write_all(b"key=value\n").unwrap();
        w.start_file("bin/tool", opts.unix_permissions(0o755))
            .unwrap();
        w.write_all(b"#!/bin/sh\n").unwrap();
        w.add_symlink("bin/tool-link", "tool", opts).unwrap();
    });
    let scratch = tempfile::tempdir().expect("scratch");
    let zip_path = write_zip_file(scratch.path(), &zip_bytes);

    let mut staging = BuildStaging::new().expect("new");
    staging.unpack_zip(&zip_path).expect("unpack zip");

    assert_eq!(
        std::fs::read(staging.path().join("etc/app.conf")).expect("read"),
        b"key=value\n"
    );
    let tool = staging.path().join("bin/tool");
    let mode = std::fs::metadata(&tool).expect("meta").permissions().mode();
    assert_eq!(mode & 0o777, 0o755, "unix permissions preserved");
    let link = staging.path().join("bin/tool-link");
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("lstat")
            .file_type()
            .is_symlink(),
        "symlink entry lands as a symlink"
    );
    assert_eq!(
        std::fs::read_link(&link).expect("read_link"),
        PathBuf::from("tool")
    );
    // The relative link resolves inside the tree.
    assert_eq!(std::fs::read(&link).expect("follow"), b"#!/bin/sh\n");
}

#[test]
fn unpack_zip_refuses_an_escaping_entry_name() {
    let zip_bytes = build_zip(|w, opts| {
        w.start_file("../evil.txt", opts).unwrap();
        w.write_all(b"escape").unwrap();
    });
    let scratch = tempfile::tempdir().expect("scratch");
    let zip_path = write_zip_file(scratch.path(), &zip_bytes);

    let mut staging = BuildStaging::new().expect("new");
    let err = staging
        .unpack_zip(&zip_path)
        .expect_err("a `..` entry name must be refused");
    assert!(
        matches!(err, StagingError::ZipEntryEscape { .. }),
        "expected ZipEntryEscape, got {err:?}"
    );
    assert!(
        !scratch.path().parent().unwrap().join("evil.txt").exists(),
        "nothing may land outside the staging root"
    );
}

#[test]
fn unpack_zip_refuses_a_file_write_through_a_planted_symlink() {
    // A symlink pointing outside the tree, then a file whose path descends
    // through it. Files are extracted before any symlink is created, so the
    // write lands in a real `escape/` directory inside the root — and the
    // symlink entry itself then collides with that directory. Either way,
    // nothing may appear outside the staging root.
    let scratch = tempfile::tempdir().expect("scratch");
    let outside = scratch.path().join("outside");
    std::fs::create_dir_all(&outside).expect("mk outside");

    let zip_bytes = build_zip(|w, opts| {
        w.add_symlink("escape", outside.to_str().unwrap(), opts)
            .unwrap();
        w.start_file("escape/owned.txt", opts).unwrap();
        w.write_all(b"gotcha").unwrap();
    });
    let zip_path = write_zip_file(scratch.path(), &zip_bytes);

    let mut staging = BuildStaging::new().expect("new");
    // The unpack may succeed or fail (the deferred symlink collides with the
    // directory the file pass created) — the invariant is containment.
    let _ = staging.unpack_zip(&zip_path);
    assert!(
        !outside.join("owned.txt").exists(),
        "no write may traverse an archive-controlled symlink"
    );
}

#[test]
fn unpack_from_file_path() {
    let tar_bytes = build_tar(&[("README", b"hi")]);
    let scratch = tempfile::tempdir().expect("scratch");
    let path = scratch.path().join("test.tar");
    std::fs::write(&path, &tar_bytes).expect("seed");

    let mut staging = BuildStaging::new().expect("new");
    staging.unpack_tarball(&path).expect("unpack");
    assert_eq!(
        std::fs::read(staging.path().join("README")).expect("read"),
        b"hi"
    );
}

#[test]
fn into_path_persists_tree_until_caller_removes_it() {
    let tar_bytes = build_tar(&[("a", b"1")]);
    let mut staging = BuildStaging::new().expect("new");
    staging.unpack_tar_bytes(&tar_bytes).expect("unpack");
    let path = staging.into_path();
    assert!(path.join("a").is_file());
    std::fs::remove_dir_all(&path).expect("manual cleanup");
}
