//! Unit tests for the `kernel` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use std::fs;

fn seed_staging(layout_entries: &[(&str, &[u8])]) -> BuildStaging {
    let staging = BuildStaging::new().expect("staging");
    for (rel_path, payload) in layout_entries {
        let full = staging.path().join(rel_path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&full, payload).expect("write");
    }
    staging
}

#[test]
fn detects_release_suffixed_layout() {
    let staging = seed_staging(&[
        ("boot/vmlinuz-6.6.79", b"fake-kernel"),
        ("lib/modules/6.6.79/kernel/fs/ext4.ko", b"fake-module"),
    ]);
    let layout = detect_kernel_layout(&staging).expect("detect");
    assert_eq!(layout.release, "6.6.79");
    assert!(layout.vmlinuz.ends_with("boot/vmlinuz-6.6.79"));
    assert!(layout.modules.ends_with("lib/modules/6.6.79"));
}

#[test]
fn detects_alpine_style_vmlinuz_lts() {
    // Alpine-style: vmlinuz-lts + lib/modules/6.6-lts/
    let staging = seed_staging(&[
        ("boot/vmlinuz-lts", b"fake-kernel"),
        ("lib/modules/6.6-lts/kernel/dummy.ko", b"fake"),
    ]);
    let layout = detect_kernel_layout(&staging).expect("detect");
    // Suffix `lts` does not match the directory name `6.6-lts`, so we
    // fall back to alphabetical-first release.
    assert_eq!(layout.release, "6.6-lts");
    assert!(layout.modules.ends_with("lib/modules/6.6-lts"));
}

#[test]
fn prefers_suffix_match_when_multiple_releases_present() {
    // Two release dirs; vmlinuz suffix matches 7.0 → pick 7.0 even
    // though 6.6.79 sorts first alphabetically.
    let staging = seed_staging(&[
        ("boot/vmlinuz-7.0", b"newer"),
        ("lib/modules/6.6.79/kernel/x.ko", b"old"),
        ("lib/modules/7.0/kernel/x.ko", b"new"),
    ]);
    let layout = detect_kernel_layout(&staging).expect("detect");
    assert_eq!(layout.release, "7.0");
}

#[test]
fn errors_when_no_vmlinuz_present() {
    let staging = seed_staging(&[("lib/modules/6.6.79/dummy.ko", b"x")]);
    let err = detect_kernel_layout(&staging).unwrap_err();
    assert!(matches!(err, KernelLayoutError::NoVmlinuz), "got {err:?}");
}

#[test]
fn errors_when_no_modules_present() {
    let staging = seed_staging(&[("boot/vmlinuz-6.6.79", b"x")]);
    let err = detect_kernel_layout(&staging).unwrap_err();
    assert!(
        matches!(err, KernelLayoutError::NoModules { .. }),
        "got {err:?}"
    );
}

#[test]
fn errors_when_modules_dir_empty() {
    let staging = BuildStaging::new().expect("staging");
    fs::create_dir_all(staging.path().join("boot")).unwrap();
    fs::write(staging.path().join("boot/vmlinuz-6.6.79"), b"x").unwrap();
    fs::create_dir_all(staging.path().join("lib/modules/6.6.79")).unwrap();
    let err = detect_kernel_layout(&staging).unwrap_err();
    assert!(
        matches!(err, KernelLayoutError::NoModules { .. }),
        "got {err:?}"
    );
}
