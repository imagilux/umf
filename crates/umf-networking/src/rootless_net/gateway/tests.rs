//! Unit / integration tests for the `any_ip` egress gateway.
//!
//! These run **fully unprivileged**. The container's TAP is modelled by an
//! `AF_UNIX`/`SOCK_DGRAM` socketpair (whole-frame in/out, the same contract a
//! real TAP gives smoltcp): the gateway gets one end via [`TapDevice`], and a
//! second smoltcp stack (the "container") drives the other end. The host side of
//! the proxy connects to a real [`std::net::TcpListener`] on loopback. This
//! proves the core mechanism — `any_ip` terminating a connection to an
//! **arbitrary destination IP**, the **SYN-port learning** that primes the
//! matching listener on the fly, and the per-flow splice to a real host socket —
//! without needing a real netns or any privilege (which AppArmor blocks on stock
//! Ubuntu anyway).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
#![allow(unsafe_code)] // socketpair, as in the device tests

use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::os::fd::{FromRawFd, OwnedFd};
use std::thread;
use std::time::{Duration, Instant as StdInstant};

use nix::libc;
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address};

use super::*;
use crate::rootless_net::tapdev::TapDevice;
use crate::ssrf::{AddressCategory, EgressPolicy};

/// The TCP/UDP proxy tests reach a loopback echo server (their stand-in for the
/// "outside world"), so they re-allow loopback; the secure default would deny it.
fn allow_loopback() -> EgressPolicy {
    EgressPolicy::with_allowed(&[AddressCategory::Loopback])
}

fn socketpair() -> (OwnedFd, OwnedFd) {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` is a 2-element array `socketpair` fills with two valid fds.
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
    assert_eq!(rc, 0, "socketpair: {}", std::io::Error::last_os_error());
    // SAFETY: both fds are freshly returned by `socketpair` and owned here.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// Build a minimal smoltcp "container" client stack on `fd`, with a default
/// route via the gateway address so its packets to arbitrary destinations are
/// sent to the gateway's MAC. `now` is the shared real-clock the gateway also
/// uses (a consistent clock across both stacks keeps TCP timers honest).
fn client_stack(
    fd: OwnedFd,
    client_ip: Ipv4Address,
    gw_ip: Ipv4Address,
    now: Instant,
) -> (Interface, TapDevice) {
    let mut dev = TapDevice::new(fd, 1500).expect("client device");
    let mut cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress([
        0x02, 0, 0, 0, 0, 0x02,
    ])));
    cfg.random_seed = 0x1234_5678;
    let mut iface = Interface::new(cfg, &mut dev, now);
    iface.update_ip_addrs(|a| {
        let _ = a.push(IpCidr::new(IpAddress::Ipv4(client_ip), 16));
    });
    iface.routes_mut().add_default_ipv4_route(gw_ip).unwrap();
    (iface, dev)
}

#[test]
fn any_ip_gateway_learns_port_and_proxies_tcp_to_an_arbitrary_destination() {
    // A real host TCP server that upper-cases what it receives. This is the
    // "outside world" the container reaches *through the gateway*.
    let server = TcpListener::bind("127.0.0.1:0").expect("bind echo server");
    let server_addr = server.local_addr().unwrap();
    let server_port = server_addr.port();
    let srv = thread::spawn(move || {
        if let Ok((mut s, _)) = server.accept() {
            let mut buf = [0u8; 256];
            // Echo one read back, upper-cased, then close.
            if let Ok(n) = s.read(&mut buf)
                && n > 0
            {
                let up: Vec<u8> = buf[..n].iter().map(|b| b.to_ascii_uppercase()).collect();
                let _ = s.write_all(&up);
            }
        }
    });

    // The TAP, modelled by a socketpair.
    let (gw_fd, client_fd) = socketpair();

    // Gateway: NO ports configured. It must learn the destination port from the
    // container's SYN. The destination IP the container dials is arbitrary — and
    // crucially NOT the gateway's own address — to prove `any_ip`.
    let gw_dev = TapDevice::new(gw_fd, 1500).expect("gw device");
    let mut gw = Gateway::new(gw_dev, allow_loopback()).expect("gateway");
    let gw_ip = Ipv4Address::new(10, 71, 0, 1);
    assert_eq!(gw.gateway_ip().octets(), gw_ip.octets());
    assert_eq!(
        gw.stats().learned_ports,
        0,
        "no ports should be known before any SYN"
    );

    // The arbitrary destination the container connects to. The host-side proxy
    // dials the *recovered* dst:port, so we make the recovered IP loopback (where
    // the real server listens) but a DIFFERENT, arbitrary-from-smoltcp's-view
    // address than the gateway IP. 127.0.0.1 is not 10.71.0.1, so acceptance
    // still exercises `any_ip`.
    let dst_ip = Ipv4Address::new(127, 0, 0, 1);
    let client_ip = Ipv4Address::new(10, 71, 0, 2);

    // A real-clock anchor shared with the client's smoltcp time source.
    let anchor = StdInstant::now();
    let smol_now =
        || Instant::from_micros(anchor.elapsed().as_micros().min(i64::MAX as u128) as i64);

    let (mut client_if, mut client_dev) = client_stack(client_fd, client_ip, gw_ip, smol_now());

    let mut client_sockets = SocketSet::new(Vec::new());
    let ch = {
        let s = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; 8192]),
            tcp::SocketBuffer::new(vec![0u8; 8192]),
        );
        client_sockets.add(s)
    };
    // Connect to the ARBITRARY destination IP:port.
    {
        let cx = client_if.context();
        client_sockets
            .get_mut::<tcp::Socket>(ch)
            .connect(
                cx,
                (IpAddress::Ipv4(dst_ip), server_port),
                (client_ip, 49152),
            )
            .expect("client connect");
    }

    // Co-drive both stacks: poll the client (draining its fd first — the device
    // buffers, mirroring how the gateway's `step` drains before polling), run one
    // gateway turn (learn port → accept → host-connect → splice), poll the client
    // again to absorb the reply.
    let msg = b"hello via gateway";
    let deadline = StdInstant::now() + Duration::from_secs(5);
    let mut sent = false;
    let mut got = Vec::new();
    while StdInstant::now() < deadline {
        client_dev.fill_rx();
        client_if.poll(smol_now(), &mut client_dev, &mut client_sockets);
        gw.step();
        client_dev.fill_rx();
        client_if.poll(smol_now(), &mut client_dev, &mut client_sockets);

        let c = client_sockets.get_mut::<tcp::Socket>(ch);
        if c.may_send() && c.can_send() && !sent {
            c.send_slice(msg).expect("client send");
            sent = true;
        }
        if c.can_recv() {
            let _ = c.recv(|data| {
                got.extend_from_slice(data);
                (data.len(), ())
            });
        }
        if got.len() >= msg.len() {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }

    let _ = srv.join();

    // The gateway learned exactly the server's port, accepted one flow to the
    // arbitrary destination, and the bytes round-tripped through the real host
    // socket (upper-cased).
    let stats = gw.stats();
    assert!(
        stats.learned_ports >= 1,
        "gateway should have learned the destination port from the SYN: {stats:?}"
    );
    assert_eq!(
        stats.tcp_accepted, 1,
        "gateway should have accepted one flow"
    );
    assert!(stats.tcp_c2h_bytes >= 17, "container→host bytes: {stats:?}");
    assert_eq!(
        String::from_utf8_lossy(&got),
        "HELLO VIA GATEWAY",
        "client should receive the host server's upper-cased echo through the gateway",
    );
}

#[test]
fn gateway_new_starts_with_no_learned_ports() {
    // A pure construction smoke that does not need the socketpair peer: the
    // gateway comes up with `any_ip` on and nothing learned yet.
    let (gw_fd, _peer) = socketpair();
    let dev = TapDevice::new(gw_fd, 1500).expect("device");
    let gw = Gateway::new(dev, EgressPolicy::default()).expect("gateway");
    assert_eq!(gw.gateway_ip(), std::net::Ipv4Addr::new(10, 71, 0, 1));
    assert_eq!(gw.stats(), GatewayStats::default());
}

#[test]
fn any_ip_gateway_relays_udp_and_sources_the_reply_from_the_queried_server() {
    // A real host UDP echo server (the "outside world", e.g. a DNS server the
    // container queries). It upper-cases one datagram and replies.
    let server = UdpSocket::bind("127.0.0.1:0").expect("bind udp echo server");
    let server_addr = server.local_addr().unwrap();
    let server_port = server_addr.port();
    let srv = thread::spawn(move || {
        let mut buf = [0u8; 256];
        if let Ok((n, peer)) = server.recv_from(&mut buf) {
            let up: Vec<u8> = buf[..n].iter().map(|b| b.to_ascii_uppercase()).collect();
            let _ = server.send_to(&up, peer);
        }
    });

    let (gw_fd, client_fd) = socketpair();
    let gw_dev = TapDevice::new(gw_fd, 1500).expect("gw device");
    let mut gw = Gateway::new(gw_dev, allow_loopback()).expect("gateway");
    let gw_ip = Ipv4Address::new(10, 71, 0, 1);
    // Arbitrary destination IP (loopback, where the echo server listens) — not
    // the gateway's own address, so `any_ip` is genuinely exercised.
    let dst_ip = Ipv4Address::new(127, 0, 0, 1);
    let client_ip = Ipv4Address::new(10, 71, 0, 2);

    let anchor = StdInstant::now();
    let smol_now =
        || Instant::from_micros(anchor.elapsed().as_micros().min(i64::MAX as u128) as i64);

    let (mut client_if, mut client_dev) = client_stack(client_fd, client_ip, gw_ip, smol_now());

    let mut client_sockets = SocketSet::new(Vec::new());
    let uh = {
        let s = udp::Socket::new(
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0u8; 4096]),
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0u8; 4096]),
        );
        client_sockets.add(s)
    };
    client_sockets
        .get_mut::<udp::Socket>(uh)
        .bind(49152u16)
        .expect("client udp bind");
    // The arbitrary destination the client addresses.
    let dst_ep = smoltcp::wire::IpEndpoint::new(IpAddress::Ipv4(dst_ip), server_port);

    let msg = b"ping via gateway";
    let deadline = StdInstant::now() + Duration::from_secs(5);
    let mut sent = false;
    let mut got: Vec<u8> = Vec::new();
    let mut reply_src: Option<smoltcp::wire::IpEndpoint> = None;
    while StdInstant::now() < deadline {
        client_dev.fill_rx();
        client_if.poll(smol_now(), &mut client_dev, &mut client_sockets);
        gw.step();
        client_dev.fill_rx();
        client_if.poll(smol_now(), &mut client_dev, &mut client_sockets);

        let c = client_sockets.get_mut::<udp::Socket>(uh);
        if !sent && c.can_send() {
            // Send to the ARBITRARY destination IP:port.
            c.send_slice(msg, dst_ep).expect("client udp send");
            sent = true;
        }
        if let Ok((data, meta)) = c.recv() {
            got.extend_from_slice(data);
            reply_src = Some(meta.endpoint);
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }

    let _ = srv.join();

    let stats = gw.stats();
    assert_eq!(
        stats.udp_c2h, 1,
        "one datagram forwarded container→host: {stats:?}"
    );
    assert_eq!(
        stats.udp_h2c, 1,
        "one reply pumped host→container: {stats:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&got),
        "PING VIA GATEWAY",
        "client should receive the echo server's upper-cased reply"
    );
    // The crux: the reply must look like it came *from the server the client
    // queried* (127.0.0.1:server_port), not from the gateway's own address — a
    // real resolver drops a reply whose source doesn't match the query's dst.
    let src = reply_src.expect("a reply should have arrived");
    assert_eq!(
        ipaddress_to_ipaddr(src.addr),
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        "reply source IP must be the queried destination, not the gateway"
    );
    assert_eq!(
        src.port, server_port,
        "reply source port must be the queried port"
    );
}

#[test]
fn default_policy_refuses_a_host_internal_tcp_destination() {
    // A real loopback server the container will try (and must fail) to reach.
    let server = TcpListener::bind("127.0.0.1:0").expect("bind server");
    let server_port = server.local_addr().unwrap().port();
    // Accept-and-drop, so a *successful* proxy would visibly connect. With the
    // SSRF default it must never get here.
    let srv = thread::spawn(move || {
        let _ = server.set_nonblocking(true);
        let start = StdInstant::now();
        let mut accepted = false;
        while start.elapsed() < Duration::from_secs(2) {
            if server.accept().is_ok() {
                accepted = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        accepted
    });

    let (gw_fd, client_fd) = socketpair();
    let gw_dev = TapDevice::new(gw_fd, 1500).expect("gw device");
    // SECURE DEFAULT: deny all host-internal categories, including loopback.
    let mut gw = Gateway::new(gw_dev, EgressPolicy::default()).expect("gateway");
    let gw_ip = Ipv4Address::new(10, 71, 0, 1);
    let dst_ip = Ipv4Address::new(127, 0, 0, 1); // a denied (loopback) destination
    let client_ip = Ipv4Address::new(10, 71, 0, 2);

    let anchor = StdInstant::now();
    let smol_now =
        || Instant::from_micros(anchor.elapsed().as_micros().min(i64::MAX as u128) as i64);
    let (mut client_if, mut client_dev) = client_stack(client_fd, client_ip, gw_ip, smol_now());

    let mut client_sockets = SocketSet::new(Vec::new());
    let ch = {
        let s = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; 8192]),
            tcp::SocketBuffer::new(vec![0u8; 8192]),
        );
        client_sockets.add(s)
    };
    {
        let cx = client_if.context();
        client_sockets
            .get_mut::<tcp::Socket>(ch)
            .connect(
                cx,
                (IpAddress::Ipv4(dst_ip), server_port),
                (client_ip, 49152),
            )
            .expect("client connect");
    }

    let deadline = StdInstant::now() + Duration::from_secs(2);
    while StdInstant::now() < deadline {
        client_dev.fill_rx();
        client_if.poll(smol_now(), &mut client_dev, &mut client_sockets);
        gw.step();
        client_dev.fill_rx();
        client_if.poll(smol_now(), &mut client_dev, &mut client_sockets);
        thread::sleep(Duration::from_millis(2));
    }

    let server_saw_a_connection = srv.join().unwrap_or(false);
    let stats = gw.stats();
    // The SYN's port is still learned (the listener is per-port, not per-dst),
    // but the host-side connect is refused, so no flow is ever accepted and the
    // real loopback server is never reached.
    assert_eq!(
        stats.tcp_accepted, 0,
        "a denied (loopback) destination must not be proxied: {stats:?}"
    );
    assert!(
        !server_saw_a_connection,
        "the loopback server must never receive a connection through the gateway"
    );
}
