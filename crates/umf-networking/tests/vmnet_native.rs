//! Root-gated native integration smoke for [`VmNet`] (real netns / veth / tap /
//! nft). Skipped unless run as root with `/dev/net/tun` present — i.e. only in
//! the privileged CI lane. Proves the pure-Rust plumbing (unshare + rtnetlink +
//! tap ioctl + setns) sets up and tears down without leaking host state, the
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

/// Check for `name` inside the namespace referenced by `netns_fd`, on a
/// throwaway thread so the `setns` doesn't disturb the test's own namespace.
fn link_exists_in_netns(netns_fd: RawFd, name: &str) -> bool {
    let name = name.to_string();
    std::thread::spawn(move || {
        // SAFETY: `netns_fd` is owned by the still-live `VmNet` for the duration
        // of this call; `borrow_raw` only wraps it for the `setns`.
        let ns = unsafe { BorrowedFd::borrow_raw(netns_fd) };
        setns(ns, CloneFlags::CLONE_NEWNET).expect("setns into vm netns");
        Path::new(&format!("/sys/class/net/{name}")).exists()
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

#[test]
fn native_vmnet_sets_up_and_tears_down_leak_free() {
    if !Uid::current().is_root() || !Path::new("/dev/net/tun").exists() {
        eprintln!("skipping native_vmnet smoke: needs root + /dev/net/tun");
        return;
    }

    // An id unlikely to collide with a concurrent real VM run.
    let id: u32 = 60_343;
    let host_veth = format!("vmh{id}");
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
    assert!(
        link_exists_in_netns(net.netns_raw_fd(), &tap),
        "tap present in VM netns after setup",
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
