//! Unit tests for the `runtime_config` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use umf_parser::parse;

fn first_stage_from(src: &str) -> umf_core::ast::Stage {
    let ast = parse(src).expect("parse");
    ast.stages.into_iter().next().expect("stage")
}

/// Seed a fake `nft` binary in the staging tree — EXPOSE now fails closed
/// unless the userland ships one, so every "happy path" test must represent an
/// image that carries the nftables package.
fn seed_nft_binary(staging: &BuildStaging) {
    let sbin = staging.path().join("usr/sbin");
    fs::create_dir_all(&sbin).expect("mkdir usr/sbin");
    fs::write(sbin.join("nft"), b"#!/bin/sh\n").expect("seed nft");
}

// HOSTNAME / LOCALE / TIMEZONE are not UMF directives (first-boot concerns
// for cloud-init / ignition), so there are no runtime_config write-paths or
// tests for them. EXPOSE remains the runtime-config directive.

#[test]
fn expose_writes_nftables_with_default_deny() {
    let stage = first_stage_from(
        "FROM imagilux/kernel-linux:7.0\n\
         EXPOSE 22/tcp\n\
         EXPOSE 443/tcp\n\
         EXPOSE 53/udp\n",
    );
    let mut staging = BuildStaging::new().expect("staging");
    seed_nft_binary(&staging);
    let report = apply_runtime_config(&stage, &mut staging).expect("apply");
    assert_eq!(report.exposed_ports, 3);

    let nft = fs::read_to_string(
        staging
            .path()
            .join("etc")
            .join("nftables.d")
            .join("umf-expose.nft"),
    )
    .unwrap();
    assert!(nft.contains("policy drop"));
    assert!(nft.contains("tcp dport 22 accept"));
    assert!(nft.contains("tcp dport 443 accept"));
    assert!(nft.contains("udp dport 53 accept"));
    assert!(nft.contains("ct state established,related accept"));

    // The drop-in is actually loaded: nftables.conf flushes + includes it
    // so the stock nftables.service applies the default-deny rules
    // rather than leaving the fragment orphaned.
    let conf = fs::read_to_string(staging.path().join("etc").join("nftables.conf")).unwrap();
    assert!(conf.contains("flush ruleset"));
    assert!(conf.contains("include \"/etc/nftables.d/*.nft\""));
}

// ENABLE / DISABLE are not UMF directives (manage units directly); there are
// no service-symlink write-paths or tests for them.

#[test]
fn entrypoint_none_errors() {
    let stage = first_stage_from("FROM imagilux/kernel-linux:7.0\nENTRYPOINT none\n");
    let mut staging = BuildStaging::new().expect("staging");
    let err = apply_runtime_config(&stage, &mut staging).unwrap_err();
    assert!(matches!(err, RuntimeConfigError::EntrypointNone));
}

#[test]
fn expose_also_enables_nftables_service() {
    let stage = first_stage_from("FROM imagilux/kernel-linux:7.0\nEXPOSE 80/tcp\n");
    let mut staging = BuildStaging::new().expect("staging");
    seed_nft_binary(&staging);
    apply_runtime_config(&stage, &mut staging).expect("apply");
    let link = staging
        .path()
        .join("etc/systemd/system/multi-user.target.wants/nftables.service");
    assert!(link.is_symlink(), "nftables service not enabled");
}

#[test]
fn expose_without_nft_binary_fails_closed() {
    // EXPOSE promises a default-deny firewall. An image that ships no `nft`
    // binary can't load the ruleset, so the build must fail rather than emit an
    // image whose firewall silently never comes up.
    let stage = first_stage_from("FROM imagilux/kernel-linux:7.0\nEXPOSE 80/tcp\n");
    let mut staging = BuildStaging::new().expect("staging");
    let err = apply_runtime_config(&stage, &mut staging).unwrap_err();
    assert!(
        matches!(err, RuntimeConfigError::ExposeUnenforceable { .. }),
        "expected ExposeUnenforceable, got {err:?}"
    );
    // Nothing half-written: no ruleset, no enable link.
    assert!(!staging.path().join("etc/nftables.conf").exists());
}

#[test]
fn expose_with_appliance_entrypoint_fails_closed() {
    // A binary/appliance ENTRYPOINT has no init system to load the ruleset, so
    // EXPOSE can't be enforced even if `nft` is present.
    let stage = first_stage_from(
        "FROM imagilux/kernel-linux:7.0\n\
         EXPOSE 80/tcp\n\
         ENTRYPOINT /myapp\n",
    );
    let mut staging = BuildStaging::new().expect("staging");
    seed_nft_binary(&staging);
    let err = apply_runtime_config(&stage, &mut staging).unwrap_err();
    assert!(
        matches!(err, RuntimeConfigError::ExposeUnenforceable { .. }),
        "expected ExposeUnenforceable, got {err:?}"
    );
}
