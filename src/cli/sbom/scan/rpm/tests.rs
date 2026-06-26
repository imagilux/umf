#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use tempfile::TempDir;

/// Build a minimal rpm header blob from `(tag, type, data-bytes)` triples.
/// String data must include its trailing NUL; INT32 data is 4 big-endian
/// bytes. Each entry is recorded with count 1 (enough for the scalar tags we
/// read).
fn build_header(entries: &[(u32, u32, &[u8])]) -> Vec<u8> {
    let mut index = Vec::new();
    let mut data = Vec::new();
    for &(tag, ty, bytes) in entries {
        let offset = data.len() as u32;
        index.extend_from_slice(&tag.to_be_bytes());
        index.extend_from_slice(&ty.to_be_bytes());
        index.extend_from_slice(&offset.to_be_bytes());
        index.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(bytes);
    }
    let mut blob = Vec::new();
    blob.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    blob.extend_from_slice(&(data.len() as u32).to_be_bytes());
    blob.extend_from_slice(&index);
    blob.extend_from_slice(&data);
    blob
}

fn bash_header() -> Vec<u8> {
    build_header(&[
        (TAG_NAME, TYPE_STRING, b"bash\0"),
        (TAG_VERSION, TYPE_STRING, b"5.1.16\0"),
        (TAG_RELEASE, TYPE_STRING, b"3.fc38\0"),
        (TAG_ARCH, TYPE_STRING, b"x86_64\0"),
        (TAG_EPOCH, TYPE_INT32, &1u32.to_be_bytes()),
    ])
}

#[test]
fn parse_header_extracts_nevra_and_purl() {
    let pkg = parse_header(&bash_header(), "fedora").expect("valid header parses");
    assert_eq!(pkg.name, "bash");
    assert_eq!(pkg.version, "1:5.1.16-3.fc38", "epoch is prefixed");
    assert_eq!(pkg.arch.as_deref(), Some("x86_64"));
    assert_eq!(
        pkg.purl.as_deref(),
        Some("pkg:rpm/fedora/bash@5.1.16-3.fc38?arch=x86_64&epoch=1"),
    );
}

#[test]
fn parse_header_skips_the_optional_magic_prefix() {
    let mut with_magic = vec![0x8e, 0xad, 0xe8, 0x01, 0, 0, 0, 0];
    with_magic.extend_from_slice(&bash_header());
    let pkg = parse_header(&with_magic, "fedora").expect("magic-prefixed header parses");
    assert_eq!(pkg.name, "bash");
}

#[test]
fn parse_header_without_epoch_omits_the_qualifier() {
    let blob = build_header(&[
        (TAG_NAME, TYPE_STRING, b"zlib\0"),
        (TAG_VERSION, TYPE_STRING, b"1.2.13\0"),
        (TAG_RELEASE, TYPE_STRING, b"1.fc38\0"),
        (TAG_ARCH, TYPE_STRING, b"x86_64\0"),
    ]);
    let pkg = parse_header(&blob, "fedora").unwrap();
    assert_eq!(pkg.version, "1.2.13-1.fc38");
    assert_eq!(
        pkg.purl.as_deref(),
        Some("pkg:rpm/fedora/zlib@1.2.13-1.fc38?arch=x86_64"),
    );
}

#[test]
fn parse_header_rejects_truncated_and_nameless_blobs() {
    assert!(parse_header(b"\x00\x00", "fedora").is_none(), "truncated");
    // A well-formed header with no NAME tag is not a package.
    let no_name = build_header(&[(TAG_VERSION, TYPE_STRING, b"1.0\0")]);
    assert!(parse_header(&no_name, "fedora").is_none());
}

#[test]
fn parse_rpmdb_reads_package_blobs() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("rpmdb.sqlite");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "CREATE TABLE Packages (hnum INTEGER PRIMARY KEY, blob BLOB)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO Packages (hnum, blob) VALUES (1, ?1)",
            rusqlite::params![bash_header()],
        )
        .unwrap();
    }
    let pkgs = parse_rpmdb(&db, "fedora").unwrap();
    assert_eq!(pkgs.len(), 1);
    assert_eq!(pkgs[0].name, "bash");
    assert_eq!(pkgs[0].version, "1:5.1.16-3.fc38");
}
