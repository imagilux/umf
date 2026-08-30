//! Unit tests for the `compression` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use std::io::Write as _;

fn tar_bytes(name: &str, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut b = tar::Builder::new(&mut out);
        b.mode(tar::HeaderMode::Deterministic);
        let mut h = tar::Header::new_gnu();
        h.set_size(content.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, name, content).unwrap();
        b.finish().unwrap();
    }
    out
}

fn read_all(mut r: Box<dyn Read + '_>) -> Vec<u8> {
    let mut out = Vec::new();
    r.read_to_end(&mut out).expect("read decoded stream");
    out
}

#[test]
fn every_codec_decodes_to_the_same_tar_bytes() {
    // The point of the shared decoder: one input tar, five encodings, one
    // set of bytes back out. Before this module the layer path understood
    // only three of these and the staging path five.
    let plain = tar_bytes("etc/app.conf", b"key=value\n");

    let gz = {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(&plain).unwrap();
        e.finish().unwrap()
    };
    let zst = zstd::stream::encode_all(&plain[..], 3).unwrap();
    let xz = {
        let mut e = liblzma::write::XzEncoder::new(Vec::new(), 6);
        e.write_all(&plain).unwrap();
        e.finish().unwrap()
    };
    let bz = {
        let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        e.write_all(&plain).unwrap();
        e.finish().unwrap()
    };

    for (label, encoded, want_format) in [
        ("plain", plain.clone(), Format::Tar),
        ("gzip", gz, Format::Gzip),
        ("zstd", zst, Format::Zstd),
        ("xz", xz, Format::Xz),
        ("bzip2", bz, Format::Bzip2),
    ] {
        let (format, reader) =
            decode_tar_stream(&encoded[..]).unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(format, want_format, "{label} format");
        assert_eq!(read_all(reader), plain, "{label} decodes to the input tar");
    }
}

#[test]
fn a_zip_is_not_a_tar_stream() {
    // A zip's index lives at the end of the file, so it cannot be decoded as
    // a stream at all — the caller must route it through the seekable path.
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    w.start_file("a.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    w.write_all(b"hi").unwrap();
    let zip_bytes = w.finish().unwrap().into_inner();

    match decode_tar_stream(&zip_bytes[..]) {
        Err(DecodeError::NotATar("zip")) => {}
        Err(other) => panic!("expected NotATar(zip), got {other:?}"),
        Ok(_) => panic!("a zip must not decode as a tar stream"),
    }
}

#[test]
fn a_squashfs_image_is_not_a_tar_stream() {
    match decode_tar_stream(&b"hsqs\x00\x00\x00\x00"[..]) {
        Err(DecodeError::NotATar("squashfs")) => {}
        Err(other) => panic!("expected NotATar(squashfs), got {other:?}"),
        Ok(_) => panic!("a squashfs image must not decode as a tar stream"),
    }
}

#[test]
fn the_decompression_cap_applies_to_every_codec() {
    // A decompression bomb must abort the read rather than fill the disk, and
    // the guarantee must not depend on which branch was taken — including the
    // uncompressed one, where "expansion" is just a large payload.
    let big = tar_bytes("big", &vec![0u8; 64 * 1024]);
    let gz = {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        e.write_all(&big).unwrap();
        e.finish().unwrap()
    };

    for (label, bytes) in [("plain", big.clone()), ("gzip", gz)] {
        let (_, reader) = decode_tar_stream_capped(&bytes[..], 1024).expect("decoder starts");
        let mut sink = Vec::new();
        let err = {
            let mut r = reader;
            r.read_to_end(&mut sink)
        }
        .expect_err("reading past the cap must fail");
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidData,
            "{label}: cap trips as InvalidData",
        );
    }
}

#[test]
fn an_empty_stream_decodes_as_an_empty_plain_tar() {
    // Degenerate but reachable (a zero-length blob); must not panic on the
    // short peek.
    let (format, reader) = decode_tar_stream(&b""[..]).expect("empty input is not an error");
    assert_eq!(format, Format::Unknown);
    assert!(read_all(reader).is_empty());
}
