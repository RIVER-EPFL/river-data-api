use axum::http::Request;
use std::net::IpAddr;
use tower_governor::{key_extractor::KeyExtractor, GovernorError};

/// IP key extractor with fallback for Docker/local development.
/// Tries X-Forwarded-For, X-Real-IP, then peer address, then falls back to localhost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FallbackIpKeyExtractor;

impl KeyExtractor for FallbackIpKeyExtractor {
    type Key = IpAddr;

    fn extract<T>(&self, req: &Request<T>) -> Result<Self::Key, GovernorError> {
        // Try X-Forwarded-For header first (for reverse proxies)
        if let Some(xff) = req.headers().get("x-forwarded-for") {
            if let Ok(xff_str) = xff.to_str() {
                // Take the first IP in the chain
                if let Some(first_ip) = xff_str.split(',').next() {
                    if let Ok(ip) = first_ip.trim().parse::<IpAddr>() {
                        return Ok(ip);
                    }
                }
            }
        }

        // Try X-Real-IP header
        if let Some(real_ip) = req.headers().get("x-real-ip") {
            if let Ok(ip_str) = real_ip.to_str() {
                if let Ok(ip) = ip_str.parse::<IpAddr>() {
                    return Ok(ip);
                }
            }
        }

        // Try to get peer address from extensions
        if let Some(connect_info) = req
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        {
            return Ok(connect_info.0.ip());
        }

        // Fallback to localhost - allows rate limiting to work in Docker
        // All requests without identifiable IP share the same bucket
        Ok(IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
    }
}
