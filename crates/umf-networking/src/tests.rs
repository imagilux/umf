//! Unit tests for the `umf-networking` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn parse_default_route_iface_selects_zero_dest_zero_mask() {
    // The default route is the row with all-zero hex Destination *and* Mask.
    let sample = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT
eth0\t00000000\t0102A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
eth0\t0002A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0";
    assert_eq!(parse_default_route_iface(sample).as_deref(), Some("eth0"));

    // A table with no default route (no all-zero dest+mask row) yields None,
    // so the caller falls back to DEFAULT_MTU.
    let no_default = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT
wg0\t0002A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0";
    assert_eq!(parse_default_route_iface(no_default), None);
}

#[test]
fn iface_mtu_rejects_path_escaping_names() {
    // Defensive: an iface name must be a leaf under /sys/class/net.
    assert_eq!(iface_mtu("../../etc/hostname"), None);
    assert_eq!(iface_mtu("eth0/../foo"), None);
    assert_eq!(iface_mtu(""), None);
}

#[test]
fn forward_restore_target_only_reverts_when_we_enabled_it() {
    // We flipped it on (was off) -> restore "0" on teardown.
    assert_eq!(forward_restore_target(Some("0")), Some("0".to_string()));
    // Already on -> nothing to revert (don't disturb a concurrent build).
    assert_eq!(forward_restore_target(Some("1")), None);
    // Unreadable -> leave it alone.
    assert_eq!(forward_restore_target(None), None);
}

#[test]
fn ip_plan_carves_distinct_30s_per_pid() {
    let base = Ipv4Addr::new(10, 69, 0, 0);
    let a = IpPlan::for_pid(base, 1);
    let b = IpPlan::for_pid(base, 2);

    // /30 layout: .0 network, .1 host, .2 container, .3 broadcast.
    assert_eq!(a.network, Ipv4Addr::new(10, 69, 0, 4));
    assert_eq!(a.host, Ipv4Addr::new(10, 69, 0, 5));
    assert_eq!(a.container, Ipv4Addr::new(10, 69, 0, 6));
    assert_eq!(a.prefix, 30);
    assert_eq!(a.cidr(), "10.69.0.4/30");
    // Distinct pids land in distinct /30s.
    assert_ne!(a.network, b.network);
    assert_eq!(b.host, Ipv4Addr::new(10, 69, 0, 9));
}

#[test]
fn ip_plan_wraps_within_the_16_base() {
    let base = Ipv4Addr::new(10, 69, 0, 0);
    // pid 0 → block 0 (.0 network, .1 host, .2 container); host stays in 10.69/16.
    let p = IpPlan::for_pid(base, 0);
    assert_eq!(p.network, Ipv4Addr::new(10, 69, 0, 0));
    assert_eq!(p.cidr(), "10.69.0.0/30");
    assert!(p.host.octets()[0] == 10 && p.host.octets()[1] == 69);
}

#[test]
fn masquerade_ruleset_includes_ssrf_forward_drop_by_default() {
    // The default (deny-all) policy must produce a forward-hook drop covering
    // the cloud-metadata IP and every host-internal range for the subnet.
    let deny = crate::ssrf::EgressPolicy::deny_all().denied_v4_cidrs();
    let rs = masquerade_ruleset("umf-nat-1", "10.69.0.4/30", &deny);

    // Masquerade still present.
    assert!(rs.contains("ip saddr 10.69.0.4/30 masquerade"));
    // SSRF forward drop present, scoped to the subnet, terminal `drop`.
    assert!(rs.contains("hook forward"));
    assert!(rs.contains("ip saddr 10.69.0.4/30 ip daddr {"));
    assert!(rs.trim_end().ends_with("}"));
    // The cloud-metadata range (169.254.0.0/16 ⊇ 169.254.169.254) and RFC1918
    // are in the drop set.
    assert!(rs.contains("169.254.0.0/16"));
    assert!(rs.contains("10.0.0.0/8"));
    assert!(rs.contains("192.168.0.0/16"));
    assert!(rs.contains("drop"));
}

#[test]
fn masquerade_ruleset_omits_forward_chain_when_all_allowed() {
    // Operator re-allowed every category → no forward drop chain, just NAT.
    let policy =
        crate::ssrf::EgressPolicy::from_allow_list("loopback, link-local, rfc1918, ula, cgnat")
            .expect("allow-list parses");
    let deny = policy.denied_v4_cidrs();
    assert!(deny.is_empty());
    let rs = masquerade_ruleset("umf-nat-2", "10.69.0.4/30", &deny);
    assert!(rs.contains("masquerade"));
    assert!(!rs.contains("hook forward"));
    assert!(!rs.contains("drop"));
}

#[test]
fn denied_v4_cidrs_shrinks_when_a_category_is_reallowed() {
    // Re-allowing rfc1918 drops those three prefixes but keeps metadata denied.
    let policy = crate::ssrf::EgressPolicy::from_allow_list("rfc1918").expect("parses");
    let deny = policy.denied_v4_cidrs();
    assert!(deny.contains(&"169.254.0.0/16"), "metadata stays denied");
    assert!(!deny.contains(&"10.0.0.0/8"), "rfc1918 re-allowed");
}
