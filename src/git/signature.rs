//! Text of a commit author's name and email, which git stores as bytes in whatever encoding the
//! author's machine used.

/// `author`'s name and email as text. Either one that is not UTF-8 is decoded by the commit's
/// `encoding` header, read only then; see [`signature_text`].
pub(super) fn author_text(commit: &gix::Commit<'_>, author: &gix::actor::SignatureRef<'_>) -> (String, String) {
    if let (Ok(name), Ok(email)) = (std::str::from_utf8(author.name), std::str::from_utf8(author.email)) {
        return (name.to_string(), email.to_string());
    }
    let decoded = commit.decode().ok();
    let encoding = decoded.as_ref().and_then(|c| c.encoding).map(AsRef::<[u8]>::as_ref);
    (
        signature_text(author.name, encoding),
        signature_text(author.email, encoding),
    )
}

/// `raw` as text: as is when it is UTF-8, else decoded by the commit's `encoding` header when it
/// names an encoding that reads `raw` without error, else as Windows-1252. Windows-1252 maps every
/// byte to its own character, so two authors whose names differ only in bytes that are not UTF-8
/// keep two names, where a lossy UTF-8 decode would give both the same replacement character.
pub(super) fn signature_text(raw: &[u8], encoding: Option<&[u8]>) -> String {
    if let Ok(text) = std::str::from_utf8(raw) {
        return text.to_string();
    }
    if let Some(declared) = encoding.and_then(encoding_rs::Encoding::for_label) {
        let (text, had_errors) = declared.decode_without_bom_handling(raw);
        if !had_errors {
            return text.into_owned();
        }
    }
    encoding_rs::WINDOWS_1252
        .decode_without_bom_handling(raw)
        .0
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::signature_text;

    #[test]
    fn utf8_is_kept_whatever_the_header_says() {
        assert_eq!(signature_text("José".as_bytes(), Some(b"ISO-8859-1")), "José");
    }

    #[test]
    fn latin1_without_a_header_reads_as_windows_1252() {
        assert_eq!(signature_text(b"Jos\xe9", None), "José");
        assert_eq!(signature_text(b"Jos\xe8", None), "Josè");
    }

    #[test]
    fn the_encoding_header_decodes_what_is_not_utf8() {
        assert_eq!(signature_text(b"\xbf\xd5\xe2\xe0", Some(b"ISO-8859-5")), "Петр");
    }

    #[test]
    fn a_header_that_does_not_read_the_bytes_falls_back_to_windows_1252() {
        assert_eq!(signature_text(b"Jos\xe9", Some(b"UTF-8")), "José");
        assert_eq!(signature_text(b"Jos\xe9", Some(b"no-such-encoding")), "José");
    }

    #[test]
    fn names_differing_only_in_bytes_that_are_not_utf8_stay_distinct() {
        let names: Vec<String> = (0x80..=0xffu8).map(|b| signature_text(&[b'x', b], None)).collect();
        let distinct: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(distinct.len(), 128);
    }
}
