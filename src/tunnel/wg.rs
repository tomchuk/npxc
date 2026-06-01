//! `WireGuard` transport state machine (zero-allocation hot path).
//!
//! [`WgTunnel`] wraps a single [`boringtun::noise::Tunn`]. boringtun writes each
//! result into a caller-provided buffer and returns a [`TunnResult`] that
//! *borrows* it. `WgTunnel` keeps one reusable scratch buffer and hands the
//! borrowed result straight to a callback, so the steady-state datapath does
//! **no per-packet heap allocation** — the only transform is boringtun's own
//! encrypt/decrypt, which is inherent.
//!
//! Two boringtun details are hidden from callers:
//! - **Buffer borrows.** The scratch buffer is owned here; callbacks receive a
//!   `&[u8]` valid only for the duration of the call.
//! - **Post-handshake flush.** After `decapsulate` returns `WriteToNetwork`,
//!   boringtun must be re-called with an empty datagram until it returns `Done`
//!   to drain its queue; [`WgTunnel::decapsulate`] loops internally.
//!
//! The type is IO-free, so the full handshake + data path is exercised in unit
//! tests by wiring two `WgTunnel`s together; the async device that drives it
//! over UDP lives separately.

use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};
use tracing::debug;

/// Size of the reusable scratch buffer — large enough for any
/// `WireGuard`/IP datagram boringtun emits. Allocated once per tunnel.
const SCRATCH_LEN: usize = 65_536;

/// A `WireGuard` tunnel to a single peer.
pub struct WgTunnel {
    tunn: Tunn,
    scratch: Box<[u8]>,
}

impl WgTunnel {
    /// Create a tunnel from the local static secret and the peer's public key.
    ///
    /// `index` is boringtun's local receiver index; with a single peer any
    /// value works.
    #[must_use]
    pub fn new(local_secret: &StaticSecret, peer_public: &PublicKey, index: u32) -> Self {
        // Reconstruct an owned secret from bytes to avoid relying on Clone.
        let secret = StaticSecret::from(local_secret.to_bytes());
        let tunn = Tunn::new(secret, *peer_public, None, None, index, None);
        Self {
            tunn,
            scratch: vec![0u8; SCRATCH_LEN].into_boxed_slice(),
        }
    }

    /// Decapsulate a UDP datagram received from the peer.
    ///
    /// `to_peer` is invoked with bytes to send back to the peer (handshake
    /// responses, then any queued packets flushed afterwards). `to_host` is
    /// invoked with each decrypted IP packet. Both receive borrowed slices into
    /// the internal scratch buffer, valid only for the duration of the call.
    pub fn decapsulate(
        &mut self,
        datagram: &[u8],
        mut to_peer: impl FnMut(&[u8]),
        mut to_host: impl FnMut(&[u8]),
    ) {
        if self.decapsulate_once(datagram, &mut to_peer, &mut to_host) {
            // A to-peer datagram was produced; drain boringtun's queue.
            while self.decapsulate_once(&[], &mut to_peer, &mut to_host) {}
        }
    }

    /// One `decapsulate` call. Copies nothing: the callback consumes the
    /// borrowed result before this returns. Returns `true` if a to-peer
    /// datagram was emitted (the caller should keep draining).
    fn decapsulate_once(
        &mut self,
        input: &[u8],
        to_peer: &mut impl FnMut(&[u8]),
        to_host: &mut impl FnMut(&[u8]),
    ) -> bool {
        match self.tunn.decapsulate(None, input, &mut self.scratch) {
            TunnResult::WriteToNetwork(b) => {
                to_peer(b);
                true
            }
            TunnResult::WriteToTunnelV4(b, _) | TunnResult::WriteToTunnelV6(b, _) => {
                to_host(b);
                false
            }
            TunnResult::Done => false,
            TunnResult::Err(e) => {
                debug!(?e, "WireGuard decapsulate error");
                false
            }
        }
    }

    /// Encrypt a host-originated IP packet, invoking `to_peer` with the datagram
    /// to send over UDP. No-op if boringtun queued the packet pending a
    /// handshake.
    pub fn encapsulate(&mut self, packet: &[u8], mut to_peer: impl FnMut(&[u8])) {
        match self.tunn.encapsulate(packet, &mut self.scratch) {
            TunnResult::WriteToNetwork(b) => to_peer(b),
            TunnResult::Err(e) => debug!(?e, "WireGuard encapsulate error"),
            // `encapsulate` never decrypts; `Done` means the packet was queued.
            TunnResult::Done
            | TunnResult::WriteToTunnelV4(..)
            | TunnResult::WriteToTunnelV6(..) => {}
        }
    }

    /// Advance boringtun's timers; call periodically (e.g. every ~250 ms).
    /// Invokes `to_peer` if a handshake initiation or keepalive is due.
    pub fn tick(&mut self, mut to_peer: impl FnMut(&[u8])) {
        if let TunnResult::WriteToNetwork(b) = self.tunn.update_timers(&mut self.scratch) {
            to_peer(b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnel::keys::WgKeypair;

    /// A minimal IPv4 packet (version nibble = 4) carrying a 4-byte marker, so
    /// boringtun routes it as `WriteToTunnelV4` on decapsulate.
    fn ipv4_packet(marker: u8) -> Vec<u8> {
        vec![
            0x45, 0x00, 0x00, 0x18, // ver/ihl, dscp, total length (24)
            0x00, 0x00, 0x40, 0x00, // identification, flags/fragment
            0x40, 0x11, 0x00, 0x00, // ttl=64, proto=17 (udp), checksum
            10, 0, 0, 2, // source
            1, 1, 1, 1, // destination
            marker, marker, marker, marker, // payload
        ]
    }

    /// Collect a single callback output into an owned buffer (test convenience).
    fn collect(f: impl FnOnce(&mut dyn FnMut(&[u8]))) -> Vec<u8> {
        let mut out = Vec::new();
        f(&mut |b| out.extend_from_slice(b));
        out
    }

    /// Drive a full handshake between two in-memory tunnels and confirm an IPv4
    /// packet round-trips, decrypted intact, in both directions.
    #[test]
    fn handshake_then_bidirectional_data() {
        let npxc_kp = WgKeypair::generate();
        let guest_kp = WgKeypair::generate();

        let mut npxc = WgTunnel::new(npxc_kp.secret(), guest_kp.public(), 1);
        let mut guest = WgTunnel::new(guest_kp.secret(), npxc_kp.public(), 2);

        // Guest initiates: its first encapsulate emits a handshake initiation
        // and queues the packet internally.
        let init = collect(|peer| guest.encapsulate(&ipv4_packet(0x01), peer));
        assert!(!init.is_empty(), "expected a handshake initiation");

        // Responder replies with a handshake response (no host packet yet).
        let mut host_during_hs = 0usize;
        let resp = {
            let mut out = Vec::new();
            npxc.decapsulate(&init, |b| out.extend_from_slice(b), |_| host_during_hs += 1);
            out
        };
        assert_eq!(host_during_hs, 0, "no host packet during the handshake");
        assert!(!resp.is_empty(), "expected a handshake response");

        // Initiator consumes the response, completing the handshake. Feed any
        // flushed datagrams back to the responder so its session is confirmed.
        let mut flushed: Vec<Vec<u8>> = Vec::new();
        guest.decapsulate(&resp, |b| flushed.push(b.to_vec()), |_| {});
        for datagram in &flushed {
            npxc.decapsulate(datagram, |_| {}, |_| {});
        }

        // Established session: guest -> npxc.
        let up = ipv4_packet(0xAA);
        let enc_up = collect(|peer| guest.encapsulate(&up, peer));
        let mut got_up = Vec::new();
        npxc.decapsulate(&enc_up, |_| {}, |b| got_up.extend_from_slice(b));
        assert_eq!(got_up, up, "npxc must decrypt the upstream packet");

        // Established session: npxc -> guest.
        let down = ipv4_packet(0xBB);
        let enc_down = collect(|peer| npxc.encapsulate(&down, peer));
        let mut got_down = Vec::new();
        guest.decapsulate(&enc_down, |_| {}, |b| got_down.extend_from_slice(b));
        assert_eq!(got_down, down, "guest must decrypt the downstream packet");
    }

    #[test]
    fn data_before_handshake_yields_no_host_packets() {
        let npxc_kp = WgKeypair::generate();
        let guest_kp = WgKeypair::generate();
        let mut npxc = WgTunnel::new(npxc_kp.secret(), guest_kp.public(), 1);

        let mut host_packets = 0usize;
        npxc.decapsulate(&[0u8; 64], |_| {}, |_| host_packets += 1);
        assert_eq!(host_packets, 0);
    }
}
