//! Unit tests for the `cache` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt;

use super::*;
use tempfile::TempDir;

#[test]
fn add_source_digest_changes_when_file_contents_change() {
    let dir = TempDir::new().unwrap();
    let f = dir.path().join("a.txt");
    std::fs::write(&f, b"one").unwrap();
    let d1 = add_source_digest(&f).unwrap();
    std::fs::write(&f, b"two").unwrap();
    let d2 = add_source_digest(&f).unwrap();
    assert_ne!(d1, d2);
}

/// Build two identical source trees under a fresh temp root and
/// return `(tree_a, tree_b)`. Each holds `bin/run.sh` (0644) plus a
/// plain `note.txt`. Callers tweak one file's mode to assert the
/// digest reacts.
fn twin_trees(root: &Path) -> (PathBuf, PathBuf) {
    let mk = |name: &str| -> PathBuf {
        let tree = root.join(name);
        std::fs::create_dir_all(tree.join("bin")).unwrap();
        let script = tree.join("bin").join("run.sh");
        std::fs::write(&script, b"#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(tree.join("note.txt"), b"plain\n").unwrap();
        tree
    };
    (mk("a"), mk("b"))
}

#[test]
fn add_source_digest_dir_changes_with_file_mode() {
    let root = TempDir::new().unwrap();
    let (a, b) = twin_trees(root.path());
    // Identical inputs ⇒ identical digests.
    assert_eq!(
        add_source_digest(&a).unwrap(),
        add_source_digest(&b).unwrap()
    );
    // Flip one file's executable bit in tree `b`.
    let script_b = b.join("bin").join("run.sh");
    std::fs::set_permissions(&script_b, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_ne!(
        add_source_digest(&a).unwrap(),
        add_source_digest(&b).unwrap(),
        "0644 vs 0755 on a tree member must change the ADD digest"
    );
}

#[test]
fn add_source_digest_single_file_changes_with_mode() {
    let dir = TempDir::new().unwrap();
    let f = dir.path().join("x.sh");
    std::fs::write(&f, b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    let non_exec = add_source_digest(&f).unwrap();
    // Idempotent for unchanged input.
    assert_eq!(non_exec, add_source_digest(&f).unwrap());
    // `chmod +x` must bust the cache — the regression case.
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
    let exec = add_source_digest(&f).unwrap();
    assert_ne!(non_exec, exec);
}

#[test]
fn run_cache_key_is_stable_for_equal_inputs() {
    let a = run_cache_key(
        "amd64",
        "gzip",
        "parent-state",
        &["/bin/sh".into(), "-c".into(), "echo hi".into()],
        &["PATH=/usr/bin".into()],
        Some("/"),
        None,
        &[],
    );
    let b = run_cache_key(
        "amd64",
        "gzip",
        "parent-state",
        &["/bin/sh".into(), "-c".into(), "echo hi".into()],
        &["PATH=/usr/bin".into()],
        Some("/"),
        None,
        &[],
    );
    assert_eq!(a, b);
}

#[test]
fn run_cache_key_changes_when_argv_changes() {
    let a = run_cache_key(
        "amd64",
        "gzip",
        "parent",
        &["echo".into(), "hi".into()],
        &[],
        None,
        None,
        &[],
    );
    let b = run_cache_key(
        "amd64",
        "gzip",
        "parent",
        &["echo".into(), "bye".into()],
        &[],
        None,
        None,
        &[],
    );
    assert_ne!(a, b);
}

#[test]
fn run_cache_key_changes_when_architecture_changes() {
    // Same recipe, same parent, different --platform arch ⇒ distinct keys
    // so an amd64 layer can't be reused for an arm64 build (and vice versa).
    let amd64 = run_cache_key(
        "amd64",
        "gzip",
        "parent",
        &["sh".into()],
        &[],
        None,
        None,
        &[],
    );
    let arm64 = run_cache_key(
        "arm64",
        "gzip",
        "parent",
        &["sh".into()],
        &[],
        None,
        None,
        &[],
    );
    assert_ne!(amd64, arm64);
}

#[test]
fn run_cache_key_changes_when_referenced_secrets_change() {
    let none = run_cache_key(
        "amd64",
        "gzip",
        "parent",
        &["sh".into()],
        &[],
        None,
        None,
        &[],
    );
    let one = run_cache_key(
        "amd64",
        "gzip",
        "parent",
        &["sh".into()],
        &[],
        None,
        None,
        &["signing-key".into()],
    );
    let two = run_cache_key(
        "amd64",
        "gzip",
        "parent",
        &["sh".into()],
        &[],
        None,
        None,
        &["signing-key".into(), "registry-token".into()],
    );
    assert_ne!(none, one);
    assert_ne!(one, two);
}

#[test]
fn run_cache_key_independent_of_secret_id_order() {
    let a = run_cache_key(
        "amd64",
        "gzip",
        "p",
        &[],
        &[],
        None,
        None,
        &["a".into(), "b".into()],
    );
    let b = run_cache_key(
        "amd64",
        "gzip",
        "p",
        &[],
        &[],
        None,
        None,
        &["b".into(), "a".into()],
    );
    assert_eq!(a, b);
}

#[test]
fn add_cache_key_changes_when_architecture_changes() {
    let amd64 = add_cache_key("amd64", "gzip", "parent", "src-digest", "/dst");
    let arm64 = add_cache_key("arm64", "gzip", "parent", "src-digest", "/dst");
    assert_ne!(amd64, arm64);
}

#[test]
fn cache_keys_change_when_the_compression_codec_changes() {
    // Same step, same parent, different --compression codec ⇒ distinct keys
    // so a gzip layer is never adopted by a zstd build (and vice versa).
    let gz = run_cache_key(
        "amd64",
        "gzip",
        "parent",
        &["sh".into()],
        &[],
        None,
        None,
        &[],
    );
    let zst = run_cache_key(
        "amd64",
        "zstd",
        "parent",
        &["sh".into()],
        &[],
        None,
        None,
        &[],
    );
    assert_ne!(gz, zst);

    let gz = add_cache_key("amd64", "gzip", "parent", "src-digest", "/dst");
    let zst = add_cache_key("amd64", "zstd", "parent", "src-digest", "/dst");
    assert_ne!(gz, zst);

    let gz = cross_stage_add_cache_key("gzip", "parent", "sha256:m", "/bin/app", "/app");
    let zst = cross_stage_add_cache_key("zstd", "parent", "sha256:m", "/bin/app", "/app");
    assert_ne!(gz, zst);
}
