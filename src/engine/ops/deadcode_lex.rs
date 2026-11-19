//! The identifier lexer of `basemind code deadcode` (#123): every identifier token of a file, each
//! classed as code, string or comment with its byte offset, so a name spelled only in a comment or
//! only in a string is told apart from a use in code.
//!
//! It is a lexer for that question only: comments and string literals per language family, no
//! grammar. A JS template literal is a string whose `${…}` holds code, and `{name}` inside a string
//! (a Rust format argument, a Python f-string, triple-quoted or not) is code. A misread (a Rust raw
//! string with an inner quote, a regex literal) moves a few tokens between classes and never drops
//! one; the definition count reads the class of each definition's own token, so a misread never
//! cancels a use elsewhere.

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
    /// Python: `"""` and `'''` blocks, read as documentation unless `f`-prefixed.
    triple_quotes: bool,
    /// `'…'` is a string (JS, Python, Go runes, Java chars); otherwise Rust's char-or-lifetime.
    single_quote_string: bool,
    /// Go: a backtick string.
    backtick_string: bool,
    /// JS/TS: a backtick template literal, whose `${…}` holds code.
    template_literals: bool,
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
        template_literals: false,
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
        "javascript" | "typescript" | "tsx" => Syntax {
            template_literals: true,
            ..c_like
        },
        _ => c_like,
    }
}

pub(super) fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// The callback [`identifiers`] calls: the token, its class and its byte offset in the file.
type Sink<'f> = dyn FnMut(&[u8], Class, usize) + 'f;

/// Call `f` with every identifier token of `bytes` (a `language` file), its class and its offset.
pub(super) fn identifiers(language: &str, bytes: &[u8], mut f: impl FnMut(&[u8], Class, usize)) {
    let syntax = syntax_of(language);
    code(bytes, &syntax, 0, false, &mut f);
}

/// Lex code from `from`. Nested in a template's `${…}`, stop past the `}` that closes it and return
/// that offset; otherwise run to the end.
fn code(bytes: &[u8], syntax: &Syntax, from: usize, nested: bool, f: &mut Sink<'_>) -> usize {
    let n = bytes.len();
    let mut at = from;
    let mut depth = 0usize;
    while at < n {
        let rest = &bytes[at..];
        if rest.starts_with(syntax.line_comment) {
            let end = memchr::memchr(b'\n', rest).map_or(n, |len| at + len);
            tokens(bytes, at, end, Class::Comment, f);
            at = end;
            continue;
        }
        if syntax.block_comments && rest.starts_with(b"/*") {
            let end = memmem::find(&rest[2..], b"*/").map_or(n, |len| at + 2 + len + 2);
            tokens(bytes, at, end, Class::Comment, f);
            at = end;
            continue;
        }
        if syntax.triple_quotes && (rest.starts_with(b"\"\"\"") || rest.starts_with(b"'''")) {
            let close = memmem::find(&rest[3..], &rest[..3]);
            let end = close.map_or(n, |len| at + 3 + len + 3);
            // A docstring is documentation; an f-string's `{name}` is code.
            let class = if f_prefixed(bytes, at) {
                Class::Str
            } else {
                Class::Comment
            };
            tokens(bytes, at + 3, close.map_or(n, |len| at + 3 + len), class, f);
            at = end;
            continue;
        }
        let b = bytes[at];
        if b == b'`' && syntax.template_literals {
            at = template(bytes, syntax, at + 1, f);
            continue;
        }
        let quote = match b {
            b'"' => Some((b'"', syntax.multiline_strings)),
            b'\'' if syntax.single_quote_string => Some((b'\'', false)),
            b'`' if syntax.backtick_string => Some((b'`', true)),
            _ => None,
        };
        if let Some((quote, multiline)) = quote {
            let end = string_end(bytes, at + 1, quote, multiline);
            tokens(bytes, at + 1, end, Class::Str, f);
            at = (end + 1).min(n);
            continue;
        }
        if b == b'\'' {
            at = after_rust_quote(bytes, at);
            continue;
        }
        if nested {
            match b {
                b'{' => depth += 1,
                b'}' if depth == 0 => return at + 1,
                b'}' => depth -= 1,
                _ => {}
            }
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
                tokens(bytes, at, end, Class::Str, f);
                at = end;
                continue;
            }
            if !bytes[start].is_ascii_digit() {
                f(token, Class::Code, start);
            }
            continue;
        }
        at += 1;
    }
    n
}

/// Past a JS template literal whose text starts at `from`: the text is a string, each `${…}` is
/// code (templates nest inside it).
fn template(bytes: &[u8], syntax: &Syntax, from: usize, f: &mut Sink<'_>) -> usize {
    let n = bytes.len();
    let mut text = from;
    let mut at = from;
    while at < n {
        match bytes[at] {
            b'\\' => at += 2,
            b'`' => {
                tokens(bytes, text, at, Class::Str, f);
                return at + 1;
            }
            b'$' if bytes.get(at + 1) == Some(&b'{') => {
                tokens(bytes, text, at, Class::Str, f);
                at = code(bytes, syntax, at + 2, true, f);
                text = at;
            }
            _ => at += 1,
        }
    }
    tokens(bytes, text.min(n), n, Class::Str, f);
    n
}

/// Whether the triple quote at `at` carries an `f` prefix (`f"""`, `rf'''`, `Fr"""`).
fn f_prefixed(bytes: &[u8], at: usize) -> bool {
    let prefix_start = bytes[..at]
        .iter()
        .rposition(|&b| !is_ident_byte(b))
        .map_or(0, |p| p + 1);
    let prefix = &bytes[prefix_start..at];
    prefix.len() <= 2
        && prefix.iter().any(|b| b.eq_ignore_ascii_case(&b'f'))
        && prefix.iter().all(|b| matches!(b.to_ascii_lowercase(), b'f' | b'r'))
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

/// The identifier tokens of `bytes[from..to]`, each of `class` (a captured `{name}` in a string is
/// code).
fn tokens(bytes: &[u8], from: usize, to: usize, class: Class, f: &mut Sink<'_>) {
    let to = to.min(bytes.len());
    let mut at = from;
    while at < to {
        if !is_ident_byte(bytes[at]) {
            at += 1;
            continue;
        }
        let start = at;
        while at < to && is_ident_byte(bytes[at]) {
            at += 1;
        }
        if bytes[start].is_ascii_digit() {
            continue;
        }
        // `{name}` in a string is a captured format argument (Rust `format!`, a Python f-string):
        // code, not text. `{{` is an escaped brace.
        let captured = class == Class::Str
            && start > from
            && bytes[start - 1] == b'{'
            && (start < from + 2 || bytes[start - 2] != b'{');
        f(&bytes[start..at], if captured { Class::Code } else { class }, start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(language: &str, text: &str) -> Vec<(String, Class)> {
        let mut out = Vec::new();
        identifiers(language, text.as_bytes(), |token, class, at| {
            assert_eq!(
                &text.as_bytes()[at..at + token.len()],
                token,
                "the offset points at the token"
            );
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
    fn a_python_triple_quoted_f_string_captures_its_arguments() {
        let found = classes(
            "python",
            "msg = f\"\"\"Hello {user}\nbye\"\"\"\nraw = rf'''{path} x'''\ndoc = \"\"\"{plain}\"\"\"\n",
        );
        assert_eq!(of(&found, "user"), [Class::Code], "an f-string argument is code");
        assert_eq!(of(&found, "bye"), [Class::Str]);
        assert_eq!(of(&found, "path"), [Class::Code], "`rf` is an f-string too");
        assert_eq!(of(&found, "plain"), [Class::Comment], "a docstring stays documentation");
    }

    #[test]
    fn a_js_template_literal_is_a_string_whose_substitutions_are_code() {
        let js = classes("typescript", "const a = `x ${run()} y`; // tail\n");
        assert_eq!(of(&js, "run"), [Class::Code]);
        assert_eq!(of(&js, "x"), [Class::Str]);
        assert_eq!(of(&js, "tail"), [Class::Comment]);
        let url = classes(
            "typescript",
            "fetch(`https://api/${buildPath(id)}`);\nconst g = glob(`src/**/*.ts`);\nfunction helper() {}\n/** doc */\nlater();\n",
        );
        assert_eq!(
            of(&url, "buildPath"),
            [Class::Code],
            "`//` in a template opens no comment"
        );
        assert_eq!(of(&url, "https"), [Class::Str]);
        assert_eq!(of(&url, "helper"), [Class::Code], "`/*` in a template opens no comment");
        assert_eq!(of(&url, "later"), [Class::Code]);
        let nested = classes("javascript", "t = `a ${f({k: `in ${deep}`})} b`; after();\n");
        assert_eq!(
            of(&nested, "deep"),
            [Class::Code],
            "a template nested in a substitution"
        );
        assert_eq!(of(&nested, "k"), [Class::Code], "braces in a substitution are balanced");
        assert_eq!(of(&nested, "b"), [Class::Str]);
        assert_eq!(of(&nested, "after"), [Class::Code]);
        let go = classes("go", "var s = `raw text`\nvar r = 'x'\n");
        assert_eq!(of(&go, "raw"), [Class::Str]);
        assert_eq!(of(&go, "x"), [Class::Str]);
        let url = classes("go", "u := \"http://host/path\"\nnext()\n");
        assert_eq!(of(&url, "next"), [Class::Code], "`//` inside a string is no comment");
    }
}
