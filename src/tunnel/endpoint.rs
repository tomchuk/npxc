//! Egress tunnel datapath.
//!
//! Drives `ipstack` over a [`WgDevice`] and forwards each flow the guest opens
//! to a real host socket. This is the passthrough (allow-all) datapath; egress
//! policy is layered on later.

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use ipstack::{IpStack, IpStackConfig, IpStackStream, IpStackTcpStream, IpStackUdpStream};
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpStream, UdpSocket};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::device::WgDevice;
use super::dns;
use super::keys::WgKeypair;
use super::peek;
use super::policy::{Decision, Policy};
use super::wg::WgTunnel;
use crate::error::NpxcError;

/// Guest-side tunnel MTU. Must match the `wg0` MTU configured in the guest.
pub const MTU: u16 = 1380;

/// Tunnel-internal IPv4 address assigned to the guest's `wg0` interface.
const GUEST_WG_ADDRESS: &str = "10.7.0.2/32";

/// Tunnel-internal IPv6 address assigned to the guest's `wg0` interface (a ULA
/// host route). Lets the guest source v6 packets into the tunnel so npxc can
/// forward v6 egress; the address itself is link-local to the tunnel and never
/// appears on the wire to the internet (npxc NATs via the host's real v6).
const GUEST_WG_ADDRESS6: &str = "fd07::2/128";

/// Resolver the guest is pointed at. Reached through the tunnel (the guest's
/// default route is `wg0`), so DNS is forwarded by npxc regardless of the
/// host's own DNS configuration. The egress policy implicitly permits DNS to
/// this address (see [`Policy::build`]) so hostname rules can resolve.
const TUNNEL_NAMESERVER: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);

/// Well-known port for plaintext HTTP, where the destination host is read from
/// the `Host` request header.
const HTTP_PORT: u16 = 80;

/// Well-known port for HTTPS, where the destination host is read from the TLS
/// `ClientHello` SNI.
const HTTPS_PORT: u16 = 443;

/// UDP/443 carries QUIC (HTTP/3). We can't yet peek its SNI (it's inside the
/// encrypted QUIC Initial), so it is blocked outright; clients fall back to
/// TLS-over-TCP, which we do filter.
const QUIC_PORT: u16 = 443;

/// Standard DNS port. Guest queries to the pinned resolver on this port are
/// answered by npxc's in-tunnel filtering resolver ([`dns`]).
const DNS_PORT: u16 = 53;

/// Per-UDP-flow relay buffer size.
const UDP_BUF: usize = 2048;

/// The running host side of a session's egress tunnel plus the environment the
/// guest entrypoint needs to bring up its end.
pub struct TunnelSetup {
    /// The live datapath; keep it alive for the session's duration.
    pub tunnel: Tunnel,
    /// `NPXC_WG_*` variables to inject into the container.
    pub env: Vec<(String, String)>,
    /// Per-session `resolv.conf` to mount over the guest's `/etc/resolv.conf`
    /// so DNS routes through the tunnel. Kept alive (and deleted) with the
    /// session.
    pub resolv_conf: NamedTempFile,
}

/// Establish the host side of the egress tunnel.
///
/// Generates the per-session `WireGuard` keypairs, binds a UDP socket, spawns
/// the datapath, and returns the running [`Tunnel`] together with the
/// environment variables the guest entrypoint reads to configure `wg0`.
///
/// `gateway` is the container network's gateway — the host address the guest
/// reaches over the host-only network, and thus the `WireGuard` endpoint it
/// dials. `allow` is the egress allowlist from the package config; an empty
/// list denies everything except DNS.
///
/// # Errors
///
/// Returns [`NpxcError::Config`] if an `allow` entry is malformed, or
/// [`NpxcError::Runtime`] if the UDP socket cannot be bound.
pub async fn establish(gateway: IpAddr, allow: &[String]) -> Result<TunnelSetup, NpxcError> {
    let policy = Arc::new(Policy::build(allow, IpAddr::V4(TUNNEL_NAMESERVER))?);
    let npxc_kp = WgKeypair::generate();
    let guest_kp = WgKeypair::generate();

    // Bind to all interfaces (ephemeral port). The gateway address may not be
    // assigned to a host interface until a container attaches, and WireGuard
    // authentication drops any datagram that isn't from our peer, so binding
    // broadly is both robust and safe.
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .await
        .map_err(|e| NpxcError::Runtime(format!("failed to bind tunnel UDP socket: {e}")))?;
    let port = socket
        .local_addr()
        .map_err(|e| NpxcError::Runtime(format!("tunnel socket has no local address: {e}")))?
        .port();

    let device = WgDevice::new(
        socket,
        WgTunnel::new(npxc_kp.secret(), guest_kp.public(), 1),
    );
    let tunnel = Tunnel::spawn(device, policy);

    let env = guest_env(&guest_kp, &npxc_kp, gateway, port);
    let resolv_conf = write_resolv_conf()?;
    Ok(TunnelSetup {
        tunnel,
        env,
        resolv_conf,
    })
}

/// Write a per-session `resolv.conf` pointing at a tunnel-routable resolver.
///
/// It is made world-readable so the container's unprivileged `node` user can
/// read it once mounted over `/etc/resolv.conf`.
fn write_resolv_conf() -> Result<NamedTempFile, NpxcError> {
    let mut file = tempfile::Builder::new()
        .prefix("npxc-resolv-")
        .tempfile()
        .map_err(NpxcError::Io)?;
    writeln!(file, "nameserver {TUNNEL_NAMESERVER}").map_err(NpxcError::Io)?;
    file.flush().map_err(NpxcError::Io)?;
    std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o644))
        .map_err(NpxcError::Io)?;
    Ok(file)
}

/// Build the `NPXC_WG_*` environment the guest entrypoint consumes.
fn guest_env(
    guest: &WgKeypair,
    npxc: &WgKeypair,
    gateway: IpAddr,
    port: u16,
) -> Vec<(String, String)> {
    vec![
        ("NPXC_WG_PRIVATE_KEY".to_string(), guest.private_base64()),
        ("NPXC_WG_PEER_PUBLIC_KEY".to_string(), npxc.public_base64()),
        ("NPXC_WG_ENDPOINT".to_string(), format!("{gateway}:{port}")),
        ("NPXC_WG_ADDRESS".to_string(), GUEST_WG_ADDRESS.to_string()),
        (
            "NPXC_WG_ADDRESS6".to_string(),
            GUEST_WG_ADDRESS6.to_string(),
        ),
        ("NPXC_WG_MTU".to_string(), MTU.to_string()),
    ]
}

/// A running tunnel datapath. Dropping or [`abort`](Tunnel::abort)ing it tears
/// down the `ipstack` accept loop (in-flight forwarders finish on their own).
pub struct Tunnel {
    task: JoinHandle<()>,
}

impl Tunnel {
    /// Spawn the datapath over `device`, forwarding each flow the `policy`
    /// permits and resetting the rest.
    #[must_use]
    pub fn spawn(device: WgDevice, policy: Arc<Policy>) -> Self {
        let mut config = IpStackConfig::default();
        let _ = config.mtu(MTU);
        let mut ip_stack = IpStack::new(config, device);

        let task = tokio::spawn(async move {
            loop {
                match ip_stack.accept().await {
                    Ok(IpStackStream::Tcp(tcp)) => {
                        tokio::spawn(forward_tcp(tcp, Arc::clone(&policy)));
                    }
                    Ok(IpStackStream::Udp(udp)) => {
                        tokio::spawn(forward_udp(udp, Arc::clone(&policy)));
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

/// Emit a structured egress audit event for one flow decision.
///
/// Allowed flows log at `info`, denied flows at `warn`, both under the
/// `npxc::egress` target so an operator can collect the egress decision stream
/// independently of the rest of npxc's logging (e.g. `NPXC_LOG=npxc::egress=info`).
/// `host` is the peeked SNI/`Host` when known, `-` otherwise.
fn audit(proto: &str, dest: SocketAddr, hostname: Option<&str>, decision: Decision) {
    let host = hostname.unwrap_or("-");
    match decision {
        Decision::Allow => info!(target: "npxc::egress", proto, %dest, host, "allow"),
        Decision::Deny => warn!(target: "npxc::egress", proto, %dest, host, "deny"),
    }
}

/// Forward a guest TCP connection to its real destination if the policy allows.
///
/// For HTTPS/HTTP the destination hostname is peeked (TLS SNI / HTTP `Host`)
/// before the policy decision; the peeked bytes are replayed to the upstream
/// socket so the connection is byte-for-byte intact. A denied flow is dropped,
/// which tears down the guest's connection.
async fn forward_tcp(mut guest: IpStackTcpStream, policy: Arc<Policy>) {
    let dest = guest.peer_addr();

    // Peek a hostname on the well-known TLS/HTTP ports so domain rules apply.
    // `prefix` holds whatever was consumed during the peek, for replay.
    let mut prefix = Vec::new();
    let hostname = match dest.port() {
        HTTPS_PORT => peek::tls_sni(&mut guest, &mut prefix).await,
        HTTP_PORT => peek::http_host(&mut guest, &mut prefix).await,
        _ => None,
    };

    let decision = policy.evaluate(dest.ip(), dest.port(), hostname.as_deref());
    audit("tcp", dest, hostname.as_deref(), decision);
    if decision == Decision::Deny {
        return;
    }

    let mut upstream = match TcpStream::connect(dest).await {
        Ok(upstream) => upstream,
        Err(e) => {
            debug!(?e, %dest, "tcp connect failed");
            return;
        }
    };

    // Replay the bytes consumed while peeking before splicing the rest.
    if !prefix.is_empty()
        && let Err(e) = upstream.write_all(&prefix).await
    {
        debug!(?e, %dest, "failed to replay peeked bytes");
        return;
    }

    if let Err(e) = copy_bidirectional(&mut guest, &mut upstream).await {
        debug!(?e, %dest, "tcp forward ended");
    }
}

/// Forward a guest UDP flow to its real destination by relaying datagrams, if
/// the policy allows. UDP carries no peekable hostname, so the decision is by
/// IP/port alone (DNS to the pinned resolver is implicitly permitted).
async fn forward_udp(mut guest: IpStackUdpStream, policy: Arc<Policy>) {
    let dest = guest.peer_addr();

    // Block QUIC: it can't be SNI-filtered yet, so deny UDP/443 unconditionally
    // and let the client fall back to TLS-over-TCP (which we do filter).
    if dest.port() == QUIC_PORT {
        warn!(target: "npxc::egress", proto = "udp", %dest, host = "-", "deny: udp/443 (quic) blocked");
        return;
    }

    // DNS pinning: answer queries to the pinned resolver ourselves, returning
    // records only for allowlisted names (NXDOMAIN otherwise).
    if dest.ip() == policy.dns_resolver() && dest.port() == DNS_PORT {
        dns::serve(guest, dest, policy).await;
        return;
    }

    let decision = policy.evaluate(dest.ip(), dest.port(), None);
    audit("udp", dest, None, decision);
    if decision == Decision::Deny {
        return;
    }

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

    #[test]
    fn guest_env_has_expected_keys_and_endpoint() {
        let guest = WgKeypair::generate();
        let npxc = WgKeypair::generate();
        let gateway: IpAddr = Ipv4Addr::new(192, 168, 66, 1).into();
        let env = guest_env(&guest, &npxc, gateway, 51820);

        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(map["NPXC_WG_PRIVATE_KEY"], guest.private_base64());
        assert_eq!(map["NPXC_WG_PEER_PUBLIC_KEY"], npxc.public_base64());
        assert_eq!(map["NPXC_WG_ENDPOINT"], "192.168.66.1:51820");
        assert_eq!(map["NPXC_WG_ADDRESS"], GUEST_WG_ADDRESS);
        assert_eq!(map["NPXC_WG_ADDRESS6"], GUEST_WG_ADDRESS6);
        assert_eq!(map["NPXC_WG_MTU"], MTU.to_string());
        // The guest gets its own private key and npxc's public key — never the
        // reverse, so npxc's secret stays on the host.
        assert_ne!(map["NPXC_WG_PRIVATE_KEY"], npxc.private_base64());
    }

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

    /// Build a valid IPv6 + UDP packet with correct checksums.
    fn ipv6_udp_packet(src: SocketAddr, dst: SocketAddr, payload: &[u8]) -> Vec<u8> {
        let (IpAddr::V6(sip), IpAddr::V6(dip)) = (src.ip(), dst.ip()) else {
            panic!("ipv6 only");
        };
        let builder = etherparse::PacketBuilder::ipv6(sip.octets(), dip.octets(), 64)
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

    /// The same end-to-end receive path, but with an **IPv6** inner packet. The
    /// outer `WireGuard` transport is still IPv4 (as over vmnet); only the
    /// tunneled payload is v6. Proves npxc's datapath carries v6 egress — the
    /// decrypted v6 packet must surface as an `ipstack` stream addressed to the
    /// v6 destination.
    #[tokio::test]
    async fn loopback_ipv6_udp_flow_surfaces_v6_stream() {
        let npxc_sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let npxc_addr = npxc_sock.local_addr().unwrap();
        let npxc_kp = WgKeypair::generate();
        let guest_kp = WgKeypair::generate();
        let npxc_tunnel = WgTunnel::new(npxc_kp.secret(), guest_kp.public(), 1);
        let device = WgDevice::new(npxc_sock, npxc_tunnel);

        let mut config = IpStackConfig::default();
        let _ = config.mtu(MTU);
        let mut ip_stack = IpStack::new(config, device);

        let guest_sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        guest_sock.connect(npxc_addr).await.unwrap();
        let mut guest_tunnel = WgTunnel::new(guest_kp.secret(), npxc_kp.public(), 2);

        let guest_src: SocketAddr = (Ipv6Addr::new(0xfd07, 0, 0, 0, 0, 0, 0, 2), 40000).into();
        let dest: SocketAddr = (
            Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111),
            5353,
        )
            .into();
        let packet = ipv6_udp_packet(guest_src, dest, b"hello6");

        let guest = tokio::spawn(async move {
            let mut init = Vec::new();
            guest_tunnel.encapsulate(&packet, |b| init.extend_from_slice(b));
            guest_sock.send(&init).await.unwrap();

            let mut buf = [0u8; 2048];
            let n = guest_sock.recv(&mut buf).await.unwrap();
            let mut flushed = Vec::new();
            guest_tunnel.decapsulate(&buf[..n], |b| flushed.extend_from_slice(b), |_| {});
            if !flushed.is_empty() {
                guest_sock.send(&flushed).await.unwrap();
            }

            let mut data = Vec::new();
            guest_tunnel.encapsulate(&packet, |b| data.extend_from_slice(b));
            if !data.is_empty() {
                guest_sock.send(&data).await.unwrap();
            }

            tokio::time::sleep(Duration::from_millis(300)).await;
        });

        let stream = tokio::time::timeout(Duration::from_secs(5), ip_stack.accept())
            .await
            .expect("timed out waiting for ipstack to surface a v6 stream")
            .expect("ipstack accept error");

        match stream {
            IpStackStream::Udp(udp) => {
                assert_eq!(
                    udp.peer_addr(),
                    dest,
                    "surfaced stream must target the v6 packet's destination",
                );
            }
            _ => panic!("expected a UDP stream"),
        }

        guest.await.unwrap();
    }
}
