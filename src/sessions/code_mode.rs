//! Codex code mode: the model answers with one freeform `exec` call whose input is JavaScript, and
//! the tools it runs are calls in that script on the global `tools` object
//! (`await tools.exec_command({cmd: "rg -n x src"})`, `tools.mcp__basemind__code_outline({...})`,
//! `tools["apply_patch"]("*** Begin Patch...")`). The rollout records only the script, so the
//! nested calls are recovered here with a small lexer: string literals, comments and template
//! literals are skipped as units, a call is `tools . name (` or `tools [ "name" ] (`, and the
//! arguments are read from object keys whose value is a string literal or an array of them. Nothing
//! is evaluated; a call built at run time (`tools[name](...)`) is not seen.

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

/// The value of `key` in the argument tokens; an empty `key` is the first string literal that is
/// not an object key.
fn lookup(args: &[Tok<'_>], key: &str) -> Option<Arg> {
    if key.is_empty() {
        return args.iter().enumerate().find_map(|(i, tok)| match tok {
            Tok::Str(text) if args.get(i + 1) != Some(&Tok::Punct(b':')) => Some(Arg::Text(text.clone())),
            _ => None,
        });
    }
    for (i, tok) in args.iter().enumerate() {
        let named = match tok {
            Tok::Ident(name) => *name == key,
            Tok::Str(name) => name == key,
            Tok::Punct(_) => false,
        };
        if !named || args.get(i + 1) != Some(&Tok::Punct(b':')) {
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
/// literal keeps its `${...}` as written.
fn literal(source: &str, start: usize) -> (String, usize) {
    let quote = source.as_bytes()[start];
    let mut text = String::new();
    let mut chars = source[start + 1..].char_indices();
    while let Some((offset, c)) = chars.next() {
        if c as u32 == u32::from(quote) {
            return (text, start + 1 + offset + 1);
        }
        if c != '\\' {
            text.push(c);
            continue;
        }
        match chars.next().map(|(_, e)| e) {
            Some('n') => text.push('\n'),
            Some('t') => text.push('\t'),
            Some('r') => text.push('\r'),
            Some('0') => text.push('\0'),
            Some('u') => {
                let hex: String = chars.clone().take(4).map(|(_, h)| h).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(decoded) if hex.len() == 4 => {
                        text.push(decoded);
                        chars.nth(3);
                    }
                    _ => text.push('u'),
                }
            }
            Some(other) => text.push(other),
            None => break,
        }
    }
    (text, source.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::tally::Family;

    fn names(source: &str) -> Vec<String> {
        calls(source, |name, args| {
            let detail = match args("cmd").or_else(|| args("command")).or_else(|| args("")) {
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
}
