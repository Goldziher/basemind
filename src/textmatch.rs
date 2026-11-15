//! Whole-identifier substring matching: finding a name where it stands as a name, not where it is
//! the middle of a longer one.

use memchr::memmem::Finder;

/// Whether `byte` can continue an identifier. Every non-ASCII byte counts, so a name is never
/// matched inside a Unicode identifier.
fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80
}

/// Offset of the first occurrence of the finder's needle in `haystack` that is not part of a longer
/// identifier: the byte before it and the byte after it are not identifier bytes. `os` is found in
/// `import os.path` and not in `posixpath`.
///
/// A needle edge that is itself not an identifier byte (`./utils`, `foo(`) is its own boundary.
/// An empty needle matches nothing.
pub fn find_whole(haystack: &[u8], finder: &Finder<'_>) -> Option<usize> {
    let needle = finder.needle();
    let (first, last) = (*needle.first()?, *needle.last()?);
    let left_open = !is_identifier_byte(first);
    let right_open = !is_identifier_byte(last);
    let mut from = 0;
    while let Some(found) = finder.find(haystack.get(from..)?) {
        let start = from + found;
        let end = start + needle.len();
        let left_ok = left_open || start == 0 || !is_identifier_byte(haystack[start - 1]);
        let right_ok = right_open || end == haystack.len() || !is_identifier_byte(haystack[end]);
        if left_ok && right_ok {
            return Some(start);
        }
        from = start + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(haystack: &str, needle: &str) -> Option<usize> {
        find_whole(haystack.as_bytes(), &Finder::new(needle.as_bytes()))
    }

    #[test]
    fn a_name_inside_a_longer_identifier_is_skipped() {
        assert_eq!(find("posixpath", "os"), None);
        assert_eq!(find("os_helpers", "os"), None);
        assert_eq!(find("budget.pop(get)", "get"), Some(11));
        assert_eq!(find("get$ = get", "get"), Some(7));
    }

    #[test]
    fn a_name_bounded_by_punctuation_or_the_ends_is_found() {
        assert_eq!(find("os", "os"), Some(0));
        assert_eq!(find("import os.path", "os"), Some(7));
        assert_eq!(find("import os.path", "os.path"), Some(7));
        assert_eq!(find("crate::store::Store", "store"), Some(7));
    }

    #[test]
    fn a_needle_ending_in_punctuation_is_its_own_boundary() {
        assert_eq!(find("from './utils/index'", "./utils"), Some(6));
        assert_eq!(find("cache_root(x)", "cache_root("), Some(0));
    }

    #[test]
    fn an_empty_needle_or_a_unicode_neighbour_matches_nothing() {
        assert_eq!(find("anything", ""), None);
        assert_eq!(find("éos", "os"), None);
    }
}
