//! `Url` — boundary-validated URL newtype for the web ingestion MCP tools.
//!
//! Mirrors the discipline of `RelPath`: don't let a bare `String` represent
//! either of two distinct categories in the MCP surface. URLs and repo-relative
//! paths look superficially similar to JSON callers; a typed wrapper at the
//! schema boundary prevents an agent from passing one where the other is
//! expected.
//!
//! Construction enforces the http / https scheme allowlist — no `file://`,
//! `data:`, `javascript:`, or other esoteric schemes leak into the crawler.
//! That allowlist is the single point of trust between an LLM-produced
//! argument and the network stack.
//!
//! Only compiled with `feature = "crawl"`. The newtype is intentionally not a
//! transparent re-export of `url::Url`; we want callers to go through
//! `Url::parse` so the allowlist is the only construction path.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Allowed schemes for the basemind crawler. Adding a scheme here is a
/// security decision — keep the list small.
const ALLOWED_SCHEMES: &[&str] = &["http", "https"];

/// Environment variable that, when set to `1`, disables the private-host
/// denylist. Opt-in escape hatch for users that legitimately crawl loopback /
/// RFC1918 / link-local hosts (e.g. an internal docs server).
const ALLOW_PRIVATE_HOSTS_ENV: &str = "BASEMIND_ALLOW_PRIVATE_HOSTS";

/// Host names that always resolve to the loopback interface and so must be
/// rejected alongside the literal loopback IPs. `url::Url` does not resolve
/// DNS, so a textual `localhost` host never parses into an `IpAddr` — we match
/// it by name. `ip6-localhost` / `ip6-loopback` are the conventional
/// `/etc/hosts` aliases for `::1`.
const LOOPBACK_HOST_NAMES: &[&str] = &["localhost", "ip6-localhost", "ip6-loopback"];

/// Match `candidate` against [`LOOPBACK_HOST_NAMES`] case-insensitively without
/// allocating. A single trailing FQDN dot is stripped first so the absolute
/// form `localhost.` is caught alongside `localhost`.
fn is_loopback_name(candidate: &str) -> bool {
    let candidate = candidate.strip_suffix('.').unwrap_or(candidate);
    LOOPBACK_HOST_NAMES
        .iter()
        .any(|name| name.eq_ignore_ascii_case(candidate))
}

/// Return `true` when the private-host denylist is disabled via the
/// [`ALLOW_PRIVATE_HOSTS_ENV`] escape hatch.
fn private_hosts_allowed() -> bool {
    std::env::var(ALLOW_PRIVATE_HOSTS_ENV).is_ok_and(|v| v == "1")
}

/// Classify an IP address as private / non-routable for SSRF purposes.
///
/// Rejects loopback (`127.0.0.0/8`, `::1`), RFC1918 (`10/8`, `172.16/12`,
/// `192.168/16`), link-local (`169.254/16`, `fe80::/10`), and IPv6
/// unique-local (`fc00::/7`). IPv4-mapped IPv6 addresses are unwrapped first so
/// `::ffff:127.0.0.1` cannot bypass the IPv4 classifiers.
fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_private_v4(mapped);
            }
            is_private_v6(v6)
        }
    }
}

fn is_private_v4(v4: Ipv4Addr) -> bool {
    v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
}

fn is_private_v6(v6: Ipv6Addr) -> bool {
    v6.is_loopback() || v6.is_unspecified() || v6.is_unique_local() || v6.is_unicast_link_local()
}

/// Reject a host that points at a private / loopback / link-local address.
///
/// Returns `Ok(())` when the host is allowed (public, or the escape hatch is
/// set), and [`UrlError::PrivateHost`] otherwise. Hosts that are textual names
/// (not IP literals) only trip the `localhost` check — basemind does not resolve
/// DNS at parse time, so a name that later resolves to a private IP is not
/// caught here (defence-in-depth would require a resolving HTTP client hook).
fn reject_private_host(host: Option<url::Host<&str>>) -> Result<(), UrlError> {
    if private_hosts_allowed() {
        return Ok(());
    }
    match host {
        Some(url::Host::Ipv4(v4)) if is_private_v4(v4) => Err(UrlError::PrivateHost(v4.to_string())),
        Some(url::Host::Ipv6(v6)) if is_private_ip(IpAddr::V6(v6)) => Err(UrlError::PrivateHost(v6.to_string())),
        Some(url::Host::Domain(name)) if is_loopback_name(name) => Err(UrlError::PrivateHost(name.to_string())),
        _ => Ok(()),
    }
}

/// Validated http/https URL. Cheap to clone (`url::Url` is a small struct over
/// a single `String`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Url(url::Url);

impl Url {
    /// Parse a string into a validated `Url`. Returns an error when the input
    /// is not a syntactically valid URL or the scheme is outside the allowlist.
    pub fn parse(input: &str) -> Result<Self, UrlError> {
        let parsed = url::Url::parse(input).map_err(|e| UrlError::Invalid(e.to_string()))?;
        let scheme = parsed.scheme();
        if !ALLOWED_SCHEMES.contains(&scheme) {
            return Err(UrlError::DisallowedScheme(scheme.to_string()));
        }
        reject_private_host(parsed.host())?;
        Ok(Self(parsed))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Host component of the URL when present (always present for absolute
    /// http/https URLs, which is what the parser accepts).
    pub fn host_str(&self) -> Option<&str> {
        self.0.host_str()
    }

    /// Borrow the inner `url::Url`. Use this when handing to `crawlberg`,
    /// which expects a string slice anyway — this stays available for the
    /// rare callsite that needs the parsed components.
    pub fn inner(&self) -> &url::Url {
        &self.0
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl FromStr for Url {
    type Err = UrlError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl AsRef<str> for Url {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UrlError {
    #[error("invalid URL: {0}")]
    Invalid(String),
    #[error("disallowed URL scheme: {0:?} (only http/https are accepted by the basemind crawler)")]
    DisallowedScheme(String),
    #[error(
        "private / loopback / link-local host rejected: {0:?} \
         (set BASEMIND_ALLOW_PRIVATE_HOSTS=1 to allow)"
    )]
    PrivateHost(String),
}

impl Serialize for Url {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.0.as_str())
    }
}

impl<'de> Deserialize<'de> for Url {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Url::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Process-wide lock serializing every test that mutates the
/// [`ALLOW_PRIVATE_HOSTS_ENV`] env var. The env var is one shared process-global
/// resource, so all such tests — across `url`, `web::ingest`, and
/// `mcp::helpers_web` — must contend on the SAME mutex, not per-module ones, or
/// a setter in one module observes a remover in another mid-run. Poisoning is
/// recovered via `into_inner` so one panicking test does not cascade.
#[cfg(test)]
pub(crate) static PRIVATE_HOSTS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl rmcp::schemars::JsonSchema for Url {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Url".into()
    }
    fn json_schema(_: &mut rmcp::schemars::SchemaGenerator) -> rmcp::schemars::Schema {
        rmcp::schemars::json_schema!({
            "type": "string",
            "format": "uri",
            "description": "An absolute http or https URL. Other schemes are rejected at parse time."
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_http_and_https() {
        assert!(Url::parse("http://example.com/").is_ok());
        assert!(Url::parse("https://example.com/path?q=1").is_ok());
    }

    #[test]
    fn rejects_file_scheme() {
        let err = Url::parse("file:///etc/passwd").expect_err("file:// must be rejected");
        match err {
            UrlError::DisallowedScheme(s) => assert_eq!(s, "file"),
            other => panic!("expected DisallowedScheme, got {other:?}"),
        }
    }

    #[test]
    fn rejects_javascript_scheme() {
        assert!(matches!(
            Url::parse("javascript:alert(1)"),
            Err(UrlError::DisallowedScheme(_))
        ));
    }

    #[test]
    fn rejects_data_scheme() {
        assert!(matches!(
            Url::parse("data:text/plain,hello"),
            Err(UrlError::DisallowedScheme(_))
        ));
    }

    #[test]
    fn rejects_relative_path() {
        assert!(matches!(Url::parse("/just/a/path"), Err(UrlError::Invalid(_))));
    }

    #[test]
    fn serde_roundtrips_via_json_string() {
        let u = Url::parse("https://example.com/x").unwrap();
        let json = serde_json::to_string(&u).unwrap();
        assert_eq!(json, "\"https://example.com/x\"");
        let back: Url = serde_json::from_str(&json).unwrap();
        assert_eq!(u, back);
    }

    #[test]
    fn deserialize_rejects_disallowed_scheme() {
        let res: Result<Url, _> = serde_json::from_str("\"file:///etc/passwd\"");
        assert!(res.is_err());
    }

    #[test]
    fn host_str_reports_authority() {
        let u = Url::parse("https://docs.rs/rmcp/").unwrap();
        assert_eq!(u.host_str(), Some("docs.rs"));
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        super::PRIVATE_HOSTS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn rejects_loopback_ipv4() {
        let _g = env_lock();
        unsafe { std::env::remove_var(super::ALLOW_PRIVATE_HOSTS_ENV) };
        assert!(matches!(Url::parse("http://127.0.0.1/"), Err(UrlError::PrivateHost(_))));
    }

    #[test]
    fn rejects_localhost_name() {
        let _g = env_lock();
        unsafe { std::env::remove_var(super::ALLOW_PRIVATE_HOSTS_ENV) };
        assert!(matches!(
            Url::parse("http://localhost:8080/"),
            Err(UrlError::PrivateHost(_))
        ));
        assert!(matches!(Url::parse("http://LOCALHOST/"), Err(UrlError::PrivateHost(_))));
    }

    #[test]
    fn rejects_trailing_dot_localhost() {
        let _g = env_lock();
        unsafe { std::env::remove_var(super::ALLOW_PRIVATE_HOSTS_ENV) };
        assert!(matches!(
            Url::parse("http://localhost./"),
            Err(UrlError::PrivateHost(_))
        ));
    }

    #[test]
    fn rejects_ip6_loopback_aliases() {
        let _g = env_lock();
        unsafe { std::env::remove_var(super::ALLOW_PRIVATE_HOSTS_ENV) };
        for host in ["ip6-localhost", "ip6-loopback", "IP6-LOCALHOST"] {
            assert!(
                matches!(Url::parse(&format!("http://{host}/")), Err(UrlError::PrivateHost(_))),
                "{host} must be rejected as a loopback alias"
            );
        }
    }

    #[test]
    fn rejects_rfc1918_ranges() {
        let _g = env_lock();
        unsafe { std::env::remove_var(super::ALLOW_PRIVATE_HOSTS_ENV) };
        for host in ["10.0.0.1", "172.16.5.5", "192.168.1.1"] {
            assert!(
                matches!(Url::parse(&format!("http://{host}/")), Err(UrlError::PrivateHost(_))),
                "{host} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_link_local_ipv4() {
        let _g = env_lock();
        unsafe { std::env::remove_var(super::ALLOW_PRIVATE_HOSTS_ENV) };
        assert!(matches!(
            Url::parse("http://169.254.169.254/"),
            Err(UrlError::PrivateHost(_))
        ));
    }

    #[test]
    fn rejects_ipv6_loopback_and_unique_local() {
        let _g = env_lock();
        unsafe { std::env::remove_var(super::ALLOW_PRIVATE_HOSTS_ENV) };
        assert!(matches!(Url::parse("http://[::1]/"), Err(UrlError::PrivateHost(_))));
        assert!(
            matches!(Url::parse("http://[fc00::1]/"), Err(UrlError::PrivateHost(_))),
            "fc00::/7 unique-local must be rejected"
        );
        assert!(
            matches!(Url::parse("http://[fe80::1]/"), Err(UrlError::PrivateHost(_))),
            "fe80::/10 link-local must be rejected"
        );
    }

    #[test]
    fn rejects_ipv4_mapped_loopback() {
        let _g = env_lock();
        unsafe { std::env::remove_var(super::ALLOW_PRIVATE_HOSTS_ENV) };
        assert!(matches!(
            Url::parse("http://[::ffff:127.0.0.1]/"),
            Err(UrlError::PrivateHost(_))
        ));
    }

    #[test]
    fn accepts_public_hosts() {
        let _g = env_lock();
        unsafe { std::env::remove_var(super::ALLOW_PRIVATE_HOSTS_ENV) };
        assert!(Url::parse("https://example.com/").is_ok());
        assert!(Url::parse("http://8.8.8.8/").is_ok());
        assert!(Url::parse("http://[2606:4700:4700::1111]/").is_ok());
    }

    #[test]
    fn override_allows_private_hosts() {
        let _g = env_lock();
        unsafe { std::env::set_var(super::ALLOW_PRIVATE_HOSTS_ENV, "1") };
        let result = Url::parse("http://127.0.0.1:9000/");
        let localhost = Url::parse("http://localhost/");
        unsafe { std::env::remove_var(super::ALLOW_PRIVATE_HOSTS_ENV) };
        assert!(result.is_ok(), "override must permit loopback IP");
        assert!(localhost.is_ok(), "override must permit localhost name");
    }
}
