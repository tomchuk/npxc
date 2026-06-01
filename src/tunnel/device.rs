//! `ipstack` device backed by a `WireGuard` tunnel over UDP.
//!
//! [`WgDevice`] implements [`AsyncRead`] + [`AsyncWrite`] in *packet* terms (one
//! read = one decrypted inbound IP packet, one write = one IP packet to send to
//! the peer), which is exactly the device interface `ipstack` consumes. It owns
//! the UDP socket and the [`WgTunnel`] and does the crypto inline against
//! reusable buffers, so the steady-state path allocates nothing per packet.
//!
//! - `poll_read` drives the keepalive/handshake timer, receives a datagram into
//!   a reused buffer, decapsulates it (sending any handshake replies straight
//!   back over UDP), and copies the first decrypted IP packet into `ipstack`'s
//!   buffer. Handshake-only datagrams produce no read; the method loops.
//! - `poll_write` encapsulates `ipstack`'s outbound IP packet and sends it to
//!   the peer.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;
use tokio::time::{Interval, MissedTickBehavior};

use super::wg::WgTunnel;

/// Size of the reusable UDP receive buffer.
const RX_LEN: usize = 65_536;

/// How often to advance boringtun's timers (handshake retries / keepalives).
const TIMER_PERIOD: Duration = Duration::from_millis(250);

/// A packet device that tunnels `ipstack`'s IP traffic to a peer over
/// `WireGuard`/UDP.
pub struct WgDevice {
    socket: UdpSocket,
    tunnel: WgTunnel,
    /// The peer's UDP address, learned from the first datagram received.
    peer: Option<SocketAddr>,
    /// Reusable receive buffer for inbound UDP datagrams.
    rx: Box<[u8]>,
    keepalive: Interval,
}

impl WgDevice {
    /// Build a device over a bound UDP socket and an initialized tunnel.
    #[must_use]
    pub fn new(socket: UdpSocket, tunnel: WgTunnel) -> Self {
        let mut keepalive = tokio::time::interval(TIMER_PERIOD);
        keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            socket,
            tunnel,
            peer: None,
            rx: vec![0u8; RX_LEN].into_boxed_slice(),
            keepalive,
        }
    }
}

impl AsyncRead for WgDevice {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let WgDevice {
            socket,
            tunnel,
            peer,
            rx,
            keepalive,
        } = self.get_mut();

        loop {
            // Advance handshake/keepalive timers, sending anything due.
            while keepalive.poll_tick(cx).is_ready() {
                if let Some(p) = *peer {
                    tunnel.tick(|b| {
                        let _ = socket.try_send_to(b, p);
                    });
                }
            }

            // Receive one datagram into the reusable buffer.
            let mut rb = ReadBuf::new(&mut rx[..]);
            match socket.poll_recv_from(cx, &mut rb) {
                Poll::Ready(Ok(addr)) => {
                    *peer = Some(addr);
                    let datagram = rb.filled();
                    let mut produced = false;
                    tunnel.decapsulate(
                        datagram,
                        |b| {
                            let _ = socket.try_send_to(b, addr);
                        },
                        |b| {
                            // One datagram yields at most one inner packet.
                            if !produced && b.len() <= buf.remaining() {
                                buf.put_slice(b);
                                produced = true;
                            }
                        },
                    );
                    if produced {
                        return Poll::Ready(Ok(()));
                    }
                    // Handshake-only datagram: nothing for ipstack yet, keep going.
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AsyncWrite for WgDevice {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let WgDevice {
            socket,
            tunnel,
            peer,
            ..
        } = self.get_mut();
        if let Some(p) = *peer {
            tunnel.encapsulate(data, |b| {
                let _ = socket.try_send_to(b, p);
            });
        }
        // Report the whole packet as written even if there's no peer yet (the
        // guest always speaks first, so this is effectively unreachable) or the
        // UDP send would block — ipstack treats the device as best-effort.
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
