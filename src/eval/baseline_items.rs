//! Read a baseline's output as the same items the tool's answer is scored as, so both arms are
//! held to one gold. Each reader is the agent's reading of the output, not a fuzzy match: a line
//! the reader cannot place contributes nothing, which lowers the baseline's recall.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use regex::Regex;

use super::score::Item;
use super::task::BaselineItems;

/// Items of `output` (command output) and `files` (`(path, text)` of each whole-file read).
pub fn baseline_items(kind: BaselineItems, root: &Path, output: &str, files: &[(String, String)]) -> Vec<Item> {
    match kind {
        BaselineItems::GrepLines => grep_lines(output),
        BaselineItems::Paths => output
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|p| Item {
                path: p.to_string(),
                line: None,
            })
            .collect(),
        BaselineItems::Shas => {
            let shas: Vec<&str> = output
                .lines()
                .filter_map(|l| l.split_whitespace().next())
                .filter(|t| is_sha(t))
                .collect();
            expand_shas(root, &shas)
                .into_iter()
                .map(|path| Item { path, line: None })
                .collect()
        }
        BaselineItems::CountedPaths => counted_paths(output),
        BaselineItems::GrepContext => grep_context(root, output),
        BaselineItems::Explain => explain(root, output),
        BaselineItems::Blame => blame(root, output),
        BaselineItems::Diff => diff_lines(output),
        BaselineItems::DiffSymbols => diff_symbols(output),
        BaselineItems::Read => files
            .iter()
            .flat_map(|(path, text)| {
                (1..=text.lines().count()).map(move |n| Item {
                    path: path.clone(),
                    line: u32::try_from(n).ok(),
                })
            })
            .collect(),
    }
}

/// `wc -l` rows, `<count> <path>` with leading blanks: the path after the count. The `total` row
/// `wc` prints for several files names no file.
fn counted_paths(output: &str) -> Vec<Item> {
    output
        .lines()
        .filter_map(|l| {
            let l = l.trim_start();
            let digits = l.bytes().take_while(u8::is_ascii_digit).count();
            let path = l[digits..].strip_prefix(' ')?.trim_start();
            (digits > 0 && !path.is_empty() && path != "total").then(|| Item {
                path: path.to_string(),
                line: None,
            })
        })
        .collect()
}

fn is_sha(t: &str) -> bool {
    (7..=40).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `path:line:text`: the path ends at the first `:<digits>:`, so a `:` inside a path survives.
fn grep_lines(output: &str) -> Vec<Item> {
    output
        .lines()
        .filter_map(|l| {
            l.match_indices(':').find_map(|(i, _)| {
                let rest = &l[i + 1..];
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                if i == 0 || digits == 0 || rest.as_bytes().get(digits) != Some(&b':') {
                    return None;
                }
                Some(Item {
                    path: l[..i].to_string(),
                    line: rest[..digits].parse().ok(),
                })
            })
        })
        .collect()
}

/// `rg -n -C` / `grep -n -C` output: `path:N:text` (a match) or `path-N-text` (a context line), `--`
/// between groups. A path may hold `-` (`tokio-util/src/x.rs`) and text may hold `:N:`, so the row
/// splits at the earliest `<sep><digits><sep>` (`sep` `:` or `-`, the same on both sides) whose
/// prefix is a file under `root`.
fn grep_context(root: &Path, output: &str) -> Vec<Item> {
    let mut is_file: BTreeMap<&str, bool> = BTreeMap::new();
    output
        .lines()
        .filter(|l| *l != "--")
        .filter_map(|l| {
            let bytes = l.as_bytes();
            l.match_indices([':', '-']).find_map(|(i, sep)| {
                let rest = &l[i + 1..];
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                if i == 0 || digits == 0 || bytes.get(i + 1 + digits) != Some(&sep.as_bytes()[0]) {
                    return None;
                }
                let path = &l[..i];
                let exists = *is_file.entry(path).or_insert_with(|| root.join(path).is_file());
                exists.then(|| Item {
                    path: path.to_string(),
                    line: rest[..digits].parse().ok(),
                })
            })
        })
        .collect()
}

/// Keywords that take a parenthesised operand and are no call.
const NOT_CALLS: &[&str] = &[
    "if", "while", "match", "for", "loop", "return", "in", "as", "move", "fn", "let", "else", "where", "impl", "dyn",
    "mut", "ref", "unsafe", "async", "await", "break", "continue", "yield",
];

/// The `explain` baseline: `path:N:text` rows of files under `root` give `path:line`; a body line
/// (not a row, not a path, not a `<sha> ...` history row, not a `//` comment) gives `->name` for each
/// name it calls.
fn explain(root: &Path, output: &str) -> Vec<Item> {
    let call = Regex::new(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*(?:::\s*<[^()]*>\s*)?\(").expect("static regex");
    let mut is_file: BTreeMap<&str, bool> = BTreeMap::new();
    let mut items = Vec::new();
    for l in output.lines() {
        let rows: Vec<(&str, &str)> = l
            .match_indices(':')
            .filter_map(|(i, _)| {
                let rest = &l[i + 1..];
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                (i > 0 && digits > 0 && rest.as_bytes().get(digits) == Some(&b':')).then(|| (&l[..i], &rest[..digits]))
            })
            .collect();
        if !rows.is_empty() {
            // A grep row; one naming no file of the repository places nothing.
            let row = rows
                .into_iter()
                .find(|(path, _)| *is_file.entry(path).or_insert_with(|| root.join(path).is_file()));
            if let Some((path, line)) = row {
                items.push(Item {
                    path: path.to_string(),
                    line: line.parse().ok(),
                });
            }
            continue;
        }
        let t = l.trim();
        let mut words = t.split_whitespace();
        let history = words.next().is_some_and(is_sha) && words.next().is_some();
        if t.is_empty() || t.starts_with("//") || history || root.join(t).is_file() {
            continue;
        }
        for c in call.captures_iter(l) {
            let (Some(whole), Some(name)) = (c.get(0), c.get(1)) else {
                continue;
            };
            let defined = l[..whole.start()].trim_end().ends_with("fn");
            if !defined && !NOT_CALLS.contains(&name.as_str()) {
                items.push(Item {
                    path: format!("->{}", name.as_str()),
                    line: None,
                });
            }
        }
    }
    items
}

/// Full shas for abbreviated ones, in order, resolved by `git rev-parse` in `root`. An abbreviation
/// git cannot resolve is dropped.
fn expand_shas(root: &Path, shas: &[&str]) -> Vec<String> {
    if shas.is_empty() {
        return Vec::new();
    }
    let mut unique: Vec<&str> = Vec::new();
    for s in shas {
        if !unique.contains(s) {
            unique.push(s);
        }
    }
    let resolved: Vec<Option<String>> = unique
        .chunks(500)
        .flat_map(|chunk| {
            let args: Vec<String> = chunk.iter().map(|s| format!("{s}^{{commit}}")).collect();
            let out = Command::new("git")
                .arg("rev-parse")
                .args(&args)
                .current_dir(root)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output();
            let text = out
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            let full: Vec<String> = text.lines().map(str::to_string).collect();
            // `rev-parse` stops at the first bad name; resolve one by one then.
            if full.len() == chunk.len() {
                full.into_iter().map(Some).collect::<Vec<_>>()
            } else {
                chunk.iter().map(|s| rev_parse_one(root, s)).collect()
            }
        })
        .collect();
    shas.iter()
        .filter_map(|s| {
            let i = unique.iter().position(|u| u == s)?;
            resolved[i].clone()
        })
        .collect()
}

fn rev_parse_one(root: &Path, sha: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &format!("{sha}^{{commit}}")])
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

/// `git blame` default output: `[^]sha [path] (author date time tz LINE) text`.
fn blame(root: &Path, output: &str) -> Vec<Item> {
    let rows: Vec<(&str, u32)> = output
        .lines()
        .filter_map(|l| {
            let sha = l.split_whitespace().next()?.trim_start_matches('^');
            let meta = &l[..l.find(')')?];
            let line = meta.split_whitespace().last()?.parse().ok()?;
            is_sha(sha).then_some((sha, line))
        })
        .collect();
    let shas: Vec<&str> = rows.iter().map(|(s, _)| *s).collect();
    let full = expand_shas(root, &shas);
    if full.len() != rows.len() {
        return Vec::new();
    }
    full.into_iter()
        .zip(rows)
        .map(|(sha, (_, line))| Item {
            path: sha,
            line: Some(line),
        })
        .collect()
}

/// Walk a unified diff, numbering added lines on the new side and removed lines on the old side.
fn walk_diff(output: &str, mut on: impl FnMut(char, &str, u32, &str)) {
    let mut path = String::new();
    let (mut old, mut new) = (0u32, 0u32);
    for l in output.lines() {
        if let Some(p) = l.strip_prefix("+++ ") {
            if p != "/dev/null" {
                path = p.strip_prefix("b/").unwrap_or(p).to_string();
            }
            continue;
        }
        if let Some(p) = l.strip_prefix("--- ") {
            if p != "/dev/null" {
                path = p.strip_prefix("a/").unwrap_or(p).to_string();
            }
            continue;
        }
        if let Some(h) = l.strip_prefix("@@ ") {
            let mut it = h.split_whitespace();
            let start = |s: Option<&str>, sign: char| {
                s.and_then(|s| s.strip_prefix(sign))
                    .and_then(|s| s.split(',').next())
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0)
            };
            old = start(it.next(), '-');
            new = start(it.next(), '+');
            continue;
        }
        match l.chars().next() {
            Some('+') => {
                on('+', &path, new, &l[1..]);
                new += 1;
            }
            Some('-') => {
                on('-', &path, old, &l[1..]);
                old += 1;
            }
            Some(' ') => {
                old += 1;
                new += 1;
            }
            _ => {}
        }
    }
}

fn diff_lines(output: &str) -> Vec<Item> {
    let mut items = Vec::new();
    walk_diff(output, |sign, path, line, _| {
        items.push(Item {
            path: format!("{sign}{path}"),
            line: Some(line),
        });
    });
    items
}

/// Names of Rust definitions on added / removed lines; a name on both sides changed in place and is
/// neither added nor removed.
fn diff_symbols(output: &str) -> Vec<Item> {
    let def = Regex::new(
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:async|const|unsafe|default|extern\s+\x22[^\x22]*\x22)\s+)*(?:fn|struct|enum|union|trait|type|const|static|mod)\s+([A-Za-z_][A-Za-z0-9_]*)|^\s*macro_rules!\s*([A-Za-z_][A-Za-z0-9_]*)",
    )
    .expect("static regex");
    let (mut plus, mut minus): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    walk_diff(output, |sign, _, _, text| {
        if let Some(c) = def.captures(text)
            && let Some(name) = c.get(1).or_else(|| c.get(2))
        {
            let side = if sign == '+' { &mut plus } else { &mut minus };
            side.push(name.as_str().to_string());
        }
    });
    let added = plus.iter().filter(|n| !minus.contains(n)).map(|n| format!("+{n}"));
    let removed = minus.iter().filter(|n| !plus.contains(n)).map(|n| format!("-{n}"));
    added.chain(removed).map(|path| Item { path, line: None }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn it(s: &str) -> Item {
        Item::parse(s)
    }

    #[test]
    fn grep_lines_and_paths() {
        let got = baseline_items(
            BaselineItems::GrepLines,
            Path::new("."),
            "src/a.rs:12:fn x() {}\nsrc/b:c.rs:3:y\nBinary file z matches\n",
            &[],
        );
        assert_eq!(got, vec![it("src/a.rs:12"), it("src/b:c.rs:3")]);
        let paths = baseline_items(BaselineItems::Paths, Path::new("."), "a/b.rs\n\n c.md \n", &[]);
        assert_eq!(paths, vec![it("a/b.rs"), it("c.md")]);
    }

    #[test]
    fn grep_context_reads_match_and_context_lines_of_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join("tokio-util/src")).unwrap();
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("tokio-util/src/codec.rs"), "").unwrap();
        std::fs::write(d.join("src/a.rs"), "").unwrap();
        let out = "\
tokio-util/src/codec.rs-3-    let x = 1;
tokio-util/src/codec.rs:4:fn frame() {}
tokio-util/src/codec.rs-5-
--
src/a.rs-9-    m.insert(\"k:12:v\", 1);
src/a.rs:10:let s = \"tokio-util/src/codec.rs:4:\";
src/a.rs:11-1-x
missing.rs:2:text
";
        let got = baseline_items(BaselineItems::GrepContext, d, out, &[]);
        assert_eq!(
            got,
            vec![
                it("tokio-util/src/codec.rs:3"),
                it("tokio-util/src/codec.rs:4"),
                it("tokio-util/src/codec.rs:5"),
                it("src/a.rs:9"),
                it("src/a.rs:10"),
            ],
            "mixed separators and a path that is no file place nothing"
        );
    }

    #[test]
    fn explain_reads_rows_of_files_and_the_calls_on_body_lines() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/a.rs"), "").unwrap();
        std::fs::write(d.join("src/b.rs"), "").unwrap();
        let out = "\
src/a.rs:3:pub fn run(x: u8) -> u8 {
src/b.rs:9:    run(1);
nowhere.rs:4:    run(2);
    /// Calls `doc_only(` in a comment.
    pub fn run(x: u8) -> u8 {
        let v = helper(x).map(|y| y.wrapping_add(1));
        if (v > 0) { println!(\"{v}\"); }
        parse::<u8>(s); Vec::<u8>::new()
    }
src/b.rs
a1b2c3d Jane 2026-01-01T00:00:00+00:00 fix(rt): call wake()
";
        let got = baseline_items(BaselineItems::Explain, d, out, &[]);
        let want: Vec<Item> = [
            "src/a.rs:3",
            "src/b.rs:9",
            "->helper",
            "->map",
            "->wrapping_add",
            "->parse",
            "->new",
        ]
        .iter()
        .map(|s| it(s))
        .collect();
        assert_eq!(
            got, want,
            "rows of missing files, comments, keywords, macros, the definition and history rows place nothing"
        );
    }

    #[test]
    fn counted_paths_keep_the_path_and_drop_the_total_row() {
        let out = "  2210 src/big.rs\n   980 src/with space.rs\n  3190 total\nnoise\n";
        let got = baseline_items(BaselineItems::CountedPaths, Path::new("."), out, &[]);
        assert_eq!(got, vec![it("src/big.rs"), it("src/with space.rs")]);
    }

    #[test]
    fn read_covers_every_line_of_each_file() {
        let files = vec![("f.rs".to_string(), "a\nb\nc\n".to_string())];
        let got = baseline_items(BaselineItems::Read, Path::new("."), "", &files);
        assert_eq!(got, vec![it("f.rs:1"), it("f.rs:2"), it("f.rs:3")]);
    }

    #[test]
    fn diff_numbers_added_and_removed_lines_per_side() {
        let diff =
            "diff --git a/f.rs b/f.rs\n--- a/f.rs\n+++ b/f.rs\n@@ -3,3 +3,4 @@ fn x\n ctx\n-old\n+new\n+more\n ctx\n";
        let got = baseline_items(BaselineItems::Diff, Path::new("."), diff, &[]);
        assert_eq!(got, vec![it("-f.rs:4"), it("+f.rs:4"), it("+f.rs:5")]);
    }

    #[test]
    fn diff_symbols_cancel_names_changed_in_place() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,3 @@\n-pub fn kept(a: u8) {}\n+pub fn kept(a: u16) {}\n+    pub(crate) async fn added() {}\n-struct Gone;\n+// fn not_code\n";
        let got = baseline_items(BaselineItems::DiffSymbols, Path::new("."), diff, &[]);
        assert_eq!(got, vec![it("+added"), it("-Gone")]);
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e.x")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e.x")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn shas_and_blame_resolve_abbreviations_in_the_repository() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-q"]);
        git(d, &["config", "commit.gpgsign", "false"]);
        std::fs::write(d.join("f.txt"), "one\n").unwrap();
        git(d, &["add", "f.txt"]);
        git(d, &["commit", "-qm", "first"]);
        let first = git(d, &["rev-parse", "HEAD"]);
        std::fs::write(d.join("f.txt"), "one\ntwo\n").unwrap();
        git(d, &["commit", "-qam", "second"]);
        let second = git(d, &["rev-parse", "HEAD"]);

        let log = git(d, &["log", "--oneline"]);
        let got = baseline_items(BaselineItems::Shas, d, &(log + "\nnot-a-sha line\n"), &[]);
        assert_eq!(got, vec![it(&second), it(&first)]);

        let blamed = git(d, &["blame", "f.txt"]);
        let got = baseline_items(BaselineItems::Blame, d, &blamed, &[]);
        assert_eq!(got, vec![it(&format!("{first}:1")), it(&format!("{second}:2"))]);
    }
}
