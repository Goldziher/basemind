//! Enablement gate and bearer-token authentication for the daemon's streamable-HTTP MCP front-end.
//!
//! ## Why the listener is opt-in
//!
//! The daemon used to bind `127.0.0.1:51786` unconditionally, and the only gate on a request was the
//! `Host` / `:authority` loopback check in [`http_frontend`](super::http_frontend). That check does
//! one job well — it defeats DNS rebinding, where a page on `evil.example` re-resolves its own domain
//! to `127.0.0.1` so the browser treats the loopback server as same-origin — and it does nothing at
//! all against a local process that opens a direct TCP connection and sends `Host: 127.0.0.1`.
//!
//! Behind that port sits the full shared tool router, which in the release build (compiled
//! `--features full`) includes `shell`, enabled by default. So every process on the machine that
//! could reach the port could run commands as the daemon's user. Loopback is a routing property, not
//! an authentication boundary: it is trustworthy only on a machine where no other principal — a
//! second user, a container, a browser extension, an npm postinstall script — can open a socket.
//!
//! The listener therefore no longer exists unless an operator asks for it: [`ALLOW_HTTP_ENV`], set
//! truthy in the *daemon's* environment. An environment variable rather than a config key because
//! the daemon's environment is fixed at spawn and belongs to whoever started it, whereas
//! `<repo>/basemind.toml` is authored by whoever wrote the repository — the same distinction that
//! put `scan.extra_roots` behind `BASEMIND_ALLOW_EXTRA_ROOTS` instead of a config flag. The name
//! joins the existing `BASEMIND_ALLOW_ANY_ROOT` / `BASEMIND_ALLOW_PRIVATE_HOSTS` /
//! `BASEMIND_ALLOW_EXTRA_ROOTS` family, all of which read as "the operator lifts a safety default".
//!
//! ## Why a token on top of the opt-in
//!
//! Enabling the transport must not re-open the hole for anyone who enables it. Every request on both
//! served routes therefore has to present a bearer token drawn from the OS CSPRNG when the daemon
//! started — 32 bytes, hex-encoded — and the check runs before routing, before the daemon is pinned,
//! and before any workspace is named or opened.
//!
//! The token is published in the portfile the daemon already writes, `<comms_dir>/http.addr`, which
//! is created mode `0600` (the mode is set at `open` time, not after the write, so the secret is
//! never briefly world-readable). Filesystem permissions on a per-user directory are what actually
//! separates principals here, so the portfile is both the discovery channel and the grant: a process
//! that can read it is already the daemon's user.
//!
//! A token may be presented two ways, and both are accepted on both routes:
//!
//! * `Authorization: Bearer <token>` — the standard MCP client form, and what `/mcp` callers use.
//! * `?token=<token>` in the query — `/ui` is opened by handing a URL to a *browser* (the `display`
//!   and `ui` tools launch the system opener), and a navigation cannot carry a header. Uniformity
//!   across the two routes is deliberate: a route that silently accepted a weaker credential than its
//!   sibling is exactly the drift this module exists to prevent.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Operator opt-in for the daemon's streamable-HTTP MCP front-end. Truthy (`1`, `true`, `yes`) in
/// the daemon's environment starts the listener; unset or anything else means no socket is bound.
pub const ALLOW_HTTP_ENV: &str = "BASEMIND_ALLOW_HTTP";

/// Query-string key carrying the bearer token, for clients that cannot set a header (a browser
/// navigating to `/ui`).
pub const TOKEN_QUERY_KEY: &str = "token";

/// Portfile name under `comms_dir`. Line 1 is the bound `host:port`, line 2 the bearer token, so
/// tooling reads the address instead of assuming the default port and the credential instead of
/// guessing it.
const PORTFILE_NAME: &str = "http.addr";

/// Owner-only mode for the portfile, matching the comms socket's `0600`. It now carries a
/// credential, so this is load-bearing rather than tidiness.
#[cfg(unix)]
const PORTFILE_MODE: u32 = 0o600;

/// Token entropy. 32 bytes is the conventional "unguessable forever" width for a bearer credential
/// and hex-encodes to 64 characters, which survives a query string and a shell copy-paste unescaped.
const TOKEN_BYTES: usize = 32;

/// The portfile path for a resolved comms dir.
pub fn portfile_path(comms_dir: &Path) -> PathBuf {
    comms_dir.join(PORTFILE_NAME)
}

/// `1` / `true` / `yes`, case- and whitespace-insensitive. Matches the private equivalents in
/// `config::root_guard` and `scanner_candidates`; duplicated for the same reason they are.
fn is_truthy(value: &str) -> bool {
    let value = value.trim();
    value.eq_ignore_ascii_case("1") || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
}

/// Whether the operator has granted the HTTP front-end for this daemon process.
///
/// The decision itself is [`grants_http`], which takes the value rather than reading it: this is a
/// security predicate, and a test that had to `setenv` to exercise it would be mutating the
/// environment of a test binary whose other threads are calling `getenv` — undefined behaviour, and
/// the reason `set_var` is `unsafe`.
pub fn http_enabled() -> bool {
    grants_http(std::env::var(ALLOW_HTTP_ENV).ok().as_deref())
}

/// Whether a raw [`ALLOW_HTTP_ENV`] value (absent, or as the environment reports it) grants the
/// listener. Absent is not a grant, and neither is anything but an explicit truthy word — a
/// mistyped value must fail closed.
fn grants_http(value: Option<&str>) -> bool {
    value.is_some_and(is_truthy)
}

/// One daemon process's bearer token.
///
/// Deliberately not `Debug`/`Display`/`Serialize`: the only ways out are [`HttpToken::as_str`], for
/// the portfile write and the `/ui` URL the tools hand to a browser, and [`HttpToken::matches`].
/// A derived `Debug` would put the credential into any `tracing` event that formatted a struct
/// holding one.
#[derive(Clone)]
pub struct HttpToken(String);

impl HttpToken {
    /// Draw a fresh token from the OS CSPRNG. Fails only if the OS refuses entropy, in which case the
    /// listener must not start — an unauthenticated listener is the bug this exists to fix.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).context("draw the HTTP front-end bearer token from the OS CSPRNG")?;
        Ok(Self(hex::encode(bytes)))
    }

    /// The token as it appears in a header or query string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `presented` is this token, compared without an early exit.
    pub fn matches(&self, presented: &str) -> bool {
        constant_time_eq(self.0.as_bytes(), presented.as_bytes())
    }
}

/// Byte equality that does not return early on the first difference.
///
/// The length is not secret (every token is the same fixed width), so a length mismatch may short- ~keep
/// circuit; the content comparison may not. `black_box` on the accumulator keeps LLVM from ~keep
/// rewriting the fold into a branching `memcmp`. ~keep
fn constant_time_eq(expected: &[u8], presented: &[u8]) -> bool {
    if expected.len() != presented.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in expected.iter().zip(presented) {
        diff |= a ^ b;
    }
    std::hint::black_box(diff) == 0
}

/// Extract the presented token from an `Authorization` header value or a request query, in that
/// order. Returns `None` when neither carries one.
pub fn presented_token(authorization: Option<&str>, query: Option<&str>) -> Option<String> {
    if let Some(raw) = authorization {
        let raw = raw.trim();
        // RFC 7235 makes the scheme case-insensitive; clients send "Bearer", "bearer", and "BEARER".
        // Compared as bytes so a non-ASCII header value cannot split a char boundary and panic.
        let bytes = raw.as_bytes();
        if bytes.len() > 7 && bytes[..6].eq_ignore_ascii_case(b"bearer") && bytes[6] == b' ' {
            let value = raw[7..].trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    let query = query?;
    form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == TOKEN_QUERY_KEY)
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
}

/// A portfile's contents: where the transport is listening and the credential it demands.
pub struct Portfile {
    /// The `host:port` the daemon actually bound.
    pub addr: SocketAddr,
    /// The bearer token, absent only for a portfile written by a pre-token daemon.
    pub token: Option<String>,
}

/// Publish the bound address and token, replacing any previous portfile.
///
/// On Unix the file is *created* `0600` rather than chmod'd after the write: the content is a
/// credential, so the permissive window a write-then-chmod leaves open is not acceptable.
pub fn write_portfile(comms_dir: &Path, addr: &SocketAddr, token: &HttpToken) -> std::io::Result<()> {
    use std::io::Write;

    let path = portfile_path(comms_dir);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(PORTFILE_MODE);
    }
    let mut file = options.open(&path)?;
    writeln!(file, "{addr}")?;
    writeln!(file, "{}", token.as_str())?;
    file.flush()
}

/// Read the published address and token back, if the portfile is present and its address parses.
pub fn read_portfile(comms_dir: &Path) -> Option<Portfile> {
    let raw = std::fs::read_to_string(portfile_path(comms_dir)).ok()?;
    let mut lines = raw.lines();
    let addr: SocketAddr = lines.next()?.trim().parse().ok()?;
    let token = lines
        .next()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Some(Portfile { addr, token })
}

/// Remove the portfile, ignoring an already-absent one. Called when the listener stops and when it
/// declines to start, so discovery never points at an address nothing is serving.
pub fn remove_portfile(comms_dir: &Path) {
    let path = portfile_path(comms_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::debug!(%error, path = %path.display(), "http: removing the portfile failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_wide_and_distinct() {
        let a = HttpToken::generate().expect("generate");
        let b = HttpToken::generate().expect("generate");
        assert_eq!(a.as_str().len(), TOKEN_BYTES * 2, "hex-encoded 32 bytes");
        assert!(a.as_str().chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.as_str(), b.as_str(), "each daemon draws its own token");
    }

    #[test]
    fn matches_only_the_exact_token() {
        let token = HttpToken::generate().expect("generate");
        assert!(token.matches(token.as_str()));
        assert!(!token.matches(""));
        assert!(!token.matches(&token.as_str()[..63]), "a prefix is not the token");
        let mut wrong = token.as_str().to_string();
        wrong.replace_range(0..1, if wrong.starts_with('a') { "b" } else { "a" });
        assert!(!token.matches(&wrong), "one flipped character is rejected");
    }

    #[test]
    fn presented_token_reads_header_then_query() {
        assert_eq!(presented_token(Some("Bearer abc"), None).as_deref(), Some("abc"));
        assert_eq!(presented_token(Some("bearer abc"), None).as_deref(), Some("abc"));
        assert_eq!(presented_token(Some("BEARER  abc "), None).as_deref(), Some("abc"));
        // A non-Bearer scheme falls through to the query rather than being taken literally.
        assert_eq!(
            presented_token(Some("Basic abc"), Some("token=xyz")).as_deref(),
            Some("xyz")
        );
        assert_eq!(
            presented_token(None, Some("root=%2Frepo&token=xyz")).as_deref(),
            Some("xyz")
        );
        assert_eq!(presented_token(None, Some("root=%2Frepo")), None);
        assert_eq!(presented_token(Some("Bearer"), None), None);
        assert_eq!(presented_token(Some("Bearer "), None), None);
        assert_eq!(presented_token(None, None), None);
    }

    #[test]
    fn portfile_round_trips_and_is_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let addr: SocketAddr = "127.0.0.1:51786".parse().unwrap();
        let token = HttpToken::generate().expect("generate");
        write_portfile(dir.path(), &addr, &token).expect("write portfile");

        let read = read_portfile(dir.path()).expect("portfile parses");
        assert_eq!(read.addr, addr);
        assert_eq!(read.token.as_deref(), Some(token.as_str()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(portfile_path(dir.path()))
                .expect("stat portfile")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, PORTFILE_MODE, "the portfile carries a credential");
        }

        remove_portfile(dir.path());
        assert!(read_portfile(dir.path()).is_none(), "removed portfile stops resolving");
        // Removing an absent portfile is a no-op, not a panic.
        remove_portfile(dir.path());
    }

    #[test]
    fn portfile_without_a_token_line_still_parses() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(portfile_path(dir.path()), "127.0.0.1:51786\n").expect("write legacy portfile");
        let read = read_portfile(dir.path()).expect("legacy portfile parses");
        assert_eq!(read.addr, "127.0.0.1:51786".parse::<SocketAddr>().unwrap());
        assert_eq!(read.token, None, "a pre-token daemon published no credential");
    }

    #[test]
    fn only_an_explicit_truthy_grant_starts_the_listener() {
        assert!(!grants_http(None), "an unset variable is not a grant");
        for truthy in ["1", "true", "TRUE", " yes "] {
            assert!(grants_http(Some(truthy)), "{truthy:?} grants the listener");
        }
        // Everything else fails closed, including the near-misses an operator is most likely to
        // type. `on` and `enabled` read as grants to a human and must not be to us. ~keep
        for falsy in [
            "0",
            "false",
            "no",
            "",
            "  ",
            "on",
            "enabled",
            "2",
            "yes please",
            "truthy",
        ] {
            assert!(!grants_http(Some(falsy)), "{falsy:?} does not grant the listener");
        }
    }
}
