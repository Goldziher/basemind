//! The set of commit ordinals reachable from one indexed tip.
//!
//! The commit store is head-independent: every commit reachable from any worktree HEAD is stored
//! once under an ordinal assigned in append order. A query at HEAD `X` answers from the posting
//! lists and keeps only the ordinals `X` reaches, so one store serves every worktree of a clone.
//! The set is a plain bitset (bit `ord % 8` of byte `ord / 8`): 32 KiB for a quarter-million commits.

/// A set of commit ordinals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reach {
    bits: Vec<u8>,
}

impl Reach {
    /// The set holding exactly `ords`.
    pub fn from_ords(ords: impl IntoIterator<Item = u32>) -> Self {
        let mut reach = Self::default();
        for ord in ords {
            reach.insert(ord);
        }
        reach
    }

    /// The set stored as `bytes` by [`Reach::as_bytes`].
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self { bits: bytes.to_vec() }
    }

    /// The stored form: a bitset, little-endian within each byte.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bits
    }

    /// Add `ord` to the set.
    pub fn insert(&mut self, ord: u32) {
        let byte = (ord / 8) as usize;
        if byte >= self.bits.len() {
            self.bits.resize(byte + 1, 0);
        }
        self.bits[byte] |= 1 << (ord % 8);
    }

    /// Whether `ord` is in the set.
    pub fn contains(&self, ord: u32) -> bool {
        self.bits
            .get((ord / 8) as usize)
            .is_some_and(|byte| byte & (1 << (ord % 8)) != 0)
    }

    /// How many ordinals the set holds.
    pub fn len(&self) -> usize {
        self.bits.iter().map(|byte| byte.count_ones() as usize).sum()
    }

    /// Whether the set holds no ordinal.
    pub fn is_empty(&self) -> bool {
        self.bits.iter().all(|byte| *byte == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_holds_exactly_the_ordinals_inserted_across_byte_boundaries() {
        let ords = [0u32, 7, 8, 9, 63, 64, 1_000];
        let reach = Reach::from_ords(ords);
        for ord in 0..1_100u32 {
            assert_eq!(reach.contains(ord), ords.contains(&ord), "ordinal {ord}");
        }
        assert_eq!(reach.len(), ords.len());
        assert!(!reach.is_empty());
    }

    #[test]
    fn an_empty_set_contains_nothing_and_a_far_ordinal_is_absent() {
        let reach = Reach::default();
        assert!(reach.is_empty());
        assert_eq!(reach.len(), 0);
        assert!(!reach.contains(0));
        assert!(!reach.contains(u32::MAX));
    }

    #[test]
    fn the_stored_form_round_trips() {
        let reach = Reach::from_ords([3, 4, 200]);
        assert_eq!(Reach::from_bytes(reach.as_bytes()), reach);
    }
}
