//! A [`smoltcp::phy::Device`] over a raw file descriptor: the L2 transport for
//! the rootless native egress gateway.
//!
//! In the integrated path the fd is a TAP opened in the container's network
//! namespace (`/dev/net/tun` + `TUNSETIFF`, the `vmnet.rs` pattern), so every
//! Ethernet frame the container emits arrives here and every frame we write goes
//! back to it. The device is kept fd-agnostic — anything that reads and writes
//! whole Ethernet frames works, which is what lets the unit tests run it over a
//! `socketpair` with no privilege.
//!
//! smoltcp is sans-I/O: it never touches the fd itself. [`Interface::poll`] asks
//! the device for an [`RxToken`] (a borrowed received frame) and a [`TxToken`]
//! (a slot to fill with a frame to send). This device backs the tx side with one
//! blocking `write` on the fd; the rx side is **buffered**: [`fill_rx`] drains
//! the fd into an in-memory frame queue, and [`receive`] hands smoltcp frames
//! off that queue.
//!
//! The buffering exists for one reason: **SYN-port learning** (the gateway's
//! `any_ip` listeners must already be in LISTEN on a port before smoltcp
//! processes a SYN to it, else smoltcp answers RST). The gateway calls
//! [`fill_rx`], [peeks](Self::queued_frames) the queued frames to discover the
//! destination ports the container is dialing, primes listeners for them, *then*
//! `poll`s — at which point [`receive`] replays the very frames that were peeked,
//! now matched by a ready listener. An fd cannot be un-read, so the queue is what
//! makes "peek then process the same bytes" possible.
//!
//! [`Interface::poll`]: smoltcp::iface::Interface::poll
//! [`fill_rx`]: TapDevice::fill_rx
//! [`receive`]: Device::receive

// The fd transport needs a handful of irreducibly-unsafe raw libc calls the
// workspace otherwise bans (`fcntl` for O_NONBLOCK, `read`/`write` on the raw
// frame fd). Each `unsafe` block carries a `SAFETY` justification, the same
// posture as `vmnet.rs`, the crate's designated unsafe module.
#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use nix::libc;
use smoltcp::phy::{
    Checksum, ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken,
};
use smoltcp::time::Instant;

use crate::NetError;

/// Hard ceiling on a single frame read/written over the fd. The TAP MTU plus an
/// Ethernet header; oversized reads are truncated to this. Generous enough for a
/// jumbo-frame host (the integrated path derives the real MTU from the egress
/// link, mirroring [`crate::NetOptions`]).
const FRAME_BUF: usize = 65_536;

/// Cap on frames pulled off the fd in one [`TapDevice::fill_rx`] call, so a flood
/// can't make a single poll turn unbounded. Anything still buffered in the
/// kernel is taken on the next turn (the event loop re-polls the fd for
/// readiness). 256 full-size frames is comfortably more than a build's RUN step
/// bursts in a sub-millisecond turn.
const RX_BURST: usize = 256;

/// A [`smoltcp::phy::Device`] that reads and writes Ethernet frames on an owned
/// raw fd, buffering received frames so they can be peeked before smoltcp
/// processes them (see the module docs). The fd is put into non-blocking mode at
/// construction so [`fill_rx`](Self::fill_rx) can drain it without parking the
/// gateway's single-threaded event loop.
#[derive(Debug)]
pub struct TapDevice {
    fd: OwnedFd,
    mtu: usize,
    /// Frames read off the fd by [`fill_rx`](Self::fill_rx), awaiting delivery to
    /// smoltcp via [`receive`](Device::receive). FIFO, so frame order is
    /// preserved.
    rx_queue: VecDeque<Vec<u8>>,
}

impl TapDevice {
    /// Wrap `fd` as a smoltcp device with the given link MTU, switching it to
    /// non-blocking. Takes ownership: the fd is closed when the device drops.
    ///
    /// # Errors
    /// [`NetError::Io`] if the `fcntl` to set `O_NONBLOCK` fails.
    pub fn new(fd: OwnedFd, mtu: usize) -> Result<Self, NetError> {
        set_nonblocking(fd.as_raw_fd())?;
        Ok(Self {
            fd,
            mtu,
            rx_queue: VecDeque::new(),
        })
    }

    /// Borrow the underlying raw fd so the event loop can `poll` it for
    /// readiness. The fd stays owned by this device.
    #[must_use]
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Drain frames waiting on the fd into the rx queue, up to [`RX_BURST`].
    /// Returns the number of frames buffered this call. Non-blocking: stops at
    /// the first `EWOULDBLOCK`. Called by the gateway at the top of each poll
    /// turn, before it peeks the queue and primes listeners.
    pub fn fill_rx(&mut self) -> usize {
        let mut count = 0;
        while count < RX_BURST {
            let mut buf = vec![0u8; FRAME_BUF];
            // SAFETY: `buf` is a live, `FRAME_BUF`-long allocation we just made;
            // the read writes at most that many bytes into it. The fd is valid
            // (owned by `self`) and non-blocking (set in `new`).
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                // EWOULDBLOCK (drained), EOF, or error: nothing more this turn.
                // A hard fd error surfaces on the next `write`, and the loop
                // independently watches the fd for hang-up.
                break;
            }
            buf.truncate(n as usize);
            self.rx_queue.push_back(buf);
            count += 1;
        }
        count
    }

    /// The frames currently buffered (not yet handed to smoltcp), for the
    /// gateway to peek for destination ports before `poll`. Read-only — the
    /// frames stay queued and are delivered, unmodified, by
    /// [`receive`](Device::receive).
    pub fn queued_frames(&self) -> impl Iterator<Item = &[u8]> {
        self.rx_queue.iter().map(Vec::as_slice)
    }
}

/// Set `O_NONBLOCK` on `fd` via `fcntl`. Read-modify-write so other flags
/// (`O_CLOEXEC` et al.) are preserved.
fn set_nonblocking(fd: RawFd) -> Result<(), NetError> {
    // SAFETY: `fd` is a valid open fd (owned by the caller's `TapDevice` /
    // borrowed for this call). `F_GETFL` takes no argument and only reads the
    // flags; `F_SETFL` writes back the same flags with `O_NONBLOCK` added. No
    // memory is involved.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(NetError::Io(io::Error::last_os_error()));
    }
    // SAFETY: as above; writing the read-back flags plus O_NONBLOCK.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc < 0 {
        return Err(NetError::Io(io::Error::last_os_error()));
    }
    Ok(())
}

/// A received frame, moved out of the rx queue into the token. smoltcp hands
/// this to the stack which calls [`RxToken::consume`] exactly once.
pub struct TapRxToken(Vec<u8>);

/// A transmit slot bound to the device fd. smoltcp fills it via
/// [`TxToken::consume`] and we `write` the frame on drop of the closure.
pub struct TapTxToken {
    fd: RawFd,
    mtu: usize,
}

impl Device for TapDevice {
    type RxToken<'a>
        = TapRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = TapTxToken
    where
        Self: 'a;

    fn receive(&mut self, _ts: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Frames come off the queue `fill_rx` populated this turn, not straight
        // from the fd — so the gateway has already peeked them and primed any
        // listener the frame needs. An empty queue means nothing to deliver.
        let frame = self.rx_queue.pop_front()?;
        let rx = TapRxToken(frame);
        let tx = TapTxToken {
            fd: self.fd.as_raw_fd(),
            mtu: self.mtu,
        };
        Some((rx, tx))
    }

    fn transmit(&mut self, _ts: Instant) -> Option<Self::TxToken<'_>> {
        Some(TapTxToken {
            fd: self.fd.as_raw_fd(),
            mtu: self.mtu,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = self.mtu;
        // The TAP path hands us frames the kernel has already checksum-verified
        // on ingress, and the host NIC offloads egress checksums, so we *could*
        // ask smoltcp to skip them. We keep full checksumming on (correctness
        // over the last few percent of throughput); revisit when measuring.
        caps.checksum = ChecksumCapabilities::default();
        let _ = Checksum::Both; // keep the import meaningful if caps tuning lands
        caps
    }
}

impl RxToken for TapRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}

impl TxToken for TapTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // smoltcp never asks for more than the advertised MTU, but clamp
        // defensively so a bug can't make us write an oversized frame.
        let len = len.min(self.mtu.max(FRAME_BUF));
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        // SAFETY: `buf` is a live `len`-byte allocation; the write reads at most
        // `len` bytes from it. The fd is valid and non-blocking. A short or
        // failed write drops the frame — acceptable for a datagram-style L2 link
        // (TCP retransmits; egress is not buffered).
        let _ = unsafe { libc::write(self.fd, buf.as_ptr().cast(), buf.len()) };
        r
    }
}

#[cfg(test)]
mod tests;
