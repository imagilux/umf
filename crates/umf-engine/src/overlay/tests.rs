//! Unit tests for the `overlay` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use std::fs;

/// Whether the current process is plausibly able to mount overlayfs.
/// Conservative: returns true only if we're root. Rootless detection
/// (kernel 5.11+ unprivileged mount setattr) is left as a follow-up
/// since it requires probing rather than a uid check.
fn can_mount_overlay() -> bool {
    nix::unistd::Uid::current().is_root()
}

#[test]
fn overlay_mount_smoke() {
    if !can_mount_overlay() {
        eprintln!("skipping overlay_mount_smoke: needs root");
        return;
    }
    let lower = TempDir::new().expect("lower tempdir");
    fs::write(lower.path().join("hello"), b"lower\n").expect("write lower");

    let overlay = Overlay::mount(&[lower.path()]).expect("overlay mount");
    // Merged should see the lower file.
    assert_eq!(
        fs::read(overlay.merged().join("hello")).expect("read merged"),
        b"lower\n"
    );

    // Write through the merged dir → ends up in the upper.
    fs::write(overlay.merged().join("new"), b"upper\n").expect("write merged");
    let persisted = overlay.persist_upper().expect("persist upper");
    assert_eq!(
        fs::read(persisted.path().join("new")).expect("read upper"),
        b"upper\n"
    );
}
