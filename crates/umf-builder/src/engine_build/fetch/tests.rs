//! Unit tests for `ADD <url>` fetch helpers.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn url_leaf_takes_the_last_path_segment() {
    assert_eq!(
        url_leaf("https://example.com/dist/app.tar.gz"),
        "app.tar.gz"
    );
    assert_eq!(url_leaf("http://example.com/a/b/c.bin?x=1&y=2"), "c.bin");
    assert_eq!(url_leaf("https://example.com/file#frag"), "file");
}

#[test]
fn url_leaf_falls_back_for_bare_authorities() {
    assert_eq!(url_leaf("https://example.com"), "download");
    assert_eq!(url_leaf("https://example.com/"), "download");
}
