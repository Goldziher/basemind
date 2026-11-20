//! Source bytes as the text the code map is built from. A file that is not valid UTF-8 (Latin-1,
//! cp1252, a stray byte in a comment) is still outlined, chunked and grepped: each byte of an
//! invalid sequence stands in as one ASCII byte, so every offset the index records still points at
//! the same byte of the file on disk.

use std::borrow::Cow;

/// The byte that replaces each byte of an invalid UTF-8 sequence. An identifier byte, so a name
/// spelled with a Latin-1 letter (`Caf\xe9`) parses as one identifier (`Caf_`) instead of breaking
/// the grammar's parse around it.
pub const STAND_IN: u8 = b'_';

/// `bytes` as UTF-8 text of the same length: borrowed when they are UTF-8, otherwise a copy in which
/// every byte of each invalid sequence is [`STAND_IN`].
pub fn as_text(bytes: &[u8]) -> Cow<'_, str> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Cow::Borrowed(text);
    }
    let mut text = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        text.push_str(chunk.valid());
        text.extend(std::iter::repeat_n(char::from(STAND_IN), chunk.invalid().len()));
    }
    Cow::Owned(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_bytes_stand_in_one_for_one() {
        let cases: [(&[u8], &str); 5] = [
            (b"caf\xe9 cr\xe8me", "caf_ cr_me"),
            (b"\xff\xfe", "__"),
            (b"a\xc3", "a_"),
            (b"\xe2\x82 \xe2\x82\xac", "__ \u{20ac}"),
            (b"na\xc3\xafve \xe9", "na\u{ef}ve _"),
        ];
        for (bytes, want) in cases {
            let text = as_text(bytes);
            assert_eq!(text, want, "{bytes:?}");
            assert_eq!(text.len(), bytes.len(), "{bytes:?}");
        }
    }

    #[test]
    fn utf8_is_borrowed_unchanged() {
        let text = as_text("caf\u{e9}".as_bytes());
        assert!(matches!(text, Cow::Borrowed("caf\u{e9}")), "{text:?}");
    }
}
