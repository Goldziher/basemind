//! Read a baseline's output as the same items the tool's answer is scored as, so both arms are
//! held to one gold. Each reader is the agent's reading of the output, not a fuzzy match: a line
//! the reader cannot place contributes nothing, which lowers the baseline's recall.

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
