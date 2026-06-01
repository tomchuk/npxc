//! In-tunnel DNS pinning.
//!
//! The guest's `resolv.conf` points at a single resolver that npxc pinned (see
//! [`super::endpoint`]). Rather than relaying those queries to a real upstream
//! verbatim, npxc answers them itself: a query for a name covered by a domain
//! rule in the [`Policy`] is forwarded to the real resolver and its response
//! relayed back; any other name gets a synthesized `NXDOMAIN`.
//!
//! This closes the "resolve a denied name, then connect by bare IP" gap. It is
//! defense-in-depth — connect-time SNI/IP filtering already blocks the actual
//! egress — but it fails closed at resolution, which is cleaner and auditable.
//!
//! Scope: UDP/53, the overwhelmingly common path. TCP/53 to the resolver still
//! relays (allowed by the implicit DNS rule) and is backstopped by connect-time
//! filtering; pinning it is deferred.
//!
//! Only the query is parsed (to read the name and echo it into `NXDOMAIN`);
//! allowed responses are relayed as opaque bytes, so all record types work.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hickory_proto::op::{Message, ResponseCode};
use ipstack::IpStackUdpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use super::policy::Policy;

/// Buffer for a single DNS message. 4096 covers EDNS0-advertised UDP sizes; the
/// `wg0` MTU bounds anything larger in practice.
const DNS_BUF: usize = 4096;

/// How long to wait for the upstream resolver to answer an allowed query.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(5);

/// What to do with one parsed query.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Resolve `name` upstream (a domain rule covers it).
    Resolve(String),
    /// Answer `NXDOMAIN` for `name` (no rule covers it).
    Refuse(String),
    /// The datagram wasn't a parseable query with a question.
    Unparsable,
}

/// Serve DNS for one guest UDP/53 flow with allowlist pinning.
///
/// `resolver` is the real upstream (the pinned resolver address on port 53) to
/// which allowed queries are forwarded. Runs until the guest flow closes.
pub async fn serve(mut guest: IpStackUdpStream, resolver: SocketAddr, policy: Arc<Policy>) {
    // One upstream socket for this flow, reused across queries.
    let upstream = match UdpSocket::bind(unspecified_for(resolver)).await {
        Ok(sock) => sock,
        Err(e) => {
            debug!(?e, "dns: failed to bind upstream socket");
            return;
        }
    };
    if upstream.connect(resolver).await.is_err() {
        return;
    }

    let mut query = [0u8; DNS_BUF];
    let mut answer = [0u8; DNS_BUF];
    loop {
        let n = match guest.read(&mut query).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let datagram = &query[..n];

        match decide(datagram, &policy) {
            Verdict::Resolve(name) => {
                info!(target: "npxc::egress", proto = "dns", %name, "allow");
                if upstream.send(datagram).await.is_err() {
                    break;
                }
                // Upstream error or timeout: drop the query; the client retries.
                if let Ok(Ok(len)) = timeout(UPSTREAM_TIMEOUT, upstream.recv(&mut answer)).await {
                    if guest.write_all(&answer[..len]).await.is_err() {
                        break;
                    }
                } else {
                    debug!(%name, "dns: upstream did not answer");
                }
            }
            Verdict::Refuse(name) => {
                warn!(target: "npxc::egress", proto = "dns", %name, "deny (nxdomain)");
                if let Some(response) = nxdomain(datagram)
                    && guest.write_all(&response).await.is_err()
                {
                    break;
                }
            }
            Verdict::Unparsable => debug!("dns: dropping unparseable query"),
        }
    }
}

/// Choose an unspecified bind address matching the resolver's family.
fn unspecified_for(resolver: SocketAddr) -> SocketAddr {
    if resolver.is_ipv4() {
        (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    }
}

/// Decide what to do with a raw DNS query datagram.
fn decide(datagram: &[u8], policy: &Policy) -> Verdict {
    let Ok(message) = Message::from_vec(datagram) else {
        return Verdict::Unparsable;
    };
    let Some(query) = message.queries.first() else {
        return Verdict::Unparsable;
    };
    // `to_ascii()` yields a fully-qualified name with a trailing dot.
    let name = query.name().to_ascii();
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    if policy.allows_name(&name) {
        Verdict::Resolve(name)
    } else {
        Verdict::Refuse(name)
    }
}

/// Build an `NXDOMAIN` response echoing the query's id and question.
///
/// Returns `None` only if the query can't be parsed (already handled upstream).
fn nxdomain(query: &[u8]) -> Option<Vec<u8>> {
    let request = Message::from_vec(query).ok()?;
    // `error_msg` builds a response carrying the id, op code, and response code;
    // we then echo the question and mirror the recursion flags.
    let mut response = Message::error_msg(
        request.metadata.id,
        request.metadata.op_code,
        ResponseCode::NXDomain,
    );
    response.metadata.recursion_desired = request.metadata.recursion_desired;
    response.metadata.recursion_available = true;
    response.add_queries(request.queries);
    response.to_vec().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    use hickory_proto::op::{MessageType, OpCode, Query};
    use hickory_proto::rr::{Name, RecordType};

    const DNS: IpAddr = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1));

    fn policy(allow: &[&str]) -> Policy {
        let owned: Vec<String> = allow.iter().map(|s| (*s).to_string()).collect();
        Policy::build(&owned, DNS).unwrap()
    }

    /// Encode a standard recursive A query for `name`.
    fn query_for(name: &str, id: u16) -> Vec<u8> {
        let mut message = Message::new(id, MessageType::Query, OpCode::Query);
        message.metadata.recursion_desired = true;
        let name = Name::from_ascii(name).unwrap();
        message.add_query(Query::query(name, RecordType::A));
        message.to_vec().unwrap()
    }

    #[test]
    fn allowed_name_is_resolved() {
        let p = policy(&["api.anthropic.com:443"]);
        assert_eq!(
            decide(&query_for("api.anthropic.com.", 1), &p),
            Verdict::Resolve("api.anthropic.com".to_string())
        );
    }

    #[test]
    fn denied_name_is_refused() {
        let p = policy(&["api.anthropic.com:443"]);
        assert_eq!(
            decide(&query_for("evil.example.", 2), &p),
            Verdict::Refuse("evil.example".to_string())
        );
    }

    #[test]
    fn garbage_is_unparsable() {
        let p = policy(&[]);
        assert_eq!(decide(&[0xde, 0xad, 0xbe, 0xef], &p), Verdict::Unparsable);
    }

    #[test]
    fn nxdomain_echoes_id_and_question() {
        let query = query_for("denied.example.", 0x1234);
        let response = nxdomain(&query).expect("build NXDOMAIN");
        let parsed = Message::from_vec(&response).unwrap();

        assert_eq!(parsed.metadata.id, 0x1234);
        assert_eq!(parsed.metadata.message_type, MessageType::Response);
        assert_eq!(parsed.metadata.response_code, ResponseCode::NXDomain);
        assert_eq!(parsed.queries.len(), 1);
        assert_eq!(
            parsed.queries[0].name().to_ascii(),
            "denied.example.",
            "the original question must be echoed back"
        );
    }
}
