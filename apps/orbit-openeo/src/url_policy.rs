//! **P1-7**: URL policy — SSRF protection for STAC and asset hrefs.
//!
//! Validates a URL before it reaches `reqwest` or `gdal_translate` by
//! checking:
//! - scheme is in an allowlist (default `https`)
//! - resolved host doesn't fall into a denied CIDR (default: loopback,
//!   IMDS, RFC 1918 private space, link-local, ULA, etc.)
//! - href doesn't start with `-` (option-injection vector for
//!   `gdal_translate` — **P1-8**)
//!
//! No external crates — uses `std::net::{IpAddr, Ipv4Addr, Ipv6Addr}`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use thiserror::Error;

/// Errors the URL policy can raise.
#[derive(Debug, Error, PartialEq)]
pub enum UrlPolicyError {
    /// Scheme not in the allowlist.
    #[error("scheme not allowed: {0}")]
    SchemeNotAllowed(String),
    /// Host resolved into a denied CIDR.
    #[error("host {host} resolves into denied range")]
    DeniedRange { host: String },
    /// URL parse failure.
    #[error("malformed url: {0}")]
    Malformed(String),
    /// Looks like a CLI option (leading `-`).
    #[error("href starts with '-' (option-injection): {0}")]
    OptionInjection(String),
}

/// URL policy — schemes allowlist + CIDR deny list.
#[derive(Clone, Debug)]
pub struct UrlPolicy {
    /// Allowed schemes (lower-case; e.g. `["https"]`).
    pub allowed_schemes: Vec<String>,
    /// True iff RFC-1918 private + loopback + link-local + IMDS are
    /// denied. Set to `false` only for local dev with `--allow-private-stac`.
    pub deny_internal_ranges: bool,
}

impl Default for UrlPolicy {
    fn default() -> Self {
        Self {
            allowed_schemes: vec!["https".into()],
            deny_internal_ranges: true,
        }
    }
}

impl UrlPolicy {
    /// Relaxed dev policy — allows http + private ranges.
    #[must_use]
    pub fn relaxed_dev() -> Self {
        Self {
            allowed_schemes: vec!["http".into(), "https".into()],
            deny_internal_ranges: false,
        }
    }

    /// Verify `href` passes the policy.
    pub fn check(&self, href: &str) -> Result<(), UrlPolicyError> {
        if href.starts_with('-') {
            return Err(UrlPolicyError::OptionInjection(href.to_string()));
        }
        let (scheme, rest) = href
            .split_once("://")
            .ok_or_else(|| UrlPolicyError::Malformed(href.to_string()))?;
        let scheme_lower = scheme.to_ascii_lowercase();
        if !self.allowed_schemes.iter().any(|s| s == &scheme_lower) {
            return Err(UrlPolicyError::SchemeNotAllowed(scheme_lower));
        }
        // Extract the host (up to first `/`, `?`, `#`, or end).
        let host_with_port = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        let host = host_with_port
            .rsplit_once('@')
            .map(|(_, h)| h)
            .unwrap_or(host_with_port);
        // IPv6 literals are `[::1]` or `[::1]:port` — strip the brackets
        // and ignore the port. IPv4 hosts split at the last `:`.
        let host_no_port = if let Some(stripped) = host.strip_prefix('[') {
            stripped.split(']').next().unwrap_or(stripped)
        } else if let Some((h, _)) = host.rsplit_once(':') {
            h
        } else {
            host
        };
        if self.deny_internal_ranges {
            // Try to parse as a literal IP first; otherwise leave it
            // alone (DNS resolution is the caller's responsibility — we
            // can't block on it here without an async API).
            if let Ok(ip) = host_no_port.parse::<IpAddr>() {
                if is_internal_ip(&ip) {
                    return Err(UrlPolicyError::DeniedRange { host: host_no_port.to_string() });
                }
            } else if is_obvious_internal_hostname(host_no_port) {
                return Err(UrlPolicyError::DeniedRange { host: host_no_port.to_string() });
            }
        }
        Ok(())
    }
}

/// True iff the IP falls in a known internal / sensitive range.
#[must_use]
pub fn is_internal_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_internal_v4(v4),
        IpAddr::V6(v6) => is_internal_v6(v6),
    }
}

fn is_internal_v4(ip: &Ipv4Addr) -> bool {
    let o = ip.octets();
    // Loopback 127.0.0.0/8
    if o[0] == 127 { return true; }
    // Link-local + AWS IMDS 169.254.0.0/16
    if o[0] == 169 && o[1] == 254 { return true; }
    // RFC 1918
    if o[0] == 10 { return true; }
    if o[0] == 172 && (o[1] & 0xF0) == 16 { return true; }   // 172.16.0.0/12
    if o[0] == 192 && o[1] == 168 { return true; }
    // 0.0.0.0/8 (this network)
    if o[0] == 0 { return true; }
    // 100.64.0.0/10 (CGNAT)
    if o[0] == 100 && (o[1] & 0xC0) == 64 { return true; }
    false
}

fn is_internal_v6(ip: &Ipv6Addr) -> bool {
    // ::1 loopback
    if ip.is_loopback() { return true; }
    // fc00::/7 ULA
    let s = ip.segments();
    if (s[0] & 0xFE00) == 0xFC00 { return true; }
    // fe80::/10 link-local
    if (s[0] & 0xFFC0) == 0xFE80 { return true; }
    // ::ffff:x.x.x.x v4-mapped — delegate to v4 check
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_internal_v4(&v4);
    }
    false
}

fn is_obvious_internal_hostname(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h == "localhost" || h.ends_with(".localhost")
        || h == "metadata.google.internal"
        || h.ends_with(".internal")
        || h.ends_with(".local")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_allows_https_external() {
        let p = UrlPolicy::default();
        assert!(p.check("https://earth-search.aws.element84.com/v1/search").is_ok());
    }

    #[test]
    fn rejects_http_scheme_by_default() {
        let p = UrlPolicy::default();
        assert!(matches!(p.check("http://example.com/x.tif"),
            Err(UrlPolicyError::SchemeNotAllowed(_))));
    }

    #[test]
    fn rejects_file_scheme() {
        let p = UrlPolicy::default();
        assert!(matches!(p.check("file:///etc/passwd"),
            Err(UrlPolicyError::SchemeNotAllowed(_))));
    }

    #[test]
    fn rejects_imds_169_254() {
        let p = UrlPolicy::default();
        assert!(matches!(p.check("https://169.254.169.254/latest/meta-data/"),
            Err(UrlPolicyError::DeniedRange { .. })));
    }

    #[test]
    fn rejects_loopback() {
        let p = UrlPolicy::default();
        assert!(matches!(p.check("https://127.0.0.1:9080/jobs"),
            Err(UrlPolicyError::DeniedRange { .. })));
        assert!(matches!(p.check("https://[::1]:9080/x"),
            Err(UrlPolicyError::DeniedRange { .. })));
    }

    #[test]
    fn rejects_rfc1918_private() {
        let p = UrlPolicy::default();
        assert!(matches!(p.check("https://10.0.0.5/x"),
            Err(UrlPolicyError::DeniedRange { .. })));
        assert!(matches!(p.check("https://172.16.0.1/x"),
            Err(UrlPolicyError::DeniedRange { .. })));
        assert!(matches!(p.check("https://192.168.1.10/x"),
            Err(UrlPolicyError::DeniedRange { .. })));
    }

    #[test]
    fn rejects_internal_hostnames() {
        let p = UrlPolicy::default();
        assert!(matches!(p.check("https://localhost/x"),
            Err(UrlPolicyError::DeniedRange { .. })));
        assert!(matches!(p.check("https://metadata.google.internal/x"),
            Err(UrlPolicyError::DeniedRange { .. })));
    }

    #[test]
    fn rejects_option_injection_dash_prefix() {
        let p = UrlPolicy::default();
        assert!(matches!(p.check("-config GDAL_HTTP_HEADER=X"),
            Err(UrlPolicyError::OptionInjection(_))));
    }

    #[test]
    fn relaxed_dev_allows_loopback_http() {
        let p = UrlPolicy::relaxed_dev();
        assert!(p.check("http://127.0.0.1:9080/jobs").is_ok());
    }

    #[test]
    fn rejects_malformed_url_without_scheme() {
        let p = UrlPolicy::default();
        assert!(matches!(p.check("not-a-url"),
            Err(UrlPolicyError::Malformed(_))));
    }

    #[test]
    fn cgnat_range_is_denied() {
        let p = UrlPolicy::default();
        assert!(matches!(p.check("https://100.64.0.5/x"),
            Err(UrlPolicyError::DeniedRange { .. })));
    }

    #[test]
    fn ipv6_ula_is_denied() {
        let p = UrlPolicy::default();
        assert!(matches!(p.check("https://[fc00::1]/x"),
            Err(UrlPolicyError::DeniedRange { .. })));
    }

    #[test]
    fn external_ipv4_allowed() {
        let p = UrlPolicy::default();
        assert!(p.check("https://8.8.8.8/x").is_ok());
    }
}
