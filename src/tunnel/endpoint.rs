//! Egress tunnel datapath.
//!
//! Drives `ipstack` over a [`WgDevice`] and forwards each flow the guest opens
//! to a real host socket. This is the passthrough (allow-all) datapath; egress
//! policy is layered on later.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use ipstack::{IpStack, IpStackConfig, IpStackStream, IpStackTcpStream, IpStackUdpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpStream, UdpSocket};
use tokio::task::JoinHandle;
use tracing::debug;

use super::device::WgDevice;

/// Guest-side tunnel MTU. Must match the `wg0` MTU configured in the guest.
pub const MTU: u16 = 1380;

/// Per-UDP-flow relay buffer size.
const UDP_BUF: usize = 2048;

/// A running tunnel datapath. Dropping or [`abort`](Tunnel::abort)ing it tears
/// down the `ipstack` accept loop (in-flight forwarders finish on their own).
pub struct Tunnel {
    task: JoinHandle<()>,
}

impl Tunnel {
    /// Spawn the datapath over `device`, forwarding every flow (passthrough).
    #[must_use]
    pub fn spawn(device: WgDevice) -> Self {
        let mut config = IpStackConfig::default();
        let _ = config.mtu(MTU);
        let mut ip_stack = IpStack::new(config, device);

        let task = tokio::spawn(async move {
            loop {
                match ip_stack.accept().await {
                    Ok(IpStackStream::Tcp(tcp)) => {
                        tokio::spawn(forward_tcp(tcp));
                    }
                    Ok(IpStackStream::Udp(udp)) => {
                        tokio::spawn(forward_udp(udp));
                    }
                    // ICMP and anything we don't terminate is dropped for now.
                    Ok(_) => {}
                    Err(e) => {
                        debug!(?e, "ipstack accept loop ended");
                        break;
                    }
                }
            }
        });
        Self { task }
    }

    /// Abort the accept loop.
    pub fn abort(&self) {
        self.task.abort();
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Forward a guest TCP connection to its real destination.
async fn forward_tcp(mut guest: IpStackTcpStream) {
    let dest = guest.peer_addr();
    match TcpStream::connect(dest).await {
        Ok(mut upstream) => {
            if let Err(e) = copy_bidirectional(&mut guest, &mut upstream).await {
                debug!(?e, %dest, "tcp forward ended");
            }
        }
        Err(e) => debug!(?e, %dest, "tcp connect failed"),
    }
}

/// Forward a guest UDP flow to its real destination by relaying datagrams.
async fn forward_udp(mut guest: IpStackUdpStream) {
    let dest = guest.peer_addr();
    let bind: SocketAddr = if dest.is_ipv4() {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let Ok(upstream) = UdpSocket::bind(bind).await else {
        return;
    };
    if upstream.connect(dest).await.is_err() {
        return;
    }

    let mut from_guest = [0u8; UDP_BUF];
    let mut from_upstream = [0u8; UDP_BUF];
    loop {
        tokio::select! {
            r = guest.read(&mut from_guest) => match r {
                Ok(0) | Err(_) => break,
                Ok(n) => if upstream.send(&from_guest[..n]).await.is_err() { break; },
            },
            r = upstream.recv(&mut from_upstream) => match r {
                Err(_) => break,
                Ok(n) => if guest.write_all(&from_upstream[..n]).await.is_err() { break; },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use std::time::Duration;

    use crate::tunnel::keys::WgKeypair;
    use crate::tunnel::wg::WgTunnel;

    /// Build a valid IPv4 + UDP packet with correct checksums.
    fn ipv4_udp_packet(src: SocketAddr, dst: SocketAddr, payload: &[u8]) -> Vec<u8> {
        let (IpAddr::V4(sip), IpAddr::V4(dip)) = (src.ip(), dst.ip()) else {
            panic!("ipv4 only");
        };
        let builder = etherparse::PacketBuilder::ipv4(sip.octets(), dip.octets(), 64)
            .udp(src.port(), dst.port());
        let mut out = Vec::with_capacity(builder.size(payload.len()));
        builder.write(&mut out, payload).unwrap();
        out
    }

    /// Drive a real `WireGuard` handshake over localhost UDP and confirm that a
    /// guest UDP packet surfaces through `WgDevice` + `ipstack` as a stream
    /// addressed to the packet's destination. Exercises the whole receive
    /// datapath without a container.
    #[tokio::test]
    async fn loopback_udp_flow_surfaces_stream_with_destination() {
        // npxc side: UDP socket + tunnel + device + ipstack (spawns its driver).
        let npxc_sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let npxc_addr = npxc_sock.local_addr().unwrap();
        let npxc_kp = WgKeypair::generate();
        let guest_kp = WgKeypair::generate();
        let npxc_tunnel = WgTunnel::new(npxc_kp.secret(), guest_kp.public(), 1);
        let device = WgDevice::new(npxc_sock, npxc_tunnel);

        let mut config = IpStackConfig::default();
        let _ = config.mtu(MTU);
        let mut ip_stack = IpStack::new(config, device);

        // guest side: a bare UDP socket + tunnel, pointed at npxc.
        let guest_sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        guest_sock.connect(npxc_addr).await.unwrap();
        let mut guest_tunnel = WgTunnel::new(guest_kp.secret(), npxc_kp.public(), 2);

        let guest_src: SocketAddr = (Ipv4Addr::new(10, 0, 0, 2), 40000).into();
        let dest: SocketAddr = (Ipv4Addr::new(1, 1, 1, 1), 5353).into();
        let packet = ipv4_udp_packet(guest_src, dest, b"hello");

        let guest = tokio::spawn(async move {
            // First encapsulate triggers the handshake and queues `packet`.
            let mut init = Vec::new();
            guest_tunnel.encapsulate(&packet, |b| init.extend_from_slice(b));
            guest_sock.send(&init).await.unwrap();

            // Receive npxc's handshake response and complete the handshake,
            // forwarding any flushed datagrams back.
            let mut buf = [0u8; 2048];
            let n = guest_sock.recv(&mut buf).await.unwrap();
            let mut flushed = Vec::new();
            guest_tunnel.decapsulate(&buf[..n], |b| flushed.extend_from_slice(b), |_| {});
            if !flushed.is_empty() {
                guest_sock.send(&flushed).await.unwrap();
            }

            // Send the data packet over the now-established session.
            let mut data = Vec::new();
            guest_tunnel.encapsulate(&packet, |b| data.extend_from_slice(b));
            if !data.is_empty() {
                guest_sock.send(&data).await.unwrap();
            }

            // Keep the socket alive while npxc processes.
            tokio::time::sleep(Duration::from_millis(300)).await;
        });

        let stream = tokio::time::timeout(Duration::from_secs(5), ip_stack.accept())
            .await
            .expect("timed out waiting for ipstack to surface a stream")
            .expect("ipstack accept error");

        match stream {
            IpStackStream::Udp(udp) => {
                assert_eq!(
                    udp.peer_addr(),
                    dest,
                    "surfaced stream must target the packet's destination",
                );
            }
            _ => panic!("expected a UDP stream"),
        }

        guest.await.unwrap();
    }
}
