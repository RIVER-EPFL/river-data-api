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
    pub fn key_for(
        &self,
        peer: Option<IpAddr>,
        forwarded_for: Option<&str>,
        real_ip: Option<&str>,
    ) -> IpAddr {
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
#[path = "tests/rate_limit.rs"]
mod tests;
