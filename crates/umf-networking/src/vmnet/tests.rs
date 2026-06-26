//! Unit tests for the pure VM-net generators (no root / no netns needed).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn ip_plan_carves_a_disjoint_per_id_slash29() {
    let p0 = VmIpPlan::for_id(0);
    assert_eq!(p0.bridge(), Ipv4Addr::new(10, 70, 0, 1));
    assert_eq!(p0.guest(), Ipv4Addr::new(10, 70, 0, 2));
    assert_eq!(p0.host_veth(), Ipv4Addr::new(10, 70, 0, 6));

    // Each id gets its own /29 (8 addresses apart).
    let p1 = VmIpPlan::for_id(1);
    assert_eq!(p1.bridge(), Ipv4Addr::new(10, 70, 0, 9));
    assert_eq!(p1.guest(), Ipv4Addr::new(10, 70, 0, 10));
    assert_eq!(p1.host_veth(), Ipv4Addr::new(10, 70, 0, 14));

    // The VM range is the 10.70/16 base — disjoint from the container egress
    // 10.69/16, so VMs and container builds never collide.
    assert_eq!(p0.guest().octets()[0..2], [10, 70]);
    assert_eq!(VmIpPlan::PREFIX, 29);
}

#[test]
fn ip_plan_wraps_within_the_slash16() {
    // id past 8192 wraps back into the /16 (the host CIDR octets stay 10.70).
    let p = VmIpPlan::for_id(8192);
    assert_eq!(p.guest(), VmIpPlan::for_id(0).guest());
    assert_eq!(p.guest().octets()[0..2], [10, 70]);
}

#[test]
fn vmfwd_ruleset_has_dnat_per_forward_and_a_guest_forward_chain() {
    let plan = VmIpPlan::for_id(0);
    let forwards = [
        PortForward {
            host_port: 8080,
            guest_port: 80,
            tcp: true,
        },
        PortForward {
            host_port: 5353,
            guest_port: 53,
            tcp: false,
        },
    ];
    let rs = vmfwd_ruleset("umf-vmfwd-0", plan, &forwards);

    assert!(rs.contains("table inet umf-vmfwd-0"));
    assert!(rs.contains("type nat hook prerouting priority dstnat"));
    // One DNAT rule per forward, targeting the guest .2 with the right proto.
    assert!(
        rs.contains("tcp dport 8080 dnat ip to 10.70.0.2:80"),
        "{rs}"
    );
    assert!(
        rs.contains("udp dport 5353 dnat ip to 10.70.0.2:53"),
        "{rs}"
    );
    // A forward chain accepting traffic to/from the guest.
    assert!(rs.contains("type filter hook forward priority filter"));
    assert!(rs.contains("ip daddr 10.70.0.2 accept"));
    assert!(rs.contains("ip saddr 10.70.0.2 accept"));
}

#[test]
fn vmfwd_ruleset_with_no_forwards_still_emits_the_table() {
    let rs = vmfwd_ruleset("umf-vmfwd-0", VmIpPlan::for_id(0), &[]);
    assert!(rs.contains("table inet umf-vmfwd-0"));
    assert!(rs.contains("chain prerouting"));
    // No DNAT lines.
    assert!(!rs.contains("dnat ip to"));
}

#[test]
fn dnsmasq_args_serve_the_single_guest_with_the_veth_gateway() {
    let args = dnsmasq_args(VmIpPlan::for_id(0), "umfbr0", "/tmp/umf-umfvm0.leases").join(" ");
    assert!(args.contains("--keep-in-foreground"), "{args}");
    assert!(args.contains("--interface=umfbr0"), "{args}");
    assert!(args.contains("--listen-address=10.70.0.1"), "{args}");
    // Lease exactly the guest .2, with the /29 mask.
    assert!(
        args.contains("--dhcp-range=10.70.0.2,10.70.0.2,255.255.255.248,1h"),
        "{args}",
    );
    // The default gateway handed to the guest is the host veth .6 (so replies
    // traverse host conntrack to un-DNAT), not the bridge .1.
    assert!(
        args.contains("--dhcp-option=option:router,10.70.0.6"),
        "{args}"
    );
    assert!(
        args.contains("--dhcp-leasefile=/tmp/umf-umfvm0.leases"),
        "{args}"
    );
}
