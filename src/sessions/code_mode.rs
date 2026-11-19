//! Codex code mode: the model answers with one freeform `exec` call whose input is JavaScript, and
//! the tools it runs are calls in that script on the global `tools` object
//! (`await tools.exec_command({cmd: "rg -n x src"})`, `tools.mcp__basemind__code_outline({...})`,
//! `tools["apply_patch"]("*** Begin Patch...")`). The rollout records only the script, so the
//! nested calls are recovered here with a small lexer: string literals, comments, regular expression
//! literals and template literals (with their `${...}` expressions, nested templates included) are
//! skipped as units, a call is `tools . name (` or `tools [ "name" ] (`, and the arguments are read
//! from the keys of the call's own argument object whose value is a string literal or an array of
//! them. Nothing is evaluated; a call built at run time (`tools[name](...)`) is not seen.

use super::tally::Call;

/// An argument value read from a call: a string, or an argv of strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    Text(String),
    List(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok<'a> {
    Ident(&'a str),
    Str(String),
    Punct(u8),
}

/// The nested tool calls of a code-mode script, classified by `classify(name, args)`, where `args`
/// looks up an argument by key (an empty key is the first positional string).
pub fn calls(source: &str, classify: impl Fn(&str, &dyn Fn(&str) -> Option<Arg>) -> Call) -> Vec<Call> {
    let toks = lex(source);
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let after_dot = i > 0 && toks[i - 1] == Tok::Punct(b'.');
        if toks[i] != Tok::Ident("tools") || after_dot {
            i += 1;
            continue;
        }
        let (name, open) = match (toks.get(i + 1), toks.get(i + 2), toks.get(i + 3)) {
            (Some(Tok::Punct(b'.')), Some(Tok::Ident(name)), _) => ((*name).to_owned(), i + 3),
            (Some(Tok::Punct(b'[')), Some(Tok::Str(name)), Some(Tok::Punct(b']'))) => (name.clone(), i + 4),
            _ => {
                i += 1;
                continue;
            }
        };
        if toks.get(open) != Some(&Tok::Punct(b'(')) {
            i = open;
            continue;
        }
        let close = matching(&toks, open);
        let args = &toks[open + 1..close];
        out.push(classify(&name, &|key: &str| lookup(args, key)));
        // Calls nested in the arguments (`tools.a(await tools.b())`) are still scanned.
        i = open + 1;
    }
    out
}

/// The index of the `)` closing the `(` at `open`, or the end.
fn matching(toks: &[Tok<'_>], open: usize) -> usize {
    let mut depth = 0usize;
    for (offset, tok) in toks[open..].iter().enumerate() {
        match tok {
            Tok::Punct(b'(') => depth += 1,
            Tok::Punct(b')') => {
                depth -= 1;
                if depth == 0 {
                    return open + offset;
                }
            }
            _ => {}
        }
    }
    toks.len()
}

/// The value of `key` in the argument tokens: a key of the call's own argument object, not of an
/// object nested in it. An empty `key` is the first string literal passed directly.
fn lookup(args: &[Tok<'_>], key: &str) -> Option<Arg> {
    let mut open: Vec<u8> = Vec::new();
    for (i, tok) in args.iter().enumerate() {
        match tok {
            Tok::Punct(b @ (b'(' | b'[' | b'{')) => {
                open.push(*b);
                continue;
            }
            Tok::Punct(b')' | b']' | b'}') => {
                open.pop();
                continue;
            }
            _ => {}
        }
        if key.is_empty() {
            if let Tok::Str(text) = tok
                && open.is_empty()
            {
                return Some(Arg::Text(text.clone()));
            }
            continue;
        }
        let named = match tok {
            Tok::Ident(name) => *name == key,
            Tok::Str(name) => name == key,
            Tok::Punct(_) => false,
        };
        if !named || open != b"{" || args.get(i + 1) != Some(&Tok::Punct(b':')) {
            continue;
        }
        match args.get(i + 2) {
            Some(Tok::Str(text)) => return Some(Arg::Text(text.clone())),
            Some(Tok::Punct(b'[')) => {
                let mut list = Vec::new();
                for tok in &args[i + 3..] {
                    match tok {
                        Tok::Str(word) => list.push(word.clone()),
                        Tok::Punct(b',') => {}
                        _ => break,
                    }
                }
                return Some(Arg::List(list));
            }
            _ => return None,
        }
    }
    None
}

/// Whether a `/` after `prev` starts a regular expression literal rather than a division: at the
/// start, after an operator or opening punctuation, or after a keyword that takes an expression.
fn regex_allowed(prev: Option<&Tok<'_>>) -> bool {
    match prev {
        None => true,
        Some(Tok::Punct(b)) => !matches!(b, b')' | b']' | b'}'),
        Some(Tok::Ident(word)) => matches!(
            *word,
            "return"
                | "typeof"
                | "case"
                | "do"
                | "else"
                | "in"
                | "of"
                | "new"
                | "delete"
                | "void"
                | "throw"
                | "await"
                | "yield"
        ),
        Some(Tok::Str(_)) => false,
    }
}

/// The index after the regular expression literal opening at `start`, its flags included.
fn skip_regex(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    let mut class = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'[' => class = true,
            b']' => class = false,
            b'/' if !class => {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                return i;
            }
            b'\n' => return i,
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

fn lex(source: &str) -> Vec<Tok<'_>> {
    let bytes = source.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'"' | b'\'' | b'`' => {
                let (text, end) = literal(source, i);
                toks.push(Tok::Str(text));
                i = end;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i = memchr::memchr(b'\n', &bytes[i..]).map_or(bytes.len(), |n| i + n);
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = memchr::memmem::find(&bytes[i + 2..], b"*/").map_or(bytes.len(), |n| i + 2 + n + 2);
            }
            b'/' if regex_allowed(toks.last()) => i = skip_regex(bytes, i),
            _ if b.is_ascii_alphabetic() || b == b'_' || b == b'$' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$') {
                    i += 1;
                }
                toks.push(Tok::Ident(&source[start..i]));
            }
            _ if b.is_ascii_whitespace() || b.is_ascii_digit() || !b.is_ascii() => i += 1,
            _ => {
                toks.push(Tok::Punct(b));
                i += 1;
            }
        }
    }
    toks
}

/// The unescaped text of the string literal opening at `start`, and the index after it. A template
/// literal keeps each `${...}` as written, and the literal ends only after its expressions do, so a
/// string or template inside one does not end it.
fn literal(source: &str, start: usize) -> (String, usize) {
    let quote = source.as_bytes()[start];
    let mut text = String::new();
    let mut i = start + 1;
    while let Some(c) = source[i..].chars().next() {
        let next = i + c.len_utf8();
        if c as u32 == u32::from(quote) {
            return (text, next);
        }
        if quote == b'`' && c == '$' && source.as_bytes().get(next) == Some(&b'{') {
            let end = expression_end(source, next + 1);
            text.push_str(&source[i..end]);
            i = end;
            continue;
        }
        if c != '\\' {
            text.push(c);
            i = next;
            continue;
        }
        let Some(escaped) = source[next..].chars().next() else {
            break;
        };
        i = next + escaped.len_utf8();
        match escaped {
            'n' => text.push('\n'),
            't' => text.push('\t'),
            'r' => text.push('\r'),
            '0' => text.push('\0'),
            'u' => {
                let (decoded, used) = unicode_escape(&source[i..]);
                match decoded {
                    Some(decoded) => {
                        text.push(decoded);
                        i += used;
                    }
                    None => text.push('u'),
                }
            }
            other => text.push(other),
        }
    }
    (text, source.len())
}

/// The index after the `}` closing the template expression whose body starts at `start`. Strings,
/// templates and comments inside it are skipped whole.
fn expression_end(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut depth = 1usize;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' | b'`' => {
                i = literal(source, i).1;
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i = memchr::memchr(b'\n', &bytes[i..]).map_or(bytes.len(), |n| i + n);
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = memchr::memmem::find(&bytes[i + 2..], b"*/").map_or(bytes.len(), |n| i + 2 + n + 2);
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

/// The character of a `\u` escape whose `u` was just read: `XXXX` or `{X...}`, and the bytes used.
fn unicode_escape(rest: &str) -> (Option<char>, usize) {
    let (hex, used) = match rest.strip_prefix('{') {
        Some(braced) => match braced.find('}') {
            Some(end) if (1..=6).contains(&end) => (&braced[..end], end + 2),
            _ => return (None, 0),
        },
        None => match rest.get(..4) {
            Some(hex) => (hex, 4),
            None => return (None, 0),
        },
    };
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return (None, 0);
    }
    match u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
        Some(decoded) => (Some(decoded), used),
        None => (None, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::tally::Family;

    fn names(source: &str) -> Vec<String> {
        calls(source, |name, args| {
            let detail = match args("cmd")
                .or_else(|| args("command"))
                .or_else(|| args("path"))
                .or_else(|| args(""))
            {
                Some(Arg::Text(text)) => text,
                Some(Arg::List(list)) => list.join(" "),
                None => String::new(),
            };
            Call {
                read: vec![format!("{name}:{detail}")],
                ..Call::of(Family::Other)
            }
        })
        .into_iter()
        .flat_map(|call| call.read)
        .collect()
    }

    #[test]
    fn nested_calls_are_read_from_the_script() {
        let source = r#"
            // tools.commented_out({cmd: "x"})
            const out = await tools.exec_command({ cmd: "rg -n \"a b\" src", yield_time_ms: 1000 });
            const label = "tools.in_a_string()";
            await tools["mcp__basemind__code_outline"]({path: 'src/lib.rs'});
            /* tools.block_comment() */
            await tools.apply_patch(`*** Begin Patch
*** Update File: src/a.rs
*** End Patch`);
            text(myTools.not_this(), out.tools.nor_this());
        "#;
        assert_eq!(
            names(source),
            vec![
                r#"exec_command:rg -n "a b" src"#.to_owned(),
                "mcp__basemind__code_outline:src/lib.rs".to_owned(),
                "apply_patch:*** Begin Patch\n*** Update File: src/a.rs\n*** End Patch".to_owned(),
            ]
        );
    }

    #[test]
    fn keys_may_be_quoted_and_values_may_be_lists() {
        let source = r#"await tools.shell({"command": ["bash", "-lc", "cat a.rs"]}); tools.wait(tools.x())"#;
        assert_eq!(
            names(source),
            vec![
                "shell:bash -lc cat a.rs".to_owned(),
                "wait:".to_owned(),
                "x:".to_owned()
            ]
        );
        assert_eq!(
            lookup(&lex(r#"{cmd: "A\n"}"#), "cmd"),
            Some(Arg::Text("A\n".to_owned()))
        );
    }

    #[test]
    fn regex_literals_nested_templates_and_braced_escapes_do_not_derail_the_lexer() {
        let source = r#"
            const parts = s.split(/'/);
            const re = /["`]/g, half = total / 2 / 1;
            const label = `a ${`inner ${x}`} b`;
            await tools.exec_command({cmd: "rg \u{1F600} src"});
        "#;
        assert_eq!(names(source), vec!["exec_command:rg \u{1F600} src".to_owned()]);
    }

    #[test]
    fn only_the_calls_own_keys_are_its_arguments() {
        let source = r#"tools.a({opts: tools.b({cmd: "rg x"})})"#;
        assert_eq!(names(source), vec!["a:".to_owned(), "b:rg x".to_owned()]);
        let positional = r#"tools.apply_patch(["no"], "*** Begin Patch")"#;
        assert_eq!(names(positional), vec!["apply_patch:*** Begin Patch".to_owned()]);
    }
}
