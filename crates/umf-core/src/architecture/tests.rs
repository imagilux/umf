//! Unit tests for the `architecture` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn parses_oci_amd64_and_arm64() {
    assert_eq!(
        Architecture::from_platform_str("linux/amd64").unwrap(),
        Architecture::X86_64,
    );
    assert_eq!(
        Architecture::from_platform_str("linux/arm64").unwrap(),
        Architecture::Aarch64,
    );
}

#[test]
fn parses_linux_x86_64_and_aarch64() {
    assert_eq!(
        Architecture::from_platform_str("linux/x86_64").unwrap(),
        Architecture::X86_64,
    );
    assert_eq!(
        Architecture::from_platform_str("linux/aarch64").unwrap(),
        Architecture::Aarch64,
    );
}

#[test]
fn rejects_non_linux_os() {
    let err = Architecture::from_platform_str("darwin/arm64").unwrap_err();
    assert!(matches!(err.reason, PlatformParseReason::UnsupportedOs(ref s) if s == "darwin"));
}

#[test]
fn rejects_unknown_arch() {
    let err = Architecture::from_platform_str("linux/riscv64").unwrap_err();
    assert!(
        matches!(err.reason, PlatformParseReason::UnsupportedArchitecture(ref s) if s == "riscv64")
    );
}

#[test]
fn rejects_missing_separator() {
    let err = Architecture::from_platform_str("amd64").unwrap_err();
    assert!(matches!(err.reason, PlatformParseReason::MissingSeparator));
}

#[test]
fn arch_strings_round_trip() {
    for arch in [Architecture::X86_64, Architecture::Aarch64] {
        assert_eq!(
            Architecture::from_arch_str(arch.linux_arch_string()),
            Some(arch),
        );
        assert_eq!(
            Architecture::from_arch_str(arch.oci_arch_string()),
            Some(arch)
        );
    }
}

#[test]
fn arch_strings_differ_between_variants() {
    assert_ne!(
        Architecture::X86_64.qemu_binary_name(),
        Architecture::Aarch64.qemu_binary_name(),
    );
    assert_ne!(
        Architecture::X86_64.uefi_fallback_filename(),
        Architecture::Aarch64.uefi_fallback_filename(),
    );
}
