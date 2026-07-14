//! Byte-level key encoding/decoding for the **governance-tier** Fjall keyspaces: agent memory
//! (`memory_by_key` / `memory_archive`) and mined proposals (`proposals`).
//!
//! Carved out of [`crate::index::keys`], which holds the *code-map* keyspaces. The two groups
//! share the length-prefix codec but nothing else: a code-map key is addressed by
//! `(rel_path, byte offset)` — a position in the scanned tree — whereas a governance key is
//! addressed by a `(scope, ordinal byte, owner)` namespace triple with NUL separators, and never
//! names a file at all. They are written by different subsystems (the rayon scanner vs. the MCP
//! memory/proposal tools), evolve on different schedules, and have no shared encoder.
//!
//! Both encoders here place the namespace triple ahead of the payload identifier so that one
//! namespace's entries sort contiguously and a single range scan returns exactly that namespace.
//! The `*_VIS_*` / `*_KIND_*` ordinals are persisted on disk: they are stable and append-only.
//!
//! Public paths are unchanged — [`crate::index::keys`] re-exports every item below, so call sites
//! keep using `crate::index::keys::memory_by_key` and friends.

use super::keys::{read_len_prefixed, read_len_prefixed_ref, write_len_prefixed};

/// Visibility ordinal for the **group** (shared) memory tier. Stable, append-only.
pub const MEMORY_VIS_GROUP: u8 = 0;
/// Visibility ordinal for the **individual** (per-agent) memory tier. Stable, append-only.
pub const MEMORY_VIS_INDIVIDUAL: u8 = 1;

/// `memory_by_key`:
/// `u16:scope_len ‖ scope ‖ NUL ‖ vis_byte ‖ u16:owner_len ‖ owner ‖ NUL ‖ u16:key_len ‖ key`.
///
/// The `(scope, vis_byte, owner)` triple forms the namespace; placing it ahead of the key
/// keeps every namespace's keys contiguous, so a [`memory_by_key_ns_prefix`] range scan
/// returns exactly one namespace's entries. `vis_byte` is one of [`MEMORY_VIS_GROUP`] /
/// [`MEMORY_VIS_INDIVIDUAL`]. `owner` is the empty string for the group tier and the
/// validated `AgentId` for the individual tier (NUL-free by construction).
pub fn memory_by_key(scope: &str, vis_byte: u8, owner: &str, key: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + scope.len() + 1 + 1 + 2 + owner.len() + 1 + 2 + key.len());
    let _ = write_len_prefixed(&mut out, scope.as_bytes());
    out.push(0u8);
    out.push(vis_byte);
    let _ = write_len_prefixed(&mut out, owner.as_bytes());
    out.push(0u8);
    let _ = write_len_prefixed(&mut out, key.as_bytes());
    out
}

/// Prefix bytes for "all memory entries in this `(scope, vis_byte, owner)` namespace" —
/// everything up to and including the owner's NUL separator. Feed to `keyspace.prefix(..)`
/// or use as the lower bound of a range scan.
pub fn memory_by_key_ns_prefix(scope: &str, vis_byte: u8, owner: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + scope.len() + 1 + 1 + 2 + owner.len() + 1);
    let _ = write_len_prefixed(&mut out, scope.as_bytes());
    out.push(0u8);
    out.push(vis_byte);
    let _ = write_len_prefixed(&mut out, owner.as_bytes());
    out.push(0u8);
    out
}

/// Prefix bytes for "every memory entry in this `scope`" — across all visibility tiers and
/// owners. Because `scope` is length-prefixed, this prefix bounds exactly one scope's keys
/// (a longer scope encodes a different `u16` length, so no spillover). Used by the background
/// rescan audit to scope its scan to one repo without enumerating per-agent owners.
pub fn memory_scope_prefix(scope: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + scope.len() + 1);
    let _ = write_len_prefixed(&mut out, scope.as_bytes());
    out.push(0u8);
    out
}

/// Decode `(scope, vis_byte, owner, key)` from a raw `memory_by_key` key buffer.
pub fn parse_memory_by_key(buf: &[u8]) -> Option<(String, u8, String, String)> {
    let mut c = 0;
    let scope = String::from_utf8(read_len_prefixed(buf, &mut c)?).ok()?;
    if buf.len() <= c {
        return None;
    }
    c += 1;
    if buf.len() <= c {
        return None;
    }
    let vis_byte = buf[c];
    c += 1;
    let owner = String::from_utf8(read_len_prefixed(buf, &mut c)?).ok()?;
    if buf.len() <= c {
        return None;
    }
    c += 1;
    let key = String::from_utf8(read_len_prefixed(buf, &mut c)?).ok()?;
    Some((scope, vis_byte, owner, key))
}

/// Zero-copy decode of just the trailing `key` from a raw `memory_by_key` buffer, skipping the
/// scope/vis_byte/owner namespace prefix without allocating. Use on scan paths (e.g.
/// `memory_list`) that only need the key and discard the namespace components.
pub fn parse_memory_key_only(buf: &[u8]) -> Option<&str> {
    let mut c = 0;
    read_len_prefixed_ref(buf, &mut c)?;
    c += 1;
    if buf.len() <= c {
        return None;
    }
    c += 1;
    read_len_prefixed_ref(buf, &mut c)?;
    if buf.len() <= c {
        return None;
    }
    c += 1;
    let key = read_len_prefixed_ref(buf, &mut c)?;
    std::str::from_utf8(key).ok()
}

/// Proposal kind ordinal for a **memory** candidate. Stable, append-only.
pub const PROPOSAL_KIND_MEMORY: u8 = 0;
/// Proposal kind ordinal for a **skill** candidate (co-change association-rule). Stable, append-only.
pub const PROPOSAL_KIND_SKILL: u8 = 1;
/// Tombstone kind — written when a proposal is rejected so re-mining cannot resurface it.
/// Value bytes are empty (marker only). Stable, append-only.
pub const PROPOSAL_KIND_TOMBSTONE: u8 = 2;

/// `proposal_by_id`: `u16:scope_len ‖ scope ‖ NUL ‖ kind_byte ‖ u16:id_len ‖ id`.
///
/// `(scope, kind_byte)` is the namespace; `id` is the content-addressed proposal id (hex blake3
/// of the normalized candidate) so re-mining the same candidate overwrites rather than dupes.
/// Layout mirrors [`memory_by_key`]: the namespace prefix sorts contiguously, so a
/// [`proposal_ns_prefix`] range scan returns exactly one `(scope, kind)` namespace.
pub fn proposal_by_id(scope: &str, kind_byte: u8, id: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + scope.len() + 1 + 1 + 2 + id.len());
    let _ = write_len_prefixed(&mut out, scope.as_bytes());
    out.push(0u8);
    out.push(kind_byte);
    let _ = write_len_prefixed(&mut out, id.as_bytes());
    out
}

/// Prefix bytes for "all proposals in this `(scope, kind_byte)` namespace" — feed to a range scan.
pub fn proposal_ns_prefix(scope: &str, kind_byte: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + scope.len() + 1 + 1);
    let _ = write_len_prefixed(&mut out, scope.as_bytes());
    out.push(0u8);
    out.push(kind_byte);
    out
}

/// Decode `(scope, kind_byte, id)` from a raw `proposal_by_id` key buffer.
pub fn parse_proposal_by_id(buf: &[u8]) -> Option<(String, u8, String)> {
    let mut c = 0;
    let scope = String::from_utf8(read_len_prefixed(buf, &mut c)?).ok()?;
    if buf.len() <= c {
        return None;
    }
    c += 1;
    if buf.len() <= c {
        return None;
    }
    let kind_byte = buf[c];
    c += 1;
    let id = String::from_utf8(read_len_prefixed(buf, &mut c)?).ok()?;
    Some((scope, kind_byte, id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_by_key_roundtrips_group() {
        let raw = memory_by_key("scope-a", MEMORY_VIS_GROUP, "", "my.key");
        assert_eq!(
            parse_memory_by_key(&raw),
            Some((
                "scope-a".to_string(),
                MEMORY_VIS_GROUP,
                String::new(),
                "my.key".to_string()
            ))
        );
    }

    #[test]
    fn memory_by_key_roundtrips_individual() {
        let raw = memory_by_key("scope-a", MEMORY_VIS_INDIVIDUAL, "agent-7", "my.key");
        assert_eq!(
            parse_memory_by_key(&raw),
            Some((
                "scope-a".to_string(),
                MEMORY_VIS_INDIVIDUAL,
                "agent-7".to_string(),
                "my.key".to_string()
            ))
        );
    }

    /// The zero-copy `parse_memory_key_only` must return exactly the `key` component that the
    /// allocating `parse_memory_by_key` yields, for every namespace shape — including keys whose
    /// own bytes contain the NUL-adjacent separators and length-prefix-sized values.
    #[test]
    fn parse_memory_key_only_matches_full_parse() {
        let cases = [
            ("scope-a", MEMORY_VIS_GROUP, "", "my.key"),
            ("scope-a", MEMORY_VIS_INDIVIDUAL, "agent-7", "ns:sub.key"),
            ("", MEMORY_VIS_GROUP, "", ""),
            ("s", MEMORY_VIS_INDIVIDUAL, "owner-with-dashes", "k"),
            ("scope/with/slashes", MEMORY_VIS_GROUP, "", "key.with.many.dots"),
        ];
        for (scope, vis, owner, key) in cases {
            let raw = memory_by_key(scope, vis, owner, key);
            let full = parse_memory_by_key(&raw).map(|(_, _, _, k)| k);
            let only = parse_memory_key_only(&raw).map(str::to_string);
            assert_eq!(only, full, "key-only parse diverged for key {key:?}");
            assert_eq!(only.as_deref(), Some(key));
        }
    }

    #[test]
    fn parse_memory_key_only_rejects_truncated_buffer() {
        let raw = memory_by_key("scope-a", MEMORY_VIS_INDIVIDUAL, "agent-7", "my.key");
        assert_eq!(parse_memory_key_only(&raw[..raw.len() - 3]), None);
        assert_eq!(parse_memory_key_only(&[]), None);
    }

    /// A group key and an individual key for the same `(scope, key)` must live in
    /// disjoint namespaces: neither key may fall within the other's namespace prefix.
    #[test]
    fn memory_namespace_prefixes_do_not_overlap() {
        let scope = "scope-a";
        let key = "shared.key";
        let group_key = memory_by_key(scope, MEMORY_VIS_GROUP, "", key);
        let indiv_key = memory_by_key(scope, MEMORY_VIS_INDIVIDUAL, "agent-7", key);

        let group_prefix = memory_by_key_ns_prefix(scope, MEMORY_VIS_GROUP, "");
        let indiv_prefix = memory_by_key_ns_prefix(scope, MEMORY_VIS_INDIVIDUAL, "agent-7");

        assert!(
            group_key.starts_with(&group_prefix),
            "group key must extend the group namespace prefix"
        );
        assert!(
            indiv_key.starts_with(&indiv_prefix),
            "individual key must extend the individual namespace prefix"
        );
        assert!(
            !group_key.starts_with(&indiv_prefix),
            "a group key must NOT fall within an individual namespace prefix"
        );
        assert!(
            !indiv_key.starts_with(&group_prefix),
            "an individual key must NOT fall within the group namespace prefix"
        );
    }

    /// Two different agents' individual namespaces for the same scope+key are disjoint.
    #[test]
    fn memory_individual_namespaces_isolate_by_owner() {
        let scope = "scope-a";
        let key = "k";
        let a_key = memory_by_key(scope, MEMORY_VIS_INDIVIDUAL, "agent-a", key);
        let b_prefix = memory_by_key_ns_prefix(scope, MEMORY_VIS_INDIVIDUAL, "agent-b");
        assert!(
            !a_key.starts_with(&b_prefix),
            "agent-a's key must NOT fall within agent-b's namespace prefix"
        );
    }
}
