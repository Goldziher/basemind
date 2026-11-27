//! The Rust text reads of `basemind code deadcode` (#123): the attributes above an item, the block
//! each item sits in, and the module declarations the resolver cannot follow.

use memchr::memmem;

use super::deadcode_lex::is_ident_byte;

/// Rust attributes that make an item an entry point a harness or the linker calls. A path whose
/// last segment is `test` or starts with `test_` (`sqlx::test`, `test_case::test_case`) is one too.
const RUST_ENTRY_ATTRIBUTES: &[&str] = &[
    "bench",
    "no_mangle",
    "export_name",
    "proc_macro",
    "proc_macro_derive",
    "proc_macro_attribute",
    "macro_export",
    "main",
    "tokio::main",
    "rstest",
    "wasm_bindgen",
    "wasm_bindgen_test",
    "napi",
    "pyfunction",
    "pyclass",
    "pymodule",
];

/// Rust attributes that generate no use of the item they mark.
pub(super) const RUST_INERT_ATTRIBUTES: &[&str] = &[
    "derive",
    "doc",
    "allow",
    "warn",
    "deny",
    "expect",
    "must_use",
    "inline",
    "cold",
    "track_caller",
    "non_exhaustive",
    "repr",
    "cfg",
    "cfg_attr",
    "deprecated",
    "serde",
];

/// Whether an attribute path marks an item a harness, runtime or the linker calls.
pub(super) fn is_entry_attribute(path: &str) -> bool {
    let last = path.rsplit("::").next().unwrap_or(path);
    RUST_ENTRY_ATTRIBUTES.contains(&path) || last == "test" || last.starts_with("test_")
}

/// The Rust block a definition sits in, for the rules that depend on it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Container {
    /// A module, an inherent `impl` or a function body.
    #[default]
    None,
    /// A `trait` body: implementations and dispatch use its methods.
    Trait,
    /// An `impl Trait for Type` body: the trait requires its members.
    TraitImpl,
}

/// The attribute paths on the lines directly above `line_start` (and before the item on its own
/// line), skipping comments; a multi-line attribute is read from its opening line.
pub(super) fn attributes(bytes: &[u8], line_start: usize, prefix: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    collect_attributes(prefix, &mut out);
    let mut end = line_start;
    let mut open_tail = false;
    for _ in 0..40 {
        if end == 0 {
            break;
        }
        let begin = memchr::memrchr(b'\n', &bytes[..end - 1]).map_or(0, |at| at + 1);
        let line = String::from_utf8_lossy(&bytes[begin..end - 1]);
        let line = line.trim();
        end = begin;
        if line.starts_with("#[") {
            collect_attributes(line.as_bytes(), &mut out);
            open_tail = false;
            continue;
        }
        if line.starts_with("//") {
            continue;
        }
        if line.is_empty() || !(open_tail || line.ends_with(']') || line.ends_with(',') || line.ends_with(')')) {
            break;
        }
        open_tail = true;
    }
    out
}

/// The path of each `#[path…]` in `text`; an edition-2024 `#[unsafe(no_mangle)]` names the path
/// inside.
fn collect_attributes(text: &[u8], out: &mut Vec<String>) {
    let path_end = |start: usize| {
        text[start..]
            .iter()
            .position(|&b| !(is_ident_byte(b) || b == b':'))
            .map_or(text.len(), |len| start + len)
    };
    let mut from = 0;
    while let Some(at) = memmem::find(&text[from..], b"#[") {
        let mut start = from + at + 2;
        let mut end = path_end(start);
        if &text[start..end] == b"unsafe" && text.get(end) == Some(&b'(') {
            start = end + 1;
            end = path_end(start);
        }
        if end > start {
            out.push(String::from_utf8_lossy(&text[start..end]).into_owned());
        }
        from = end.max(start);
    }
}

/// The block around each byte offset of `starts`, in one forward pass over the file: a stack of
/// the open blocks, each classed by the header before its `{`. Comments, strings and char literals
/// are skipped, and a `;` inside brackets (`[T; N]`) does not cut a header.
pub(super) fn containers(bytes: &[u8], starts: &[usize]) -> Vec<Container> {
    let mut order: Vec<usize> = (0..starts.len()).collect();
    order.sort_unstable_by_key(|&i| starts[i]);
    let mut out = vec![Container::None; starts.len()];
    let mut next = 0;
    // Each open block: its class, and the bracket depth outside it.
    let mut stack: Vec<(Container, usize)> = Vec::new();
    let mut header_start = 0usize;
    let mut brackets = 0usize;
    let n = bytes.len();
    let mut at = 0;
    loop {
        while next < order.len() && starts[order[next]] <= at {
            out[order[next]] = stack.last().map_or(Container::None, |(class, _)| *class);
            next += 1;
        }
        if at >= n || next == order.len() {
            break;
        }
        let b = bytes[at];
        match b {
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                at = memchr::memchr(b'\n', &bytes[at..]).map_or(n, |len| at + len);
                continue;
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at = memmem::find(&bytes[at + 2..], b"*/").map_or(n, |len| at + 2 + len + 2);
                continue;
            }
            b'"' => {
                at = string_end(bytes, at + 1);
                continue;
            }
            b'\'' => {
                at = match (bytes.get(at + 1), bytes.get(at + 2)) {
                    (Some(b'\\'), _) => bytes[at + 2..]
                        .iter()
                        .take(12)
                        .position(|&c| c == b'\'')
                        .map_or(at + 1, |len| at + 2 + len + 1),
                    (Some(_), Some(b'\'')) => at + 3,
                    _ => at + 1,
                };
                continue;
            }
            b'r' if at == 0 || !is_ident_byte(bytes[at - 1]) => {
                let hashes = bytes[at + 1..].iter().take_while(|&&c| c == b'#').count();
                if bytes.get(at + 1 + hashes) == Some(&b'"') {
                    let mut closing = vec![b'"'];
                    closing.resize(hashes + 1, b'#');
                    let body = at + 1 + hashes + 1;
                    at = memmem::find(&bytes[body..], &closing).map_or(n, |len| body + len + closing.len());
                    continue;
                }
            }
            b'(' | b'[' => brackets += 1,
            b')' | b']' => brackets = brackets.saturating_sub(1),
            b';' if brackets == 0 => header_start = at + 1,
            b'{' => {
                stack.push((classify(&bytes[header_start..at]), brackets));
                brackets = 0;
                header_start = at + 1;
            }
            b'}' => {
                brackets = stack.pop().map_or(0, |(_, outside)| outside);
                header_start = at + 1;
            }
            _ => {}
        }
        at += 1;
    }
    out
}

/// Past the `"` closing a Rust string whose body starts at `from`.
fn string_end(bytes: &[u8], from: usize) -> usize {
    let mut at = from;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'"' => return at + 1,
            _ => at += 1,
        }
    }
    bytes.len()
}

/// The class of the block a header opens: `trait …`, `impl … for …`, or anything else.
fn classify(header: &[u8]) -> Container {
    if memmem::find(header, b"impl").is_none() && memmem::find(header, b"trait").is_none() {
        return Container::None;
    }
    let header = String::from_utf8_lossy(header);
    let words: Vec<&str> = header
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("//") && !line.starts_with("#["))
        .flat_map(str::split_whitespace)
        .collect();
    let keyword = words
        .iter()
        .position(|word| *word == "impl" || word.starts_with("impl<") || *word == "trait");
    match keyword.map(|at| (words[at], &words[at..])) {
        Some(("trait", _)) => Container::Trait,
        Some((_, rest)) if rest.contains(&"for") => Container::TraitImpl,
        _ => Container::None,
    }
}

/// What a Rust file says about module declarations the resolver cannot follow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct HiddenModules {
    /// Every name a `mod` token declares in code, macro bodies included (`cfg_rt! { pub mod x; }`).
    pub mod_names: Vec<String>,
    /// The file names of string literals ending in `.rs` (`include!("gen.rs")`,
    /// `#[cfg_attr(unix, path = "unix.rs")]`).
    pub rs_strings: Vec<String>,
    /// A macro declares modules from its arguments (`mod $name;`).
    pub macro_modules: bool,
}

/// Feed one code token of a Rust file: a token after `mod` is a declared module name.
pub(super) fn note_module_token(
    hidden: &mut HiddenModules,
    previous_was_mod: bool,
    bytes: &[u8],
    token: &[u8],
    at: usize,
) {
    if !previous_was_mod {
        return;
    }
    if at > 0 && bytes[at - 1] == b'$' {
        hidden.macro_modules = true;
    } else if hidden.mod_names.len() < MODULE_NAMES_MAX {
        hidden.mod_names.push(String::from_utf8_lossy(token).into_owned());
    }
}

/// Module names kept per file.
const MODULE_NAMES_MAX: usize = 4096;

/// The file names of the string literals ending in `.rs` in `bytes`.
pub(super) fn rs_strings(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for at in memmem::find_iter(bytes, b".rs\"") {
        let line_start = memchr::memrchr(b'\n', &bytes[..at]).map_or(0, |p| p + 1);
        let Some(open) = memchr::memrchr(b'"', &bytes[line_start..at]).map(|p| line_start + p) else {
            continue;
        };
        let literal = String::from_utf8_lossy(&bytes[open + 1..at + 3]);
        let name = literal.rsplit('/').next().unwrap_or(&literal).to_string();
        if !name.is_empty() && out.len() < MODULE_NAMES_MAX {
            out.push(name);
        }
    }
    out
}

#[cfg(test)]
pub(super) fn attributes_for_test(bytes: &[u8], start: usize) -> Vec<String> {
    let line_start = memchr::memrchr(b'\n', &bytes[..start]).map_or(0, |at| at + 1);
    attributes(bytes, line_start, &bytes[line_start..start])
}
