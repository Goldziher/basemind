//! `file://` URL construction for basemind's rendered exports (RFC 8089 / RFC 3986).
//!
//! The `graph` domain's `open` mode hands a human (or a browser) a URL to the export it just wrote.
//! Concatenating the path onto `file://` produces a malformed URL on Windows — `C:\a\b.html` has no
//! leading slash and uses the wrong separator — and on any platform an unencoded space or `#` makes
//! the URL parse as something other than the path it names. [`file_url`] is RFC-conformant on both.

use std::fmt::Write;
use std::path::Path;

/// Bytes that survive a path segment unencoded: RFC 3986 `unreserved` plus the two structural
/// characters this builder emits itself. `/` is the separator, and `:` is a legal `pchar` — leaving
/// it literal keeps a Windows drive letter readable (`file:///C:/…`) instead of `file:///C%3A/…`.
fn is_url_safe(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':')
}

/// Build a spec-valid `file://` URL for an absolute filesystem path.
///
/// Works from the path's raw bytes, so a non-UTF-8 path percent-encodes losslessly instead of being
/// mangled by a `to_string_lossy` round-trip.
pub(crate) fn file_url(path: &Path) -> String {
    from_bytes(path.as_os_str().as_encoded_bytes(), cfg!(windows))
}

/// The platform-parameterized core of [`file_url`], split out so both the POSIX and the Windows
/// shape are testable from any host. `windows` selects backslash-as-separator: on POSIX a backslash
/// is an ordinary filename byte and must not be rewritten into a path boundary.
fn from_bytes(bytes: &[u8], windows: bool) -> String {
    let mut encoded = String::with_capacity(bytes.len() + 8);
    for &byte in bytes {
        match byte {
            b'\\' if windows => encoded.push('/'),
            byte if is_url_safe(byte) => encoded.push(byte as char),
            byte => {
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    // On Windows a leading `//` is a UNC share (`\\server\share\…`), whose server name is the URL
    // *authority*, so it keeps the two slashes. This is gated on the platform because POSIX also
    // permits a leading `//` (and naive path joins produce one), where it is an ordinary rooted
    // path with no authority — emitting `file://tmp/x` there would silently reinterpret the first
    // component as a host. Everything else takes the empty authority: `file://` + `/` + the
    // path. ~keep
    if windows && encoded.starts_with("//") {
        return format!("file:{encoded}");
    }
    format!("file:///{}", encoded.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_absolute_path_keeps_three_slashes() {
        assert_eq!(
            from_bytes(b"/home/u/exports/graph-ab.html", false),
            "file:///home/u/exports/graph-ab.html"
        );
    }

    #[test]
    fn windows_path_becomes_three_slashes_with_forward_separators() {
        assert_eq!(from_bytes(br"C:\Users\x\g.html", true), "file:///C:/Users/x/g.html");
    }

    #[test]
    fn spaces_and_reserved_characters_are_percent_encoded() {
        assert_eq!(
            from_bytes(b"/tmp/my repo/a#b?c.html", false),
            "file:///tmp/my%20repo/a%23b%3Fc.html"
        );
        assert_eq!(
            from_bytes(br"C:\My Docs\g raph.html", true),
            "file:///C:/My%20Docs/g%20raph.html"
        );
    }

    #[test]
    fn posix_backslash_stays_a_filename_byte() {
        assert_eq!(from_bytes(br"/tmp/a\b.html", false), "file:///tmp/a%5Cb.html");
    }

    #[test]
    fn non_utf8_bytes_percent_encode_losslessly() {
        assert_eq!(from_bytes(b"/tmp/\xff\xfe/g.html", false), "file:///tmp/%FF%FE/g.html");
    }

    #[test]
    fn unc_share_keeps_the_server_as_authority() {
        assert_eq!(
            from_bytes(br"\\server\share\g.html", true),
            "file://server/share/g.html"
        );
    }

    /// POSIX also permits a leading `//` (and a naive path join produces one), but there it is an
    /// ordinary rooted path, not a share. Reading it as an authority would point the URL at a host
    /// named after the first path component.
    #[test]
    fn posix_double_slash_is_a_path_not_an_authority() {
        assert_eq!(from_bytes(b"//tmp/exports/g.html", false), "file:///tmp/exports/g.html");
    }

    #[test]
    fn real_path_roundtrips_through_the_platform_builder() {
        let path = std::env::temp_dir().join("basemind-file-url.html");
        let url = file_url(&path);
        assert!(
            url.starts_with("file:///"),
            "a local absolute path takes the empty authority: {url}"
        );
        assert!(url.ends_with("basemind-file-url.html"), "got {url}");
    }
}
