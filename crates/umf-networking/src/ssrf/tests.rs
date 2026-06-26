//! Unit tests for the SSRF egress policy core (pure classification + policy —
//! no DNS, no sockets).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::net::IpAddr;
use std::str::FromStr;

use super::*;

/// Parse a string into an `IpAddr` for terse test fixtures.
fn ip(s: &str) -> IpAddr {
    s.parse().expect("test address parses")
}

#[test]
fn classify_ipv4_loopback_and_unspecified() {
    assert_eq!(classify(ip("127.0.0.1")), Some(AddressCategory::Loopback));
    assert_eq!(
        classify(ip("127.255.255.254")),
        Some(AddressCategory::Loopback)
    );
    // Unspecified is folded into Loopback.
    assert_eq!(classify(ip("0.0.0.0")), Some(AddressCategory::Loopback));
}

#[test]
fn classify_ipv4_link_local_includes_the_metadata_ip() {
    assert_eq!(
        classify(ip("169.254.0.1")),
        Some(AddressCategory::LinkLocal)
    );
    // The cloud-metadata endpoint MUST classify as link-local (the headline
    // SSRF target this policy exists to deny by default).
    assert_eq!(
        classify(ip("169.254.169.254")),
        Some(AddressCategory::LinkLocal),
        "169.254.169.254 must be link-local"
    );
    assert_eq!(
        classify(ip("169.254.255.255")),
        Some(AddressCategory::LinkLocal)
    );
    // 169.255.x is just outside the /16 and is public.
    assert_eq!(classify(ip("169.255.0.1")), None);
}

#[test]
fn classify_ipv4_rfc1918_all_three_blocks() {
    assert_eq!(classify(ip("10.0.0.1")), Some(AddressCategory::Rfc1918));
    assert_eq!(
        classify(ip("10.255.255.255")),
        Some(AddressCategory::Rfc1918)
    );
    assert_eq!(classify(ip("172.16.0.1")), Some(AddressCategory::Rfc1918));
    assert_eq!(
        classify(ip("172.31.255.255")),
        Some(AddressCategory::Rfc1918)
    );
    assert_eq!(classify(ip("192.168.1.1")), Some(AddressCategory::Rfc1918));
    // Boundaries just outside 172.16/12 are public.
    assert_eq!(classify(ip("172.15.255.255")), None);
    assert_eq!(classify(ip("172.32.0.0")), None);
}

#[test]
fn classify_ipv4_cgnat() {
    assert_eq!(classify(ip("100.64.0.1")), Some(AddressCategory::Cgnat));
    assert_eq!(
        classify(ip("100.127.255.255")),
        Some(AddressCategory::Cgnat)
    );
    // 100.63 and 100.128 sit just outside 100.64.0.0/10 and are public.
    assert_eq!(classify(ip("100.63.255.255")), None);
    assert_eq!(classify(ip("100.128.0.0")), None);
}

#[test]
fn classify_ipv6_loopback_link_local_and_ula() {
    assert_eq!(classify(ip("::1")), Some(AddressCategory::Loopback));
    // Unspecified :: is folded into Loopback.
    assert_eq!(classify(ip("::")), Some(AddressCategory::Loopback));
    assert_eq!(classify(ip("fe80::1")), Some(AddressCategory::LinkLocal));
    // fe80::/10 covers febf:: too.
    assert_eq!(classify(ip("febf::1")), Some(AddressCategory::LinkLocal));
    assert_eq!(classify(ip("fc00::1")), Some(AddressCategory::UniqueLocal));
    // fc00::/7 also covers fd00::.
    assert_eq!(
        classify(ip("fd12:3456::1")),
        Some(AddressCategory::UniqueLocal)
    );
}

#[test]
fn classify_ipv4_mapped_ipv6_uses_the_embedded_v4() {
    // ::ffff:127.0.0.1 is loopback-via-v4-mapped: same host as 127.0.0.1.
    assert_eq!(
        classify(ip("::ffff:127.0.0.1")),
        Some(AddressCategory::Loopback)
    );
    // ::ffff:10.0.0.1 maps onto the RFC 1918 block.
    assert_eq!(
        classify(ip("::ffff:10.0.0.1")),
        Some(AddressCategory::Rfc1918)
    );
    // A v4-mapped public address is public.
    assert_eq!(classify(ip("::ffff:1.1.1.1")), None);
}

#[test]
fn classify_public_addresses_are_none() {
    assert_eq!(classify(ip("1.1.1.1")), None);
    assert_eq!(classify(ip("8.8.8.8")), None);
    assert_eq!(classify(ip("2606:4700::")), None);
}

#[test]
fn category_display_and_fromstr_round_trip_canonical_names() {
    for cat in AddressCategory::ALL {
        assert_eq!(AddressCategory::from_str(&cat.to_string()).unwrap(), cat);
    }
    assert_eq!(AddressCategory::Loopback.to_string(), "loopback");
    assert_eq!(AddressCategory::LinkLocal.to_string(), "link-local");
    assert_eq!(AddressCategory::Rfc1918.to_string(), "rfc1918");
    assert_eq!(AddressCategory::UniqueLocal.to_string(), "ula");
    assert_eq!(AddressCategory::Cgnat.to_string(), "cgnat");
}

#[test]
fn category_fromstr_is_case_insensitive_and_trims() {
    assert_eq!(
        AddressCategory::from_str("  RFC1918 ").unwrap(),
        AddressCategory::Rfc1918
    );
    assert_eq!(
        AddressCategory::from_str("Link-Local").unwrap(),
        AddressCategory::LinkLocal
    );
    // Hyphen-tolerant aliases.
    assert_eq!(
        AddressCategory::from_str("linklocal").unwrap(),
        AddressCategory::LinkLocal
    );
    assert_eq!(
        AddressCategory::from_str("unique-local").unwrap(),
        AddressCategory::UniqueLocal
    );
}

#[test]
fn category_fromstr_rejects_unknown_names() {
    let err = AddressCategory::from_str("public").unwrap_err();
    assert_eq!(err.token, "public");
    // The message lists the valid names so it's actionable.
    let msg = err.to_string();
    assert!(msg.contains("rfc1918"), "{msg}");
    assert!(msg.contains("cgnat"), "{msg}");
}

#[test]
fn default_policy_denies_every_category_but_allows_public() {
    let policy = EgressPolicy::default();
    for cat in AddressCategory::ALL {
        assert!(policy.denies(cat), "default must deny {cat}");
    }
    // Representative host-internal addresses are all denied.
    assert_eq!(
        policy.decision(ip("169.254.169.254")),
        EgressDecision::Deny(AddressCategory::LinkLocal)
    );
    assert_eq!(
        policy.decision(ip("10.0.0.1")),
        EgressDecision::Deny(AddressCategory::Rfc1918)
    );
    assert_eq!(
        policy.check(ip("127.0.0.1")).unwrap_err().category,
        AddressCategory::Loopback
    );
    // A public address is allowed even under deny-all.
    assert_eq!(policy.decision(ip("1.1.1.1")), EgressDecision::Allow);
    assert!(policy.check(ip("8.8.8.8")).is_ok());
}

#[test]
fn with_allowed_flips_only_the_named_category() {
    let policy = EgressPolicy::with_allowed(&[AddressCategory::Rfc1918]);
    // rfc1918 is now allowed...
    assert!(!policy.denies(AddressCategory::Rfc1918));
    assert_eq!(policy.decision(ip("10.0.0.1")), EgressDecision::Allow);
    assert!(policy.check(ip("192.168.1.1")).is_ok());
    // ...but every other category is still denied.
    assert!(policy.denies(AddressCategory::Loopback));
    assert!(policy.denies(AddressCategory::LinkLocal));
    assert_eq!(
        policy.decision(ip("169.254.169.254")),
        EgressDecision::Deny(AddressCategory::LinkLocal)
    );
}

#[test]
fn from_allow_list_parses_lists_and_tolerates_empty() {
    // Empty / whitespace-only spec is the deny-all default.
    assert_eq!(
        EgressPolicy::from_allow_list("").unwrap(),
        EgressPolicy::default()
    );
    assert_eq!(
        EgressPolicy::from_allow_list("   ").unwrap(),
        EgressPolicy::default()
    );

    // Mixed comma + whitespace separators, repeats tolerated.
    let policy = EgressPolicy::from_allow_list("rfc1918, ula  cgnat,rfc1918").unwrap();
    assert!(!policy.denies(AddressCategory::Rfc1918));
    assert!(!policy.denies(AddressCategory::UniqueLocal));
    assert!(!policy.denies(AddressCategory::Cgnat));
    assert!(policy.denies(AddressCategory::Loopback));
    assert!(policy.denies(AddressCategory::LinkLocal));

    // An unknown name in the list is a clear error.
    let err = EgressPolicy::from_allow_list("rfc1918, bogus").unwrap_err();
    assert_eq!(err.token, "bogus");
}

#[test]
fn filter_resolved_strips_denied_ips() {
    let policy = EgressPolicy::default();
    let resolved = [
        ip("1.1.1.1"),         // public -> kept
        ip("169.254.169.254"), // metadata -> stripped
        ip("10.0.0.5"),        // rfc1918 -> stripped
        ip("8.8.8.8"),         // public -> kept
        ip("::1"),             // loopback -> stripped
    ];
    assert_eq!(
        policy.filter_resolved(&resolved),
        vec![ip("1.1.1.1"), ip("8.8.8.8")]
    );

    // Re-allowing rfc1918 keeps the 10/8 address too.
    let lax = EgressPolicy::with_allowed(&[AddressCategory::Rfc1918]);
    assert_eq!(
        lax.filter_resolved(&resolved),
        vec![ip("1.1.1.1"), ip("10.0.0.5"), ip("8.8.8.8")]
    );

    // A name resolving entirely to denied addresses yields an empty vec.
    let all_denied = [ip("127.0.0.1"), ip("169.254.169.254")];
    assert!(policy.filter_resolved(&all_denied).is_empty());
}
