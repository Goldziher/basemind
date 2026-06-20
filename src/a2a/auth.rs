//! Bearer-token authentication for the A2A server.
//!
//! Both A2A transports (JSON-RPC and gRPC) share one axum listener, so a single
//! HTTP-level middleware ([`require_bearer`]) enforces auth for both: gRPC carries
//! the token as the HTTP/2 `authorization` header just like the JSON-RPC binding,
//! so no separate tonic interceptor is needed. The public agent card
//! (`/.well-known/agent-card.json`) is always reachable unauthenticated so clients
//! can discover the security scheme before they hold a token.
//!
//! The token is supplied via `--token` or loaded from a `--token-file`
//! ([`load_or_create_token`]) that is auto-generated with `0600` permissions when
//! missing. When no token is configured the server runs unauthenticated and is
//! refused a non-loopback bind (see [`crate::a2a::run_server`]).

use std::path::Path;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::a2a::server::AGENT_CARD_PATH;
use crate::a2a::state::A2aState;

/// Read the bearer token from `path`, generating it if the file is missing.
///
/// On creation the file is written with `0600` permissions (owner read/write
/// only) and seeded with a UUID v4 (122 bits of entropy). An existing file is
/// trimmed of trailing whitespace; an empty file is rejected.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] when the parent directory cannot be
/// created, the file cannot be read or written, or an existing token file is
/// empty.
pub(crate) fn load_or_create_token(path: &Path) -> std::io::Result<String> {
    if path.exists() {
        // Refuse a token file readable by group/other: trusting a world-readable
        // secret would silently defeat the auth it backs.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "auth token file '{}' is group/other accessible (mode {mode:o}); expected 0600",
                        path.display()
                    ),
                ));
            }
        }
        let token = std::fs::read_to_string(path)?;
        let trimmed = token.trim().to_owned();
        if trimmed.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("auth token file '{}' is empty", path.display()),
            ));
        }
        return Ok(trimmed);
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let token = uuid::Uuid::new_v4().to_string();
    write_token_with_owner_only_perms(path, &token)?;
    Ok(token)
}

#[cfg(unix)]
fn write_token_with_owner_only_perms(path: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(token.as_bytes())?;
    file.write_all(b"\n")
}

// NOTE: no filesystem ACL is applied on non-Unix targets. basemind ships
// Apple-Silicon/Unix binaries only (see the `macos-apple-silicon-only` policy),
// so this path is not used in production; harden it before any Windows release.
#[cfg(not(unix))]
fn write_token_with_owner_only_perms(path: &Path, token: &str) -> std::io::Result<()> {
    std::fs::write(path, token)
}

/// Constant-time byte-slice equality, backed by the audited [`subtle`] crate so
/// the comparison is not short-circuited by the optimizer.
///
/// Returns as soon as the lengths differ (length is not a secret here); for
/// equal-length inputs the running time does not depend on the position of the
/// first mismatch, avoiding a timing oracle on the token.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq as _;
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// Extract the token from an `Authorization` header value, matching the `Bearer`
/// scheme case-insensitively (RFC 7235 §2.1) and tolerating extra whitespace.
fn parse_bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.trim_start().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim_start())
}

/// 401 response carrying a `WWW-Authenticate: Bearer` challenge.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        "unauthorized: missing or invalid bearer token\n",
    )
        .into_response()
}

/// axum middleware enforcing `Authorization: Bearer <token>` on every route
/// except the public agent card.
///
/// No-ops when [`A2aState::auth_token`] is `None` (auth disabled). The token is
/// compared in constant time. Covers both the JSON-RPC and the mounted gRPC
/// routes because they share this listener.
pub(crate) async fn require_bearer(
    State(state): State<A2aState>,
    request: Request,
    next: Next,
) -> Response {
    // Auth disabled: pass everything through unchanged.
    let Some(expected) = state.auth_token.as_deref() else {
        return next.run(request).await;
    };

    // The discovery card is always public so clients can read the security
    // scheme before they obtain a token.
    if request.uri().path() == AGENT_CARD_PATH {
        return next.run(request).await;
    }

    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_bearer);

    match provided {
        Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => {
            next.run(request).await
        }
        _ => unauthorized(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_or_create_token_generates_uuid_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.token");
        let token = load_or_create_token(&path).expect("token must be generated");
        assert_eq!(token.len(), 36, "UUID v4 token must be 36 chars: {token}");
        assert!(path.exists(), "token file must be created");
    }

    #[test]
    fn load_or_create_token_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.token");
        let first = load_or_create_token(&path).expect("first call generates");
        let second = load_or_create_token(&path).expect("second call reads");
        assert_eq!(first, second, "token must persist across calls");
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.token");
        load_or_create_token(&path).expect("token must be generated");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "token file must be 0600");
    }

    #[test]
    fn empty_token_file_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.token");
        std::fs::write(&path, "   \n").expect("write empty");
        // 0600 so the permission check passes and we reach the empty-content check.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod 0600");
        }
        let err = load_or_create_token(&path).expect_err("empty token must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn constant_time_eq_matches_only_identical_slices() {
        assert!(constant_time_eq(b"secret-token", b"secret-token"));
        assert!(!constant_time_eq(b"secret-token", b"secret-toker"));
        assert!(!constant_time_eq(b"secret", b"secret-token"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn parse_bearer_is_case_insensitive_and_trims() {
        assert_eq!(parse_bearer("Bearer tok"), Some("tok"));
        assert_eq!(parse_bearer("bearer tok"), Some("tok"));
        assert_eq!(parse_bearer("BEARER tok"), Some("tok"));
        assert_eq!(parse_bearer("  Bearer   tok"), Some("tok"));
        assert_eq!(parse_bearer("Basic tok"), None);
        assert_eq!(parse_bearer("tok"), None);
    }

    #[cfg(unix)]
    #[test]
    fn group_readable_token_file_is_rejected() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.token");
        std::fs::write(&path, "tok\n").expect("write token");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
            .expect("chmod 0640");
        let err = load_or_create_token(&path).expect_err("group-readable file must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }
}
