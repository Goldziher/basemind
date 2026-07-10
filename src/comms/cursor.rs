//! Opaque pagination cursor for comms history / inbox.
//!
//! A cursor encodes `(room, seq)` — the last message a page returned. Because the comms log
//! is append-only and `seq` is monotonic per room, a cursor never invalidates: resuming from
//! `(room, seq)` always picks up at `seq + 1`. The wire shape is a single base64url
//! (no-pad) string so clients never look inside.

use serde::{Deserialize, Serialize};

/// Opaque resume token. Pass the `next_cursor` from a previous page back as `cursor` to fetch
/// the following page. Stable across daemon restarts — append-only log, content-free position.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cursor(pub String);

/// Decoded cursor payload: the room and the last-seen `seq`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorPos {
    /// Room the cursor belongs to. Empty string ⇒ a cross-room (inbox) cursor.
    pub room: String,
    /// Last `seq` the previous page returned. The next page starts at `seq + 1`.
    pub seq: u64,
}

impl Cursor {
    /// Encode a `(room, seq)` position as an opaque cursor.
    pub fn encode(room: &str, seq: u64) -> Self {
        let payload = CursorPos {
            room: room.to_string(),
            seq,
        };
        let bytes = rmp_serde::to_vec_named(&payload).expect("encoding a (room, seq) cursor never fails");
        Cursor(base64url_encode(&bytes))
    }

    /// Decode a cursor back into its `(room, seq)` position.
    pub fn decode(&self) -> Result<CursorPos, CursorError> {
        let bytes = base64url_decode(&self.0).map_err(|_| CursorError::Malformed)?;
        rmp_serde::from_slice(&bytes).map_err(|_| CursorError::Malformed)
    }
}

/// Why a cursor could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CursorError {
    /// The cursor was not valid base64url or did not decode to a [`CursorPos`].
    #[error("malformed cursor")]
    Malformed,
}

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
            b'=' => continue,
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
    fn cursor_round_trips() {
        let c = Cursor::encode("room-1", 99);
        let pos = c.decode().expect("decode");
        assert_eq!(pos.room, "room-1");
        assert_eq!(pos.seq, 99);
    }

    #[test]
    fn inbox_cursor_has_empty_room() {
        let c = Cursor::encode("", 7);
        assert_eq!(c.decode().expect("decode").room, "");
    }

    #[test]
    fn malformed_cursor_rejected() {
        assert_eq!(Cursor("***".to_string()).decode(), Err(CursorError::Malformed));
    }
}
