use axum::http::Request;
use std::net::IpAddr;
use std::sync::Arc;
use tower_governor::{GovernorError, key_extractor::KeyExtractor};

/// One entry of the trusted-proxy set: a network the forwarded chain may be believed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpCidr {
    base: IpAddr,
    prefix: u8,
}

impl IpCidr {
    /// Parses `10.0.0.0/8` or a bare address, which is its own /32 or /128.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p.parse::<u8>().ok()?)),
            None => (s, None),
        };
        let base: IpAddr = addr.parse().ok()?;
        let full = if base.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(full);
        if prefix > full {
            return None;
        }
        Some(Self { base, prefix })
    }

    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.base, ip) {
            (IpAddr::V4(base), IpAddr::V4(ip)) => {
                prefix_eq(&base.octets(), &ip.octets(), self.prefix)
            }
            (IpAddr::V6(base), IpAddr::V6(ip)) => {
                prefix_eq(&base.octets(), &ip.octets(), self.prefix)
            }
            _ => false,
        }
    }
}

fn prefix_eq(a: &[u8], b: &[u8], prefix: u8) -> bool {
    let whole = usize::from(prefix / 8);
    if a[..whole] != b[..whole] {
        return false;
    }
    let rest = prefix % 8;
    if rest == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rest);
    a[whole] & mask == b[whole] & mask
}

/// The networks a proxy is believed from when nothing is configured: loopback, the RFC1918
/// ranges, CGNAT, link-local and IPv6 loopback and unique-local. Every ingress this API is
/// deployed behind reaches it from one of these, and nothing outside the cluster can.
const DEFAULT_TRUSTED_PROXIES: &[&str] = &[
    "127.0.0.0/8",
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "100.64.0.0/10",
    "169.254.0.0/16",
    "::1/128",
    "fc00::/7",
    "fe80::/10",
];

#[must_use]
pub fn default_trusted_proxies() -> Vec<IpCidr> {
    DEFAULT_TRUSTED_PROXIES
        .iter()
        .filter_map(|s| IpCidr::parse(s))
        .collect()
}

/// Rate-limit key: the client's own address, taken from the connection unless the connection is
/// a trusted proxy that forwarded it.
///
/// The forwarded headers are client input. `X-Forwarded-For` is appended to, not replaced, so its
/// left-most entry is whatever the client wrote and keying on it lets one header per request land
/// every request in a fresh bucket. The right-most entry that is not itself a trusted proxy is the
/// address the outermost trusted hop actually saw, which is the one a client cannot forge.
#[derive(Debug, Clone)]
pub struct FallbackIpKeyExtractor {
    trusted: Arc<[IpCidr]>,
}

impl FallbackIpKeyExtractor {
    #[must_use]
    pub fn new(trusted: &[IpCidr]) -> Self {
        Self {
            trusted: trusted.into(),
        }
    }

    fn trusts(&self, ip: IpAddr) -> bool {
        self.trusted.iter().any(|c| c.contains(ip))
    }

    /// The key, given the peer the connection came from. Split out from `extract` so the rule is
    /// testable without building a request extension.
    #[must_use]
    pub fn key_for(&self, peer: Option<IpAddr>, forwarded_for: Option<&str>, real_ip: Option<&str>) -> IpAddr {
        // No peer at all: no connection info, so there is nothing to believe a header against.
        // Everything shares the loopback bucket, which is what local and test runs want.
        let Some(peer) = peer else {
            return IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        };
        if !self.trusts(peer) {
            return peer;
        }
        if let Some(chain) = forwarded_for
            && let Some(ip) = chain
                .split(',')
                .filter_map(|e| e.trim().parse::<IpAddr>().ok())
                .rev()
                .find(|ip| !self.trusts(*ip))
        {
            return ip;
        }
        // A proxy replaces X-Real-IP rather than appending, so from a trusted peer it is one
        // claim rather than a chain, and it is only consulted when the chain named nobody.
        if let Some(ip) = real_ip.and_then(|s| s.trim().parse::<IpAddr>().ok()) {
            return ip;
        }
        peer
    }
}

impl KeyExtractor for FallbackIpKeyExtractor {
    type Key = IpAddr;

    fn extract<T>(&self, req: &Request<T>) -> Result<Self::Key, GovernorError> {
        let peer = req
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|c| c.0.ip());
        let header = |name| req.headers().get(name).and_then(|v| v.to_str().ok());
        Ok(self.key_for(peer, header("x-forwarded-for"), header("x-real-ip")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extractor() -> FallbackIpKeyExtractor {
        FallbackIpKeyExtractor::new(&default_trusted_proxies())
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn cidr_matches_on_the_prefix_only() {
        let net = IpCidr::parse("172.16.0.0/12").unwrap();
        assert!(net.contains(ip("172.16.0.1")));
        assert!(net.contains(ip("172.31.255.254")));
        assert!(!net.contains(ip("172.32.0.1")));
        assert!(!net.contains(ip("172.15.255.254")));
    }

    #[test]
    fn a_bare_address_is_its_own_host_route() {
        let net = IpCidr::parse("10.1.2.3").unwrap();
        assert!(net.contains(ip("10.1.2.3")));
        assert!(!net.contains(ip("10.1.2.4")));
    }

    #[test]
    fn a_v4_cidr_never_matches_a_v6_address() {
        assert!(!IpCidr::parse("10.0.0.0/8").unwrap().contains(ip("::1")));
    }

    #[test]
    fn an_oversized_prefix_is_not_a_cidr() {
        assert!(IpCidr::parse("10.0.0.0/33").is_none());
        assert!(IpCidr::parse("not-an-address").is_none());
    }

    #[test]
    fn a_spoofed_header_from_an_untrusted_peer_keys_on_the_peer() {
        let key = extractor().key_for(Some(ip("203.0.113.7")), Some("1.2.3.4"), Some("5.6.7.8"));
        assert_eq!(key, ip("203.0.113.7"));
    }

    #[test]
    fn a_spoofed_prefix_from_a_trusted_peer_keys_on_what_the_proxy_appended() {
        // The client wrote "1.2.3.4"; the ingress appended the address it actually saw.
        let key = extractor().key_for(Some(ip("10.42.0.9")), Some("1.2.3.4, 203.0.113.7"), None);
        assert_eq!(key, ip("203.0.113.7"));
    }

    #[test]
    fn a_forged_chain_of_private_addresses_does_not_shorten_the_walk() {
        let key = extractor().key_for(
            Some(ip("10.42.0.9")),
            Some("8.8.8.8, 10.0.0.1, 192.168.1.1, 203.0.113.7"),
            None,
        );
        assert_eq!(key, ip("203.0.113.7"));
    }

    #[test]
    fn a_chain_of_only_trusted_hops_keys_on_the_peer() {
        let key = extractor().key_for(Some(ip("10.42.0.9")), Some("10.0.0.1, 192.168.1.1"), None);
        assert_eq!(key, ip("10.42.0.9"));
    }

    #[test]
    fn real_ip_is_read_from_a_trusted_peer_when_the_chain_names_nobody() {
        let key = extractor().key_for(Some(ip("10.42.0.9")), None, Some("203.0.113.7"));
        assert_eq!(key, ip("203.0.113.7"));
    }

    #[test]
    fn a_trusted_peer_with_no_headers_keys_on_itself() {
        assert_eq!(
            extractor().key_for(Some(ip("10.42.0.9")), None, None),
            ip("10.42.0.9")
        );
    }

    #[test]
    fn unparseable_entries_are_walked_past_rather_than_ending_the_walk() {
        let key = extractor().key_for(
            Some(ip("10.42.0.9")),
            Some("203.0.113.7, unknown, _hidden"),
            None,
        );
        assert_eq!(key, ip("203.0.113.7"));
    }

    #[test]
    fn no_connection_info_falls_back_to_localhost() {
        let key = extractor().key_for(None, Some("1.2.3.4"), None);
        assert_eq!(key, ip("127.0.0.1"));
    }

    #[test]
    fn an_empty_trusted_set_believes_no_header() {
        let strict = FallbackIpKeyExtractor::new(&[]);
        assert_eq!(
            strict.key_for(Some(ip("10.42.0.9")), Some("1.2.3.4, 203.0.113.7"), None),
            ip("10.42.0.9")
        );
    }
}
