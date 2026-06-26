//! Unit tests for the `filesystem` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use backhand::FilesystemReader;
use std::io::Cursor;
use tempfile::TempDir;

fn seed_basic_tree() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("etc")).unwrap();
    std::fs::create_dir_all(root.join("usr/bin")).unwrap();
    std::fs::write(root.join("etc/os-release"), b"NAME=\"Alpine Linux\"\n").unwrap();
    std::fs::write(root.join("usr/bin/hello"), b"#!/bin/sh\necho hi\n").unwrap();
    std::os::unix::fs::symlink("hello", root.join("usr/bin/hi")).expect("symlink");
    dir
}

#[test]
fn round_trip_basic_tree() {
    let dir = seed_basic_tree();
    let mut buf: Vec<u8> = Vec::new();
    let mut cursor = Cursor::new(&mut buf);
    let report = write_squashfs_from_dir(dir.path(), &mut cursor).expect("write");
    assert!(report.files >= 2, "expected files in report: {report:?}");
    assert!(report.symlinks >= 1, "expected symlinks: {report:?}");

    let mut rdr = Cursor::new(&buf);
    let fs = FilesystemReader::from_reader(&mut rdr).expect("read squashfs");
    let nodes: Vec<_> = fs.files().collect();
    let has = |path: &str| {
        nodes
            .iter()
            .any(|n| n.fullpath.to_string_lossy().as_ref() == path)
    };
    assert!(has("/etc/os-release"), "missing /etc/os-release");
    assert!(has("/usr/bin/hello"), "missing /usr/bin/hello");
    assert!(has("/usr/bin/hi"), "missing /usr/bin/hi (symlink)");
}

#[test]
fn symlinks_preserve_target() {
    let dir = seed_basic_tree();
    let mut buf: Vec<u8> = Vec::new();
    write_squashfs_from_dir(dir.path(), &mut Cursor::new(&mut buf)).expect("write");

    let mut rdr = Cursor::new(&buf);
    let fs = FilesystemReader::from_reader(&mut rdr).expect("read");
    let node = fs
        .files()
        .find(|n| n.fullpath.to_string_lossy() == "/usr/bin/hi")
        .expect("hi symlink");
    match &node.inner {
        backhand::InnerNode::Symlink(sym) => {
            assert_eq!(sym.link.to_string_lossy(), "hello");
        }
        other => panic!("expected symlink, got {other:?}"),
    }
}

#[test]
fn permissions_round_trip() {
    let dir = TempDir::new().expect("tempdir");
    let exec_path = dir.path().join("bin").join("script.sh");
    std::fs::create_dir_all(dir.path().join("bin")).unwrap();
    {
        let mut f = std::fs::File::create(&exec_path).unwrap();
        f.write_all(b"#!/bin/sh\n").unwrap();
        f.set_permissions(std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }

    let mut buf: Vec<u8> = Vec::new();
    write_squashfs_from_dir(dir.path(), &mut Cursor::new(&mut buf)).expect("write");

    let mut rdr = Cursor::new(&buf);
    let fs = FilesystemReader::from_reader(&mut rdr).expect("read");
    let node = fs
        .files()
        .find(|n| n.fullpath.to_string_lossy() == "/bin/script.sh")
        .expect("script");
    assert_eq!(node.header.permissions & 0o7777, 0o755);
}
