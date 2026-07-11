//! Unit tests for the `run` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

/// `touch <root>/<rel>`, creating parent directories.
fn touch(root: &Path, rel: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().expect("has parent")).expect("mkdir -p");
    std::fs::write(&p, b"\0").expect("write fixture");
}

#[test]
fn resolve_prefers_single_file_bios() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    // Lay down BOTH a single-file blob and a split pair; single-file wins.
    touch(root, "usr/share/OVMF/OVMF.fd");
    touch(root, "usr/share/OVMF/OVMF_CODE.fd");
    touch(root, "usr/share/OVMF/OVMF_VARS.fd");
    match resolve_uefi_firmware(root, VmArch::X86_64) {
        Some(Firmware::Bios(p)) => assert_eq!(p, root.join("usr/share/OVMF/OVMF.fd")),
        other => panic!("expected single-file Bios, got {other:?}"),
    }
}

#[test]
fn resolve_finds_split_code_vars_when_no_single_file() {
    // The modern-distro case: only the split layout is present. This is
    // exactly the host shape that previously failed without `--firmware`.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    touch(root, "usr/share/OVMF/OVMF_CODE.fd");
    touch(root, "usr/share/OVMF/OVMF_VARS.fd");
    match resolve_uefi_firmware(root, VmArch::X86_64) {
        Some(Firmware::Pflash { code, vars }) => {
            assert_eq!(code, root.join("usr/share/OVMF/OVMF_CODE.fd"));
            assert_eq!(vars, root.join("usr/share/OVMF/OVMF_VARS.fd"));
        }
        other => panic!("expected split Pflash, got {other:?}"),
    }
}

#[test]
fn resolve_matches_arch_4m_split_layout() {
    // Arch ships sized `.4m` variants under usr/share/edk2/x64.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    touch(root, "usr/share/edk2/x64/OVMF_CODE.4m.fd");
    touch(root, "usr/share/edk2/x64/OVMF_VARS.4m.fd");
    match resolve_uefi_firmware(root, VmArch::X86_64) {
        Some(Firmware::Pflash { code, vars }) => {
            assert_eq!(code, root.join("usr/share/edk2/x64/OVMF_CODE.4m.fd"));
            assert_eq!(vars, root.join("usr/share/edk2/x64/OVMF_VARS.4m.fd"));
        }
        other => panic!("expected Arch .4m split Pflash, got {other:?}"),
    }
}

#[test]
fn resolve_skips_half_installed_split_pair() {
    // CODE present but VARS missing: not a usable pflash pair, so the
    // resolver must not return it (it would yield an unbootable VM).
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    touch(root, "usr/share/OVMF/OVMF_CODE.fd");
    assert!(
        resolve_uefi_firmware(root, VmArch::X86_64).is_none(),
        "a lone CODE half must not resolve",
    );
}

#[test]
fn resolve_returns_none_on_empty_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(resolve_uefi_firmware(dir.path(), VmArch::X86_64).is_none());
    assert!(resolve_uefi_firmware(dir.path(), VmArch::Aarch64).is_none());
}

#[test]
fn resolve_aarch64_finds_aavmf_pflash_pair() {
    // The aarch64 happy path: AAVMF CODE + VARS present, wired as pflash
    // (there is no `-bios` form on ARM).
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    touch(root, "usr/share/AAVMF/AAVMF_CODE.fd");
    touch(root, "usr/share/AAVMF/AAVMF_VARS.fd");
    match resolve_uefi_firmware(root, VmArch::Aarch64) {
        Some(Firmware::Pflash { code, vars }) => {
            assert_eq!(code, root.join("usr/share/AAVMF/AAVMF_CODE.fd"));
            assert_eq!(vars, root.join("usr/share/AAVMF/AAVMF_VARS.fd"));
        }
        other => panic!("expected AAVMF split Pflash, got {other:?}"),
    }
}

#[test]
fn resolve_aarch64_never_returns_bios() {
    // Even with an x86 single-file OVMF.fd lying around, an aarch64
    // lookup must not pick it (wrong arch + `-bios` is invalid on ARM).
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    touch(root, "usr/share/OVMF/OVMF.fd");
    touch(root, "usr/share/OVMF/OVMF_CODE.fd");
    touch(root, "usr/share/OVMF/OVMF_VARS.fd");
    assert!(
        resolve_uefi_firmware(root, VmArch::Aarch64).is_none(),
        "x86 OVMF must not satisfy an aarch64 firmware lookup",
    );
}

#[test]
fn resolve_aarch64_skips_half_installed_aavmf() {
    // CODE present but VARS missing: not a usable pflash pair.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    touch(root, "usr/share/AAVMF/AAVMF_CODE.fd");
    assert!(
        resolve_uefi_firmware(root, VmArch::Aarch64).is_none(),
        "a lone AAVMF CODE half must not resolve",
    );
}

#[test]
fn firmware_hints_are_arch_specific() {
    assert!(firmware_install_hint(VmArch::X86_64).contains("OVMF"));
    assert!(firmware_install_hint(VmArch::Aarch64).contains("AAVMF"));
    assert_eq!(arch_label(VmArch::X86_64), "x86_64");
    assert_eq!(arch_label(VmArch::Aarch64), "aarch64");
}

#[test]
fn dhcp_command_flag_maps_to_daemon() {
    use umf_networking::DhcpDaemon;
    // Absent: the default dnsmasq.
    assert!(matches!(parse_dhcp_command(None), DhcpDaemon::Dnsmasq));
    // `none` (any case / surrounding space): launch nothing.
    assert!(matches!(parse_dhcp_command(Some("none")), DhcpDaemon::None));
    assert!(matches!(
        parse_dhcp_command(Some("  NoNe ")),
        DhcpDaemon::None
    ));
    // Anything else: a whitespace-split argv launched in the VM netns.
    match parse_dhcp_command(Some("kea-dhcp4 -c /etc/kea.conf")) {
        DhcpDaemon::Custom(argv) => assert_eq!(argv, ["kea-dhcp4", "-c", "/etc/kea.conf"]),
        other => panic!("expected Custom argv, got {other:?}"),
    }
}

#[test]
fn parse_port_forward_plain_host_guest() {
    let pf = parse_port_forward("8080:80").expect("parses");
    assert_eq!(pf.bind_addr, None);
    assert_eq!((pf.host_port, pf.guest_port, pf.tcp), (8080, 80, true));
}

#[test]
fn parse_port_forward_proto_and_bind() {
    let udp = parse_port_forward("5353:53/udp").expect("parses");
    assert_eq!(udp.bind_addr, None);
    assert!(!udp.tcp);

    let bound = parse_port_forward("127.0.0.1:8080:80").expect("parses");
    assert_eq!(bound.bind_addr, Some(std::net::Ipv4Addr::new(127, 0, 0, 1)));
    assert_eq!(
        (bound.host_port, bound.guest_port, bound.tcp),
        (8080, 80, true)
    );

    let bound_udp = parse_port_forward("10.0.0.5:9090:90/udp").expect("parses");
    assert_eq!(
        bound_udp.bind_addr,
        Some(std::net::Ipv4Addr::new(10, 0, 0, 5))
    );
    assert!(!bound_udp.tcp);
}

#[test]
fn parse_port_forward_rejects_bad_specs() {
    // Missing guest.
    assert!(parse_port_forward("8080").is_err());
    // Non-numeric port.
    assert!(parse_port_forward("http:80").is_err());
    // Bad bind address.
    assert!(parse_port_forward("999.0.0.1:8080:80").is_err());
    // Too many fields.
    assert!(parse_port_forward("1.2.3.4:8080:80:70").is_err());
}
