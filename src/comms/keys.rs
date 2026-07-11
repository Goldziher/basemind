//! Byte-level composite-key encoders for the comms Fjall store.
//!
//! Mirrors the conventions of `crate::index::keys`: every variable-length component is
//! `u16`-big-endian length-prefixed so a `Foo` prefix never spills into `Foobar`, and the
//! per-thread `seq` suffix is `u64`-big-endian so a prefix range scan over `messages_by_thread`
//! returns a thread's messages in total post order.
//!
//! The encoders take already-validated [`AgentId`](super::ids::AgentId) /
//! [`ThreadId`](super::ids::ThreadId) handles, so the only failure mode (a component exceeding
//! the `u16` ceiling) is structurally impossible — ids are capped at 128 bytes by
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

/// `messages_by_thread`: `u16:len(thread) ‖ thread ‖ seq:u64_be`.
///
/// The big-endian `seq` suffix gives total order per thread: a prefix scan over
/// [`messages_by_thread_prefix`] walks a thread's messages oldest-first.
pub fn message_by_thread(thread: &str, seq: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + thread.len() + 8);
    write_len_prefixed(&mut out, thread.as_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out
}

/// Prefix bytes for "all messages in this thread" — feed to `keyspace.prefix(..)`.
pub fn messages_by_thread_prefix(thread: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + thread.len());
    write_len_prefixed(&mut out, thread.as_bytes());
    out
}

/// Decode a `messages_by_thread` key back into `(thread, seq)`.
pub fn parse_message_by_thread(key: &[u8]) -> Option<(String, u64)> {
    let mut c = 0;
    let thread = read_len_prefixed(key, &mut c)?;
    let thread = std::str::from_utf8(thread).ok()?.to_string();
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
    Some((thread, seq))
}

/// `thread_members` / `thread_subs`: `u16:len(thread) ‖ thread ‖ u16:len(agent) ‖ agent`.
pub fn thread_agent(thread: &str, agent: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + thread.len() + 2 + agent.len());
    write_len_prefixed(&mut out, thread.as_bytes());
    write_len_prefixed(&mut out, agent.as_bytes());
    out
}

/// Prefix bytes for "all agents in this thread".
pub fn thread_agent_prefix(thread: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + thread.len());
    write_len_prefixed(&mut out, thread.as_bytes());
    out
}

/// Decode a `thread_members` / `thread_subs` key back into `(thread, agent)`.
pub fn parse_thread_agent(key: &[u8]) -> Option<(String, String)> {
    let mut c = 0;
    let thread = read_len_prefixed(key, &mut c)?;
    let thread = std::str::from_utf8(thread).ok()?.to_string();
    let agent = read_len_prefixed(key, &mut c)?;
    let agent = std::str::from_utf8(agent).ok()?.to_string();
    Some((thread, agent))
}

/// `cursors`: `u16:len(agent) ‖ agent ‖ u16:len(thread) ‖ thread`. Value is the agent's last-read
/// `seq` for that thread as `u64_be`.
pub fn cursor_key(agent: &str, thread: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + agent.len() + 2 + thread.len());
    write_len_prefixed(&mut out, agent.as_bytes());
    write_len_prefixed(&mut out, thread.as_bytes());
    out
}

/// `threads` primary key: the raw thread id bytes — no composite encoding needed, but expose a
/// helper so callers never hand-roll the conversion.
pub fn thread_key(thread: &str) -> Vec<u8> {
    thread.as_bytes().to_vec()
}

/// Primary key for the `agents` keyspace.
pub fn agent_key(agent: &str) -> Vec<u8> {
    agent.as_bytes().to_vec()
}

/// Per-thread `seq` counter key inside the `meta` keyspace: `b"seq:" ‖ thread`.
pub fn thread_seq_meta_key(thread: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + thread.len());
    out.extend_from_slice(b"seq:");
    out.extend_from_slice(thread.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_by_thread_round_trips() {
        for (thread, seq) in [("th-1", 0u64), ("backend:team", 42), ("x", u64::MAX)] {
            let key = message_by_thread(thread, seq);
            let (t, s) = parse_message_by_thread(&key).expect("parse");
            assert_eq!(t, thread);
            assert_eq!(s, seq);
        }
    }

    #[test]
    fn message_keys_sort_by_seq_within_a_thread() {
        let k0 = message_by_thread("th", 0);
        let k1 = message_by_thread("th", 1);
        let k2 = message_by_thread("th", 256);
        assert!(k0 < k1, "seq 0 sorts before seq 1");
        assert!(k1 < k2, "seq 1 sorts before seq 256 (big-endian)");
    }

    #[test]
    fn prefix_does_not_spill_into_sibling_thread() {
        let foo = messages_by_thread_prefix("Foo");
        let foobar_msg = message_by_thread("Foobar", 0);
        assert!(
            !foobar_msg.starts_with(&foo),
            "length prefix must isolate Foo from Foobar"
        );
        let foo_msg = message_by_thread("Foo", 0);
        assert!(foo_msg.starts_with(&foo), "Foo message starts with Foo prefix");
    }

    #[test]
    fn thread_agent_round_trips() {
        let key = thread_agent("th-1", "agent-1");
        let (t, a) = parse_thread_agent(&key).expect("parse");
        assert_eq!(t, "th-1");
        assert_eq!(a, "agent-1");
    }

    #[test]
    fn thread_agent_prefix_isolates_threads() {
        let prefix = thread_agent_prefix("th");
        assert!(thread_agent("th", "a").starts_with(&prefix));
        assert!(!thread_agent("thx", "a").starts_with(&prefix));
    }

    #[test]
    fn cursor_key_is_deterministic() {
        assert_eq!(cursor_key("a", "t"), cursor_key("a", "t"));
        assert_ne!(cursor_key("a", "t"), cursor_key("t", "a"));
    }

    #[test]
    fn thread_seq_meta_key_namespaced() {
        let key = thread_seq_meta_key("th-1");
        assert!(key.starts_with(b"seq:"));
        assert_eq!(&key[4..], b"th-1");
    }
}
