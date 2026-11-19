//! The identifier lexer of `basemind code deadcode` (#123): every identifier token of a file, each
//! classed as code, string or comment, so a name spelled only in a comment or only in a string is
//! told apart from a use in code.
//!
//! It is a lexer for that question only: comments and string literals per language family, no
//! grammar. A misread (a Rust raw string with an inner quote, a regex literal) moves a few tokens
//! between classes and never drops one.

use memchr::memmem;

/// Where an identifier token sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Class {
    Code = 0,
    Str = 1,
    Comment = 2,
}

/// How one language writes comments and string literals.
#[derive(Debug, Clone, Copy)]
struct Syntax {
    line_comment: &'static [u8],
    block_comments: bool,
    /// Python: `"""` and `'''` blocks, read as documentation.
    triple_quotes: bool,
    /// `'…'` is a string (JS, Python, Go runes, Java chars); otherwise Rust's char-or-lifetime.
    single_quote_string: bool,
    /// Go: a backtick string. In JS a template literal stays code, since `${…}` holds code.
    backtick_string: bool,
    /// A `"…"` string may span lines (Rust).
    multiline_strings: bool,
    /// Rust raw strings: `r"…"`, `r#"…"#`, `br#"…"#`.
    raw_strings: bool,
}

fn syntax_of(language: &str) -> Syntax {
    let c_like = Syntax {
        line_comment: b"//",
        block_comments: true,
        triple_quotes: false,
        single_quote_string: true,
        backtick_string: false,
        multiline_strings: false,
        raw_strings: false,
    };
    match language {
        "python" => Syntax {
            line_comment: b"#",
            block_comments: false,
            triple_quotes: true,
            ..c_like
        },
        "rust" => Syntax {
            single_quote_string: false,
            multiline_strings: true,
            raw_strings: true,
            ..c_like
        },
        "go" => Syntax {
            backtick_string: true,
            ..c_like
        },
        _ => c_like,
    }
}

pub(super) fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Call `f` with every identifier token of `bytes` (a `language` file) and its class.
pub(super) fn identifiers(language: &str, bytes: &[u8], mut f: impl FnMut(&[u8], Class)) {
    let syntax = syntax_of(language);
    let n = bytes.len();
    let mut at = 0;
    while at < n {
        let rest = &bytes[at..];
        if rest.starts_with(syntax.line_comment) {
            let end = memchr::memchr(b'\n', rest).map_or(n, |len| at + len);
            tokens(&bytes[at..end], Class::Comment, &mut f);
            at = end;
            continue;
        }
        if syntax.block_comments && rest.starts_with(b"/*") {
            let end = memmem::find(&rest[2..], b"*/").map_or(n, |len| at + 2 + len + 2);
            tokens(&bytes[at..end], Class::Comment, &mut f);
            at = end;
            continue;
        }
        if syntax.triple_quotes && (rest.starts_with(b"\"\"\"") || rest.starts_with(b"'''")) {
            let end = memmem::find(&rest[3..], &rest[..3]).map_or(n, |len| at + 3 + len + 3);
            tokens(&bytes[at..end], Class::Comment, &mut f);
            at = end;
            continue;
        }
        let b = bytes[at];
        let quote = match b {
            b'"' => Some((b'"', syntax.multiline_strings)),
            b'\'' if syntax.single_quote_string => Some((b'\'', false)),
            b'`' if syntax.backtick_string => Some((b'`', true)),
            _ => None,
        };
        if let Some((quote, multiline)) = quote {
            let end = string_end(bytes, at + 1, quote, multiline);
            tokens(&bytes[at + 1..end], Class::Str, &mut f);
            at = (end + 1).min(n);
            continue;
        }
        if b == b'\'' {
            at = after_rust_quote(bytes, at);
            continue;
        }
        if is_ident_byte(b) {
            let start = at;
            while at < n && is_ident_byte(bytes[at]) {
                at += 1;
            }
            let token = &bytes[start..at];
            if syntax.raw_strings
                && (token == b"r" || token == b"br")
                && let Some(end) = raw_string_end(bytes, at)
            {
                tokens(&bytes[at..end], Class::Str, &mut f);
                at = end;
                continue;
            }
            if !bytes[start].is_ascii_digit() {
                f(token, Class::Code);
            }
            continue;
        }
        at += 1;
    }
}

/// The index of the quote closing a string opened before `from`, or the end of its line when
/// `multiline` is off, or the end of the file.
fn string_end(bytes: &[u8], from: usize, quote: u8, multiline: bool) -> usize {
    let mut at = from;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'\n' if !multiline => return at,
            b if b == quote => return at,
            _ => at += 1,
        }
    }
    bytes.len()
}

/// The end of a Rust raw string whose `#`s and opening quote start at `at`, or `None` when no raw
/// string starts there.
fn raw_string_end(bytes: &[u8], at: usize) -> Option<usize> {
    let hashes = bytes[at..].iter().take_while(|&&b| b == b'#').count();
    if bytes.get(at + hashes) != Some(&b'"') {
        return None;
    }
    let mut closing = vec![b'"'];
    closing.resize(hashes + 1, b'#');
    let body = at + hashes + 1;
    Some(memmem::find(&bytes[body..], &closing).map_or(bytes.len(), |len| body + len + closing.len()))
}

/// Past a Rust `'`: a char literal (`'a'`, `'\n'`) is skipped whole, a lifetime's quote alone.
fn after_rust_quote(bytes: &[u8], at: usize) -> usize {
    match bytes.get(at + 1) {
        Some(b'\\') => bytes[at + 2..]
            .iter()
            .take(12)
            .position(|&b| b == b'\'')
            .map_or(at + 1, |len| at + 2 + len + 1),
        Some(_) if bytes.get(at + 2) == Some(&b'\'') => at + 3,
        _ => at + 1,
    }
}

fn tokens(region: &[u8], class: Class, f: &mut impl FnMut(&[u8], Class)) {
    let mut at = 0;
    while at < region.len() {
        if !is_ident_byte(region[at]) {
            at += 1;
            continue;
        }
        let start = at;
        while at < region.len() && is_ident_byte(region[at]) {
            at += 1;
        }
        if region[start].is_ascii_digit() {
            continue;
        }
        // `{name}` in a string is a captured format argument (Rust `format!`, a Python f-string):
        // code, not text. `{{` is an escaped brace.
        let captured =
            class == Class::Str && start > 0 && region[start - 1] == b'{' && (start < 2 || region[start - 2] != b'{');
        f(&region[start..at], if captured { Class::Code } else { class });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(language: &str, text: &str) -> Vec<(String, Class)> {
        let mut out = Vec::new();
        identifiers(language, text.as_bytes(), |token, class| {
            out.push((String::from_utf8_lossy(token).into_owned(), class));
        });
        out
    }

    fn of(found: &[(String, Class)], name: &str) -> Vec<Class> {
        found.iter().filter(|(t, _)| t == name).map(|(_, c)| *c).collect()
    }

    #[test]
    fn rust_comments_strings_chars_and_lifetimes() {
        let found = classes(
            "rust",
            "// alpha\nfn beta<'a>(x: &'a str) -> char { let c = '\"'; gamma(\"delta \\\" eps\"); /* zeta */ 'q' }\n",
        );
        assert_eq!(of(&found, "alpha"), [Class::Comment]);
        assert_eq!(of(&found, "beta"), [Class::Code]);
        assert_eq!(of(&found, "a"), [Class::Code, Class::Code], "lifetimes stay code");
        assert_eq!(
            of(&found, "gamma"),
            [Class::Code],
            "the quote char literal opens no string"
        );
        assert_eq!(of(&found, "delta"), [Class::Str]);
        assert_eq!(
            of(&found, "eps"),
            [Class::Str],
            "an escaped quote does not close the string"
        );
        assert_eq!(of(&found, "zeta"), [Class::Comment]);
        assert_eq!(of(&found, "q"), Vec::<Class>::new(), "a char literal is skipped");
        let raw = classes(
            "rust",
            "const T: &str = r#\"{\"a\": \"lib/*\"}\"#;\nfn after() {}\nlet b = br\"x\";\n",
        );
        assert_eq!(
            of(&raw, "a"),
            [Class::Str],
            "a quote inside a raw string does not close it"
        );
        assert_eq!(of(&raw, "lib"), [Class::Str]);
        assert_eq!(
            of(&raw, "after"),
            [Class::Code],
            "`/*` inside a raw string opens no comment"
        );
        assert_eq!(of(&raw, "x"), [Class::Str]);
        let format = classes("rust", "println!(\"{BOLD}x{RESET:>4} {{LIT}}\");\n");
        assert_eq!(of(&format, "BOLD"), [Class::Code], "a captured format argument is code");
        assert_eq!(of(&format, "RESET"), [Class::Code]);
        assert_eq!(of(&format, "LIT"), [Class::Str], "`{{` escapes the brace");
    }

    #[test]
    fn python_hash_comments_docstrings_and_quotes() {
        let found = classes(
            "python",
            "def f():\n    \"\"\"Calls helper.\"\"\"\n    x = 'name'  # other\n    return g()\n",
        );
        assert_eq!(of(&found, "helper"), [Class::Comment]);
        assert_eq!(of(&found, "name"), [Class::Str]);
        assert_eq!(of(&found, "other"), [Class::Comment]);
        assert_eq!(of(&found, "g"), [Class::Code]);
    }

    #[test]
    fn a_js_template_literal_stays_code_and_a_go_backtick_is_a_string() {
        let js = classes("typescript", "const a = `x ${run()} y`; // tail\n");
        assert_eq!(of(&js, "run"), [Class::Code]);
        assert_eq!(of(&js, "tail"), [Class::Comment]);
        let go = classes("go", "var s = `raw text`\nvar r = 'x'\n");
        assert_eq!(of(&go, "raw"), [Class::Str]);
        assert_eq!(of(&go, "x"), [Class::Str]);
        let url = classes("go", "u := \"http://host/path\"\nnext()\n");
        assert_eq!(of(&url, "next"), [Class::Code], "`//` inside a string is no comment");
    }
}
