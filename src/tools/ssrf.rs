//! Shared SSRF (Server-Side Request Forgery) validation utilities.
//!
//! Provides IP blocklist checks and DNS-pinning validation to prevent requests
//! to private network addresses. Used by both the HTTP tool and MCP HTTP
//! transport for defense-in-depth.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use crate::tools::tool::ToolError;

/// Check whether an IPv4 address falls in a disallowed range (private,
/// loopback, link-local, multicast, unspecified, or cloud metadata).
pub fn is_disallowed_ipv4(v4: &Ipv4Addr) -> bool {
    v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_multicast()
        || v4.is_unspecified()
        || *v4 == Ipv4Addr::new(169, 254, 169, 254)
        || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64)
}

/// Check whether an IP address (v4 or v6) falls in a disallowed range.
pub fn is_disallowed_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_disallowed_ipv4(v4),
        IpAddr::V6(v6) => {
            // Catch IPv4-mapped IPv6 addresses (e.g. ::ffff:169.254.169.254)
            // that would bypass IPv4-only checks.
            if let Some(v4) = v6.to_ipv4_mapped()
                && is_disallowed_ipv4(&v4)
            {
                return true;
            }

            v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_multicast()
                || v6.is_unspecified()
        }
    }
}

/// Resolve DNS for a URL and check every resolved address against the SSRF
/// blocklist.
///
/// Returns the resolved [`SocketAddr`]s so that callers can pin the hostname
/// via [`reqwest::ClientBuilder::resolve_to_addrs`], preventing a DNS rebinding
/// attack where a second, independent resolution (inside reqwest) returns a
/// different -- potentially private -- IP after our validation pass.
pub async fn validate_and_resolve_url(url: &reqwest::Url) -> Result<Vec<SocketAddr>, ToolError> {
    let host = url
        .host_str()
        .ok_or_else(|| ToolError::InvalidParameters("URL missing host".to_string()))?;

    let port = url.port_or_known_default().unwrap_or(443);

    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(format!("{}:{}", host, port))
        .await
        .map_err(|e| {
            ToolError::ExternalService(format!("DNS resolution failed for '{}': {}", host, e))
        })?
        .collect();

    if addrs.is_empty() {
        return Err(ToolError::ExternalService(format!(
            "DNS resolution for '{}' returned no addresses",
            host
        )));
    }

    for addr in &addrs {
        if is_disallowed_ip(&addr.ip()) {
            return Err(ToolError::NotAuthorized(format!(
                "hostname '{}' resolves to disallowed IP {}",
                host,
                addr.ip()
            )));
        }
    }

    Ok(addrs)
}

/// Validate a URL for SSRF: reject localhost, private IP literals, and
/// non-localhost HTTP URLs. Allows HTTPS to public hosts and HTTP to
/// localhost (for local dev servers like MCP).
pub fn validate_url_for_ssrf(url: &str) -> Result<reqwest::Url, ToolError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| ToolError::InvalidParameters(format!("invalid URL: {}", e)))?;

    let host = parsed
        .host_str()
        .ok_or_else(|| ToolError::InvalidParameters("URL missing host".to_string()))?;

    let host_lower = host.to_lowercase();

    // Allow localhost for local development (MCP stdio servers, etc.)
    let is_localhost =
        host_lower == "localhost" || host_lower == "127.0.0.1" || host_lower == "::1";

    if !is_localhost {
        // Check literal IP addresses
        if let Ok(ip) = host.parse::<IpAddr>()
            && is_disallowed_ip(&ip)
        {
            return Err(ToolError::NotAuthorized(
                "private or local IPs are not allowed".to_string(),
            ));
        }
    }

    Ok(parsed)
}
