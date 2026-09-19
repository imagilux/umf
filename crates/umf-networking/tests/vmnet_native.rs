//! Root-gated native integration smoke for [`VmNet`] (real netns / veth / tap /
//! nft). Needs root plus `/dev/net/tun`, so it runs in the privileged CI lane
//! and skips elsewhere; set `UMF_REQUIRE_PRIVILEGED=1` (as that lane does) to
//! turn the skip into a failure, so the lane can never silently degrade to
//! running nothing. Proves the pure-Rust plumbing (unshare + rtnetlink + tap
//! ioctl + setns) sets up and tears down without leaking host state, the
//! `iproute2`-free replacement for the old `ip netns` shell-outs.

#![allow(clippy::expect_used, clippy::unwrap_used)]
// Borrows the netns raw fd for an in-namespace `setns` check.
#![allow(unsafe_code)]

use std::os::fd::{BorrowedFd, RawFd};
use std::path::Path;
use std::process::Command;

use nix::sched::{CloneFlags, setns};
use nix::unistd::Uid;
use umf_networking::{DhcpDaemon, PortForward, VmNet};

/// A link is in the host netns iff `/sys/class/net/<name>` exists.
fn host_link_exists(name: &str) -> bool {
    Path::new(&format!("/sys/class/net/{name}")).exists()
}

/// The interface names visible in the namespace referenced by `netns_fd`,
/// read on a throwaway thread so the `setns` doesn't disturb the test's own
/// namespace.
///
/// The listing comes from `/proc/thread-self/net/dev`, **not** `/sys/class/net`.
/// sysfs's per-netns filtering is keyed on the namespace its superblock was
/// mounted in, captured at mount time — so a bare `setns` leaves an
/// already-mounted `/sys` showing the *host's* interfaces forever (this is why
/// `ip netns exec` remounts `/sys`). `/proc/thread-self/net` resolves through
/// the calling **thread's** nsproxy at open time, so it follows the `setns`
/// with no remount. `/proc/self/net` would not: it resolves to the thread-group
/// leader, which never entered the namespace.
fn links_in_netns(netns_fd: RawFd) -> Vec<String> {
    std::thread::spawn(move || {
        // SAFETY: `netns_fd` is owned by the still-live `VmNet` for the duration
        // of this call; `borrow_raw` only wraps it for the `setns`.
        let ns = unsafe { BorrowedFd::borrow_raw(netns_fd) };
        setns(ns, CloneFlags::CLONE_NEWNET).expect("setns into vm netns");
        let dev = std::fs::read_to_string("/proc/thread-self/net/dev")
            .expect("read /proc/thread-self/net/dev in vm netns");
        // Two header lines, then `  <name>: <counters…>` per interface.
        dev.lines()
            .skip(2)
            .filter_map(|line| line.split(':').next())
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect()
    })
    .join()
    .expect("netns check thread")
}

/// Best-effort nft table presence check; `None` when the `nft` binary is absent.
fn nft_table_exists(table: &str) -> Option<bool> {
    match Command::new("nft")
        .args(["list", "table", "inet", table])
        .output()
    {
        Ok(out) => Some(out.status.success()),
        Err(_) => None,
    }
}

/// Whether the caller demands the privileged tests actually run. The privileged
/// CI lane sets this so a missing prerequisite is a red build, not a silent pass.
fn privileged_required() -> bool {
    std::env::var("UMF_REQUIRE_PRIVILEGED")
        .is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
}

#[test]
fn native_vmnet_sets_up_and_tears_down_leak_free() {
    if !Uid::current().is_root() || !Path::new("/dev/net/tun").exists() {
        let why = "needs root + /dev/net/tun";
        assert!(
            !privileged_required(),
            "UMF_REQUIRE_PRIVILEGED=1 but the native_vmnet smoke cannot run: {why}"
        );
        eprintln!("skipping native_vmnet smoke: {why}");
        return;
    }

    // An id unlikely to collide with a concurrent real VM run.
    let id: u32 = 60_343;
    let host_veth = format!("vmh{id}");
    let guest_veth = format!("vmc{id}");
    let bridge = format!("umfbr{id}");
    let tap = format!("umftap{id}");
    let table = format!("umf-vmfwd-{id}");
    let forwards = [PortForward {
        bind_addr: None,
        host_port: 18080,
        guest_port: 80,
        tcp: true,
    }];

    // `DhcpDaemon::None`: this smoke validates the netns / veth / bridge / tap /
    // nft plumbing and its leak-free teardown, not DHCP — so launch no daemon.
    let net = VmNet::setup(id, &forwards, &DhcpDaemon::None)
        .expect("native VmNet::setup should succeed as root");
    assert_eq!(net.tap_name(), tap, "tap name");
    assert!(
        host_link_exists(&host_veth),
        "host veth present in host netns after setup",
    );

    // The whole guest side — veth peer, bridge, tap — must be inside the netns,
    // and the host veth must NOT be (that is what makes it the *host* end).
    let inside = links_in_netns(net.netns_raw_fd());
    for expected in [&tap, &bridge, &guest_veth] {
        assert!(
            inside.iter().any(|l| l == expected),
            "`{expected}` present in VM netns after setup (netns has {inside:?})",
        );
    }
    assert!(
        !inside.iter().any(|l| l == &host_veth),
        "host veth stays in the host netns (netns has {inside:?})",
    );

    if let Some(present) = nft_table_exists(&table) {
        assert!(present, "nft DNAT table present after setup");
    }

    drop(net);

    // Host-visible state is gone — the netns (and its bridge/tap) is reaped with
    // the dropped fd, and the host veth + nft table are explicitly removed.
    assert!(
        !host_link_exists(&host_veth),
        "host veth gone after drop (no host-state leak)",
    );
    if let Some(present) = nft_table_exists(&table) {
        assert!(!present, "nft DNAT table gone after drop");
    }
}
