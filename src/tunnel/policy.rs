//! Egress allowlist policy (default-deny).
//!
//! A [`Policy`] is the list of destinations a sandboxed guest is permitted to
//! reach. Each rule matches by **destination IP/CIDR** or by **hostname** (the
//! TLS SNI on port 443 or the HTTP `Host` on port 80, peeked by [`super::peek`])
//! and, optionally, by a single port. Anything not matched by a rule is denied —
//! an empty allowlist denies all egress.
//!
//! The policy is protocol-agnostic (a port rule matches both TCP and UDP); the
//! forwarder applies it per flow before dialing the real destination.

use std::net::IpAddr;

use ipnet::{IpNet, Ipv4Net, Ipv6Net};

use crate::error::NpxcError;

/// An allow/deny decision for a single egress flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The flow matches the allowlist; forward it.
    Allow,
    /// The flow matches no rule; reset/drop it.
    Deny,
}

/// What a rule matches against the connection's destination.
#[derive(Debug, Clone)]
enum HostMatch {
    /// Destination IP within a network. A bare IP is stored as a host route
    /// (`/32` or `/128`).
    Net(IpNet),
    /// Destination hostname (TLS SNI or HTTP `Host`), matched case-insensitively.
    Domain(String),
}

/// Which destination port(s) a rule matches.
#[derive(Debug, Clone)]
enum PortMatch {
    /// Any port.
    Any,
    /// Exactly this port.
    Only(u16),
}

/// One allowlist entry.
#[derive(Debug, Clone)]
struct Rule {
    host: HostMatch,
    port: PortMatch,
}

impl Rule {
    /// Whether this rule permits `(ip, port)` with an optional peeked hostname.
    fn matches(&self, ip: IpAddr, port: u16, hostname: Option<&str>) -> bool {
        let port_ok = match self.port {
            PortMatch::Any => true,
            PortMatch::Only(p) => p == port,
        };
        if !port_ok {
            return false;
        }
        match &self.host {
            HostMatch::Net(net) => net.contains(&ip),
            HostMatch::Domain(d) => hostname.is_some_and(|h| h.eq_ignore_ascii_case(d)),
        }
    }
}

/// A default-deny egress allowlist.
#[derive(Debug, Clone)]
pub struct Policy {
    rules: Vec<Rule>,
    /// The resolver npxc pinned in the guest's `resolv.conf`. DNS to this
    /// address on port 53 is answered by npxc's in-tunnel filtering resolver
    /// (see [`crate::tunnel::dns`]) rather than relayed.
    dns_resolver: IpAddr,
}

impl Policy {
    /// Build a policy from `allow` config entries plus an implicit allowance for
    /// DNS (port 53) to the resolver npxc pinned in the guest's `resolv.conf`.
    ///
    /// The DNS allowance is mandatory: hostname rules are useless if the guest
    /// can't resolve names, and the resolver is one npxc itself chose, so
    /// permitting it is both necessary and safe.
    ///
    /// # Errors
    ///
    /// Returns [`NpxcError::Config`] if any entry is malformed.
    pub fn build(allow: &[String], dns_resolver: IpAddr) -> Result<Self, NpxcError> {
        let mut rules = Vec::with_capacity(allow.len() + 1);
        for entry in allow {
            rules.push(parse_rule(entry)?);
        }
        rules.push(Rule {
            host: HostMatch::Net(host_net(dns_resolver)),
            port: PortMatch::Only(53),
        });
        Ok(Self {
            rules,
            dns_resolver,
        })
    }

    /// Decide whether a flow to `(ip, port)` — optionally carrying a peeked
    /// `hostname` — is permitted.
    #[must_use]
    pub fn evaluate(&self, ip: IpAddr, port: u16, hostname: Option<&str>) -> Decision {
        if self.rules.iter().any(|r| r.matches(ip, port, hostname)) {
            Decision::Allow
        } else {
            Decision::Deny
        }
    }

    /// Whether any domain rule in the allowlist covers `name`, ignoring ports.
    ///
    /// Used by the in-tunnel DNS resolver to decide which names to resolve:
    /// names with a matching domain rule are resolved upstream, all others get
    /// `NXDOMAIN`. `name` should be the bare hostname (no trailing dot).
    #[must_use]
    pub fn allows_name(&self, name: &str) -> bool {
        self.rules.iter().any(|r| match &r.host {
            HostMatch::Domain(d) => d.eq_ignore_ascii_case(name),
            HostMatch::Net(_) => false,
        })
    }

    /// The pinned DNS resolver address. Queries the guest sends here on port 53
    /// are handled by npxc's filtering resolver.
    #[must_use]
    pub fn dns_resolver(&self) -> IpAddr {
        self.dns_resolver
    }
}

/// Validate that every `allow` entry parses, without building a full policy.
///
/// Used at config-resolution time so a typo surfaces before a container starts.
///
/// # Errors
///
/// Returns [`NpxcError::Config`] on the first malformed entry.
pub fn validate(allow: &[String]) -> Result<(), NpxcError> {
    for entry in allow {
        parse_rule(entry)?;
    }
    Ok(())
}

/// Represent a single address as a host-route network (`/32` or `/128`).
fn host_net(ip: IpAddr) -> IpNet {
    match ip {
        IpAddr::V4(a) => IpNet::V4(Ipv4Net::new(a, 32).expect("/32 is a valid prefix")),
        IpAddr::V6(a) => IpNet::V6(Ipv6Net::new(a, 128).expect("/128 is a valid prefix")),
    }
}

/// Parse one `allow` entry: `host[:port]`, `cidr[:port]`, or `[ipv6][:port]`,
/// where `host` is a domain or a bare IP.
fn parse_rule(entry: &str) -> Result<Rule, NpxcError> {
    let (host_part, port) = split_host_port(entry)?;
    let port = port.map_or(PortMatch::Any, PortMatch::Only);

    if let Ok(net) = host_part.parse::<IpNet>() {
        return Ok(Rule {
            host: HostMatch::Net(net),
            port,
        });
    }
    if let Ok(ip) = host_part.parse::<IpAddr>() {
        return Ok(Rule {
            host: HostMatch::Net(host_net(ip)),
            port,
        });
    }
    if host_part.is_empty() {
        return Err(NpxcError::Config(format!(
            "invalid allow entry {entry:?}: empty host"
        )));
    }
    Ok(Rule {
        host: HostMatch::Domain(host_part.to_ascii_lowercase()),
        port,
    })
}

/// Split an entry into its host part and an optional port.
///
/// Handles the three colon cases: bracketed IPv6 (`[::1]:443`, also
/// `[2001:db8::/32]:443` for a CIDR with a port), a single colon (`host:443`,
/// including IPv4 CIDR like `10.0.0.5/32:443`), and bare IPv6 / IPv6 CIDR with
/// multiple colons and no port. IPv6 with a port must use brackets, since the
/// address itself contains colons.
fn split_host_port(entry: &str) -> Result<(&str, Option<u16>), NpxcError> {
    let parse_port = |p: &str| {
        p.parse::<u16>()
            .map_err(|_| NpxcError::Config(format!("invalid port in allow entry {entry:?}")))
    };

    if let Some(rest) = entry.strip_prefix('[') {
        let (addr, after) = rest.split_once(']').ok_or_else(|| {
            NpxcError::Config(format!("unterminated '[' in allow entry {entry:?}"))
        })?;
        if after.is_empty() {
            return Ok((addr, None));
        }
        let port = after
            .strip_prefix(':')
            .ok_or_else(|| NpxcError::Config(format!("expected ':port' after ']' in {entry:?}")))?;
        return Ok((addr, Some(parse_port(port)?)));
    }

    // A single colon means `host:port`; a CIDR's `/32:port` also has one colon.
    // Zero colons (domain or CIDR) or many (bare IPv6) carry no port.
    if entry.bytes().filter(|&b| b == b':').count() == 1 {
        let (host, port) = entry.rsplit_once(':').expect("one colon present");
        return Ok((host, Some(parse_port(port)?)));
    }
    Ok((entry, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DNS: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(1, 1, 1, 1));

    fn policy(entries: &[&str]) -> Policy {
        let owned: Vec<String> = entries.iter().map(|s| (*s).to_string()).collect();
        Policy::build(&owned, DNS).unwrap()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn empty_allowlist_denies_everything_but_dns() {
        let p = policy(&[]);
        assert_eq!(
            p.evaluate(ip("93.184.216.34"), 443, Some("example.com")),
            Decision::Deny
        );
        // DNS to the pinned resolver is always allowed.
        assert_eq!(p.evaluate(DNS, 53, None), Decision::Allow);
        // ...but only on port 53, and only to that resolver.
        assert_eq!(p.evaluate(DNS, 443, None), Decision::Deny);
        assert_eq!(p.evaluate(ip("8.8.8.8"), 53, None), Decision::Deny);
    }

    #[test]
    fn domain_rule_matches_sni_case_insensitively() {
        let p = policy(&["api.anthropic.com:443"]);
        assert_eq!(
            p.evaluate(ip("1.2.3.4"), 443, Some("api.anthropic.com")),
            Decision::Allow
        );
        assert_eq!(
            p.evaluate(ip("1.2.3.4"), 443, Some("API.Anthropic.COM")),
            Decision::Allow
        );
        // Wrong host, wrong port, or no SNI → deny.
        assert_eq!(
            p.evaluate(ip("1.2.3.4"), 443, Some("evil.com")),
            Decision::Deny
        );
        assert_eq!(
            p.evaluate(ip("1.2.3.4"), 80, Some("api.anthropic.com")),
            Decision::Deny
        );
        assert_eq!(p.evaluate(ip("1.2.3.4"), 443, None), Decision::Deny);
    }

    #[test]
    fn domain_rule_without_port_matches_any_port() {
        let p = policy(&["example.com"]);
        assert_eq!(
            p.evaluate(ip("1.2.3.4"), 443, Some("example.com")),
            Decision::Allow
        );
        assert_eq!(
            p.evaluate(ip("1.2.3.4"), 8443, Some("example.com")),
            Decision::Allow
        );
    }

    #[test]
    fn cidr_rule_matches_destination_ip() {
        let p = policy(&["10.0.0.0/24:5432"]);
        assert_eq!(p.evaluate(ip("10.0.0.5"), 5432, None), Decision::Allow);
        assert_eq!(p.evaluate(ip("10.0.0.250"), 5432, None), Decision::Allow);
        assert_eq!(p.evaluate(ip("10.0.1.5"), 5432, None), Decision::Deny);
        assert_eq!(p.evaluate(ip("10.0.0.5"), 5433, None), Decision::Deny);
    }

    #[test]
    fn bare_ip_rule_is_a_host_route() {
        let p = policy(&["10.0.0.5:5432"]);
        assert_eq!(p.evaluate(ip("10.0.0.5"), 5432, None), Decision::Allow);
        assert_eq!(p.evaluate(ip("10.0.0.6"), 5432, None), Decision::Deny);
    }

    #[test]
    fn ip_rule_ignores_hostname() {
        // An IP/port rule allows regardless of any peeked SNI.
        let p = policy(&["93.184.216.34:443"]);
        assert_eq!(
            p.evaluate(ip("93.184.216.34"), 443, Some("anything.example")),
            Decision::Allow
        );
        assert_eq!(p.evaluate(ip("93.184.216.34"), 443, None), Decision::Allow);
    }

    #[test]
    fn bracketed_ipv6_with_port() {
        let p = policy(&["[2001:db8::1]:443"]);
        assert_eq!(p.evaluate(ip("2001:db8::1"), 443, None), Decision::Allow);
        assert_eq!(p.evaluate(ip("2001:db8::2"), 443, None), Decision::Deny);
    }

    #[test]
    fn bare_ipv6_without_port_matches_any_port() {
        let p = policy(&["2001:db8::1"]);
        assert_eq!(p.evaluate(ip("2001:db8::1"), 443, None), Decision::Allow);
        assert_eq!(p.evaluate(ip("2001:db8::1"), 9999, None), Decision::Allow);
    }

    #[test]
    fn ipv6_cidr_rule_without_port() {
        let p = policy(&["2001:db8::/32"]);
        assert_eq!(p.evaluate(ip("2001:db8::dead"), 443, None), Decision::Allow);
        assert_eq!(p.evaluate(ip("2001:dead::1"), 443, None), Decision::Deny);
    }

    #[test]
    fn bracketed_ipv6_cidr_with_port() {
        let p = policy(&["[2001:db8::/32]:443"]);
        assert_eq!(p.evaluate(ip("2001:db8::dead"), 443, None), Decision::Allow);
        assert_eq!(p.evaluate(ip("2001:db8::dead"), 80, None), Decision::Deny);
        assert_eq!(p.evaluate(ip("2001:dead::1"), 443, None), Decision::Deny);
    }

    #[test]
    fn invalid_port_is_an_error() {
        let err = Policy::build(&["host:notaport".to_string()], DNS).unwrap_err();
        assert!(matches!(err, NpxcError::Config(_)));
    }

    #[test]
    fn invalid_port_out_of_range_is_an_error() {
        let err = Policy::build(&["host:70000".to_string()], DNS).unwrap_err();
        assert!(matches!(err, NpxcError::Config(_)));
    }

    #[test]
    fn validate_accepts_well_formed_entries() {
        let entries = vec![
            "api.anthropic.com:443".to_string(),
            "10.0.0.5/32:5432".to_string(),
            "example.com".to_string(),
            "[2001:db8::1]:443".to_string(),
        ];
        assert!(validate(&entries).is_ok());
    }

    #[test]
    fn validate_rejects_bad_entry() {
        assert!(validate(&["good.com:443".to_string(), "bad:port".to_string()]).is_err());
    }

    #[test]
    fn allows_name_matches_domain_rules_case_insensitively() {
        let p = policy(&["api.anthropic.com:443", "10.0.0.0/24:5432"]);
        assert!(p.allows_name("api.anthropic.com"));
        assert!(p.allows_name("API.Anthropic.com"));
        // A name not covered by any domain rule.
        assert!(!p.allows_name("evil.com"));
        // IP/CIDR rules don't grant name resolution.
        assert!(!p.allows_name("10.0.0.5"));
    }

    #[test]
    fn allows_name_ignores_port() {
        // The domain rule has a port, but name resolution is port-agnostic.
        let p = policy(&["example.com:443"]);
        assert!(p.allows_name("example.com"));
    }

    #[test]
    fn dns_resolver_is_recorded() {
        let p = policy(&[]);
        assert_eq!(p.dns_resolver(), DNS);
    }
}
