//! Native in-process rootless egress: a tap in the owned netns wired to a
//! `smoltcp` `any_ip` gateway running in the host netns.
//!
//! [`EgressMode::Native`](super::EgressMode::Native) selects this — the
//! sovereign default the umbrella targets: no external binary, works
//! air-gapped. The mechanism (the gateway itself lives in [`super::gateway`]):
//!
//! 1. Create a tap **inside the netns UMF owns** ([`OwnedNetns`]) and configure
//!    the container side: an address and a default route via the gateway.
//! 2. Keep the tap fd and run a [`Gateway`] over it on a **host-netns** thread,
//!    so the connections it re-originates leave from the host network namespace
//!    (the only unprivileged crossing of the userns→host boundary).
//! 3. The container — which joins the same namespace via the OCI spec path —
//!    sends to real destinations; the gateway terminates them with `any_ip` and
//!    proxies each to an ordinary host socket.
//!
//! DNS needs no `resolv.conf` rewrite: the engine surfaces the host's real
//! upstream nameservers into the container (preferring
//! `/run/systemd/resolve/resolv.conf`, not the systemd-resolved stub), the
//! default route sends those queries to the gateway, and `any_ip` + the UDP
//! relay forward them from the host. The one gap is a host whose *only*
//! nameserver is a loopback stub (e.g. `127.0.0.53` with no run-path file):
//! loopback is not routed via the gateway, so name resolution won't reach
//! upstream there. Egress to literal IPs is unaffected.

// The tap is created with a `TUNSETIFF` ioctl and the namespace is joined with
// `setns` over a borrowed raw fd — the same localized unsafe as `vmnet.rs`.
#![allow(unsafe_code)]

use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::mpsc::{self, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::libc;
use nix::sched::{CloneFlags, setns};
use tracing::{debug, warn};

use super::gateway::{GATEWAY_IP, Gateway};
use super::tapdev::TapDevice;
use crate::NetError;
use crate::owned_netns::OwnedNetns;
use crate::ssrf::EgressPolicy;

/// The container end of the gateway subnet (the gateway is `.1`,
/// [`GATEWAY_IP`]). One host carved out of `10.71.0.0/16`.
const CONTAINER_IP: Ipv4Addr = Ipv4Addr::new(10, 71, 0, 2);
/// Prefix length for the gateway subnet.
const PREFIX: u8 = 16;
/// Name of the tap created inside the container netns.
const TAP_NAME: &str = "umfeg0";
/// A deadline far enough out to be "never" for a RUN step (the stop signal is
/// the real exit; `Gateway::run_until` caps its wait so stop is observed within
/// ~50ms regardless of this).
const FAR_FUTURE_SECS: u64 = 60 * 60 * 24 * 365;

// TUN/TAP ioctl request code — stable kernel ABI, not exported by `libc`.
// `_IOW('T', 202, int)`. Typed `libc::Ioctl` so the literal fits whichever width
// the target uses (`c_ulong` on gnu, `c_int` on musl). Mirrors `vmnet.rs`.
const TUNSETIFF: libc::Ioctl = 0x4004_54ca;

/// A live native egress: the gateway thread plus the channel that stops it.
/// Dropping it signals the gateway to stop (which drops the tap fd, tearing down
/// the tap device) and joins the thread.
#[derive(Debug)]
pub(crate) struct NativeEgress {
    stop_tx: mpsc::Sender<()>,
    join: Option<JoinHandle<()>>,
}

impl NativeEgress {
    /// Bring native egress up for `netns` — the namespace UMF owns and the
    /// container joins. Creates and configures the tap inside it, then runs the
    /// gateway (enforcing `policy` on every re-originated connection) over the
    /// tap fd on a host-netns thread.
    ///
    /// # Errors
    /// [`NetError`] if the tap can't be created or configured in the namespace.
    pub(crate) fn start(netns: &OwnedNetns, policy: EgressPolicy) -> Result<Self, NetError> {
        let mtu = crate::host_egress_mtu().unwrap_or(1500) as usize;
        let tap_fd = create_tap_in_netns(netns.raw_fd())?;

        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let join = std::thread::Builder::new()
            .name("umf-native-egress".to_string())
            .spawn(move || run_gateway(tap_fd, mtu, policy, &stop_rx))
            .map_err(|e| NetError::Runtime(format!("spawning native egress thread: {e}")))?;
        debug!(container = %CONTAINER_IP, gateway = %GATEWAY_IP, "rootless native egress up");
        Ok(Self {
            stop_tx,
            join: Some(join),
        })
    }
}

impl Drop for NativeEgress {
    fn drop(&mut self) {
        // Signal the loop to stop; it re-checks within `run_until`'s wait cap.
        // Dropping `stop_tx` also disconnects the channel as a backstop.
        let _ = self.stop_tx.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        // The gateway (and its `TapDevice` / tap fd) dropped on the joined
        // thread, tearing down the tap device in the netns.
    }
}

/// Drive a [`Gateway`] over `tap_fd` until the stop signal fires. Runs on the
/// host-netns thread — the sockets it re-originates connections from must live
/// there.
fn run_gateway(tap_fd: OwnedFd, mtu: usize, policy: EgressPolicy, stop_rx: &mpsc::Receiver<()>) {
    let mut gw = match TapDevice::new(tap_fd, mtu).and_then(|dev| Gateway::new(dev, policy)) {
        Ok(gw) => gw,
        Err(e) => {
            warn!(error = %e, "rootless native egress: gateway failed to start");
            return;
        }
    };
    let far = Instant::now() + Duration::from_secs(FAR_FUTURE_SECS);
    // Stop when a `()` arrives or the sender is dropped; keep running only while
    // the channel is open and empty.
    let stats = gw.run_until(far, |_| {
        !matches!(stop_rx.try_recv(), Err(TryRecvError::Empty))
    });
    debug!(?stats, "rootless native egress: gateway stopped");
}

/// Create a tap named [`TAP_NAME`] **inside** the namespace referred to by
/// `netns_fd`, configure the container side (address + default route via the
/// gateway), and return the owning tap fd. The tap is **not** made persistent —
/// it lives exactly as long as the returned fd, so dropping the gateway tears it
/// down with no leftover device.
///
/// Runs on a dedicated thread: `setns(CLONE_NEWNET)` moves only the calling
/// thread, so the caller stays in the host netns. The returned [`OwnedFd`]
/// outlives the thread (moved out via `join`), keeping the tap device alive.
fn create_tap_in_netns(netns_fd: RawFd) -> Result<OwnedFd, NetError> {
    std::thread::Builder::new()
        .name("umf-native-tap".to_string())
        .spawn(move || -> Result<OwnedFd, NetError> {
            // SAFETY: `netns_fd` is owned by the caller's `OwnedNetns` for the
            // whole of this synchronous call (we join this thread before
            // returning), so it is valid; `borrow_raw` only wraps it for `setns`.
            let ns = unsafe { BorrowedFd::borrow_raw(netns_fd) };
            setns(ns, CloneFlags::CLONE_NEWNET)
                .map_err(|e| NetError::VmNet(format!("setns(owned netns): {e}")))?;

            // Create the tap in *this* netns (we're setns'd in) and keep its fd.
            let tap = open_tap(TAP_NAME)?;

            // Configure the container side from inside the netns: a fresh
            // rtnetlink socket opened here lands in this namespace.
            let rt = crate::current_thread_rt()?;
            rt.block_on(async {
                let (conn, handle, _) = rtnetlink::new_connection()?;
                let _conn = tokio::spawn(conn);
                let idx = crate::link_index(&handle, TAP_NAME).await?;
                handle
                    .address()
                    .add(idx, IpAddr::V4(CONTAINER_IP), PREFIX)
                    .execute()
                    .await
                    .map_err(crate::nl)?;
                handle
                    .link()
                    .set(idx)
                    .up()
                    .execute()
                    .await
                    .map_err(crate::nl)?;
                // Default route via the gateway, so traffic to any destination
                // reaches the gateway's `any_ip` interface.
                handle
                    .route()
                    .add()
                    .v4()
                    .destination_prefix(Ipv4Addr::UNSPECIFIED, 0)
                    .gateway(GATEWAY_IP)
                    .execute()
                    .await
                    .map_err(crate::nl)?;
                Ok::<(), NetError>(())
            })?;

            Ok(tap)
        })
        .map_err(|e| NetError::Runtime(format!("spawning native-tap thread: {e}")))?
        .join()
        .map_err(|_| NetError::Runtime("native-tap thread panicked".to_string()))?
}

/// Open `/dev/net/tun` and bind a new tap named `name` in the current netns via
/// `TUNSETIFF` (`IFF_TAP | IFF_NO_PI`), returning the owning fd. Unlike
/// `vmnet`'s persistent tap, this is **not** `TUNSETPERSIST`'d: the device is
/// reaped when the fd closes (gateway teardown), so a failed or finished build
/// leaves no tap behind.
fn open_tap(name: &str) -> Result<OwnedFd, NetError> {
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
    // SAFETY: `ifreq` is plain-old-data; all-zero is a valid initial value (empty
    // name, zero flags) we then fill in.
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    for (i, b) in name_bytes.iter().enumerate() {
        ifr.ifr_name[i] = *b as libc::c_char;
    }
    // Writing (not reading) the `ifru_flags` union arm per the TUNSETIFF
    // contract (interface flags).
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
    Ok(OwnedFd::from(tun))
}
