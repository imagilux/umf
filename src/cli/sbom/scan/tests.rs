#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use tempfile::TempDir;

#[test]
fn dpkg_status_yields_installed_packages_only() {
    let body = "\
Package: bash
Status: install ok installed
Version: 5.1-2
Architecture: amd64

Package: gone
Status: deinstall ok config-files
Version: 1.0
Architecture: amd64

Package: coreutils
Status: install ok installed
Architecture: amd64
Version: 8.32-4
";
    let pkgs = parse_dpkg(body, "debian");
    assert_eq!(
        pkgs.len(),
        2,
        "the removed (config-files) package is excluded"
    );
    let bash = pkgs
        .iter()
        .find(|p| p.name == "bash")
        .expect("bash present");
    assert_eq!(bash.version, "5.1-2");
    assert_eq!(bash.arch.as_deref(), Some("amd64"));
    assert_eq!(
        bash.purl.as_deref(),
        Some("pkg:deb/debian/bash@5.1-2?arch=amd64"),
    );
}

#[test]
fn apk_installed_db_parses_records() {
    let body = "P:musl\nV:1.2.3-r0\nA:x86_64\n\nP:busybox\nV:1.35.0-r17\nA:x86_64\n";
    let pkgs = parse_apk(body, "alpine");
    assert_eq!(pkgs.len(), 2);
    let musl = pkgs
        .iter()
        .find(|p| p.name == "musl")
        .expect("musl present");
    assert_eq!(musl.version, "1.2.3-r0");
    assert_eq!(
        musl.purl.as_deref(),
        Some("pkg:apk/alpine/musl@1.2.3-r0?arch=x86_64"),
    );
}

#[test]
fn pacman_desc_extracts_fields() {
    let desc = "%NAME%\nbash\n\n%VERSION%\n5.1.016-1\n\n%ARCH%\nx86_64\n";
    let (name, version, arch) = parse_pacman_desc(desc);
    assert_eq!(name.as_deref(), Some("bash"));
    assert_eq!(version.as_deref(), Some("5.1.016-1"));
    assert_eq!(arch.as_deref(), Some("x86_64"));
}

#[test]
fn scan_rootfs_reads_dpkg_sorted_and_namespaces_purls_by_os_id() {
    let root = TempDir::new().unwrap();
    std::fs::create_dir_all(root.path().join("var/lib/dpkg")).unwrap();
    std::fs::create_dir_all(root.path().join("etc")).unwrap();
    std::fs::write(
        root.path().join("etc/os-release"),
        "ID=ubuntu\nVERSION_ID=\"22.04\"\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("var/lib/dpkg/status"),
        "Package: zlib1g\nStatus: install ok installed\nVersion: 1.2.11\nArchitecture: amd64\n\n\
         Package: bash\nStatus: install ok installed\nVersion: 5.1-2\nArchitecture: amd64\n",
    )
    .unwrap();

    let inv = scan_rootfs(root.path()).unwrap();
    assert_eq!(inv.os_id.as_deref(), Some("ubuntu"));
    let names: Vec<_> = inv.packages.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["bash", "zlib1g"], "sorted by name");
    assert_eq!(
        inv.packages[0].purl.as_deref(),
        Some("pkg:deb/ubuntu/bash@5.1-2?arch=amd64"),
        "purl namespace follows the os-release ID",
    );
}

#[test]
fn scan_rootfs_with_no_package_db_is_empty_not_an_error() {
    let root = TempDir::new().unwrap();
    let inv = scan_rootfs(root.path()).unwrap();
    assert!(inv.packages.is_empty());
    assert!(inv.os_id.is_none());
}

#[test]
fn scan_rootfs_refuses_to_read_a_db_symlink_escaping_the_rootfs() {
    // A hostile layer plants `var/lib/dpkg/status` as a symlink to a host file
    // (here a secret outside the rootfs). The scanner must NOT follow it — else
    // the host file's bytes land in the SBOM.
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let secret = outside.path().join("shadow");
    std::fs::write(&secret, "root:$6$hostsecret:0:0\n").unwrap();

    std::fs::create_dir_all(root.path().join("var/lib/dpkg")).unwrap();
    std::os::unix::fs::symlink(&secret, root.path().join("var/lib/dpkg/status")).unwrap();
    // Also point os-release off-rootfs to be thorough.
    std::fs::create_dir_all(root.path().join("etc")).unwrap();
    std::os::unix::fs::symlink(&secret, root.path().join("etc/os-release")).unwrap();

    let inv = scan_rootfs(root.path()).unwrap();
    // The escaping db is treated as absent: no packages, no os-id leaked.
    assert!(
        inv.packages.is_empty(),
        "escaping db symlink must not be read"
    );
    assert!(
        inv.os_id.is_none(),
        "escaping os-release symlink must not be read"
    );
}

#[test]
fn scan_rootfs_follows_an_internal_db_symlink() {
    // An internal symlink whose target stays inside the rootfs is legitimate
    // (e.g. a distro layout) and must still be read.
    let root = TempDir::new().unwrap();
    std::fs::create_dir_all(root.path().join("var/lib/dpkg")).unwrap();
    std::fs::create_dir_all(root.path().join("real")).unwrap();
    std::fs::write(
        root.path().join("real/status"),
        "Package: bash\nStatus: install ok installed\nVersion: 5.1-2\nArchitecture: amd64\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(
        root.path().join("real/status"),
        root.path().join("var/lib/dpkg/status"),
    )
    .unwrap();

    let inv = scan_rootfs(root.path()).unwrap();
    let names: Vec<_> = inv.packages.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["bash"],
        "internal symlink is followed within the rootfs"
    );
}
