//! Unit tests for the `run` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn argv_uses_image_defaults_when_no_overrides() {
    let argv = compose_argv(
        &["/bin/sh".into()],
        &["-c".into(), "echo hi".into()],
        None,
        None,
    );
    assert_eq!(argv, vec!["/bin/sh", "-c", "echo hi"]);
}

#[test]
fn cmd_only_override_keeps_image_entrypoint() {
    let argv = compose_argv(
        &["/usr/local/bin/app".into()],
        &["--default".into()],
        None,
        Some(&["--from-cli".into()]),
    );
    assert_eq!(argv, vec!["/usr/local/bin/app", "--from-cli"]);
}

#[test]
fn entrypoint_only_override_drops_image_cmd() {
    let argv = compose_argv(
        &["/usr/local/bin/app".into()],
        &["--default".into()],
        Some(&["/bin/bash".into()]),
        None,
    );
    assert_eq!(argv, vec!["/bin/bash"]);
}

#[test]
fn both_overrides_concatenate() {
    let argv = compose_argv(
        &["/usr/local/bin/app".into()],
        &["--default".into()],
        Some(&["/bin/bash".into()]),
        Some(&["-c".into(), "ls -la".into()]),
    );
    assert_eq!(argv, vec!["/bin/bash", "-c", "ls -la"]);
}

#[test]
fn default_state_root_uses_xdg_runtime_dir_when_set() {
    let root = default_state_root_for(Some(std::ffi::OsString::from("/tmp/xdg-test")));
    assert_eq!(root, PathBuf::from("/tmp/xdg-test/umf-engine/run"));
}

#[test]
fn default_state_root_falls_back_to_run_when_unset() {
    let root = default_state_root_for(None);
    assert_eq!(root, PathBuf::from("/run/umf-engine/run"));
}

#[test]
fn container_id_starts_with_umf_run_prefix() {
    let id = generate_container_id();
    assert!(id.starts_with("umf-run-"), "got: {id}");
    // Should contain at least two `-`-separated trailing components
    // (pid + nanos).
    let parts: Vec<&str> = id.split('-').collect();
    assert!(parts.len() >= 4, "got: {id}");
}
