//! Unit tests for the `path` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn accepts_root() {
    let p = AbsolutePath::new("/").unwrap();
    assert_eq!(p.as_str(), "/");
}

#[test]
fn accepts_typical_paths() {
    AbsolutePath::new("/etc/nginx/nginx.conf").unwrap();
    AbsolutePath::new("/usr/local/bin/myapp").unwrap();
    AbsolutePath::new("/var/lib/cloud/seed/nocloud/user-data").unwrap();
}

#[test]
fn rejects_empty() {
    assert!(matches!(
        AbsolutePath::new(""),
        Err(AbsolutePathError::Empty)
    ));
}

#[test]
fn rejects_relative() {
    let err = AbsolutePath::new("etc/foo").unwrap_err();
    assert!(matches!(err, AbsolutePathError::NotAbsolute { ch: 'e' }));
    assert_eq!(err.offset(), Some(0));
}

#[test]
fn rejects_relative_dot_prefix() {
    assert!(matches!(
        AbsolutePath::new("./foo").unwrap_err(),
        AbsolutePathError::NotAbsolute { ch: '.' }
    ));
}

#[test]
fn rejects_nul_byte() {
    let err = AbsolutePath::new("/etc\0/foo").unwrap_err();
    assert!(matches!(err, AbsolutePathError::NulByte { offset: 4 }));
}

#[test]
fn rejects_backslash() {
    let err = AbsolutePath::new("/etc\\foo").unwrap_err();
    assert!(matches!(err, AbsolutePathError::Backslash { offset: 4 }));
}

#[test]
fn rejects_trailing_slash() {
    let err = AbsolutePath::new("/etc/").unwrap_err();
    assert!(matches!(
        err,
        AbsolutePathError::TrailingSlash { offset: 4 }
    ));
}

#[test]
fn rejects_double_slash() {
    let err = AbsolutePath::new("/etc//foo").unwrap_err();
    assert!(matches!(
        err,
        AbsolutePathError::EmptyComponent { offset: 5 }
    ));
}

// ── RecipePath ────────────────────────────────────────────────────────

#[test]
fn recipe_accepts_absolute() {
    let p = RecipePath::new("/etc/nginx/nginx.conf").unwrap();
    assert_eq!(p.as_str(), "/etc/nginx/nginx.conf");
    assert!(p.is_absolute());
    assert!(!p.has_trailing_slash());
}

#[test]
fn recipe_accepts_relative() {
    let p = RecipePath::new("./foo").unwrap();
    assert_eq!(p.as_str(), "./foo");
    assert!(!p.is_absolute());
}

#[test]
fn recipe_accepts_bare_relative() {
    let p = RecipePath::new("relative/path").unwrap();
    assert_eq!(p.as_str(), "relative/path");
    assert!(!p.is_absolute());
}

#[test]
fn recipe_preserves_trailing_slash() {
    let p = RecipePath::new("/usr/src/").unwrap();
    assert_eq!(p.as_str(), "/usr/src/");
    assert!(p.has_trailing_slash());
}

#[test]
fn recipe_relative_dot_slash_is_trailing() {
    let p = RecipePath::new("./").unwrap();
    assert_eq!(p.as_str(), "./");
    assert!(p.has_trailing_slash());
}

#[test]
fn recipe_root_is_not_trailing() {
    let p = RecipePath::new("/").unwrap();
    assert!(p.is_absolute());
    assert!(!p.has_trailing_slash());
}

#[test]
fn recipe_accepts_double_slash() {
    let p = RecipePath::new("/etc//foo").unwrap();
    assert_eq!(p.as_str(), "/etc//foo");
}

#[test]
fn recipe_rejects_empty() {
    assert!(matches!(
        RecipePath::new("").unwrap_err(),
        RecipePathError::Empty
    ));
}

#[test]
fn recipe_rejects_nul_byte() {
    let err = RecipePath::new("/etc\0/foo").unwrap_err();
    assert!(matches!(err, RecipePathError::NulByte { offset: 4 }));
}

#[test]
fn recipe_rejects_backslash() {
    let err = RecipePath::new("/etc\\foo").unwrap_err();
    assert!(matches!(err, RecipePathError::Backslash { offset: 4 }));
}
