//! Byte-level composite-key encoders for the comms Fjall store.
//!
//! Mirrors the conventions of `crate::index::keys`: every variable-length component is
//! `u16`-big-endian length-prefixed so a `Foo` prefix never spills into `Foobar`, and the
//! per-room `seq` suffix is `u64`-big-endian so a prefix range scan over `messages_by_room`
//! returns a room's messages in total post order.
//!
//! The encoders take already-validated [`AgentId`](super::ids::AgentId) /
//! [`RoomId`](super::ids::RoomId) handles, so the only failure mode (a component exceeding the
//! `u16` ceiling) is structurally impossible — ids are capped at 128 bytes by
//! [`MAX_ID_LEN`](super::ids::MAX_ID_LEN). The encoders therefore return `Vec<u8>` directly.

/// `u16:len ‖ bytes`. Internal helper. Ids are `<= 128` bytes, so the `u16` cast never
/// truncates; we debug-assert that invariant rather than threading an `Option` through every
/// caller.
fn write_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    debug_assert!(
        bytes.len() <= u16::MAX as usize,
        "comms key component exceeds u16 length ceiling"
    );
    let len = bytes.len() as u16;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn read_len_prefixed<'buf>(buf: &'buf [u8], cursor: &mut usize) -> Option<&'buf [u8]> {
    if buf.len() < *cursor + 2 {
        return None;
    }
    let len = u16::from_be_bytes([buf[*cursor], buf[*cursor + 1]]) as usize;
    *cursor += 2;
    if buf.len() < *cursor + len {
        return None;
    }
    let out = &buf[*cursor..*cursor + len];
    *cursor += len;
    Some(out)
}

/// `messages_by_room`: `u16:len(room) ‖ room ‖ seq:u64_be`.
///
/// The big-endian `seq` suffix gives total order per room: a prefix scan over
/// [`messages_by_room_prefix`] walks a room's messages oldest-first.
pub fn message_by_room(room: &str, seq: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + room.len() + 8);
    write_len_prefixed(&mut out, room.as_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out
}

/// Prefix bytes for "all messages in this room" — feed to `keyspace.prefix(..)`.
pub fn messages_by_room_prefix(room: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + room.len());
    write_len_prefixed(&mut out, room.as_bytes());
    out
}

/// Decode a `messages_by_room` key back into `(room, seq)`.
pub fn parse_message_by_room(key: &[u8]) -> Option<(String, u64)> {
    let mut c = 0;
    let room = read_len_prefixed(key, &mut c)?;
    let room = std::str::from_utf8(room).ok()?.to_string();
    if key.len() < c + 8 {
        return None;
    }
    let seq = u64::from_be_bytes([
        key[c],
        key[c + 1],
        key[c + 2],
        key[c + 3],
        key[c + 4],
        key[c + 5],
        key[c + 6],
        key[c + 7],
    ]);
    Some((room, seq))
}

/// `subs_by_room`: `u16:len(room) ‖ room ‖ u16:len(agent) ‖ agent`.
pub fn sub_by_room(room: &str, agent: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + room.len() + 2 + agent.len());
    write_len_prefixed(&mut out, room.as_bytes());
    write_len_prefixed(&mut out, agent.as_bytes());
    out
}

/// Prefix bytes for "all subscribers of this room".
pub fn subs_by_room_prefix(room: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + room.len());
    write_len_prefixed(&mut out, room.as_bytes());
    out
}

/// Decode a `subs_by_room` key back into `(room, agent)`.
pub fn parse_sub_by_room(key: &[u8]) -> Option<(String, String)> {
    let mut c = 0;
    let room = read_len_prefixed(key, &mut c)?;
    let room = std::str::from_utf8(room).ok()?.to_string();
    let agent = read_len_prefixed(key, &mut c)?;
    let agent = std::str::from_utf8(agent).ok()?.to_string();
    Some((room, agent))
}

/// `cursors`: `u16:len(agent) ‖ agent ‖ u16:len(room) ‖ room`. Value is the agent's last-read
/// `seq` for that room as `u64_be`.
pub fn cursor_key(agent: &str, room: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + agent.len() + 2 + room.len());
    write_len_prefixed(&mut out, agent.as_bytes());
    write_len_prefixed(&mut out, room.as_bytes());
    out
}

/// `rooms` / `agents` primary keys are the raw id bytes — no composite encoding needed, but
/// expose helpers so callers never hand-roll the conversion.
pub fn room_key(room: &str) -> Vec<u8> {
    room.as_bytes().to_vec()
}

/// Primary key for the `agents` keyspace.
pub fn agent_key(agent: &str) -> Vec<u8> {
    agent.as_bytes().to_vec()
}

/// Per-room `seq` counter key inside the `meta` keyspace: `b"seq:" ‖ room`.
pub fn room_seq_meta_key(room: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + room.len());
    out.extend_from_slice(b"seq:");
    out.extend_from_slice(room.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_by_room_round_trips() {
        for (room, seq) in [("room-1", 0u64), ("backend:team", 42), ("x", u64::MAX)] {
            let key = message_by_room(room, seq);
            let (r, s) = parse_message_by_room(&key).expect("parse");
            assert_eq!(r, room);
            assert_eq!(s, seq);
        }
    }

    #[test]
    fn message_keys_sort_by_seq_within_a_room() {
        let k0 = message_by_room("room", 0);
        let k1 = message_by_room("room", 1);
        let k2 = message_by_room("room", 256);
        assert!(k0 < k1, "seq 0 sorts before seq 1");
        assert!(k1 < k2, "seq 1 sorts before seq 256 (big-endian)");
    }

    #[test]
    fn prefix_does_not_spill_into_sibling_room() {
        // `Foo` must not match keys for `Foobar`.
        let foo = messages_by_room_prefix("Foo");
        let foobar_msg = message_by_room("Foobar", 0);
        assert!(
            !foobar_msg.starts_with(&foo),
            "length prefix must isolate Foo from Foobar"
        );
        let foo_msg = message_by_room("Foo", 0);
        assert!(
            foo_msg.starts_with(&foo),
            "Foo message starts with Foo prefix"
        );
    }

    #[test]
    fn sub_by_room_round_trips() {
        let key = sub_by_room("room-1", "agent-1");
        let (r, a) = parse_sub_by_room(&key).expect("parse");
        assert_eq!(r, "room-1");
        assert_eq!(a, "agent-1");
    }

    #[test]
    fn sub_prefix_isolates_rooms() {
        let prefix = subs_by_room_prefix("room");
        assert!(sub_by_room("room", "a").starts_with(&prefix));
        assert!(!sub_by_room("roomx", "a").starts_with(&prefix));
    }

    #[test]
    fn cursor_key_is_deterministic() {
        assert_eq!(cursor_key("a", "r"), cursor_key("a", "r"));
        assert_ne!(cursor_key("a", "r"), cursor_key("r", "a"));
    }

    #[test]
    fn room_seq_meta_key_namespaced() {
        let key = room_seq_meta_key("room-1");
        assert!(key.starts_with(b"seq:"));
        assert_eq!(&key[4..], b"room-1");
    }
}
