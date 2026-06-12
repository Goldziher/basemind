//! Opaque pagination cursors for MCP list/search tools.
//!
//! Two backends, one wire shape:
//!
//! * **Fjall-backed** cursors hold the raw last-seen key bytes. On resume the helper
//!   range-scans `(Excluded(last_key)..Excluded(prefix_upper_bound))` so the next page
//!   picks up immediately after the previous one. Stable across rescans because Fjall
//!   keys are content-addressed.
//! * **In-memory** cursors hold a `{ offset, snapshot_id }` pair. `snapshot_id` is whatever
//!   the calling tool uses to detect that its view changed between pages — the source-index
//!   tools use the `cache_generation` AtomicU32, the git-iterator tools derive it from the
//!   first 4 bytes of the HEAD sha. On resume the helper checks the decoded `snapshot_id`
//!   against the current one; on mismatch the response carries `cursor_invalidated = true`
//!   and the caller must restart pagination from the top.
//!
//! Blame tools reuse the in-memory encoder with `snapshot_id = 0` because blame is
//! deterministic per `(suspect_sha, path)` — the `offset` field carries the last returned
//! hunk's `start_line` and there is no invalidation flag.
//!
//! Both encodings are wrapped in a base64url-without-padding `String` so the wire shape
//! is always a single opaque field on the response — clients never look inside.

use rmcp::ErrorData as McpError;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Opaque resume token. Pass `cursor` from the previous response's `next_cursor`
/// to fetch the next page.
///
/// Stability guarantee depends on the source:
/// * Fjall-backed tools (`find_references`, `find_callers`, `memory_list`):
///   cursors are stable across rescans — Fjall keys are content-addressed.
/// * In-memory tools (`search_symbols`, `list_files`): cursors are valid only
///   for the same in-RAM snapshot. If the cache is swapped (rescan), the
///   response carries `cursor_invalidated = true` and the caller must restart.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(transparent)]
pub struct Cursor(pub String);

impl Cursor {
    /// Encode the raw last-seen key bytes from a Fjall scan as an opaque cursor.
    pub(super) fn encode_fjall(last_key: &[u8]) -> Self {
        Cursor(base64url_encode(last_key))
    }

    /// Decode a Fjall cursor back into the raw key bytes.
    pub(super) fn decode_fjall(&self) -> Result<Vec<u8>, McpError> {
        base64url_decode(&self.0)
            .map_err(|e| McpError::invalid_params(format!("invalid cursor: {e}"), None))
    }

    /// Encode an in-memory (offset, snapshot_id) cursor. `pub(crate)` so the
    /// integration-test scaffolding in `lib.rs::testing` can forge cursors with arbitrary
    /// `snapshot_id` values to exercise the `cursor_invalidated` path.
    pub(crate) fn encode_in_memory(offset: u64, snapshot_id: u32) -> Self {
        let payload = InMemCursor {
            o: offset,
            s: snapshot_id,
        };
        // msgpack is already a workspace dep and the payload is two ints — cheap.
        let bytes = rmp_serde::to_vec(&payload).expect("encode in-memory cursor never fails");
        Cursor(base64url_encode(&bytes))
    }

    /// Decode an in-memory cursor back into `(offset, snapshot_id)`.
    pub(super) fn decode_in_memory(&self) -> Result<(u64, u32), McpError> {
        let bytes = base64url_decode(&self.0)
            .map_err(|e| McpError::invalid_params(format!("invalid cursor: {e}"), None))?;
        let payload: InMemCursor = rmp_serde::from_slice(&bytes).map_err(|e| {
            McpError::invalid_params(format!("invalid in-memory cursor payload: {e}"), None)
        })?;
        Ok((payload.o, payload.s))
    }
}

#[derive(Serialize, Deserialize)]
struct InMemCursor {
    o: u64,
    s: u32,
}

/// Compute the exclusive upper bound for a prefix scan: increment the last byte that
/// isn't `0xFF`, dropping trailing `0xFF` bytes. Returns `None` when the prefix is all
/// `0xFF` (the upper bound is "past the end of the keyspace" — use unbounded then).
pub(super) fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.last_mut() {
        if *last == 0xFF {
            out.pop();
            continue;
        }
        *last += 1;
        return Some(out);
    }
    None
}

// ─── tiny base64url codec (no_pad) ───────────────────────────────────────────
//
// We avoid pulling the `base64` crate for ~30 lines of stable encoding logic.
// RFC 4648 §5 (`URL and Filename Safe Alphabet`), no padding.

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        out.push(B64URL[((n >> 18) & 0x3F) as usize] as char);
        out.push(B64URL[((n >> 12) & 0x3F) as usize] as char);
        out.push(B64URL[((n >> 6) & 0x3F) as usize] as char);
        out.push(B64URL[(n & 0x3F) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(B64URL[((n >> 18) & 0x3F) as usize] as char);
        out.push(B64URL[((n >> 12) & 0x3F) as usize] as char);
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(B64URL[((n >> 18) & 0x3F) as usize] as char);
        out.push(B64URL[((n >> 12) & 0x3F) as usize] as char);
        out.push(B64URL[((n >> 6) & 0x3F) as usize] as char);
    }
    out
}

fn base64url_decode(s: &str) -> Result<Vec<u8>, &'static str> {
    let bytes = s.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => continue, // tolerate padding even though we don't emit it
            _ => return Err("non-base64url byte"),
        };
        buf = (buf << 6) | (v as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_roundtrip_random_lengths() {
        for len in 0..32usize {
            let bytes: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(37)).collect();
            let encoded = base64url_encode(&bytes);
            let decoded = base64url_decode(&encoded).expect("decode roundtrips");
            assert_eq!(bytes, decoded, "len={len}");
        }
    }

    #[test]
    fn base64url_rejects_invalid_chars() {
        assert!(base64url_decode("**").is_err());
    }

    #[test]
    fn fjall_cursor_roundtrip() {
        let key = b"\x00\x01\x02\xFFabc";
        let c = Cursor::encode_fjall(key);
        let decoded = c.decode_fjall().expect("decode");
        assert_eq!(decoded, key);
    }

    #[test]
    fn in_memory_cursor_roundtrip() {
        let c = Cursor::encode_in_memory(42, 7);
        let (offset, snapshot_id) = c.decode_in_memory().expect("decode");
        assert_eq!(offset, 42);
        assert_eq!(snapshot_id, 7);
    }

    #[test]
    fn prefix_upper_bound_increments_last_byte() {
        // Use a non-ASCII tail byte so the dictionary-based typos linter
        // doesn't object to plausible-looking three-letter test strings.
        assert_eq!(
            prefix_upper_bound(b"prefix\x01"),
            Some(b"prefix\x02".to_vec())
        );
    }

    #[test]
    fn prefix_upper_bound_strips_trailing_ff() {
        assert_eq!(prefix_upper_bound(b"ab\xFF\xFF"), Some(b"ac".to_vec()));
    }

    #[test]
    fn prefix_upper_bound_all_ff_returns_none() {
        assert_eq!(prefix_upper_bound(&[0xFF, 0xFF]), None);
    }
}
