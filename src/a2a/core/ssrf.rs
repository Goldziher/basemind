//! SSRF guard for A2A webhook push-notification targets.
//!
//! Remote agents register webhook URLs that a background worker later POSTs to.
//! This module is the single source of truth for deciding whether a destination
//! address is safe: it rejects loopback, private, link-local, cloud-metadata,
//! and other reserved ranges so a registered webhook cannot be pointed at
//! internal infrastructure (the classic Server-Side Request Forgery vector).
//!
//! Two checkpoints share [`ip_is_blocked`]:
//! - **create-time** — [`validate_webhook_url`] parses the URL and, when the
//!   host is an IP literal, blocks it immediately; hostnames pass here because
//!   they cannot be checked until they resolve.
//! - **delivery-time** — the webhook worker re-runs [`ip_is_blocked`] on every
//!   address a hostname resolves to, defeating DNS-rebinding attacks.
//!
//! Pure `std::net`: no async, no `reqwest`, no extra dependencies. URL parsing
//! is hand-rolled to match the existing style in `push_notifications.rs`
//! (the `url` crate is not enabled under the `a2a` feature).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

// ── Named range constants (no magic numbers — see CLAUDE.md anti-patterns) ────

/// Default port for the `https` scheme.
const HTTPS_DEFAULT_PORT: u16 = 443;
/// Default port for the `http` scheme.
const HTTP_DEFAULT_PORT: u16 = 80;

/// Shared address space / CGNAT: `100.64.0.0/10` (RFC 6598). The `/10` mask
/// fixes the top 6 bits of the first octet, so any first octet in
/// `[100.64, 100.127]` matches.
const CGNAT_FIRST_OCTET: u8 = 100;
const CGNAT_SECOND_OCTET_MIN: u8 = 64;
const CGNAT_SECOND_OCTET_MAX: u8 = 127;

/// Benchmarking range: `198.18.0.0/15` (RFC 2544) — first octet `198`, second
/// octet `18` or `19`.
const BENCHMARK_FIRST_OCTET: u8 = 198;
const BENCHMARK_SECOND_OCTET_LO: u8 = 18;
const BENCHMARK_SECOND_OCTET_HI: u8 = 19;

/// Reserved-for-future-use: `240.0.0.0/4` (RFC 1112 §4) — top 4 bits all set,
/// i.e. first octet `>= 240`. `255.255.255.255` is the broadcast address and is
/// handled by `Ipv4Addr::is_broadcast`, but it also falls in this block.
const RESERVED_FUTURE_FIRST_OCTET_MIN: u8 = 240;

/// IPv6 unique-local prefix `fc00::/7` (RFC 4193): top 7 bits are `1111110`, so
/// the high byte of the first segment is `0xfc` or `0xfd`.
const ULA_HIGH_BYTE_MASK: u16 = 0xfe00;
const ULA_HIGH_BYTE_PATTERN: u16 = 0xfc00;

// ── Public types ──────────────────────────────────────────────────────────────

/// A webhook target parsed + syntactically SSRF-checked at config-create time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookTarget {
    /// URL scheme, lowercased: `"http"` or `"https"`.
    pub scheme: String,
    /// Host without brackets; IPv6 literals are stored un-bracketed.
    pub host: String,
    /// Explicit port, else `443` for `https` / `80` for `http`.
    pub port: u16,
}

/// Reason a webhook URL/IP was rejected by the SSRF guard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SsrfRejected {
    /// Human-readable description of what was rejected.
    pub reason: String,
}

// ── IP blocklist ──────────────────────────────────────────────────────────────

/// Returns `Some(reason)` if `ip` MUST NOT be a webhook destination, else
/// `None`.
///
/// This is the single source of truth for SSRF address filtering, used both at
/// create-time (for IP-literal hosts) and at delivery-time (by the worker, on
/// every DNS-resolved address). IPv4-mapped / IPv4-compatible IPv6 addresses
/// are un-mapped and re-checked against the IPv4 rules so that, e.g.,
/// `::ffff:169.254.169.254` is caught.
pub fn ip_is_blocked(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => ipv4_is_blocked(v4),
        IpAddr::V6(v6) => ipv6_is_blocked(v6),
    }
}

/// Block rules for an IPv4 address. Prefers stable inherent predicates and
/// implements the rest by hand against octets with named constants.
fn ipv4_is_blocked(ip: Ipv4Addr) -> Option<&'static str> {
    if ip.is_loopback() {
        return Some("loopback address (127.0.0.0/8)");
    }
    if ip.is_private() {
        return Some("private address (10/8, 172.16/12, 192.168/16)");
    }
    if ip.is_link_local() {
        // Covers 169.254.0.0/16, including the 169.254.169.254 metadata IP.
        return Some("link-local address (169.254.0.0/16, incl. cloud metadata)");
    }
    if ip.is_unspecified() {
        return Some("unspecified address (0.0.0.0)");
    }
    if ip.is_broadcast() {
        return Some("broadcast address (255.255.255.255)");
    }
    if ip.is_documentation() {
        return Some("documentation address (192.0.2/24, 198.51.100/24, 203.0.113/24)");
    }
    if ip.is_multicast() {
        return Some("multicast address (224.0.0.0/4)");
    }
    let [a, b, _, _] = ip.octets();
    if a == CGNAT_FIRST_OCTET && (CGNAT_SECOND_OCTET_MIN..=CGNAT_SECOND_OCTET_MAX).contains(&b) {
        return Some("shared/CGNAT address (100.64.0.0/10)");
    }
    if a == BENCHMARK_FIRST_OCTET
        && (b == BENCHMARK_SECOND_OCTET_LO || b == BENCHMARK_SECOND_OCTET_HI)
    {
        return Some("benchmarking address (198.18.0.0/15)");
    }
    if a >= RESERVED_FUTURE_FIRST_OCTET_MIN {
        return Some("reserved/future address (240.0.0.0/4)");
    }
    None
}

/// Block rules for an IPv6 address. IPv4-mapped (`::ffff:0:0/96`) and
/// IPv4-compatible (`::/96`) addresses are un-mapped and delegated to the IPv4
/// rules.
fn ipv6_is_blocked(ip: Ipv6Addr) -> Option<&'static str> {
    // Un-map embedded IPv4 first so the full IPv4 blocklist applies.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return ipv4_is_blocked(v4).or(Some("IPv4-mapped address with blocked v4 target"));
    }
    // `to_ipv4` also matches the deprecated IPv4-compatible `::/96` form
    // (excluding `::` and `::1`, which the predicates below already reject).
    if let Some(v4) = ip.to_ipv4()
        && let Some(reason) = ipv4_is_blocked(v4)
    {
        return Some(reason);
    }
    if ip.is_loopback() {
        return Some("IPv6 loopback (::1)");
    }
    if ip.is_unspecified() {
        return Some("IPv6 unspecified (::)");
    }
    if ip.is_multicast() {
        return Some("IPv6 multicast (ff00::/8)");
    }
    let segments = ip.segments();
    if segments[0] & ULA_HIGH_BYTE_MASK == ULA_HIGH_BYTE_PATTERN {
        return Some("IPv6 unique-local (fc00::/7)");
    }
    if is_ipv6_link_local(ip) {
        return Some("IPv6 link-local (fe80::/10)");
    }
    if is_ipv6_documentation(ip) {
        return Some("IPv6 documentation (2001:db8::/32)");
    }
    None
}

/// `fe80::/10` link-local check: top 10 bits are `1111111010`.
fn is_ipv6_link_local(ip: Ipv6Addr) -> bool {
    const LINK_LOCAL_MASK: u16 = 0xffc0;
    const LINK_LOCAL_PATTERN: u16 = 0xfe80;
    ip.segments()[0] & LINK_LOCAL_MASK == LINK_LOCAL_PATTERN
}

/// `2001:db8::/32` documentation check: first two segments fixed.
fn is_ipv6_documentation(ip: Ipv6Addr) -> bool {
    const DOC_SEGMENT_0: u16 = 0x2001;
    const DOC_SEGMENT_1: u16 = 0x0db8;
    let segments = ip.segments();
    segments[0] == DOC_SEGMENT_0 && segments[1] == DOC_SEGMENT_1
}

// ── URL validation ────────────────────────────────────────────────────────────

/// Parse + syntactically validate a webhook URL at config-create time.
///
/// - requires an `http`/`https` scheme and a non-empty host;
/// - if the host is an IP literal, runs [`ip_is_blocked`] and rejects if
///   blocked;
/// - hostnames PASS here — they are DNS-resolved and re-checked against
///   [`ip_is_blocked`] at delivery time.
///
/// # Errors
///
/// Returns [`SsrfRejected`] when the URL is malformed, uses a non-`http(s)`
/// scheme, has an empty host, has an unparseable port, or resolves to a blocked
/// IP literal.
pub fn validate_webhook_url(url: &str) -> Result<WebhookTarget, SsrfRejected> {
    let reject = |reason: String| SsrfRejected { reason };

    let Some((scheme_raw, rest)) = url.split_once("://") else {
        return Err(reject(format!(
            "push notification url '{url}' is invalid: missing scheme separator '://'"
        )));
    };
    let scheme = scheme_raw.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") {
        return Err(reject(format!(
            "push notification url '{url}' must use http or https; got '{scheme}'"
        )));
    }

    // Authority ends at the first path / query / fragment delimiter.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err(reject(format!(
            "push notification url '{url}' is invalid: empty host"
        )));
    }

    // Strip optional `userinfo@` — only the last '@' separates userinfo from
    // the host (userinfo may itself contain '@').
    let host_port = match authority.rsplit_once('@') {
        Some((_userinfo, hp)) => hp,
        None => authority,
    };
    if host_port.is_empty() {
        return Err(reject(format!(
            "push notification url '{url}' is invalid: empty host"
        )));
    }

    let default_port = if scheme == "https" {
        HTTPS_DEFAULT_PORT
    } else {
        HTTP_DEFAULT_PORT
    };
    let (host, port) = split_host_port(host_port, default_port).map_err(|reason| {
        reject(format!(
            "push notification url '{url}' is invalid: {reason}"
        ))
    })?;
    if host.is_empty() {
        return Err(reject(format!(
            "push notification url '{url}' is invalid: empty host"
        )));
    }

    // IP-literal hosts are checked now; hostnames defer to delivery time.
    match host.parse::<IpAddr>() {
        Ok(ip) => {
            if let Some(blocked) = ip_is_blocked(ip) {
                return Err(reject(format!(
                    "push notification url '{url}' targets a blocked address: {blocked}"
                )));
            }
        }
        Err(_) => {
            // The host is not a standard IP literal. Reject the non-standard
            // numeric encodings that `parse::<IpAddr>` rejects but the OS
            // resolver / reqwest's WHATWG URL parser normalize to an IP:
            // decimal (`2130706433`), hex (`0x7f000001`), and octal / short
            // forms (`0177.0.0.1`, `127.1`). These can resolve to internal
            // addresses, and because the delivery-time pin keys on the raw host
            // string it would NOT match reqwest's normalized host — so a host
            // like `0177.0.0.1` could be vetted as one address yet connected to
            // another (loopback). No legitimate DNS hostname is digits-and-dots
            // only or `0x`-prefixed, so rejecting them here closes the gap at
            // the source. Bracketed IPv6 literals already parsed as `Ok` above.
            let is_hex_encoded = host.len() > 2 && host[..2].eq_ignore_ascii_case("0x");
            let is_numeric_encoded = host.bytes().all(|b| b.is_ascii_digit() || b == b'.');
            if is_hex_encoded || is_numeric_encoded {
                return Err(reject(format!(
                    "push notification url '{url}' has a non-standard numeric host encoding"
                )));
            }
        }
    }

    Ok(WebhookTarget {
        scheme,
        host: host.to_owned(),
        port,
    })
}

/// Split a `host[:port]` authority into an un-bracketed host and a port,
/// handling bracketed IPv6 literals (`[::1]:8080`).
///
/// Returns the host (IPv6 literals un-bracketed) and the resolved port,
/// defaulting to `default_port` when no explicit port is present.
fn split_host_port(host_port: &str, default_port: u16) -> Result<(&str, u16), String> {
    if let Some(after_bracket) = host_port.strip_prefix('[') {
        // Bracketed IPv6 literal: `[host]` or `[host]:port`.
        let Some((host, tail)) = after_bracket.split_once(']') else {
            return Err("unterminated IPv6 bracket".to_owned());
        };
        let port = match tail {
            "" => default_port,
            t => {
                let Some(p) = t.strip_prefix(':') else {
                    return Err("unexpected characters after IPv6 bracket".to_owned());
                };
                parse_port(p)?
            }
        };
        return Ok((host, port));
    }

    // Unbracketed: a lone ':' separates host from port. A bare IPv6 literal
    // (multiple ':') without brackets is not valid authority syntax.
    match host_port.rsplit_once(':') {
        Some((host, port_str)) if !host.contains(':') => Ok((host, parse_port(port_str)?)),
        Some(_) => Err("IPv6 literal host must be bracketed".to_owned()),
        None => Ok((host_port, default_port)),
    }
}

/// Parse an explicit port string as a `u16`, rejecting empties and overflow.
fn parse_port(port_str: &str) -> Result<u16, String> {
    port_str
        .parse::<u16>()
        .map_err(|_| format!("port '{port_str}' is not a valid u16"))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse::<Ipv4Addr>().expect("valid IPv4 literal"))
    }

    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse::<Ipv6Addr>().expect("valid IPv6 literal"))
    }

    #[test]
    fn ip_is_blocked_rejects_ipv4_loopback() {
        assert!(ip_is_blocked(v4("127.0.0.1")).is_some());
        assert!(ip_is_blocked(v4("127.255.255.254")).is_some());
    }

    #[test]
    fn ip_is_blocked_rejects_ipv4_private_ranges() {
        assert!(ip_is_blocked(v4("10.0.0.5")).is_some());
        assert!(ip_is_blocked(v4("172.16.0.1")).is_some());
        assert!(ip_is_blocked(v4("172.31.255.255")).is_some());
        assert!(ip_is_blocked(v4("192.168.1.1")).is_some());
    }

    #[test]
    fn ip_is_blocked_rejects_ipv4_link_local_and_metadata() {
        assert!(ip_is_blocked(v4("169.254.0.1")).is_some());
        assert!(
            ip_is_blocked(v4("169.254.169.254")).is_some(),
            "cloud metadata endpoint must be blocked"
        );
    }

    #[test]
    fn ip_is_blocked_rejects_ipv4_unspecified_and_broadcast() {
        assert!(ip_is_blocked(v4("0.0.0.0")).is_some());
        assert!(ip_is_blocked(v4("255.255.255.255")).is_some());
    }

    #[test]
    fn ip_is_blocked_rejects_ipv4_cgnat() {
        assert!(ip_is_blocked(v4("100.64.0.1")).is_some());
        assert!(ip_is_blocked(v4("100.127.255.255")).is_some());
        // 100.63.x and 100.128.x are outside the /10 and are public.
        assert!(ip_is_blocked(v4("100.63.0.1")).is_none());
        assert!(ip_is_blocked(v4("100.128.0.1")).is_none());
    }

    #[test]
    fn ip_is_blocked_rejects_ipv4_benchmarking() {
        assert!(ip_is_blocked(v4("198.18.0.1")).is_some());
        assert!(ip_is_blocked(v4("198.19.255.255")).is_some());
    }

    #[test]
    fn ip_is_blocked_rejects_ipv4_documentation() {
        assert!(ip_is_blocked(v4("192.0.2.1")).is_some());
        assert!(ip_is_blocked(v4("198.51.100.1")).is_some());
        assert!(ip_is_blocked(v4("203.0.113.1")).is_some());
    }

    #[test]
    fn ip_is_blocked_rejects_ipv4_reserved_future_and_multicast() {
        assert!(ip_is_blocked(v4("240.0.0.1")).is_some());
        assert!(ip_is_blocked(v4("250.1.2.3")).is_some());
        assert!(ip_is_blocked(v4("224.0.0.1")).is_some());
        assert!(ip_is_blocked(v4("239.255.255.255")).is_some());
    }

    #[test]
    fn ip_is_blocked_allows_public_ipv4() {
        assert!(ip_is_blocked(v4("8.8.8.8")).is_none());
        assert!(ip_is_blocked(v4("1.1.1.1")).is_none());
    }

    #[test]
    fn ip_is_blocked_rejects_ipv6_loopback_ula_link_local() {
        assert!(ip_is_blocked(v6("::1")).is_some());
        assert!(ip_is_blocked(v6("fc00::1")).is_some());
        assert!(ip_is_blocked(v6("fd12:3456::1")).is_some());
        assert!(ip_is_blocked(v6("fe80::1")).is_some());
    }

    #[test]
    fn ip_is_blocked_rejects_ipv6_unspecified_multicast_documentation() {
        assert!(ip_is_blocked(v6("::")).is_some());
        assert!(ip_is_blocked(v6("ff02::1")).is_some());
        assert!(ip_is_blocked(v6("2001:db8::1")).is_some());
    }

    #[test]
    fn ip_is_blocked_rejects_ipv4_mapped_internal() {
        assert!(
            ip_is_blocked(v6("::ffff:127.0.0.1")).is_some(),
            "IPv4-mapped loopback must be blocked"
        );
        assert!(
            ip_is_blocked(v6("::ffff:169.254.169.254")).is_some(),
            "IPv4-mapped metadata endpoint must be blocked"
        );
        assert!(ip_is_blocked(v6("::ffff:10.0.0.1")).is_some());
    }

    #[test]
    fn ip_is_blocked_allows_public_ipv6() {
        assert!(ip_is_blocked(v6("2606:4700:4700::1111")).is_none());
    }

    #[test]
    fn validate_webhook_url_accepts_https_hostname() {
        let target = validate_webhook_url("https://example.com/hook").expect("must accept");
        assert_eq!(
            target,
            WebhookTarget {
                scheme: "https".to_owned(),
                host: "example.com".to_owned(),
                port: 443,
            }
        );
    }

    #[test]
    fn validate_webhook_url_accepts_http_with_explicit_port() {
        let target = validate_webhook_url("http://example.com:8080/x").expect("must accept");
        assert_eq!(
            target,
            WebhookTarget {
                scheme: "http".to_owned(),
                host: "example.com".to_owned(),
                port: 8080,
            }
        );
    }

    #[test]
    fn validate_webhook_url_accepts_public_ip_literal() {
        let target = validate_webhook_url("https://8.8.8.8/hook").expect("must accept public IP");
        assert_eq!(target.host, "8.8.8.8");
        assert_eq!(target.port, 443);
    }

    #[test]
    fn validate_webhook_url_strips_userinfo() {
        let target =
            validate_webhook_url("https://user:pass@example.com/x").expect("must accept userinfo");
        assert_eq!(target.host, "example.com");
    }

    #[test]
    fn validate_webhook_url_rejects_non_http_scheme() {
        let err = validate_webhook_url("ftp://example.com/x").expect_err("ftp must reject");
        assert!(err.reason.contains("http"), "reason: {}", err.reason);
    }

    #[test]
    fn validate_webhook_url_rejects_missing_scheme() {
        assert!(validate_webhook_url("not a url").is_err());
    }

    #[test]
    fn validate_webhook_url_rejects_empty_host() {
        assert!(validate_webhook_url("http:///path").is_err());
    }

    #[test]
    fn validate_webhook_url_rejects_loopback_literal() {
        assert!(validate_webhook_url("http://127.0.0.1/").is_err());
    }

    #[test]
    fn validate_webhook_url_rejects_metadata_endpoint() {
        let err = validate_webhook_url("http://169.254.169.254/latest/meta-data")
            .expect_err("metadata endpoint must reject");
        assert!(err.reason.contains("blocked"), "reason: {}", err.reason);
    }

    #[test]
    fn validate_webhook_url_rejects_ipv6_loopback_bracketed() {
        assert!(validate_webhook_url("http://[::1]/").is_err());
    }

    #[test]
    fn validate_webhook_url_rejects_private_ipv4_literal() {
        assert!(validate_webhook_url("http://10.0.0.5/").is_err());
    }

    #[test]
    fn validate_webhook_url_accepts_bracketed_public_ipv6_with_port() {
        let target = validate_webhook_url("https://[2606:4700:4700::1111]:8443/hook")
            .expect("public bracketed IPv6 must accept");
        assert_eq!(target.host, "2606:4700:4700::1111");
        assert_eq!(target.port, 8443);
    }

    #[test]
    fn validate_webhook_url_rejects_bad_port() {
        assert!(validate_webhook_url("http://example.com:99999/").is_err());
        assert!(validate_webhook_url("http://example.com:abc/").is_err());
    }

    #[test]
    fn validate_webhook_url_rejects_alternate_ip_encodings() {
        // Decimal, hex, octal, and short forms that parse::<IpAddr> rejects but
        // the OS resolver / reqwest WHATWG parser normalize to an IP literal.
        for url in [
            "http://2130706433/", // decimal 127.0.0.1
            "http://0x7f000001/", // hex 127.0.0.1
            "http://0177.0.0.1/", // octal-leading-zero 127.0.0.1
            "http://127.1/",      // short form 127.0.0.1
            "http://2852039166/", // decimal 169.254.169.254 (metadata)
            "http://0xA9FEA9FE/", // hex 169.254.169.254
        ] {
            assert!(
                validate_webhook_url(url).is_err(),
                "non-standard numeric host encoding must be rejected: {url}",
            );
        }
    }

    #[test]
    fn validate_webhook_url_accepts_numeric_looking_hostnames() {
        // A real hostname with a numeric prefix but an alphabetic label is not
        // an IP encoding and must still be accepted (DNS-checked at delivery).
        assert!(validate_webhook_url("https://1.2.3.4.nip.io/hook").is_ok());
        assert!(validate_webhook_url("https://8-8-8-8.example.com/hook").is_ok());
    }
}
