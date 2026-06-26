//! Unit tests for the ADD handlers' destination + containment helpers.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use tempfile::TempDir;

#[test]
fn add_destination_with_trailing_slash_appends_basename() {
    let src = std::path::PathBuf::from("/tmp/nginx.conf");
    assert_eq!(
        compute_add_destination("/etc/nginx/", &src),
        "/etc/nginx/nginx.conf"
    );
}

#[test]
fn add_destination_to_file_keeps_destination_verbatim() {
    let src = std::path::PathBuf::from("/tmp/source.txt");
    assert_eq!(
        compute_add_destination("/etc/target.txt", &src),
        "/etc/target.txt"
    );
}

#[test]
fn add_destination_for_directory_source_forces_directory_form() {
    let dir = TempDir::new().unwrap();
    // make src a directory
    let dst = compute_add_destination("/opt/foo", dir.path());
    assert_eq!(dst, "/opt/foo/");
}

#[test]
fn path_within_upper_strips_leading_slash() {
    let upper = std::path::Path::new("/tmp/staging/upper");
    assert_eq!(
        path_within_upper(upper, "/etc/foo"),
        std::path::PathBuf::from("/tmp/staging/upper/etc/foo"),
    );
    assert_eq!(
        path_within_upper(upper, "etc/foo"),
        std::path::PathBuf::from("/tmp/staging/upper/etc/foo"),
    );
}

#[test]
fn reject_traversal_blocks_parent_dir_in_source_and_destination() {
    // Sources that climb out of the containment root.
    assert!(reject_traversal("source", "../../etc/passwd").is_err());
    assert!(reject_traversal("source", "a/../../b").is_err());
    // A cross-stage source like `ADD --from=s ../../../etc/shadow`.
    assert!(reject_traversal("source", "../../../etc/shadow").is_err());
    // Destinations that escape the upper-dir even after the leading-slash
    // strip in `path_within_upper`.
    assert!(reject_traversal("destination", "/../../etc/cron.d/x").is_err());
    assert!(reject_traversal("destination", "opt/../../escape").is_err());
}

#[test]
fn reject_traversal_allows_normal_paths() {
    assert!(reject_traversal("source", "foo/bar.conf").is_ok());
    assert!(reject_traversal("source", "./nginx.conf").is_ok());
    assert!(reject_traversal("destination", "/etc/app/app.conf").is_ok());
    assert!(reject_traversal("destination", "usr/local/bin/app").is_ok());
    // A bare `..`-free name with internal dots is fine.
    assert!(reject_traversal("source", "a.b.c/d..e").is_ok());
}

#[test]
fn plain_copy_rejects_remote_sources_only() {
    use umf_core::ast::{Span, Spanned};
    use umf_core::types::{HttpsUrl, OciReference};

    let span = Span::new(0, 0);
    // COPY accepts local-context paths (and, by the same token, --from paths).
    let local = AddSource::Path(Spanned::new("./app".to_string(), span));
    assert_eq!(plain_copy_rejected_kind(&local), None);

    // COPY refuses remote sources — those are ADD's job.
    let url = AddSource::Url(Spanned::new(
        HttpsUrl::new("https://example.com/x.tar".to_string()).unwrap(),
        span,
    ));
    assert_eq!(plain_copy_rejected_kind(&url), Some("a URL"));

    let oci = AddSource::Oci(Spanned::new(
        OciReference::new("imagilux/rootfs:v7.0".to_string()).unwrap(),
        span,
    ));
    assert_eq!(
        plain_copy_rejected_kind(&oci),
        Some("an OCI image reference")
    );
}
