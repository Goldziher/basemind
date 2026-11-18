//! Line count of a file's contents, recorded on its [`crate::store::FileEntry`] at scan time so a
//! read tool can tell a file too large to read whole without opening it.

/// Number of lines in `bytes`, counted the way `code_read` numbers them: every `\n` ends a line,
/// and a final line without one still counts. Empty contents have no lines. A `\r\n` ending counts
/// once; a lone `\r` is not a line ending. Saturates at `u32::MAX`.
pub(crate) fn count_lines(bytes: &[u8]) -> u32 {
    let newlines = memchr::memchr_iter(b'\n', bytes).count();
    let unterminated = usize::from(bytes.last().is_some_and(|&last| last != b'\n'));
    u32::try_from(newlines + unterminated).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::count_lines;

    #[test]
    fn should_count_no_lines_in_empty_contents() {
        assert_eq!(count_lines(b""), 0);
    }

    #[test]
    fn should_count_a_final_line_without_a_newline() {
        assert_eq!(count_lines(b"a"), 1);
        assert_eq!(count_lines(b"a\nb"), 2);
    }

    #[test]
    fn should_count_each_terminated_line_once() {
        assert_eq!(count_lines(b"a\n"), 1);
        assert_eq!(count_lines(b"a\nb\n"), 2);
        assert_eq!(count_lines(b"\n\n\n"), 3);
    }

    #[test]
    fn should_count_crlf_endings_once() {
        assert_eq!(count_lines(b"a\r\nb\r\n"), 2);
        assert_eq!(count_lines(b"a\r\nb"), 2);
    }

    #[test]
    fn should_not_count_a_lone_carriage_return_as_an_ending() {
        assert_eq!(count_lines(b"a\rb\r"), 1);
    }
}
