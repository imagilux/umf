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
