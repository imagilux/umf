//! Unit tests for the `util` module.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

fn write(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write fixture");
}

#[test]
fn truncate_for_column_is_char_safe() {
    // Fits (by char count): returned unchanged.
    assert_eq!(truncate_for_column("abc", 5), "abc");
    // ASCII overflow: width-1 chars + ellipsis.
    assert_eq!(truncate_for_column("abcdef", 4), "abc…");
    // Regression: a multi-byte char straddling the width-1 *byte* boundary
    // must not panic — the old `&s[..4]` byte-slice paniced inside 'é'.
    assert_eq!(truncate_for_column("café-registry", 5), "café…");
    // A short multi-byte string under the limit is returned as-is.
    assert_eq!(truncate_for_column("éàü", 5), "éàü");
}

#[test]
fn truncate_chars_never_splits_a_codepoint() {
    assert_eq!(truncate_chars("sha256:abcdef", 9), "sha256:ab");
    // Fewer than `max` chars: returned whole.
    assert_eq!(truncate_chars("abc", 9), "abc");
    assert_eq!(truncate_chars("", 5), "");
    // Regression: a non-ASCII digest (crafted archive / hostile registry)
    // must not panic the way `&s[..n]` byte-slicing did.
    assert_eq!(truncate_chars("éàüφ-digest", 4), "éàüφ");
}

#[test]
fn explicit_file_positional_is_the_recipe() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recipe = dir.path().join("my-recipe");
    write(&recipe, "FROM scratch\n");

    let resolved = resolve_recipe(Some(&recipe), None).expect("resolve");
    assert_eq!(resolved.recipe, recipe);
    assert_eq!(resolved.context, dir.path());
}

#[test]
fn directory_discovers_containerfile() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("Containerfile"), "FROM scratch\n");

    let resolved = resolve_recipe(Some(dir.path()), None).expect("resolve");
    assert_eq!(resolved.recipe, dir.path().join("Containerfile"));
    assert_eq!(resolved.context, dir.path());
}

#[test]
fn containerfile_wins_over_dockerfile() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("Containerfile"), "FROM scratch\n");
    write(&dir.path().join("Dockerfile"), "FROM scratch\n");

    let resolved = resolve_recipe(Some(dir.path()), None).expect("resolve");
    assert_eq!(resolved.recipe, dir.path().join("Containerfile"));
}

#[test]
fn directory_falls_back_to_dockerfile() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir.path().join("Dockerfile"), "FROM scratch\n");

    let resolved = resolve_recipe(Some(dir.path()), None).expect("resolve");
    assert_eq!(resolved.recipe, dir.path().join("Dockerfile"));
}

#[test]
fn empty_directory_errors_with_names_listed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = resolve_recipe(Some(dir.path()), None).expect_err("should fail");
    let msg = err.to_string();
    assert!(msg.contains("Containerfile"), "got: {msg}");
    assert!(msg.contains("Dockerfile"), "got: {msg}");
    assert!(msg.contains("-f/--file"), "got: {msg}");
}

#[test]
fn missing_positional_path_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist");
    let err = resolve_recipe(Some(&missing), None).expect_err("should fail");
    assert!(matches!(err, RecipeResolveError::NotFound(_)));
}

#[test]
fn file_override_takes_precedence_and_sets_context() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recipe = dir.path().join("prod.recipe");
    write(&recipe, "FROM scratch\n");
    // Even with a Containerfile present, -f wins; the positional dir
    // becomes the context.
    write(&dir.path().join("Containerfile"), "FROM scratch\n");

    let resolved = resolve_recipe(Some(dir.path()), Some(&recipe)).expect("resolve");
    assert_eq!(resolved.recipe, recipe);
    assert_eq!(resolved.context, dir.path());
}

#[test]
fn file_override_without_positional_defaults_context_to_cwd() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recipe = dir.path().join("standalone");
    write(&recipe, "FROM scratch\n");

    let resolved = resolve_recipe(None, Some(&recipe)).expect("resolve");
    assert_eq!(resolved.recipe, recipe);
    assert_eq!(resolved.context, Path::new("."));
}

#[test]
fn file_override_missing_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("nope");
    let err = resolve_recipe(None, Some(&missing)).expect_err("should fail");
    assert!(matches!(err, RecipeResolveError::FileMissing(_)));
}

#[test]
fn file_override_pointing_at_directory_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = resolve_recipe(None, Some(dir.path())).expect_err("should fail");
    assert!(matches!(err, RecipeResolveError::FileIsDir(_)));
}
