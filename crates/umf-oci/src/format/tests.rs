//! Unit tests for the `format` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use std::io::Write;

/// Build a tiny uncompressed tar carrying the named entries (path, bytes).
fn tar_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut out);
        builder.mode(tar::HeaderMode::Deterministic);
        for (path, payload) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, &payload[..])
                .expect("append");
        }
        builder.finish().expect("finish");
    }
    out
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(bytes).expect("gz write");
    enc.finish().expect("gz finish")
}

#[test]
fn detect_compression_magics() {
    assert_eq!(detect(&[0x1F, 0x8B, 0x08, 0x00]), Format::Gzip);
    assert_eq!(detect(&[0x28, 0xB5, 0x2F, 0xFD, 0x00]), Format::Zstd);
    assert_eq!(detect(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]), Format::Xz);
    assert_eq!(detect(b"BZh91AY"), Format::Bzip2);
}

#[test]
fn detect_squashfs_both_endians() {
    assert_eq!(detect(b"hsqs____"), Format::Squashfs);
    assert_eq!(detect(b"sqsh____"), Format::Squashfs);
}

#[test]
fn detect_real_tar_and_gzipped_tar() {
    let tar = tar_with(&[("etc/os-release", b"NAME=x")]);
    assert_eq!(detect(&tar), Format::Tar);
    // Gzipped tar reports Gzip (we don't decompress in detect).
    assert_eq!(detect(&gzip(&tar)), Format::Gzip);
}

#[test]
fn detect_unknown_for_raw_bytes() {
    assert_eq!(
        detect(b"just some plain text, not an archive"),
        Format::Unknown
    );
    assert_eq!(detect(&[]), Format::Unknown);
}

#[test]
fn is_compressed_only_for_wrappers() {
    for f in [Format::Gzip, Format::Zstd, Format::Xz, Format::Bzip2] {
        assert!(f.is_compressed(), "{f:?}");
    }
    for f in [Format::Tar, Format::Squashfs, Format::Unknown] {
        assert!(!f.is_compressed(), "{f:?}");
    }
}

#[test]
fn oci_layout_detected_in_plain_and_gzipped_tar() {
    let oci = tar_with(&[
        ("oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#),
        ("index.json", b"{}"),
    ]);
    assert!(is_oci_layout(&oci), "plain OCI-layout tar");
    assert!(is_oci_layout(&gzip(&oci)), "gzipped OCI-layout tar");
}

#[test]
fn plain_rootfs_tar_is_not_oci_layout() {
    let rootfs = tar_with(&[("etc/os-release", b"NAME=x"), ("bin/sh", b"#!")]);
    assert!(!is_oci_layout(&rootfs));
    assert!(!is_oci_layout(&gzip(&rootfs)));
    // A bare blob isn't a tar at all.
    assert!(!is_oci_layout(b"not a tar"));
}
