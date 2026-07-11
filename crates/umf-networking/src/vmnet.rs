//! Host-side networking plumbing for a cloud-hypervisor VM's port-forwarding.
//!
//! Cloud Hypervisor has no QEMU-style user-mode `hostfwd`; its `NetConfig` is
//! tap-based. So host port-forwarding to a CH guest is Neutron-style plumbing:
//! this crate owns the namespace + tap + L3 + NAT, and **delegates** DHCP/DNS to
//! a detached dnsmasq running inside the namespace (an operator may run their
//! own instead). umf-networking never blocks the VM spawn on DHCP — it sets the
//! plumbing up and gets out of the way.
//!
//! The path is **pure-Rust, no `iproute2`**: the network namespace is created
//! with `unshare(CLONE_NEWNET)` and held by an owning fd (no `/run/netns` name);
//! the veth / bridge / addresses are programmed over `rtnetlink`; the tap is
//! created with a `TUNSETIFF` ioctl; and cloud-hypervisor / dnsmasq are launched
//! into the namespace by `setns`-ing the forked child before exec (the native
//! equivalent of `ip netns exec`).
//!
//! Topology, per VM (`id` carves a `/29` out of `10.70.0.0/16`, disjoint from
//! the container egress `10.69.0.0/16`):
//!
//! ```text
//!   host netns                         VM netns (held by an fd, no name)
//!   ┌─────────────┐  veth   ┌───────────────────────────────────────┐
//!   │ vmh{id}     ├─────────┤ vmc{id} ── umfbr{id} (bridge, .1) ──┐  │
//!   │  .6/29      │         │                                    │  │
//!   │  (gateway)  │         │                          umftap{id}┘  │  ← CH attaches here
//!   └──────┬──────┘         │   dnsmasq (detached): leases .2,      │
//!          │                │   gateway .6, on the bridge .1        │
//!     ip_forward + nft      └───────────────────────────────────────┘
//!     DNAT host:hp → .2:gp
//! ```
//!
//! The guest's gateway is the **host veth** (`.6`), not the bridge (`.1`), so the
//! guest's replies to a DNAT'd connection traverse the host's conntrack and get
//! un-DNAT'd. Dropping [`VmNet`] tears the whole thing down: kill dnsmasq, delete
//! the nft table, delete the host veth (its peer goes with it), restore
//! `ip_forward`, and drop the netns fd (the namespace is reaped once the guest
//! has also exited).

// The VM-net plumbing needs a handful of irreducibly-unsafe operations the
// workspace otherwise bans: the tap `TUNSETIFF` ioctl, borrowing the netns raw
// fd for `setns`, and a `pre_exec` hook. Each `unsafe` block carries a `SAFETY`
// justification; the container path (`lib.rs`) stays unsafe-free.
#![allow(unsafe_code)]

use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use nix::libc;
use nix::sched::{CloneFlags, setns, unshare};
use tracing::{debug, warn};

use crate::{
    IP_FORWARD, NetError, current_thread_rt, del_link, enable_ipv4_forwarding, link_index,
    nft_apply, nl, run_off_runtime,
};

/// A host:guest port mapping to install as a DNAT rule. The integration layer
/// (`umf run`) maps `umf-vmm`'s `PortForward` onto this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortForward {
    /// Host address the forward is scoped to. `None` publishes on **all** host
    /// interfaces (the historical behaviour); `Some(addr)` restricts the DNAT to
    /// traffic destined for `addr`, matching `docker -p 127.0.0.1:8080:80`.
    pub bind_addr: Option<Ipv4Addr>,
    /// Port the host listens on.
    pub host_port: u16,
    /// Port inside the guest the traffic is DNAT'd to.
    pub guest_port: u16,
    /// `true` for TCP, `false` for UDP.
    pub tcp: bool,
}

/// The `/29` addressing plan for one VM network. Offsets within the block:
/// `.1` bridge (in the netns, what dnsmasq binds to), `.2` guest (leased),
/// `.6` host veth (the guest's gateway).
#[derive(Debug, Clone, Copy)]
pub(crate) struct VmIpPlan {
    block: u32,
}

impl VmIpPlan {
    /// `/29` netmask in dotted form (`/29` = 8 addresses).
    pub(crate) const MASK: &'static str = "255.255.255.248";
    /// Prefix length of the per-VM block.
    pub(crate) const PREFIX: u8 = 29;

    /// Carve a `/29` out of `10.70.0.0/16`, indexed by `id` so concurrent VMs
    /// get distinct subnets (`id % 8192` keeps it inside the `/16`). The base is
    /// deliberately separate from the container egress range (`10.69.0.0/16`).
    pub(crate) fn for_id(id: u32) -> Self {
        let base16 = u32::from_be_bytes([10, 70, 0, 0]);
        Self {
            block: base16 + (id % 8192) * 8,
        }
    }

    fn addr(self, offset: u32) -> Ipv4Addr {
        Ipv4Addr::from(self.block + offset)
    }

    pub(crate) fn bridge(self) -> Ipv4Addr {
        self.addr(1)
    }
    pub(crate) fn guest(self) -> Ipv4Addr {
        self.addr(2)
    }
    pub(crate) fn host_veth(self) -> Ipv4Addr {
        self.addr(6)
    }
}

/// The nft DNAT + forward ruleset for the VM's port-forwards: a `prerouting`
/// dstnat chain (one `dnat` rule per forward, host port → the guest `.2`) and a
/// `forward` chain accepting traffic to/from the guest. Its own table, so
/// teardown is one atomic `delete table`.
pub(crate) fn vmfwd_ruleset(table: &str, plan: VmIpPlan, forwards: &[PortForward]) -> String {
    let guest = plan.guest();
    let dnat: String = forwards
        .iter()
        .map(|pf| {
            let proto = if pf.tcp { "tcp" } else { "udp" };
            // Scope to the bind address when the operator gave one, so the
            // forward isn't reachable via every host interface (least surprise
            // vs `docker -p 127.0.0.1:8080:80`). `None` ⇒ any destination.
            let daddr = match pf.bind_addr {
                Some(addr) => format!("ip daddr {addr} "),
                None => String::new(),
            };
            format!(
                "    {daddr}{proto} dport {hp} dnat ip to {guest}:{gp}\n",
                hp = pf.host_port,
                gp = pf.guest_port,
            )
        })
        .collect();
    format!(
        "table inet {table} {{\n\
         \x20 chain prerouting {{\n\
         \x20   type nat hook prerouting priority dstnat; policy accept;\n\
         {dnat}\
         \x20 }}\n\
         \x20 chain forward {{\n\
         \x20   type filter hook forward priority filter; policy accept;\n\
         \x20   ip daddr {guest} accept\n\
         \x20   ip saddr {guest} accept\n\
         \x20 }}\n\
         }}\n"
    )
}

/// dnsmasq argv (without the leading launcher) to serve the single guest lease
/// on the bridge. `--keep-in-foreground` keeps it our tracked child (killed on
/// teardown) rather than daemonizing; the router option points the guest at the
/// host veth gateway (`.6`).
pub(crate) fn dnsmasq_args(plan: VmIpPlan, bridge_if: &str, leasefile: &str) -> Vec<String> {
    let guest = plan.guest();
    vec![
        "--keep-in-foreground".to_string(),
        "--bind-interfaces".to_string(),
        format!("--interface={bridge_if}"),
        format!("--listen-address={}", plan.bridge()),
        "--except-interface=lo".to_string(),
        "--no-resolv".to_string(),
        "--no-hosts".to_string(),
        format!(
            "--dhcp-range={guest},{guest},{mask},1h",
            mask = VmIpPlan::MASK
        ),
        format!("--dhcp-option=option:router,{}", plan.host_veth()),
        format!("--dhcp-leasefile={leasefile}"),
        "--pid-file=".to_string(),
    ]
}

/// Which DHCP daemon `umf-networking` launches inside the VM's network
/// namespace to lease the guest. The crate creates the namespace, launches the
/// daemon into it (setns'd, detached), then gets out of the way; it never
/// blocks the VM spawn on DHCP.
#[derive(Debug, Clone, Default)]
pub enum DhcpDaemon {
    /// The default: `dnsmasq` with UMF-generated args (lease the single guest
    /// on the bridge, with the host veth as the guest's gateway).
    #[default]
    Dnsmasq,
    /// An operator-supplied command, launched setns'd into the namespace.
    /// `argv[0]` is the program; it runs with the bridge already up at `.1/29`
    /// and owns its own configuration. An empty argv behaves like [`Self::None`].
    Custom(Vec<String>),
    /// Launch nothing; the operator runs their own DHCP in the namespace, or
    /// the guest uses a static address.
    None,
}

/// A live VM port-forward network. Dropping it tears everything down.
#[derive(Debug)]
pub struct VmNet {
    tap: String,
    host_veth: String,
    nft_table: String,
    guest_ip: Ipv4Addr,
    /// The DHCP daemon child (dnsmasq by default, or a `--dhcp-command`
    /// daemon) when one was launched; killed on teardown.
    dhcp_child: Option<Child>,
    prior_ip_forward: Option<String>,
    /// Owning fd to the VM's (unnamed) network namespace. Keeps the namespace
    /// alive with no process in it and is what cloud-hypervisor / dnsmasq
    /// `setns` into. Dropped last on teardown, releasing the namespace (the
    /// kernel reaps it once the guest has also exited).
    netns: OwnedFd,
}

impl VmNet {
    /// Set up the netns + tap + bridge + veth + nft DNAT for `id`, and launch
    /// `dhcp` inside the namespace to lease the guest. Pure-Rust (`rtnetlink` +
    /// `setns` + tap ioctl), no `iproute2`. Requires `CAP_NET_ADMIN` (VM spawn
    /// already runs privileged). The returned guard owns teardown.
    ///
    /// # Errors
    /// [`NetError`] if any plumbing step fails (the partial state is rolled back).
    pub fn setup(
        id: u32,
        port_forwards: &[PortForward],
        dhcp: &DhcpDaemon,
    ) -> Result<Self, NetError> {
        // Own the inputs and run the whole setup on a runtime-free thread: the
        // rtnetlink steps below build a `current_thread` runtime, which panics
        // if started from within the caller's `#[tokio::main]` worker. (Same
        // rationale as `ContainerNet::setup`.)
        let forwards = port_forwards.to_vec();
        let dhcp = dhcp.clone();
        run_off_runtime(move || setup_inner(id, &forwards, &dhcp))
    }

    /// Name of the tap device cloud-hypervisor should attach (`NetConfig.tap`).
    #[must_use]
    pub fn tap_name(&self) -> &str {
        &self.tap
    }

    /// Raw fd of the VM's network namespace. The caller passes this to the VMM
    /// backend, which `setns`-es the forked VMM child into it before exec. The
    /// fd is owned by this `VmNet` and stays valid for as long as the guard is
    /// alive (which the caller holds across the VM's lifetime).
    #[must_use]
    pub fn netns_raw_fd(&self) -> RawFd {
        self.netns.as_raw_fd()
    }

    /// The address the guest is leased (the DNAT target).
    #[must_use]
    pub fn guest_ip(&self) -> Ipv4Addr {
        self.guest_ip
    }
}

/// Body of [`VmNet::setup`], run on a runtime-free thread.
fn setup_inner(
    id: u32,
    port_forwards: &[PortForward],
    dhcp: &DhcpDaemon,
) -> Result<VmNet, NetError> {
    let plan = VmIpPlan::for_id(id);
    let host_veth = format!("vmh{id}");
    let ctr_veth = format!("vmc{id}");
    let bridge = format!("umfbr{id}");
    let tap = format!("umftap{id}");
    let nft_table = format!("umf-vmfwd-{id}");

    // Toggle ip_forward up front so the error-path rollback can restore it even
    // when a later plumbing step fails.
    let prior_forward = enable_ipv4_forwarding()?;

    // Create the namespace; the owning fd is the only handle that keeps it
    // alive (no name). If this is the only thing built so far, restore
    // ip_forward and bail.
    let netns = match create_netns() {
        Ok(fd) => fd,
        Err(e) => {
            restore_ip_forward(prior_forward.as_deref());
            return Err(e);
        }
    };

    let built = (|| -> Result<(), NetError> {
        // Host side: veth pair, address + raise the host end, move the peer
        // into the VM netns by fd. rtnetlink on a current-thread runtime.
        let rt = current_thread_rt()?;
        rt.block_on(setup_host_side(
            &host_veth,
            &ctr_veth,
            plan,
            netns.as_raw_fd(),
        ))?;
        // Netns side: bridge + tap, enslave the peer + tap, address + raise.
        configure_netns_side(netns.as_raw_fd(), &ctr_veth, &bridge, &tap, plan)?;
        // Host: the DNAT/forward ruleset.
        nft_apply(&vmfwd_ruleset(&nft_table, plan, port_forwards))?;
        Ok(())
    })();

    if let Err(err) = built {
        teardown(&host_veth, &nft_table, None, prior_forward.as_deref());
        // `netns` (OwnedFd) drops here, releasing the namespace.
        return Err(err);
    }

    // DHCP: detached + best-effort. If the daemon is absent (or `none`) the
    // operator is expected to run their own DHCP in the namespace (delegated,
    // not blocking); we do not fail the setup over it.
    let dhcp_child = spawn_dhcp(id, netns.as_raw_fd(), plan, &bridge, dhcp);

    debug!(tap = %tap, guest = %plan.guest(), "umf-networking: VM port-forward net up");
    Ok(VmNet {
        tap,
        host_veth,
        nft_table,
        guest_ip: plan.guest(),
        dhcp_child,
        prior_ip_forward: prior_forward,
        netns,
    })
}

impl Drop for VmNet {
    fn drop(&mut self) {
        // Best-effort, off a runtime-free thread (the guard is often dropped on
        // a runtime worker, and `del_link` builds a runtime). The netns
        // `OwnedFd` stays on `self` and drops after the thread joins.
        let host_veth = self.host_veth.clone();
        let table = self.nft_table.clone();
        let prior = self.prior_ip_forward.clone();
        let mut dhcp_child = self.dhcp_child.take();
        let _ = std::thread::spawn(move || {
            teardown(&host_veth, &table, dhcp_child.as_mut(), prior.as_deref());
        })
        .join();
        // self.netns (OwnedFd) drops here, releasing the namespace.
    }
}

/// Create a fresh network namespace and return an owning fd to it.
///
/// `unshare(CLONE_NEWNET)` moves the **calling thread** into the new namespace,
/// so we run it on a dedicated short-lived thread (network-namespace membership
/// is per-task) and open the namespace there. We read `/proc/thread-self/ns/net`,
/// **not** `/proc/self/ns/net`: `/proc/self` resolves to the thread-group leader
/// (the main thread, which did not unshare), whereas `/proc/thread-self` is the
/// calling thread, the one now in the new namespace. The returned fd outlives the
/// thread and keeps the namespace alive with no process in it and no
/// `/run/netns/<name>` bind-mount — the namespace is reaped when the fd (and any
/// process that `setns`-ed into it) is gone.
fn create_netns() -> Result<OwnedFd, NetError> {
    std::thread::Builder::new()
        .name("umf-vmnet-unshare".to_string())
        .spawn(|| -> Result<OwnedFd, NetError> {
            unshare(CloneFlags::CLONE_NEWNET)
                .map_err(|e| NetError::VmNet(format!("unshare(CLONE_NEWNET): {e}")))?;
            // This thread is now in the new netns; `/proc/thread-self` (the
            // calling thread) refers to it. `File` opens O_RDONLY|O_CLOEXEC, so
            // it isn't inherited by later execs.
            let f = std::fs::File::open("/proc/thread-self/ns/net")
                .map_err(|e| NetError::VmNet(format!("open /proc/thread-self/ns/net: {e}")))?;
            Ok(OwnedFd::from(f))
        })
        .map_err(|e| NetError::Runtime(format!("spawning unshare thread: {e}")))?
        .join()
        .map_err(|_| NetError::Runtime("netns unshare thread panicked".to_string()))?
}

/// Create the veth pair in the host netns, address + raise the host end, and
/// move the peer into the VM netns by fd. Mirrors `ContainerNet`'s host side but
/// hands the peer to a netns referenced by fd rather than by pid.
async fn setup_host_side(
    host_veth: &str,
    ctr_veth: &str,
    plan: VmIpPlan,
    netns_fd: RawFd,
) -> Result<(), NetError> {
    let (conn, handle, _) = rtnetlink::new_connection()?;
    // Detach the connection driver; it is dropped (socket closed exactly once)
    // when this runtime is torn down. Do NOT `abort()` it — that races the
    // `AsyncFd` close against runtime shutdown and trips std's double-close
    // guard (SIGABRT).
    let _conn = tokio::spawn(conn);

    handle
        .link()
        .add()
        .veth(host_veth.to_string(), ctr_veth.to_string())
        .execute()
        .await
        .map_err(nl)?;

    let host_idx = link_index(&handle, host_veth).await?;
    let ctr_idx = link_index(&handle, ctr_veth).await?;

    handle
        .address()
        .add(host_idx, IpAddr::V4(plan.host_veth()), VmIpPlan::PREFIX)
        .execute()
        .await
        .map_err(nl)?;
    handle
        .link()
        .set(host_idx)
        .up()
        .execute()
        .await
        .map_err(nl)?;

    // Hand the peer to the VM netns (by fd); it's configured from inside.
    handle
        .link()
        .set(ctr_idx)
        .setns_by_fd(netns_fd)
        .execute()
        .await
        .map_err(nl)?;
    Ok(())
}

/// Configure the VM side from *inside* the VM netns, on a dedicated thread (so
/// the `setns` only affects that thread — the caller stays in the host netns for
/// the subsequent nft / teardown steps). Creates the bridge + tap, enslaves the
/// veth peer + tap to the bridge, addresses the bridge, and raises everything.
fn configure_netns_side(
    netns_fd: RawFd,
    ctr_veth: &str,
    bridge: &str,
    tap: &str,
    plan: VmIpPlan,
) -> Result<(), NetError> {
    let ctr_veth = ctr_veth.to_string();
    let bridge = bridge.to_string();
    let tap = tap.to_string();

    std::thread::Builder::new()
        .name("umf-vmnet-config".to_string())
        .spawn(move || -> Result<(), NetError> {
            // SAFETY: `netns_fd` is owned by the caller's `VmNet` for the whole
            // of this synchronous call (we join this thread before returning),
            // so it is valid; `borrow_raw` only wraps it for the `setns` call.
            let ns = unsafe { BorrowedFd::borrow_raw(netns_fd) };
            setns(ns, CloneFlags::CLONE_NEWNET)
                .map_err(|e| NetError::VmNet(format!("setns(vm netns): {e}")))?;

            // Create the tap *in this netns* (we're setns'd in) via ioctl, made
            // persistent so it outlives this thread for cloud-hypervisor to open.
            create_persistent_tap(&tap)?;

            let rt = current_thread_rt()?;
            rt.block_on(async {
                let (conn, handle, _) = rtnetlink::new_connection()?;
                let _conn = tokio::spawn(conn);

                handle
                    .link()
                    .add()
                    .bridge(bridge.clone())
                    .execute()
                    .await
                    .map_err(nl)?;
                let br_idx = link_index(&handle, &bridge).await?;

                // Enslave the veth peer + tap to the bridge.
                let ctr_idx = link_index(&handle, &ctr_veth).await?;
                let tap_idx = link_index(&handle, &tap).await?;
                handle
                    .link()
                    .set(ctr_idx)
                    .controller(br_idx)
                    .execute()
                    .await
                    .map_err(nl)?;
                handle
                    .link()
                    .set(tap_idx)
                    .controller(br_idx)
                    .execute()
                    .await
                    .map_err(nl)?;

                // Address the bridge (.1/29) — what dnsmasq binds to.
                handle
                    .address()
                    .add(br_idx, IpAddr::V4(plan.bridge()), VmIpPlan::PREFIX)
                    .execute()
                    .await
                    .map_err(nl)?;

                // Raise loopback, the peer, the tap, and the bridge.
                let lo = link_index(&handle, "lo").await?;
                for idx in [lo, ctr_idx, tap_idx, br_idx] {
                    handle.link().set(idx).up().execute().await.map_err(nl)?;
                }
                Ok::<(), NetError>(())
            })
        })
        .map_err(|e| NetError::Runtime(format!("spawning netns config thread: {e}")))?
        .join()
        .map_err(|_| NetError::Runtime("vmnet config thread panicked".to_string()))?
}

// TUN/TAP ioctl request codes — stable kernel ABI, not exported by `libc`.
// `_IOW('T', 202, int)` / `_IOW('T', 203, int)`. Typed `libc::Ioctl` so the
// literal fits whichever width the target uses (`c_ulong` on gnu, `c_int` on
// musl).
const TUNSETIFF: libc::Ioctl = 0x4004_54ca;
const TUNSETPERSIST: libc::Ioctl = 0x4004_54cb;

/// Create a persistent TAP device named `name` in the **current** network
/// namespace via the `/dev/net/tun` clone device. `TUNSETPERSIST` makes it
/// outlive the control fd so cloud-hypervisor can open it by name after we've
/// left the netns-config thread. Must run on the thread that is `setns`'d into
/// the target namespace.
fn create_persistent_tap(name: &str) -> Result<(), NetError> {
    let tun = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/net/tun")
        .map_err(|e| NetError::VmNet(format!("open /dev/net/tun: {e}")))?;
    let fd = tun.as_raw_fd();

    let name_bytes = name.as_bytes();
    if name_bytes.len() >= libc::IFNAMSIZ {
        return Err(NetError::VmNet(format!("tap name too long: {name}")));
    }

    // SAFETY: `ifreq` is a plain-old-data C struct; all-zero is a valid
    // initial value (empty name, zero flags) we then fill in.
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    for (i, b) in name_bytes.iter().enumerate() {
        ifr.ifr_name[i] = *b as libc::c_char;
    }
    // Writing (not reading) a union field is safe; this sets the `ifru_flags`
    // arm per the documented TUNSETIFF contract (interface flags).
    ifr.ifr_ifru.ifru_flags = (libc::IFF_TAP | libc::IFF_NO_PI) as libc::c_short;

    // SAFETY: `fd` is a valid, open fd to `/dev/net/tun`; `&mut ifr` points to a
    // correctly-initialised `ifreq`. TUNSETIFF reads the name + flags and binds
    // the fd to a new tap of that name.
    let rc = unsafe { libc::ioctl(fd, TUNSETIFF, &mut ifr) };
    if rc < 0 {
        return Err(NetError::VmNet(format!(
            "TUNSETIFF {name}: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: `fd` is the tap control fd just bound above; TUNSETPERSIST takes
    // an int (1 = persist) and detaches the device's lifetime from the fd.
    let rc = unsafe { libc::ioctl(fd, TUNSETPERSIST, 1 as libc::c_int) };
    if rc < 0 {
        return Err(NetError::VmNet(format!(
            "TUNSETPERSIST {name}: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Restore the host `ip_forward` value (only when we flipped it). Used by the
/// early `create_netns` failure path, before any teardown is warranted.
fn restore_ip_forward(prior_forward: Option<&str>) {
    if let Some(prior) = prior_forward
        && let Err(e) = std::fs::write(IP_FORWARD, prior.as_bytes())
    {
        warn!(error = %e, "umf-networking: ip_forward restore failed");
    }
}

/// Tear down whatever plumbing exists, best-effort + logged. Deleting the host
/// veth removes its in-netns peer with it; the bridge + tap live in the netns,
/// which the kernel reaps when the [`VmNet`]'s fd drops and the guest exits.
fn teardown(
    host_veth: &str,
    table: &str,
    dnsmasq: Option<&mut Child>,
    prior_forward: Option<&str>,
) {
    if let Some(child) = dnsmasq {
        let _ = child.kill();
        let _ = child.wait();
    }
    if let Err(e) = nft_apply(&format!("delete table inet {table}\n")) {
        warn!(table = %table, error = %e, "umf-networking: nft teardown failed");
    }
    // Delete the host veth (its netns peer goes with it). rtnetlink needs a
    // runtime; we're already on a runtime-free thread (Drop / setup rollback).
    match current_thread_rt().and_then(|rt| rt.block_on(del_link(host_veth))) {
        Ok(()) => {}
        Err(e) => warn!(link = %host_veth, error = %e, "umf-networking: host veth teardown failed"),
    }
    restore_ip_forward(prior_forward);
}

/// Launch the chosen DHCP daemon detached, `setns`'d into the VM netns, to lease
/// the guest. Non-blocking: the spawn returns immediately and we hold the
/// [`Child`] only to kill it on teardown. Best-effort: a missing daemon is a
/// warning (the operator can run their own DHCP in the namespace), not an error.
/// [`DhcpDaemon::None`] (and an empty custom argv) launch nothing.
fn spawn_dhcp(
    id: u32,
    netns_fd: RawFd,
    plan: VmIpPlan,
    bridge: &str,
    dhcp: &DhcpDaemon,
) -> Option<Child> {
    let (program, args): (String, Vec<String>) = match dhcp {
        DhcpDaemon::None => return None,
        DhcpDaemon::Dnsmasq => {
            let leasefile = std::env::temp_dir().join(format!("umf-vmnet-{id}.leases"));
            (
                "dnsmasq".to_string(),
                dnsmasq_args(plan, bridge, &leasefile.to_string_lossy()),
            )
        }
        DhcpDaemon::Custom(argv) => match argv.split_first() {
            Some((prog, rest)) => (prog.clone(), rest.to_vec()),
            None => return None,
        },
    };
    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: the closure runs in the forked child between fork and exec, so it
    // must be async-signal-safe: it only calls `setns` (a bare syscall) on a
    // raw fd the parent's `VmNet` keeps open for the child's lifetime — no
    // allocation, no locks, no inherited-lock hazards.
    unsafe {
        cmd.pre_exec(move || {
            // SAFETY: `netns_fd` is valid for the child's lifetime (owned by
            // the parent's `VmNet`); `borrow_raw` wraps it for `setns`.
            let ns = BorrowedFd::borrow_raw(netns_fd);
            setns(ns, CloneFlags::CLONE_NEWNET).map_err(std::io::Error::from)?;
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(child) => {
            debug!(program = %program, "umf-networking: DHCP daemon up (detached, in netns)");
            Some(child)
        }
        Err(e) => {
            warn!(
                program = %program, error = %e,
                "umf-networking: DHCP daemon not started; install it, pass --dhcp-command, or run your own DHCP in the namespace"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests;
