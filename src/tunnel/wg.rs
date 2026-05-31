//! `WireGuard` transport state machine.
//!
//! [`WgTunnel`] wraps a single [`boringtun::noise::Tunn`] and turns its
//! borrow-heavy, scratch-buffer API into owned [`Outbound`] actions, hiding two
//! boringtun details:
//!
//! - **Buffer borrows.** `Tunn`'s methods write into a caller-provided `dst`
//!   and return a [`TunnResult`] that *borrows* it. We copy the result out
//!   immediately so callers get owned `Vec<u8>`s with no lifetime juggling.
//! - **Post-handshake flush.** After `decapsulate` returns `WriteToNetwork`,
//!   boringtun requires repeated `decapsulate` calls with an empty datagram to
//!   drain its queue. [`WgTunnel::handle_datagram`] does that internally.
//!
//! The type is deliberately IO-free so it can be exercised end-to-end in unit
//! tests by wiring two `WgTunnel`s together; the async UDP endpoint that drives
//! it lives separately.

use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};
use tracing::debug;

/// Maximum size of a `WireGuard`/IP datagram we'll emit into the scratch buffer.
const SCRATCH_LEN: usize = 65_536;

/// An action produced by feeding data to a [`WgTunnel`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outbound {
    /// Encrypted/handshake bytes to send to the peer (the guest) over UDP.
    ToPeer(Vec<u8>),
    /// A decrypted inbound IP packet from the peer, to hand to the host
    /// netstack for forwarding.
    ToHost(Vec<u8>),
}

/// One copied-out result of a single boringtun call.
enum Step {
    Done,
    ToPeer(Vec<u8>),
    ToHost(Vec<u8>),
}

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

    /// Process a UDP datagram received from the peer.
    ///
    /// Returns any combination of replies to send back to the peer (handshake
    /// responses, flushed queued packets) and decrypted IP packets bound for
    /// the host.
    pub fn handle_datagram(&mut self, datagram: &[u8]) -> Vec<Outbound> {
        let mut out = Vec::new();
        match self.decapsulate_step(datagram) {
            Step::Done => {}
            Step::ToHost(p) => out.push(Outbound::ToHost(p)),
            Step::ToPeer(p) => {
                out.push(Outbound::ToPeer(p));
                // boringtun queues packets during a handshake; drain them by
                // decapsulating empty input until it reports Done.
                loop {
                    match self.decapsulate_step(&[]) {
                        Step::ToPeer(p) => out.push(Outbound::ToPeer(p)),
                        Step::ToHost(p) => out.push(Outbound::ToHost(p)),
                        Step::Done => break,
                    }
                }
            }
        }
        out
    }

    /// Encrypt a host-originated IP packet for the peer.
    ///
    /// Returns the datagram to send over UDP, or `None` if boringtun produced
    /// nothing (e.g. the packet was queued pending a handshake, or an error
    /// was logged).
    pub fn encapsulate_packet(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        match self.tunn.encapsulate(packet, &mut self.scratch) {
            TunnResult::WriteToNetwork(b) => Some(b.to_vec()),
            TunnResult::Err(e) => {
                debug!(?e, "WireGuard encapsulate error");
                None
            }
            // `encapsulate` never decrypts, so the tunnel variants don't occur;
            // `Done` means the packet was queued pending a handshake.
            TunnResult::Done
            | TunnResult::WriteToTunnelV4(..)
            | TunnResult::WriteToTunnelV6(..) => None,
        }
    }

    /// Advance boringtun's timers; call periodically (e.g. every ~250ms).
    ///
    /// Returns a datagram to send to the peer if a handshake initiation or
    /// keepalive is due.
    pub fn tick(&mut self) -> Option<Vec<u8>> {
        match self.tunn.update_timers(&mut self.scratch) {
            TunnResult::WriteToNetwork(b) => Some(b.to_vec()),
            _ => None,
        }
    }

    /// One `decapsulate` call, copied out of the scratch buffer so no borrow
    /// outlives the call.
    fn decapsulate_step(&mut self, input: &[u8]) -> Step {
        match self.tunn.decapsulate(None, input, &mut self.scratch) {
            TunnResult::Done => Step::Done,
            TunnResult::Err(e) => {
                debug!(?e, "WireGuard decapsulate error");
                Step::Done
            }
            TunnResult::WriteToNetwork(b) => Step::ToPeer(b.to_vec()),
            TunnResult::WriteToTunnelV4(b, _) | TunnResult::WriteToTunnelV6(b, _) => {
                Step::ToHost(b.to_vec())
            }
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

    fn to_peer(out: Vec<Outbound>) -> Vec<Vec<u8>> {
        out.into_iter()
            .filter_map(|o| match o {
                Outbound::ToPeer(b) => Some(b),
                Outbound::ToHost(_) => None,
            })
            .collect()
    }

    fn to_host(out: Vec<Outbound>) -> Vec<Vec<u8>> {
        out.into_iter()
            .filter_map(|o| match o {
                Outbound::ToHost(b) => Some(b),
                Outbound::ToPeer(_) => None,
            })
            .collect()
    }

    /// Drive a full handshake between two in-memory tunnels and confirm that an
    /// IP packet round-trips, decrypted intact, in both directions.
    #[test]
    fn handshake_then_bidirectional_data() {
        let npxc_kp = WgKeypair::generate();
        let guest_kp = WgKeypair::generate();

        let mut npxc = WgTunnel::new(npxc_kp.secret(), guest_kp.public(), 1);
        let mut guest = WgTunnel::new(guest_kp.secret(), npxc_kp.public(), 2);

        // Guest initiates: its first encapsulate emits a handshake initiation
        // and queues the packet internally.
        let init = guest
            .encapsulate_packet(&ipv4_packet(0x01))
            .expect("handshake initiation");
        // Responder replies with a handshake response.
        let resp = to_peer(npxc.handle_datagram(&init));
        assert_eq!(resp.len(), 1, "responder must emit one handshake response");
        // Initiator consumes the response, completing the handshake. Any queued
        // packet flushed here is fed to the responder so its session is fully
        // confirmed before the explicit data exchange below.
        let flushed = to_peer(guest.handle_datagram(&resp[0]));
        for datagram in flushed {
            let _ = npxc.handle_datagram(&datagram);
        }

        // Established session: guest -> npxc.
        let up = ipv4_packet(0xAA);
        let enc_up = guest.encapsulate_packet(&up).expect("encrypted upstream");
        let got_up = to_host(npxc.handle_datagram(&enc_up));
        assert_eq!(got_up, vec![up], "npxc must decrypt the upstream packet");

        // Established session: npxc -> guest.
        let down = ipv4_packet(0xBB);
        let enc_down = npxc
            .encapsulate_packet(&down)
            .expect("encrypted downstream");
        let got_down = to_host(guest.handle_datagram(&enc_down));
        assert_eq!(
            got_down,
            vec![down],
            "guest must decrypt the downstream packet"
        );
    }

    #[test]
    fn data_before_handshake_is_dropped_by_peer() {
        // A datagram that isn't a valid handshake/transport for the peer must
        // not yield host packets (it's gibberish to a tunnel with no session).
        let npxc_kp = WgKeypair::generate();
        let guest_kp = WgKeypair::generate();
        let mut npxc = WgTunnel::new(npxc_kp.secret(), guest_kp.public(), 1);

        let out = npxc.handle_datagram(&[0u8; 64]);
        assert!(to_host(out).is_empty());
    }
}
