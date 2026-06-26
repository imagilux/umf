//! Unit tests for the raw-fd smoltcp device.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
// The test harness builds a `socketpair` and wraps the raw fds — the same
// localized unsafe as the module under test.
#![allow(unsafe_code)]

use std::os::fd::{FromRawFd, OwnedFd};

use nix::libc;
use smoltcp::phy::{Device, RxToken, TxToken};
use smoltcp::time::Instant;

use super::*;

/// A connected `AF_UNIX`/`SOCK_DGRAM` pair: writes on one fd are read as whole
/// datagrams on the other, which is exactly the frame-in/frame-out contract a
/// TAP gives smoltcp. Lets us exercise the device with zero privilege.
fn socketpair() -> (OwnedFd, OwnedFd) {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` is a 2-element array `socketpair` fills with two valid fds.
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
    assert_eq!(rc, 0, "socketpair: {}", std::io::Error::last_os_error());
    // SAFETY: both fds are freshly returned by `socketpair` and owned here.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

#[test]
fn idle_fill_rx_buffers_nothing_and_receive_yields_none() {
    let (a, _b) = socketpair();
    let mut dev = TapDevice::new(a, 1500).expect("device");
    // No frame written to the peer: a non-blocking drain must buffer nothing and
    // not park, and `receive` then has nothing to hand the stack.
    assert_eq!(dev.fill_rx(), 0, "idle fill_rx should buffer no frames");
    assert!(
        dev.receive(Instant::from_millis(0)).is_none(),
        "idle non-blocking receive should yield None, not block"
    );
}

#[test]
fn transmit_then_fill_rx_then_receive_roundtrips_a_frame() {
    let (a, b) = socketpair();
    let mut tx_dev = TapDevice::new(a, 1500).expect("tx device");
    let mut rx_dev = TapDevice::new(b, 1500).expect("rx device");

    // Write a frame out of `tx_dev` via a TxToken.
    let frame = b"\xde\xad\xbe\xef hello frame";
    let tok = tx_dev.transmit(Instant::from_millis(0)).expect("tx token");
    tok.consume(frame.len(), |buf| buf.copy_from_slice(frame));

    // It is not visible to `receive` until drained off the fd into the queue.
    assert!(
        rx_dev.receive(Instant::from_millis(1)).is_none(),
        "receive should be empty before fill_rx"
    );
    assert_eq!(rx_dev.fill_rx(), 1, "one frame should drain off the fd");

    // After draining, it is peekable *and* deliverable, unchanged.
    assert_eq!(
        rx_dev.queued_frames().next(),
        Some(frame.as_slice()),
        "the queued frame should be peekable before receive consumes it"
    );
    let (rx, _tx) = rx_dev
        .receive(Instant::from_millis(1))
        .expect("a frame should be readable");
    rx.consume(|got| assert_eq!(got, frame));
}

#[test]
fn fill_rx_preserves_frame_order_for_peek_and_delivery() {
    let (a, b) = socketpair();
    let mut tx_dev = TapDevice::new(a, 1500).expect("tx device");
    let mut rx_dev = TapDevice::new(b, 1500).expect("rx device");

    let frames: [&[u8]; 3] = [b"frame-one", b"frame-two", b"frame-three"];
    for f in frames {
        let tok = tx_dev.transmit(Instant::from_millis(0)).expect("tx token");
        tok.consume(f.len(), |buf| buf.copy_from_slice(f));
    }

    assert_eq!(rx_dev.fill_rx(), 3, "all three frames should drain");
    // Peek sees them FIFO (so the gateway learns ports in send order).
    let peeked: Vec<&[u8]> = rx_dev.queued_frames().collect();
    assert_eq!(peeked, frames.to_vec(), "peek order must be FIFO");
    // And `receive` delivers them in the same order, then runs dry.
    for expected in frames {
        let (rx, _tx) = rx_dev
            .receive(Instant::from_millis(1))
            .expect("frame should be deliverable");
        rx.consume(|got| assert_eq!(got, expected));
    }
    assert!(
        rx_dev.receive(Instant::from_millis(1)).is_none(),
        "queue should be empty after delivering every frame"
    );
}

#[test]
fn capabilities_report_ethernet_and_mtu() {
    let (a, _b) = socketpair();
    let dev = TapDevice::new(a, 1492).expect("device");
    let caps = dev.capabilities();
    assert_eq!(caps.medium, smoltcp::phy::Medium::Ethernet);
    assert_eq!(caps.max_transmission_unit, 1492);
}
