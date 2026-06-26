//! The userspace egress gateway: a [`smoltcp::iface::Interface`] in `any_ip`
//! (transparent-gateway) mode over a [`TapDevice`], plus a per-flow proxy that
//! re-originates the container's connections from ordinary host sockets.
//!
//! # Why `any_ip`
//!
//! A normal IP stack only accepts packets addressed to one of its own interface
//! addresses. The container, though, sends packets to *real* outside
//! destinations (`D:P`). `any_ip` flips that: smoltcp's ingress dst-address check
//! (`has_ip_addr`) returns `true` unconditionally when `any_ip` is set, so a
//! packet to **any** destination IP is accepted and dispatched to sockets rather
//! than dropped. After the TCP handshake the accepting socket's `local_endpoint`
//! is the original `D:P` the container dialed — exactly the transparent gateway
//! the design calls for (prior art: `onetun`).
//!
//! # SYN-port learning (how the port caveat is solved)
//!
//! `any_ip` makes smoltcp accept an arbitrary destination **IP**, but a smoltcp
//! TCP listener still matches on one **port** — there is no "accept any port"
//! socket. So to catch a connection to `D:P` a listener must already be in LISTEN
//! on port `P` (with `addr: None`, any address) *before* smoltcp processes the
//! SYN, or smoltcp answers with a RST.
//!
//! Rather than pre-configure a fixed port list, the gateway **learns ports from
//! the traffic**. Each poll turn it drains the tap fd into the device's frame
//! queue ([`TapDevice::fill_rx`]), **peeks** those queued frames for TCP SYNs and
//! UDP datagrams ([`peek_l4_dst`]), primes a listener (TCP) or relay (UDP) for
//! every freshly-seen destination port, and only *then* calls `iface.poll`, which
//! replays the same queued frames — now matched by a ready socket. The fd is
//! never re-read, so peek-then-process sees identical bytes. This removes any
//! hardcoded port list: HTTP, HTTPS, package mirrors on odd ports, DNS, NTP — all
//! are caught the instant the container dials them.
//!
//! # The proxy
//!
//! For each accepted container flow the gateway holds a pair: the **smoltcp
//! socket** (container side) and a **real host socket** ([`std::net::TcpStream`] /
//! [`std::net::UdpSocket`]) opened to `D:P` *in the host netns* (the gateway
//! thread lives there; the SSRF filter will gate this `connect`). Each poll
//! turn it splices: bytes smoltcp received go to the host socket, and bytes
//! readable from the host socket are pushed into smoltcp. This L4 re-origination
//! is the only unprivileged way across the container/host boundary.
//!
//! # Event loop
//!
//! One thread per RUN step. Each iteration: drain + learn + `iface.poll()` (drain
//! ingress, emit egress), service every flow's host socket, then block in
//! `poll(2)` on the tap fd plus the host fds until either side is readable or
//! smoltcp's next timer ([`poll_delay`]) is due. No busy-spin. [`Gateway::run_until`]
//! is bounded by a deadline for deterministic tests; the integrated path passes a
//! `done` predicate backed by a stop signal.
//!
//! [`poll_delay`]: smoltcp::iface::Interface::poll_delay
//! [`TapDevice::fill_rx`]: super::tapdev::TapDevice::fill_rx

// One irreducibly-unsafe call: `libc::poll` over the dynamic fd set (the tap fd
// plus the live host-socket fds). It carries a `SAFETY` justification; same
// posture as `vmnet.rs`.
#![allow(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant as StdInstant};

use nix::libc;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, HardwareAddress, IpAddress, IpCidr,
    IpEndpoint, IpListenEndpoint, IpProtocol, Ipv4Packet, TcpPacket, UdpPacket,
};
use tracing::{debug, trace, warn};

use super::tapdev::TapDevice;
use crate::NetError;
use crate::ssrf::EgressPolicy;

/// The gateway's own L3 address inside the container subnet — the container's
/// default-route next hop, and (with `any_ip`) the address smoltcp treats as
/// "us". Mirrors [`crate::ContainerNet`]'s host-end convention (`.1`). The
/// rootless plan uses `10.71.0.0/16`, disjoint from container egress
/// (`10.69.0.0/16`) and VM port-forward (`10.70.0.0/16`).
pub(crate) const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 71, 0, 1);
/// A locally-administered MAC for the gateway interface (the `0x02` local bit).
const GATEWAY_MAC: [u8; 6] = [0x02, 0x55, 0x4d, 0x46, 0x00, 0x01];
/// Per-flow ring-buffer size (each direction). 64 KiB is a reasonable window for
/// a build's `apt`/`git` traffic without over-committing memory per flow.
const FLOW_BUF: usize = 64 * 1024;
/// Scratch buffer for one splice hop.
const SPLICE_CHUNK: usize = 16 * 1024;
/// Free listeners kept waiting per learned destination port (one smoltcp
/// listener = one pending connection; a small pool absorbs concurrent dials to
/// the same port — e.g. a browser-style fan-out of parallel HTTPS connections).
const LISTENERS_PER_PORT: usize = 4;
/// Safety cap on distinct learned destination ports (TCP and UDP each). A build
/// dials a handful; this only bounds a pathological or hostile RUN step from
/// growing the socket set without limit. Ports beyond the cap are dropped with a
/// warning (their connections simply won't be proxied).
const MAX_LEARNED_PORTS: usize = 64;
/// Receive/transmit datagram capacity for a UDP relay socket.
const UDP_RELAY_SLOTS: usize = 8;
/// Per-datagram buffer for UDP (DNS replies, NTP, etc. all fit comfortably).
const UDP_DGRAM_BUF: usize = 2 * 1024;

/// One proxied TCP flow: the smoltcp socket handle (container side) and the host
/// socket re-originating it. `dst` is the original destination the container
/// dialed, recovered from the smoltcp socket's local endpoint after accept.
struct TcpFlow {
    handle: SocketHandle,
    host: TcpStream,
    dst: SocketAddr,
    /// Set once the container's half-close is seen (`may_recv` false) and the
    /// host write side is shut, so we don't repeat the shutdown.
    host_wr_shut: bool,
}

/// One in-flight UDP datagram exchange (request/response, which carries DNS, and
/// is enough for the stateless query/reply protocols a build uses). Holds the
/// host socket and the container endpoint to reply to.
struct UdpFlow {
    /// The relay socket the reply is sent back out of (keyed by dst port).
    relay: SocketHandle,
    host: UdpSocket,
    /// The container's source endpoint (where the reply goes).
    peer: IpEndpoint,
    /// The destination the container addressed (recovered from the datagram).
    dst: SocketAddr,
}

/// A learned layer-4 destination peeked out of a container frame.
struct L4Dst {
    proto: IpProtocol,
    port: u16,
}

/// The egress gateway. Owns the smoltcp interface + device + socket set and the
/// live proxied flows. Build with [`Gateway::new`], drive with
/// [`Gateway::run_until`].
pub struct Gateway {
    iface: Interface,
    device: TapDevice,
    sockets: SocketSet<'static>,
    /// Destination TCP ports learned from peeked SYNs; each is kept topped up to
    /// [`LISTENERS_PER_PORT`] free listeners by [`replenish_listeners`].
    learned_tcp_ports: BTreeSet<u16>,
    /// Free TCP listeners across all learned ports (the port is re-derived from
    /// each socket's `listen_endpoint`).
    listeners: VecDeque<SocketHandle>,
    tcp_flows: Vec<TcpFlow>,
    /// One UDP relay socket per learned destination port (bound `addr: None`).
    udp_relays: BTreeMap<u16, SocketHandle>,
    udp_flows: Vec<UdpFlow>,
    /// SSRF policy: consulted on the literal address every re-originated
    /// connection is about to dial, so a RUN step can't reach host-internal
    /// services (loopback, cloud metadata, RFC1918, …) it shouldn't.
    policy: EgressPolicy,
    stats: GatewayStats,
}

/// Observable counters, surfaced for tests and `tracing`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GatewayStats {
    /// TCP connections accepted from the container and connected host-side.
    pub tcp_accepted: u64,
    /// Bytes spliced container→host across all TCP flows.
    pub tcp_c2h_bytes: u64,
    /// Bytes spliced host→container across all TCP flows.
    pub tcp_h2c_bytes: u64,
    /// UDP datagrams forwarded container→host.
    pub udp_c2h: u64,
    /// UDP datagrams forwarded host→container.
    pub udp_h2c: u64,
    /// Distinct destination ports learned (TCP + UDP), for observability.
    pub learned_ports: u64,
}

impl Gateway {
    /// Stand up the gateway over `device`, enforcing `policy` on every
    /// re-originated connection. Configures the smoltcp interface with the
    /// gateway address, a default route via itself, and `any_ip = true`. No
    /// listeners are created up front — they are learned from the container's
    /// traffic (see the module docs).
    ///
    /// # Errors
    /// [`NetError`] if the interface route table cannot be set up.
    pub fn new(mut device: TapDevice, policy: EgressPolicy) -> Result<Self, NetError> {
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GATEWAY_MAC)));
        config.random_seed = rand_seed();
        let mut iface = Interface::new(config, &mut device, now());

        iface.update_ip_addrs(|addrs| {
            // A fresh interface has room; ignore a (theoretical) overflow.
            let _ = addrs.push(IpCidr::new(IpAddress::Ipv4(GATEWAY_IP), 16));
        });
        iface
            .routes_mut()
            .add_default_ipv4_route(GATEWAY_IP)
            .map_err(|e| NetError::Runtime(format!("adding gateway default route: {e:?}")))?;
        iface.set_any_ip(true);

        Ok(Self {
            iface,
            device,
            sockets: SocketSet::new(Vec::new()),
            learned_tcp_ports: BTreeSet::new(),
            listeners: VecDeque::new(),
            tcp_flows: Vec::new(),
            udp_relays: BTreeMap::new(),
            udp_flows: Vec::new(),
            policy,
            stats: GatewayStats::default(),
        })
    }

    /// The gateway's own address — the container's default-route next hop and
    /// (transparently, via `any_ip`) the path to whatever DNS server the
    /// container's `resolv.conf` names.
    #[must_use]
    pub fn gateway_ip(&self) -> Ipv4Addr {
        GATEWAY_IP
    }

    /// Current counters.
    #[must_use]
    pub fn stats(&self) -> GatewayStats {
        self.stats
    }

    /// Drive the gateway until `deadline`, or until `done` returns true after a
    /// poll turn. Returns the final stats. The integrated loop passes a `done`
    /// predicate backed by a stop signal in place of a near deadline.
    pub fn run_until<F: FnMut(&GatewayStats) -> bool>(
        &mut self,
        deadline: StdInstant,
        mut done: F,
    ) -> GatewayStats {
        loop {
            self.step();

            if done(&self.stats) || StdInstant::now() >= deadline {
                return self.stats;
            }

            let smol_delay = self
                .iface
                .poll_delay(now(), &self.sockets)
                .map(|d| Duration::from_micros(d.total_micros()));
            let budget = deadline.saturating_duration_since(StdInstant::now());
            // Cap at 50ms so an idle host socket (which `poll(2)` watches but
            // smoltcp's timer doesn't) is still serviced promptly, and the
            // deadline is re-checked.
            let wait = [smol_delay, Some(budget), Some(Duration::from_millis(50))]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(Duration::from_millis(50));
            self.wait_readable(wait);
        }
    }

    /// One poll turn without the readiness wait: drain the tap fd, learn ports
    /// and prime sockets for them, let smoltcp process ingress and emit egress,
    /// then promote/splice/reap flows. The loop in [`run_until`](Self::run_until)
    /// is `step` + a `poll(2)` wait; tests co-drive a client stack and call
    /// `step` directly between client turns.
    pub(crate) fn step(&mut self) {
        // Drain the fd into the device's frame queue, then learn destination
        // ports from those queued frames and prime a listener/relay for each
        // *before* smoltcp processes them — otherwise a SYN to an unlistened port
        // gets a RST. `poll` below replays the same queued frames.
        self.device.fill_rx();
        self.learn_ports();
        self.replenish_listeners();

        self.iface.poll(now(), &mut self.device, &mut self.sockets);
        self.service_tcp();
        self.service_udp();
    }

    /// Peek the frames the device buffered this turn and register a listener
    /// (TCP) or relay (UDP) for every freshly-seen destination port.
    fn learn_ports(&mut self) {
        // Collect first (immutable borrow of the device), then mutate — keeps the
        // borrow checker happy and bounds work to the current burst.
        let mut new_tcp: Vec<u16> = Vec::new();
        let mut new_udp: Vec<u16> = Vec::new();
        for frame in self.device.queued_frames() {
            let Some(l4) = peek_l4_dst(frame) else {
                continue;
            };
            match l4.proto {
                IpProtocol::Tcp
                    if !self.learned_tcp_ports.contains(&l4.port)
                        && !new_tcp.contains(&l4.port) =>
                {
                    new_tcp.push(l4.port);
                }
                IpProtocol::Udp
                    if !self.udp_relays.contains_key(&l4.port) && !new_udp.contains(&l4.port) =>
                {
                    new_udp.push(l4.port);
                }
                _ => {}
            }
        }

        for port in new_tcp {
            if self.learned_tcp_ports.len() >= MAX_LEARNED_PORTS {
                warn!(
                    port,
                    cap = MAX_LEARNED_PORTS,
                    "rootless gw: learned-TCP-port cap reached; not proxying this port"
                );
                break;
            }
            debug!(port, "rootless gw: learned TCP destination port");
            self.learned_tcp_ports.insert(port);
            self.stats.learned_ports += 1;
        }
        for port in new_udp {
            if self.udp_relays.len() >= MAX_LEARNED_PORTS {
                warn!(
                    port,
                    cap = MAX_LEARNED_PORTS,
                    "rootless gw: learned-UDP-port cap reached; not relaying this port"
                );
                break;
            }
            let handle = add_udp_relay(&mut self.sockets, port);
            debug!(port, "rootless gw: learned UDP destination port");
            self.udp_relays.insert(port, handle);
            self.stats.learned_ports += 1;
        }
    }

    /// Promote accepted listeners into flows (opening the host connection), then
    /// splice both directions for every live flow and reap closed ones.
    fn service_tcp(&mut self) {
        // Find listeners that have caught a connection. A socket in Established is
        // active; its `local_endpoint` is then the destination the container
        // dialed.
        let mut promoted = Vec::new();
        for (slot, &handle) in self.listeners.iter().enumerate() {
            let sock = self.sockets.get::<tcp::Socket>(handle);
            // Promote only once the handshake has completed (Established), not at
            // SynReceived: a blocking host connect mid-handshake would stall the
            // poll loop and the peer's final ACK.
            if sock.state() == tcp::State::Established
                && let Some(local) = sock.local_endpoint()
            {
                promoted.push((slot, handle, local));
            }
        }
        // Remove from the listener queue high-slot-first so indices stay valid.
        promoted.sort_by_key(|p| std::cmp::Reverse(p.0));
        for (slot, handle, local) in promoted {
            self.listeners.remove(slot);
            let dst = endpoint_to_socketaddr(local);
            match open_host_tcp(dst, &self.policy) {
                Ok(host) => {
                    debug!(%dst, "rootless gw: TCP flow up (container → host)");
                    self.stats.tcp_accepted += 1;
                    self.tcp_flows.push(TcpFlow {
                        handle,
                        host,
                        dst,
                        host_wr_shut: false,
                    });
                }
                Err(e) => {
                    warn!(error = %e, %dst, "rootless gw: refusing/failing host-side TCP connect; resetting");
                    self.sockets.get_mut::<tcp::Socket>(handle).abort();
                    self.sockets.remove(handle);
                }
            }
        }

        // Splice every live flow; collect the finished ones.
        let mut reap = Vec::new();
        for (i, flow) in self.tcp_flows.iter_mut().enumerate() {
            let sock = self.sockets.get_mut::<tcp::Socket>(flow.handle);
            if let FlowProgress::Done = splice_tcp(sock, flow, &mut self.stats) {
                reap.push(i);
            }
        }
        for i in reap.into_iter().rev() {
            let flow = self.tcp_flows.remove(i);
            trace!(dst = %flow.dst, "rootless gw: TCP flow closed");
            self.sockets.remove(flow.handle);
        }
    }

    /// Forward datagrams the container sent (to any destination on a learned UDP
    /// port, caught via `any_ip`) out a fresh host socket, and pump replies back.
    /// Carries DNS and other stateless query/response UDP.
    fn service_udp(&mut self) {
        // Container → host: drain each relay's received datagrams.
        let relays: Vec<SocketHandle> = self.udp_relays.values().copied().collect();
        for relay in relays {
            loop {
                let sock = self.sockets.get_mut::<udp::Socket>(relay);
                let mut buf = [0u8; UDP_DGRAM_BUF];
                let (n, meta) = match sock.recv_slice(&mut buf) {
                    Ok(v) => v,
                    Err(_) => break, // nothing buffered
                };
                // The destination the container addressed: its IP is
                // `meta.local_address` (the dst the gateway received on, exposed
                // because `any_ip` accepted it); the port is the relay's bound
                // port.
                let dst_port = sock.endpoint().port;
                let dst_ip = meta
                    .local_address
                    .map_or(IpAddr::V4(GATEWAY_IP), ipaddress_to_ipaddr);
                let dst = SocketAddr::new(dst_ip, dst_port);
                match self.forward_udp(relay, &buf[..n], meta.endpoint, dst) {
                    Ok(()) => self.stats.udp_c2h += 1,
                    Err(e) => warn!(error = %e, %dst, "rootless gw: UDP forward failed"),
                }
            }
        }

        // Host → container: pump replies from host UDP sockets back.
        let mut reap = Vec::new();
        for (i, flow) in self.udp_flows.iter_mut().enumerate() {
            let mut buf = [0u8; UDP_DGRAM_BUF];
            match flow.host.recv(&mut buf) {
                Ok(n) if n > 0 => {
                    let sock = self.sockets.get_mut::<udp::Socket>(flow.relay);
                    // The reply must appear to come *from the server the
                    // container queried* (`flow.dst`), not from the gateway's own
                    // address — a UDP client (a DNS resolver above all) drops a
                    // reply whose source doesn't match the query's destination.
                    // `any_ip` lets us source it from that arbitrary address by
                    // setting `local_address`; the source port is the relay's
                    // bound port (= the queried port).
                    let mut meta = udp::UdpMetadata::from(flow.peer);
                    meta.local_address = Some(ipaddr_to_ipaddress(flow.dst.ip()));
                    if sock.send_slice(&buf[..n], meta).is_ok() {
                        self.stats.udp_h2c += 1;
                    }
                    reap.push(i); // request/response: one-shot
                }
                Ok(_) => reap.push(i),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => reap.push(i),
            }
        }
        for i in reap.into_iter().rev() {
            let f = self.udp_flows.remove(i);
            trace!(dst = %f.dst, "rootless gw: UDP flow done");
        }
    }

    /// Open a host UDP socket to `dst`, send `payload`, register the flow so the
    /// reply is pumped back to `peer`. Refuses (per the SSRF policy) a `dst` in a
    /// denied category — including a resolver the container points at a
    /// host-internal address.
    fn forward_udp(
        &mut self,
        relay: SocketHandle,
        payload: &[u8],
        peer: IpEndpoint,
        dst: SocketAddr,
    ) -> Result<(), NetError> {
        self.policy
            .check(dst.ip())
            .map_err(|d| NetError::Runtime(d.to_string()))?;
        let host = UdpSocket::bind(("0.0.0.0", 0)).map_err(NetError::Io)?;
        host.set_nonblocking(true).map_err(NetError::Io)?;
        host.send_to(payload, dst).map_err(NetError::Io)?;
        self.udp_flows.push(UdpFlow {
            relay,
            host,
            peer,
            dst,
        });
        Ok(())
    }

    /// Keep [`LISTENERS_PER_PORT`] free TCP listeners waiting on each learned
    /// port. Counts how many free listeners each port currently has and tops
    /// them up. Called every turn after [`learn_ports`](Self::learn_ports).
    fn replenish_listeners(&mut self) {
        let ports: Vec<u16> = self.learned_tcp_ports.iter().copied().collect();
        for port in ports {
            let have = self
                .listeners
                .iter()
                .filter(|&&h| self.sockets.get::<tcp::Socket>(h).listen_endpoint().port == port)
                .count();
            for _ in have..LISTENERS_PER_PORT {
                match add_listener(&mut self.sockets, port) {
                    Ok(h) => self.listeners.push_back(h),
                    Err(e) => {
                        warn!(error = %e, port, "rootless gw: could not add TCP listener");
                        break;
                    }
                }
            }
        }
    }

    /// Block until the tap fd or any host socket fd is readable, or `timeout`
    /// elapses. Uses `poll(2)` directly (no extra dep) over the raw fds.
    fn wait_readable(&self, timeout: Duration) {
        let mut fds: Vec<libc::pollfd> =
            Vec::with_capacity(1 + self.tcp_flows.len() + self.udp_flows.len());
        fds.push(pollfd(self.device.raw_fd()));
        for f in &self.tcp_flows {
            fds.push(pollfd(f.host.as_raw_fd()));
        }
        for f in &self.udp_flows {
            fds.push(pollfd(f.host.as_raw_fd()));
        }
        let ms: libc::c_int = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        // SAFETY: `fds` is a live, contiguous slice of `pollfd` of the length we
        // pass; `poll` only reads `events` and writes `revents` within it. A
        // negative return (EINTR) just means "re-poll", which the loop does.
        unsafe {
            libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms);
        }
    }
}

/// Outcome of splicing one TCP flow in a poll turn.
enum FlowProgress {
    Live,
    Done,
}

/// Splice one TCP flow both directions. Returns [`FlowProgress::Done`] once the
/// smoltcp socket is fully closed with nothing left to deliver.
fn splice_tcp(
    sock: &mut tcp::Socket,
    flow: &mut TcpFlow,
    stats: &mut GatewayStats,
) -> FlowProgress {
    // container → host.
    while sock.can_recv() {
        let mut chunk = [0u8; SPLICE_CHUNK];
        match sock.recv_slice(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if flow.host.write_all(&chunk[..n]).is_err() {
                    sock.abort();
                    return FlowProgress::Done;
                }
                stats.tcp_c2h_bytes += n as u64;
            }
            Err(_) => break,
        }
    }

    // host → container.
    while sock.can_send() {
        let mut chunk = [0u8; SPLICE_CHUNK];
        match flow.host.read(&mut chunk) {
            Ok(0) => {
                // Host closed its write side: half-close the container side.
                sock.close();
                break;
            }
            Ok(n) => match sock.send_slice(&chunk[..n]) {
                Ok(sent) => {
                    stats.tcp_h2c_bytes += sent as u64;
                    if sent < n {
                        // smoltcp tx ring full; the remainder is dropped here (a
                        // sized window normally prevents this). Re-queueing the
                        // tail is a future refinement.
                        break;
                    }
                }
                Err(_) => break,
            },
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => {
                sock.close();
                break;
            }
        }
    }

    // If the container stopped sending, shut the host write side so the peer
    // sees EOF (half-close propagation).
    if !sock.may_recv() && !flow.host_wr_shut {
        let _ = flow.host.shutdown(std::net::Shutdown::Write);
        flow.host_wr_shut = true;
    }

    if matches!(sock.state(), tcp::State::Closed) && !sock.can_recv() {
        return FlowProgress::Done;
    }
    FlowProgress::Live
}

/// Open the real host-side TCP connection for an accepted flow. This is the
/// re-origination point, and the SSRF choke point: `policy` is consulted on the
/// literal address about to be dialed, at connect time, every time. Because the
/// container has already resolved any hostname to this IP, validating the
/// connect IP here is inherently immune to DNS-rebinding / alias tricks — there
/// is no name-to-connect gap to exploit.
fn open_host_tcp(dst: SocketAddr, policy: &EgressPolicy) -> Result<TcpStream, NetError> {
    policy
        .check(dst.ip())
        .map_err(|d| NetError::Runtime(d.to_string()))?;
    let stream = TcpStream::connect_timeout(&dst, Duration::from_secs(10)).map_err(NetError::Io)?;
    stream.set_nonblocking(true).map_err(NetError::Io)?;
    Ok(stream)
}

/// Add a TCP socket in LISTEN on `port` for **any** destination address
/// (`addr: None`). With the interface's `any_ip` on, this catches a container
/// SYN to any `D:port`.
fn add_listener(sockets: &mut SocketSet<'static>, port: u16) -> Result<SocketHandle, NetError> {
    let rx = tcp::SocketBuffer::new(vec![0u8; FLOW_BUF]);
    let tx = tcp::SocketBuffer::new(vec![0u8; FLOW_BUF]);
    let mut sock = tcp::Socket::new(rx, tx);
    sock.listen(IpListenEndpoint { addr: None, port })
        .map_err(|e| NetError::Runtime(format!("tcp listen on :{port}: {e:?}")))?;
    Ok(sockets.add(sock))
}

/// Add a UDP relay socket bound to any address on `port`. With `any_ip`, it
/// catches a container datagram to any `D:port` and exposes the dst IP via the
/// datagram metadata's `local_address`.
fn add_udp_relay(sockets: &mut SocketSet<'static>, port: u16) -> SocketHandle {
    let rx = udp::PacketBuffer::new(
        vec![udp::PacketMetadata::EMPTY; UDP_RELAY_SLOTS],
        vec![0u8; UDP_RELAY_SLOTS * UDP_DGRAM_BUF],
    );
    let tx = udp::PacketBuffer::new(
        vec![udp::PacketMetadata::EMPTY; UDP_RELAY_SLOTS],
        vec![0u8; UDP_RELAY_SLOTS * UDP_DGRAM_BUF],
    );
    let mut sock = udp::Socket::new(rx, tx);
    // A learned port is non-zero; bind cannot fail on a fresh socket.
    let _ = sock.bind(IpListenEndpoint { addr: None, port });
    sockets.add(sock)
}

/// Peek a single Ethernet frame for an IPv4 TCP-SYN or UDP destination port,
/// without consuming or mutating it. Returns `None` for anything that is not a
/// connection-initiating SYN or a UDP datagram (existing-flow segments, ARP,
/// IPv6, malformed frames) — those need no new listener/relay. Uses smoltcp's
/// own checked wire parsers, so a short or malformed frame is rejected safely
/// rather than slicing raw bytes.
fn peek_l4_dst(frame: &[u8]) -> Option<L4Dst> {
    let eth = EthernetFrame::new_checked(frame).ok()?;
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    match ip.next_header() {
        IpProtocol::Tcp => {
            let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
            // A connection initiation is SYN set, ACK clear. A SYN-ACK (both set)
            // is the *reply* to one of our own host-side dials and must not spawn
            // a listener.
            (tcp.syn() && !tcp.ack()).then(|| L4Dst {
                proto: IpProtocol::Tcp,
                port: tcp.dst_port(),
            })
        }
        IpProtocol::Udp => {
            let udp = UdpPacket::new_checked(ip.payload()).ok()?;
            Some(L4Dst {
                proto: IpProtocol::Udp,
                port: udp.dst_port(),
            })
        }
        _ => None,
    }
}

/// smoltcp `IpAddress` → std `IpAddr`.
fn ipaddress_to_ipaddr(addr: IpAddress) -> IpAddr {
    match addr {
        IpAddress::Ipv4(v4) => IpAddr::V4(Ipv4Addr::from(v4.octets())),
        IpAddress::Ipv6(v6) => IpAddr::V6(std::net::Ipv6Addr::from(v6.octets())),
    }
}

/// std `IpAddr` → smoltcp `IpAddress` (the inverse of [`ipaddress_to_ipaddr`]),
/// used to source a UDP reply from the address the container originally
/// addressed.
fn ipaddr_to_ipaddress(ip: IpAddr) -> IpAddress {
    match ip {
        IpAddr::V4(v4) => IpAddress::Ipv4(v4),
        IpAddr::V6(v6) => IpAddress::Ipv6(v6),
    }
}

/// smoltcp `IpEndpoint` → std `SocketAddr`.
fn endpoint_to_socketaddr(ep: IpEndpoint) -> SocketAddr {
    SocketAddr::new(ipaddress_to_ipaddr(ep.addr), ep.port)
}

/// A `pollfd` watching `fd` for readability + hangup.
fn pollfd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
        revents: 0,
    }
}

/// smoltcp monotonic clock, anchored at first call (process lifetime). smoltcp
/// only needs millis monotonically increasing from an arbitrary point.
fn now() -> Instant {
    use std::sync::OnceLock;
    use std::time::Instant as StdI;
    static ANCHOR: OnceLock<StdI> = OnceLock::new();
    let anchor = ANCHOR.get_or_init(StdI::now);
    Instant::from_micros(anchor.elapsed().as_micros().min(i64::MAX as u128) as i64)
}

/// A non-cryptographic seed for smoltcp's ISN/port randomisation (the docs say
/// it need not be secure), from the wall clock.
fn rand_seed() -> u64 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15)
}

#[cfg(test)]
mod tests;
