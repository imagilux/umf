//! Unit tests for `umf doctor` rendering + version parsing.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn extract_version_handles_common_tool_output() {
    assert_eq!(
        extract_version_token("QEMU emulator version 8.2.2 (Debian 1:8.2.2)").as_deref(),
        Some("8.2.2"),
    );
    assert_eq!(
        extract_version_token("cloud-hypervisor v37.0.0").as_deref(),
        Some("v37.0.0"),
    );
    assert_eq!(
        extract_version_token("Dnsmasq version 2.90  Copyright (c) 2000-2023").as_deref(),
        Some("2.90"),
    );
    assert_eq!(
        extract_version_token("nftables v1.0.9 (Old Doc Yak)").as_deref(),
        Some("v1.0.9"),
    );
}

#[test]
fn extract_version_returns_none_without_a_dotted_version() {
    assert_eq!(extract_version_token(""), None);
    assert_eq!(extract_version_token("no numbers here"), None);
    // A bare integer with no dot is not a version token.
    assert_eq!(extract_version_token("build 12345"), None);
}

#[test]
fn status_cells_degrade_without_color_and_carry_glyphs_with_it() {
    assert_eq!(Status::Ok.cell(false), "ok");
    assert_eq!(Status::Warn.cell(false), "warn");
    assert_eq!(Status::Missing.cell(false), "missing");
    assert_eq!(Status::Unknown.cell(false), "?");
    assert!(Status::Ok.cell(true).contains('✓'));
    assert!(Status::Missing.cell(true).contains('✗'));
    // ANSI escape only on the colored path.
    assert!(!Status::Ok.cell(false).contains('\x1b'));
    assert!(Status::Ok.cell(true).contains('\x1b'));
}

#[test]
fn render_section_has_title_headers_and_aligned_columns() {
    let rows = vec![
        Row::new(
            "nft",
            "egress masquerade",
            "/usr/sbin/nft",
            "v1.0.9",
            Status::Ok,
        ),
        Row::new("dnsmasq", "in-VM DHCP", "<none on PATH>", "", Status::Warn),
    ];
    let out = render_section("VM / bootable", &rows, false);

    assert!(out.contains("VM / bootable"), "section title: {out}");
    assert!(
        out.contains("NAME") && out.contains("PURPOSE") && out.contains("STATUS"),
        "column headers: {out}",
    );
    assert!(
        out.contains("ok") && out.contains("warn"),
        "status words: {out}"
    );

    // The PURPOSE column begins at the same offset on every data row — i.e. the
    // shorter NAME (`nft`) was padded to the widest (`dnsmasq`).
    let nft_line = out
        .lines()
        .find(|l| l.contains("egress masquerade"))
        .expect("nft row");
    let dns_line = out
        .lines()
        .find(|l| l.contains("in-VM DHCP"))
        .expect("dnsmasq row");
    assert_eq!(
        nft_line.find("egress masquerade"),
        dns_line.find("in-VM DHCP"),
        "purpose column is aligned across rows:\n{out}",
    );
}

#[test]
fn seccomp_status_loads_the_embedded_profile() {
    let (detail, status) = seccomp_status();
    assert_eq!(status, Status::Ok);
    assert!(detail.contains("syscalls"), "detail: {detail}");
}
