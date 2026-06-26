//! Unit tests for the `uki` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

/// `ukify` wraps a (here, fake) kernel + cmdline into a PE/EFI binary;
/// the `.cmdline` section carries the command line verbatim. Gated on
/// `ukify` being installed.
#[test]
fn build_uki_produces_efi_embedding_the_cmdline() {
    if !ukify_available() {
        eprintln!("skipping build_uki test: ukify (systemd-ukify) not installed");
        return;
    }
    let tmp = TempDir::new().expect("tempdir");
    // ukify only reads the kernel as bytes to embed; a placeholder is
    // fine for asserting the wrapping mechanics.
    let vmlinuz = tmp.path().join("vmlinuz");
    std::fs::write(&vmlinuz, b"fake-kernel-image").expect("write vmlinuz");

    let cmdline = "root=/dev/vda2 rootfstype=squashfs ro init=/myapp";
    let uki = build_uki(&vmlinuz, None, cmdline, "7.0", Architecture::host()).expect("build uki");

    // A UKI is a PE binary → starts with the `MZ` DOS magic.
    assert_eq!(&uki[..2], b"MZ", "UKI must be a PE/EFI binary");
    // The cmdline is embedded verbatim in the `.cmdline` section; it
    // appears in the raw image bytes.
    let needle = cmdline.as_bytes();
    assert!(
        uki.windows(needle.len()).any(|w| w == needle),
        "UKI must embed the cmdline (init=/myapp …)"
    );
}

#[test]
fn build_uki_refuses_cross_arch() {
    // ukify embeds the host's EFI stub, so a UKI for a foreign target arch is
    // unbootable. The gate fires before `ukify_available`, so this holds whether
    // or not ukify is installed, on any CI host arch.
    let foreign = if Architecture::host() == Architecture::X86_64 {
        Architecture::Aarch64
    } else {
        Architecture::X86_64
    };
    let err = build_uki(
        Path::new("/nonexistent/vmlinuz"),
        None,
        "ro",
        "7.0",
        foreign,
    )
    .expect_err("cross-arch UKI must be refused");
    assert!(matches!(err, CompileError::CrossArchUki { .. }));
}
