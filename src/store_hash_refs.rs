//! Reference counts over the content hashes the file manifest holds.

use ahash::AHashMap;

use crate::path::RelPath;
use crate::store::FileEntry;

/// How many manifest entries hold each content hash. Keys are 8-byte digests of the hex string, not
/// the 64-character strings, since there is one per distinct content. Two hashes sharing a digest
/// share a count, so a collision can only keep a hash "held" a little longer, never free a held one.
#[derive(Debug, Default)]
pub(crate) struct HashRefs(AHashMap<u64, u32>);

fn digest(hash_hex: &str) -> u64 {
    // Fixed seeds: the digest only has to be stable within the process.
    ahash::RandomState::with_seeds(1, 2, 3, 4).hash_one(hash_hex)
}

impl HashRefs {
    pub(crate) fn of(files: &AHashMap<RelPath, FileEntry>) -> Self {
        let mut refs = Self::default();
        for entry in files.values() {
            refs.acquire(&entry.hash_hex);
        }
        refs
    }

    pub(crate) fn acquire(&mut self, hash_hex: &str) {
        *self.0.entry(digest(hash_hex)).or_default() += 1;
    }

    pub(crate) fn release(&mut self, hash_hex: &str) {
        if let std::collections::hash_map::Entry::Occupied(mut held) = self.0.entry(digest(hash_hex)) {
            if *held.get() > 1 {
                *held.get_mut() -= 1;
            } else {
                held.remove();
            }
        }
    }

    pub(crate) fn is_held(&self, hash_hex: &str) -> bool {
        self.0.contains_key(&digest(hash_hex))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_stays_held_until_its_last_holder_releases_it() {
        let mut refs = HashRefs::default();
        refs.acquire("h1");
        refs.acquire("h1");
        refs.acquire("h2");
        refs.release("h1");
        assert!(refs.is_held("h1") && refs.is_held("h2"));
        refs.release("h1");
        assert!(!refs.is_held("h1") && refs.is_held("h2"));
    }

    #[test]
    fn releasing_an_unheld_hash_is_a_no_op() {
        let mut refs = HashRefs::default();
        refs.release("never");
        refs.acquire("h1");
        refs.release("never");
        assert!(refs.is_held("h1"));
    }
}
