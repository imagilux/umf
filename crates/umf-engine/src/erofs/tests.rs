//! Unit tests for the `erofs` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use std::process::Command;

/// Build a one-file erofs image at `out` from `mkfs.erofs` reading a
/// directory. Returns false (skip) if the host can't encode/mount.
fn make_erofs(out: &Path, name: &str, contents: &str) -> bool {
    if !mount_available() || !umf_oci::erofs::encoder_available() {
        return false;
    }
    let src = tempfile::tempdir().expect("src");
    std::fs::write(src.path().join(name), contents).expect("write");
    let status = Command::new("mkfs.erofs")
        .arg(out)
        .arg(src.path())
        .status()
        .expect("mkfs.erofs");
    assert!(status.success(), "mkfs.erofs failed");
    true
}

#[test]
fn mounts_layers_and_unmounts_on_drop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let erofs = dir.path().join("a.erofs");
    if !make_erofs(&erofs, "hello", "erofs-content\n") {
        eprintln!("skipping erofs mount test: needs root + erofs + mkfs.erofs");
        return;
    }
    let mounted = MountedErofsLayers::mount(&[erofs]).expect("mount");
    let mps = mounted.mountpoints();
    assert_eq!(mps.len(), 1);
    let body = std::fs::read_to_string(mps[0].join("hello")).expect("read");
    assert_eq!(body, "erofs-content\n");
    let mp = mps[0].to_path_buf();
    drop(mounted);
    // After drop the mountpoint dir is gone (tempdir removed).
    assert!(!mp.exists(), "mountpoint should be cleaned up after drop");
}
