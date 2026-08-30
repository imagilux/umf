//! Unit tests for the `whiteout` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn whiteout_name_prefixes() {
    assert_eq!(whiteout_name("passwd"), ".wh.passwd");
    assert_eq!(whiteout_name(".hidden"), ".wh..hidden");
}

#[test]
fn opaque_marker_is_the_spec_spelling() {
    // Pinned deliberately: the reader matches these exact strings, and a typo
    // here would silently stop whiteouts round-tripping.
    assert_eq!(WH_OPAQUE, ".wh..wh..opq");
    assert_eq!(WH_PREFIX, ".wh.");
}

#[test]
fn a_regular_file_is_not_a_deletion_marker() {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("ordinary");
    std::fs::write(&f, b"content").expect("write");
    let meta = std::fs::symlink_metadata(&f).expect("lstat");
    assert!(!is_overlay_deletion(&meta));
}

#[test]
fn a_directory_without_the_xattr_is_not_opaque() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(!is_opaque_dir(dir.path()));
}

#[test]
fn opaque_detection_reads_the_user_namespace_xattr() {
    // `user.overlay.opaque` is the spelling kernel overlayfs uses when mounted
    // inside a user namespace, and unlike `trusted.*` it is settable without
    // CAP_SYS_ADMIN — so it is the one variant a test can drive directly.
    // Skipped where the backing filesystem has no user-xattr support.
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("opaque-dir");
    std::fs::create_dir(&target).expect("mkdir");
    if xattr::set(&target, "user.overlay.opaque", b"y").is_err() {
        eprintln!("skipping: filesystem does not support user xattrs");
        return;
    }
    assert!(
        is_opaque_dir(&target),
        "y-valued xattr marks the dir opaque"
    );

    // Any other value is not the marker.
    xattr::set(&target, "user.overlay.opaque", b"n").expect("set n");
    assert!(!is_opaque_dir(&target), "only `y` marks a dir opaque");
}
