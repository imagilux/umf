//! Unit tests for the `runtime_writer` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use tempfile::TempDir;

#[test]
fn enable_systemd_writes_wants_symlink() {
    let dir = TempDir::new().expect("tempdir");
    write_enable_link(dir.path(), InitSystem::Systemd, "nginx.service").expect("write");
    let link = dir
        .path()
        .join("etc/systemd/system/multi-user.target.wants/nginx.service");
    let target = fs::read_link(&link).expect("readlink");
    assert_eq!(
        target.to_string_lossy(),
        "/usr/lib/systemd/system/nginx.service"
    );
}

#[test]
fn enable_openrc_writes_runlevel_symlink() {
    let dir = TempDir::new().expect("tempdir");
    write_enable_link(dir.path(), InitSystem::OpenRc, "crond").expect("write");
    let link = dir.path().join("etc/runlevels/default/crond");
    let target = fs::read_link(&link).expect("readlink");
    assert_eq!(target.to_string_lossy(), "/etc/init.d/crond");
}

#[test]
fn enable_is_idempotent() {
    let dir = TempDir::new().expect("tempdir");
    write_enable_link(dir.path(), InitSystem::Systemd, "x.service").expect("first");
    write_enable_link(dir.path(), InitSystem::Systemd, "x.service").expect("second");
    // Should not error — re-running is fine.
}
