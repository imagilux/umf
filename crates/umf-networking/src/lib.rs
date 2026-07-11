//! Per-container NAT'd networking for UMF build/run net namespaces.
//!
//! A container build's RUN steps execute in an isolated network namespace (set
//! up by `umf-engine`'s runtime spec). On its own that namespace has only
//! loopback, so anything that talks to the network — `apt-get`, `git clone`,
//! `curl` — fails. This crate gives that namespace compartmentalised egress:
//! a veth pair from the host into the container netns, an address/route on each
//! end, and an nftables `masquerade` rule so the container NATs out through the
//! host — without sharing the host's network namespace.
//!
//! [`rtnetlink`] (pure-Rust) drives the veth pair, addresses and routes
//! in-process over `NETLINK_ROUTE` — no `ip` shell-out. The host-side NAT
//! masquerade rule is applied via the `nft` binary (the subprocess guardrail
//! allows `nft`, same as `ip`; the only pure-Rust nftables crate, `rustables`,
//! aborts the process on send under modern `nix`). The container side is
//! configured by briefly `setns`-ing into the target netns on a dedicated
//! thread, where a fresh netlink socket lands in that namespace.
//!
//! The entry point is [`ContainerNet::setup`], called with the container init
//! process's PID after the runtime has created the netns but before the RUN
//! command execs. The returned [`ContainerNet`] is a drop guard: dropping it
//! removes the host veth (which takes its peer with it) and the nft table, so a
//! failed or finished build leaves no host network state behind.

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::AsFd;
use std::process::{Command, Stdio};

use futures::TryStreamExt;
use nix::sched::{CloneFlags, setns};
use thiserror::Error;
use tracing::{debug, warn};

/// Errors from setting up or tearing down a container's network.
#[derive(Debug, Error)]
pub enum NetError {
    /// A netlink (veth / address / route) operation failed.
    #[error("netlink: {0}")]
    Netlink(String),
    /// Programming the nftables masquerade rule failed.
    #[error("nftables: {0}")]
    Nft(String),
    /// Entering the container's network namespace failed.
    #[error("entering netns of pid {pid}: {source}")]
    Netns {
        /// The container init process PID whose netns we tried to enter.
        pid: u32,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A link expected to exist (just-created veth) wasn't found.
    #[error("link {0} not found")]
    LinkNotFound(String),
    /// Building the netlink runtime or the netns thread failed.
    #[error("runtime: {0}")]
    Runtime(String),
    /// An I/O error (opening the netns fd, writing `ip_forward`, …).
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    /// A privileged VM-network plumbing step (unshare / setns / tap ioctl)
    /// failed.
    #[error("vm-net plumbing: {0}")]
    VmNet(String),
    /// The egress-policy allow-list (`UMF_ROOTLESS_NET_ALLOW`) named an unknown
    /// address category.
    #[error("egress policy: {0}")]
    EgressPolicy(String),
    /// The host veth for a candidate subnet block already exists — another
    /// concurrent build owns it. Internal signal driving [`ContainerNet::setup`]
    /// to try the next block; never surfaced to callers.
    #[error("subnet block already in use")]
    LinkExists,
    /// Every subnet block in the `/16` pool is in use — more than 16384
    /// concurrent container builds, which is not a real workload.
    #[error("no free /30 subnet block available (>16384 concurrent builds)")]
    NoFreeSubnet,
}

/// Fallback MTU when the host egress MTU can't be determined: the classic
/// Ethernet default.
const DEFAULT_MTU: u32 = 1500;

/// Tunables for [`ContainerNet::setup`].
#[derive(Debug, Clone)]
pub struct NetOptions {
    /// MTU for both ends of the veth pair.
    pub mtu: u32,
    /// `/16` base from which a per-container `/30` is carved (`.1` host gateway,
    /// `.2` container). `10.69.0.0` by default — unlikely to collide with common
    /// host/bridge ranges.
    pub subnet_base: Ipv4Addr,
}

impl Default for NetOptions {
    fn default() -> Self {
        Self {
            // Match the veth to the host's egress link MTU so full-size packets
            // don't black-hole across the masquerade NAT on a tunnel / PPPoE
            // (~1420 / 1492) or jumbo-frame (9000) host. Falls back to the
            // Ethernet default when the host MTU can't be read.
            mtu: host_egress_mtu().unwrap_or(DEFAULT_MTU),
            subnet_base: Ipv4Addr::new(10, 69, 0, 0),
        }
    }
}

/// Best-effort host egress MTU: the MTU of the interface carrying the IPv4
/// default route. `None` on any read/parse failure (the caller falls back to
/// [`DEFAULT_MTU`]). Read from `/proc` + `/sys` rather than over rtnetlink to
/// keep it a cheap, synchronous, unit-testable lookup.
fn host_egress_mtu() -> Option<u32> {
    let iface = parse_default_route_iface(&std::fs::read_to_string("/proc/net/route").ok()?)?;
    iface_mtu(&iface)
}

/// The interface name carrying the IPv4 default route, parsed from
/// `/proc/net/route` contents: the data row whose hex Destination and Mask are
/// both all-zero. Split out from the file read so it stays unit-testable.
fn parse_default_route_iface(proc_net_route: &str) -> Option<String> {
    // Columns: Iface Destination Gateway Flags RefCnt Use Metric Mask MTU ...
    for line in proc_net_route.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 8 && f[1] == "00000000" && f[7] == "00000000" {
            return Some(f[0].to_string());
        }
    }
    None
}

/// Read `/sys/class/net/<iface>/mtu`. Rejects a non-leaf interface name
/// defensively (the name comes from the kernel, but keep the path contained)
/// and ignores implausibly small values.
fn iface_mtu(iface: &str) -> Option<u32> {
    if iface.is_empty() || iface.contains('/') || iface.contains("..") {
        return None;
    }
    std::fs::read_to_string(format!("/sys/class/net/{iface}/mtu"))
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|&m| m >= 576)
}

/// A live, NAT'd container network. Dropping it tears the network down (removes
/// the host veth and the nft masquerade table).
#[derive(Debug)]
pub struct ContainerNet {
    host_ifname: String,
    container_ip: Ipv4Addr,
    nft_table: String,
    /// The host's `net.ipv4.ip_forward` value before we enabled forwarding,
    /// restored on teardown — `None` when forwarding was already on (or
    /// unreadable) so we leave it untouched. See [`forward_restore_target`].
    prior_ip_forward: Option<String>,
}

impl ContainerNet {
    /// Set up compartmentalised NAT'd egress for the container whose init
    /// process is `pid`.
    ///
    /// Must be called after the runtime has created the container's net
    /// namespace (so `/proc/<pid>/ns/net` exists) and before the RUN command
    /// runs, so the network is live when it executes. Requires `CAP_NET_ADMIN`
    /// (UMF's container builds already run as root).
    ///
    /// # Errors
    /// [`NetError`] if any netlink / netns / nftables step fails.
    pub fn setup(pid: u32, opts: &NetOptions) -> Result<Self, NetError> {
        let base = opts.subnet_base;
        let mtu = opts.mtu;
        // Allocate a free `/30` block by *claiming its veth*: the kernel refuses
        // a duplicate interface name (EEXIST), so `veth add` is an atomic,
        // cross-process claim. Start at `pid % 16384` to spread load, then walk
        // the pool skipping blocks another build already owns — replacing the old
        // bare `pid % 16384`, which collided for PIDs differing by a multiple of
        // 16384. Each attempt runs on a dedicated OS thread (see below).
        let start = (pid % SUBNET_BLOCKS) as u16;
        for offset in 0..SUBNET_BLOCKS {
            let block = ((u32::from(start) + offset) % SUBNET_BLOCKS) as u16;
            // Run on a dedicated OS thread. umf's CLI runs under `#[tokio::main]`,
            // and calling `Runtime::block_on` from within an existing runtime's
            // worker thread panics. A freshly-spawned thread carries no runtime in
            // its thread-local context, so the netlink `block_on`s are valid no
            // matter whether the caller is sync or already inside a runtime.
            match run_off_runtime(move || setup_inner(pid, base, block, mtu)) {
                Ok(net) => return Ok(net),
                // Lost the race (or the block was taken): try the next one.
                Err(NetError::LinkExists) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(NetError::NoFreeSubnet)
    }

    /// The address assigned to the container end of the link.
    #[must_use]
    pub fn container_ip(&self) -> Ipv4Addr {
        self.container_ip
    }

    /// The host-side veth interface name.
    #[must_use]
    pub fn host_ifname(&self) -> &str {
        &self.host_ifname
    }
}

/// Number of `/30` blocks carved from the `/16` subnet pool (`65536 / 4`).
const SUBNET_BLOCKS: u32 = 16_384;

/// Body of [`ContainerNet::setup`] for one candidate `block`, run on a
/// runtime-free thread. Resource names derive from the block (not the PID) so a
/// live veth is the block's cross-process lease; `pid` is still used to enter
/// the container's netns. Returns [`NetError::LinkExists`] if the block's veth
/// is already taken, so the caller can try the next block.
fn setup_inner(pid: u32, base: Ipv4Addr, block: u16, mtu: u32) -> Result<ContainerNet, NetError> {
    let host_if = format!("umfv{block}h");
    let ctr_if = format!("umfv{block}c");
    let nft_table = format!("umf-nat-{block}");
    let plan = IpPlan::for_block(base, block);

    // Egress SSRF policy: deny host-internal destinations by default (cloud
    // metadata, RFC1918, loopback, CGNAT), re-openable via UMF_ROOTLESS_NET_ALLOW
    // — the same knob the rootless proxy uses, so both execution modes honour one
    // policy. Built first so a bad env value fails before any veth/table exists.
    let policy =
        ssrf::EgressPolicy::from_env().map_err(|e| NetError::EgressPolicy(e.to_string()))?;
    let deny_cidrs = policy.denied_v4_cidrs();

    // Host side: create the veth, address + raise the host end, and move the
    // peer into the container's netns. Async rtnetlink on a local runtime.
    let rt = current_thread_rt()?;
    rt.block_on(setup_host_side(&host_if, &ctr_if, &plan, mtu, pid))?;

    // The host veth exists now. The remaining steps configure egress; the
    // `ContainerNet` Drop guard (the only teardown) isn't live until we return
    // `Ok`, so on any error here we must roll back what we created — otherwise
    // the host veth / NAT table / ip_forward flip would leak.
    match setup_egress(pid, &ctr_if, &plan, &nft_table, mtu, &deny_cidrs) {
        Ok(prior_ip_forward) => {
            debug!(
                pid,
                host_if, container_ip = %plan.container, subnet = %plan.cidr(),
                "umf-networking: container egress up"
            );
            Ok(ContainerNet {
                host_ifname: host_if,
                container_ip: plan.container,
                nft_table,
                prior_ip_forward,
            })
        }
        Err((err, prior_ip_forward)) => {
            let _ = del_masquerade(&nft_table);
            restore_ip_forward(prior_ip_forward.as_deref());
            let _ = rt.block_on(del_link(&host_if));
            Err(err)
        }
    }
}

/// Configure the container peer, enable forwarding, and install the NAT rule.
///
/// On success returns the `ip_forward` value to restore on teardown; on failure
/// returns the error plus any `ip_forward` value already captured, so the
/// caller can roll the host state back.
fn setup_egress(
    pid: u32,
    ctr_if: &str,
    plan: &IpPlan,
    nft_table: &str,
    mtu: u32,
    deny_cidrs: &[&str],
) -> Result<Option<String>, (NetError, Option<String>)> {
    configure_container_side(pid, ctr_if, plan, mtu).map_err(|e| (e, None))?;
    let prior = enable_ipv4_forwarding().map_err(|e| (e, None))?;
    add_masquerade(nft_table, &plan.cidr(), deny_cidrs).map_err(|e| (e, prior.clone()))?;
    Ok(prior)
}

impl Drop for ContainerNet {
    fn drop(&mut self) {
        // Best-effort teardown — never panic in drop. Off a runtime-free thread
        // for the same reason as setup (the guard is often dropped on a runtime
        // worker). Remove the nft table first, then the host veth (whose
        // container-side peer is usually already gone with the exited netns).
        let host = self.host_ifname.clone();
        let table = self.nft_table.clone();
        let prior_forward = self.prior_ip_forward.clone();
        let _ = std::thread::spawn(move || {
            if let Err(e) = del_masquerade(&table) {
                warn!(table = %table, error = %e, "umf-networking: nft teardown failed");
            }
            match current_thread_rt().and_then(|rt| rt.block_on(del_link(&host))) {
                Ok(()) => {}
                Err(e) => {
                    warn!(link = %host, error = %e, "umf-networking: veth teardown failed");
                }
            }
            // Restore the host's IPv4-forwarding posture if WE turned it on —
            // but only once no other umf build's NAT table remains (our own is
            // already deleted above), so the last teardown can't disable
            // forwarding out from under a still-running concurrent build.
            restore_ip_forward(prior_forward.as_deref());
        })
        .join();
    }
}

/// Run `f` on a fresh OS thread that has no tokio runtime in its context, then
/// join it. Lets the closure use `Runtime::block_on` even when the caller is
/// already on a runtime worker. A panic in `f` becomes a [`NetError::Runtime`].
fn run_off_runtime<T, F>(f: F) -> Result<T, NetError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, NetError> + Send + 'static,
{
    std::thread::spawn(f)
        .join()
        .map_err(|_| NetError::Runtime("network setup thread panicked".to_string()))?
}

/// The addressing plan for one container: a `/30` with a host and container end.
#[derive(Debug, Clone, Copy)]
struct IpPlan {
    network: Ipv4Addr,
    host: Ipv4Addr,
    container: Ipv4Addr,
    prefix: u8,
}

impl IpPlan {
    /// Carve the `/30` at index `block` (0..[`SUBNET_BLOCKS`)) out of `base/16`.
    /// The block index is allocated collision-free by [`ContainerNet::setup`],
    /// so distinct concurrent builds always get distinct subnets.
    fn for_block(base: Ipv4Addr, block: u16) -> Self {
        let o = base.octets();
        let base16 = u32::from_be_bytes([o[0], o[1], 0, 0]);
        let net = base16 + u32::from(block) * 4;
        Self {
            network: Ipv4Addr::from(net),
            host: Ipv4Addr::from(net + 1),
            container: Ipv4Addr::from(net + 2),
            prefix: 30,
        }
    }

    /// CIDR string for the subnet, e.g. `10.69.0.4/30` — the `ip saddr` match
    /// for the masquerade rule.
    fn cidr(&self) -> String {
        format!("{}/{}", self.network, self.prefix)
    }
}

fn current_thread_rt() -> Result<tokio::runtime::Runtime, NetError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| NetError::Runtime(e.to_string()))
}

fn nl<E: std::fmt::Display>(e: E) -> NetError {
    NetError::Netlink(e.to_string())
}

/// Resolve a link index by name on `handle`.
async fn link_index(handle: &rtnetlink::Handle, name: &str) -> Result<u32, NetError> {
    let mut links = handle.link().get().match_name(name.to_string()).execute();
    match links.try_next().await.map_err(nl)? {
        Some(link) => Ok(link.header.index),
        None => Err(NetError::LinkNotFound(name.to_string())),
    }
}

/// Create the veth pair, raise + address the host end, and move the peer into
/// the container's net namespace.
async fn setup_host_side(
    host_if: &str,
    ctr_if: &str,
    plan: &IpPlan,
    mtu: u32,
    pid: u32,
) -> Result<(), NetError> {
    let (conn, handle, _) = rtnetlink::new_connection()?;
    // Detach the connection driver; it is dropped (and its socket closed exactly
    // once) when this function's runtime is torn down. Do NOT `abort()` it —
    // aborting races the `AsyncFd` close against runtime shutdown and trips
    // std's IO-safety double-close guard (SIGABRT).
    let _conn = tokio::spawn(conn);

    // The veth pair is the atomic block claim: a duplicate interface name is
    // refused by the kernel with EEXIST, which we surface as `LinkExists` so the
    // caller advances to the next block rather than failing the build.
    match handle
        .link()
        .add()
        .veth(host_if.to_string(), ctr_if.to_string())
        .execute()
        .await
    {
        Ok(()) => {}
        Err(rtnetlink::Error::NetlinkError(msg))
            if msg.to_io().raw_os_error() == Some(nix::libc::EEXIST) =>
        {
            return Err(NetError::LinkExists);
        }
        Err(e) => return Err(nl(e)),
    }

    let host_idx = link_index(&handle, host_if).await?;
    let ctr_idx = link_index(&handle, ctr_if).await?;

    handle
        .link()
        .set(host_idx)
        .mtu(mtu)
        .execute()
        .await
        .map_err(nl)?;
    handle
        .address()
        .add(host_idx, IpAddr::V4(plan.host), plan.prefix)
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

    // Hand the peer to the container's netns; it's configured from inside.
    handle
        .link()
        .set(ctr_idx)
        .setns_by_pid(pid)
        .execute()
        .await
        .map_err(nl)?;
    Ok(())
}

/// Configure the container end from *inside* the container's net namespace, on
/// a dedicated thread (so the `setns` only affects that thread).
fn configure_container_side(
    pid: u32,
    ctr_if: &str,
    plan: &IpPlan,
    mtu: u32,
) -> Result<(), NetError> {
    let ctr_if = ctr_if.to_string();
    let plan = *plan;
    let netns_path = format!("/proc/{pid}/ns/net");

    let join = std::thread::Builder::new()
        .name("umf-netns-config".to_string())
        .spawn(move || -> Result<(), NetError> {
            let ns = std::fs::File::open(&netns_path)
                .map_err(|source| NetError::Netns { pid, source })?;
            setns(ns.as_fd(), CloneFlags::CLONE_NEWNET).map_err(|errno| NetError::Netns {
                pid,
                source: std::io::Error::from(errno),
            })?;

            let rt = current_thread_rt()?;
            rt.block_on(async {
                let (conn, handle, _) = rtnetlink::new_connection()?;
                let _conn = tokio::spawn(conn);

                // Bring up loopback inside the container.
                let lo = link_index(&handle, "lo").await?;
                handle.link().set(lo).up().execute().await.map_err(nl)?;

                let idx = link_index(&handle, &ctr_if).await?;
                handle
                    .link()
                    .set(idx)
                    .mtu(mtu)
                    .execute()
                    .await
                    .map_err(nl)?;
                handle
                    .address()
                    .add(idx, IpAddr::V4(plan.container), plan.prefix)
                    .execute()
                    .await
                    .map_err(nl)?;
                handle.link().set(idx).up().execute().await.map_err(nl)?;

                // Default route via the host end.
                handle
                    .route()
                    .add()
                    .v4()
                    .destination_prefix(Ipv4Addr::UNSPECIFIED, 0)
                    .gateway(plan.host)
                    .execute()
                    .await
                    .map_err(nl)?;
                Ok::<(), NetError>(())
            })
        })
        .map_err(|e| NetError::Runtime(format!("spawning netns thread: {e}")))?;

    join.join()
        .map_err(|_| NetError::Runtime("netns config thread panicked".to_string()))?
}

/// Host sysctl that routes the container's egress out the default interface.
const IP_FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";

/// Enable IPv4 forwarding so the host routes the container's egress, returning
/// the value to restore on teardown (see [`forward_restore_target`]).
fn enable_ipv4_forwarding() -> Result<Option<String>, NetError> {
    let prior = std::fs::read_to_string(IP_FORWARD).ok();
    std::fs::write(IP_FORWARD, b"1")?;
    Ok(forward_restore_target(prior.as_deref().map(str::trim)))
}

/// Restore `ip_forward` to `prior` (the value before we enabled forwarding),
/// but only when no other umf NAT table remains — a concurrent build still needs
/// forwarding on, so the last teardown must not disable it out from under it.
/// The per-container `umf-nat-*` tables are the cross-process refcount (ours is
/// deleted before this runs). `prior` is `None` when we didn't flip it (no-op).
fn restore_ip_forward(prior: Option<&str>) {
    let Some(prior) = prior else { return };
    if umf_nat_table_present() {
        debug!("umf-networking: leaving ip_forward on; another umf NAT table is active");
        return;
    }
    if let Err(e) = std::fs::write(IP_FORWARD, prior.as_bytes()) {
        warn!(error = %e, "umf-networking: ip_forward restore failed");
    }
}

/// Whether any `umf-nat-*` nftables table currently exists — i.e. another umf
/// build is mid-flight and still needs host forwarding. Best-effort: on any
/// `nft` failure it assumes one *is* present (fail toward leaving forwarding on,
/// the benign state), so a flaky `nft` never disables forwarding under a peer.
fn umf_nat_table_present() -> bool {
    let Ok(out) = Command::new("nft").args(["list", "tables"]).output() else {
        return true;
    };
    if !out.status.success() {
        return true;
    }
    String::from_utf8_lossy(&out.stdout).contains("umf-nat-")
}

/// Decide what `ip_forward` value to restore on teardown, given the value read
/// *before* we wrote "1". We only restore when WE flipped it on (prior "0"); if
/// it was already enabled ("1") or unreadable there is nothing to revert, so a
/// concurrent build that legitimately needs forwarding isn't disturbed.
fn forward_restore_target(prior: Option<&str>) -> Option<String> {
    match prior {
        Some("1") | None => None,
        Some(other) => Some(other.to_string()),
    }
}

/// Add a dedicated `inet` NAT table with a postrouting masquerade rule that
/// SNATs the container's `/30` out through whatever host interface the default
/// route picks, plus an SSRF forward-drop chain that refuses the container's
/// routed egress to host-internal ranges (cloud metadata `169.254.169.254`,
/// RFC1918, loopback, CGNAT — every category `deny_cidrs` carries). Its own
/// table (named per-container) so teardown is a single atomic `delete table`,
/// never touching the host's other nftables state.
///
/// The drop lives on the `forward` hook, so it only affects packets *routed*
/// between interfaces — the container reaching its own gateway or same-subnet
/// peer (delivered locally, not forwarded) is unaffected, and public egress is
/// accepted. `deny_cidrs` empty (operator re-allowed everything via
/// `UMF_ROOTLESS_NET_ALLOW`) omits the forward chain entirely.
///
/// Applied via `nft -f -` rather than a netlink crate: the subprocess guardrail
/// allows `nft` (same as `ip`), and the pure-Rust alternative (`rustables`)
/// double-closes its socket and aborts the process under modern `nix`.
fn add_masquerade(
    table_name: &str,
    subnet_cidr: &str,
    deny_cidrs: &[&str],
) -> Result<(), NetError> {
    nft_apply(&masquerade_ruleset(table_name, subnet_cidr, deny_cidrs))
}

/// Build the per-container NAT ruleset: a postrouting masquerade for the
/// subnet, plus (when `deny_cidrs` is non-empty) a forward-hook drop of the
/// subnet's routed egress to those host-internal destinations. Split from
/// [`add_masquerade`] so the generated rules are unit-testable without `nft`.
fn masquerade_ruleset(table_name: &str, subnet_cidr: &str, deny_cidrs: &[&str]) -> String {
    // `priority srcnat` (100) is the conventional source-NAT hook priority.
    let mut ruleset = format!(
        "table inet {table_name} {{\n\
         \x20   chain postrouting {{\n\
         \x20       type nat hook postrouting priority srcnat; policy accept;\n\
         \x20       ip saddr {subnet_cidr} masquerade\n\
         \x20   }}\n"
    );
    if !deny_cidrs.is_empty() {
        // An anonymous set of the denied destinations; a single terminal drop
        // for the container subnet reaching any of them.
        let set = deny_cidrs.join(", ");
        ruleset.push_str(&format!(
            "\x20   chain forward {{\n\
             \x20       type filter hook forward priority filter; policy accept;\n\
             \x20       ip saddr {subnet_cidr} ip daddr {{ {set} }} drop\n\
             \x20   }}\n"
        ));
    }
    ruleset.push_str("}\n");
    ruleset
}

/// Remove the NAT table created by [`add_masquerade`].
fn del_masquerade(table_name: &str) -> Result<(), NetError> {
    nft_apply(&format!("delete table inet {table_name}\n"))
}

/// Feed a ruleset to `nft -f -` over stdin and surface stderr on failure.
fn nft_apply(ruleset: &str) -> Result<(), NetError> {
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| NetError::Nft(format!("spawning nft: {e}")))?;

    child
        .stdin
        .take()
        .ok_or_else(|| NetError::Nft("nft stdin unavailable".to_string()))?
        .write_all(ruleset.as_bytes())
        .map_err(|e| NetError::Nft(format!("writing ruleset to nft: {e}")))?;

    let out = child
        .wait_with_output()
        .map_err(|e| NetError::Nft(format!("waiting on nft: {e}")))?;
    if !out.status.success() {
        return Err(NetError::Nft(format!(
            "nft exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Delete a link by name (removes its veth peer too). A no-op if it is already
/// gone — which is the common case at teardown: when the container's netns is
/// destroyed on exit, the kernel auto-removes the container-side veth and the
/// host-side peer with it, often before this runs.
async fn del_link(name: &str) -> Result<(), NetError> {
    let (conn, handle, _) = rtnetlink::new_connection()?;
    let _conn = tokio::spawn(conn);
    let idx = match link_index(&handle, name).await {
        Ok(idx) => idx,
        Err(NetError::LinkNotFound(_)) => return Ok(()),
        Err(e) => return Err(e),
    };
    if let Err(e) = handle.link().del(idx).execute().await {
        // Our explicit delete can race the kernel's auto-removal and lose
        // (ENODEV). Re-check: if the link is now gone the teardown succeeded;
        // only a link that's still present is a real failure worth surfacing.
        return match link_index(&handle, name).await {
            Err(NetError::LinkNotFound(_)) => Ok(()),
            _ => Err(nl(e)),
        };
    }
    Ok(())
}

mod owned_netns;
pub use owned_netns::OwnedNetns;
mod rootless_net;
pub use rootless_net::{EgressMode, Gateway, GatewayStats, RootlessNet, TapDevice};

pub mod ssrf;

mod vmnet;
pub use vmnet::{DhcpDaemon, PortForward, VmNet};

#[cfg(test)]
mod tests;
