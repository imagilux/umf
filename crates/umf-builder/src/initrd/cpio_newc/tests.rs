//! Unit tests for the `cpio_newc` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

/// Parse the produced CPIO archive entry-by-entry to confirm the bytes
/// are valid newc.
fn parse_archive(bytes: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
    let mut entries: Vec<(String, u32, Vec<u8>)> = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        // Header is 110 bytes.
        let header = &bytes[pos..pos + 110];
        assert_eq!(&header[..6], NEWC_MAGIC);
        let mode = u32::from_str_radix(std::str::from_utf8(&header[14..22]).expect("ascii"), 16)
            .expect("mode hex");
        let filesize =
            u32::from_str_radix(std::str::from_utf8(&header[54..62]).expect("ascii"), 16)
                .expect("filesize hex");
        let namesize =
            u32::from_str_radix(std::str::from_utf8(&header[94..102]).expect("ascii"), 16)
                .expect("namesize hex");
        pos += 110;
        // Name (length includes NUL).
        let name_end = pos + namesize as usize;
        let name_bytes = &bytes[pos..name_end - 1]; // drop NUL
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        pos = name_end;
        // Pad to 4.
        while pos % 4 != 0 {
            pos += 1;
        }
        // Data.
        let data = bytes[pos..pos + filesize as usize].to_vec();
        pos += filesize as usize;
        while pos % 4 != 0 {
            pos += 1;
        }
        if name == "TRAILER!!!" {
            break;
        }
        entries.push((name, mode, data));
    }
    entries
}

#[test]
fn writes_files_and_dirs() {
    let mut w = CpioWriter::new();
    w.push(CpioEntry {
        path: PathBuf::from("."),
        kind: CpioKind::Directory,
        mode: 0o755,
        ..CpioEntry::default()
    });
    w.push(CpioEntry {
        path: PathBuf::from("hello.txt"),
        kind: CpioKind::File(b"world\n".to_vec()),
        mode: 0o644,
        ..CpioEntry::default()
    });
    let archive = w.finish();
    let parsed = parse_archive(&archive);
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].0, ".");
    assert_eq!(parsed[0].1 & S_IFMT, S_IFDIR);
    assert_eq!(parsed[1].0, "hello.txt");
    assert_eq!(parsed[1].1 & S_IFMT, S_IFREG);
    assert_eq!(parsed[1].2, b"world\n");
}

#[test]
fn writes_symlinks_with_target_as_payload() {
    let mut w = CpioWriter::new();
    w.push(CpioEntry {
        path: PathBuf::from("bin/sh"),
        kind: CpioKind::Symlink(PathBuf::from("busybox")),
        mode: 0o755,
        ..CpioEntry::default()
    });
    let archive = w.finish();
    let parsed = parse_archive(&archive);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].0, "bin/sh");
    assert_eq!(parsed[0].1 & S_IFMT, S_IFLNK);
    assert_eq!(parsed[0].2, b"busybox");
}

#[test]
fn emits_trailer() {
    let archive = CpioWriter::new().finish();
    // Even an empty archive ends in a TRAILER!!! entry.
    assert!(
        std::str::from_utf8(&archive)
            .expect("ascii")
            .contains("TRAILER!!!"),
        "missing trailer: {archive:?}",
    );
}

#[test]
fn pads_to_4_bytes() {
    // A 7-byte payload should be followed by 1 byte of padding (8 = 4*2).
    let mut w = CpioWriter::new();
    w.push(CpioEntry {
        path: PathBuf::from("seven"),
        kind: CpioKind::File(vec![b'a'; 7]),
        mode: 0o644,
        ..CpioEntry::default()
    });
    let archive = w.finish();
    // Total must be a multiple of 4.
    assert_eq!(archive.len() % 4, 0, "archive length not 4-aligned");
}
