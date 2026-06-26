//! Unit tests for the `bench` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

/// Default path (no `--layout-dir`): the workspace lives under a
/// `TempDir` guard and is removed once that guard drops, so it does
/// *not* leak into `$TMPDIR`. Regression test for the `.keep()` bug
///.
#[test]
fn default_workspace_is_cleaned_up_on_drop() {
    let (workspace, guard) = bench_workspace(None).expect("workspace");

    // A guard is returned, and the directory exists while it lives.
    let guard = guard.expect("default path must return a TempDir guard");
    assert!(workspace.is_dir(), "workspace should exist during the run");

    // Dropping the guard removes the workspace (no residue).
    drop(guard);
    assert!(
        !workspace.exists(),
        "workspace must be removed when the guard drops, not leaked"
    );
}

/// Explicit `--layout-dir`: the caller owns the directory, so no
/// guard is returned and the path is used verbatim and persists.
#[test]
fn explicit_layout_dir_is_persistent_and_unguarded() {
    let caller_dir = tempfile::tempdir().expect("caller dir");

    let (workspace, guard) = bench_workspace(Some(caller_dir.path())).expect("workspace");

    assert!(guard.is_none(), "explicit path must not return a guard");
    assert_eq!(workspace, caller_dir.path());
    // The caller's directory is untouched by bench_workspace.
    assert!(caller_dir.path().is_dir());
}
