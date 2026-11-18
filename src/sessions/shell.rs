//! Classify a shell command line, and an `apply_patch` body, into a [`Call`]. Only the family and
//! the file paths named as arguments are kept; the command text itself goes no further.

use super::tally::{Call, Family};

/// The first word of a segment, after wrappers, mapped to what it does. Higher wins when one command
/// line runs several.
fn rank(family: Family) -> u8 {
    match family {
        Family::Basemind => 8,
        Family::ShellSearch => 7,
        Family::ShellGit => 6,
        Family::Edit => 5,
        Family::Write => 4,
        Family::ReadWhole => 3,
        Family::ReadRange => 2,
        Family::Other => 1,
        Family::SearchTool => 0,
    }
}

/// Classify a shell command line (a script for `bash -lc`, or an argv joined by spaces).
pub fn classify(script: &str) -> Call {
    if script.contains("*** Begin Patch") {
        return patch(script);
    }
    let mut call = Call::of(Family::Other);
    for segment in script.split(['\n', ';', '|', '&', '(', ')', '`']) {
        let words = words(segment);
        let (family, read, edited, written) = classify_words(&words);
        if rank(family) > rank(call.family) {
            call.family = family;
        }
        call.read.extend(read);
        call.edited.extend(edited);
        call.written.extend(written);
    }
    call
}

/// The files an `apply_patch` body updates (edits), adds or deletes (writes).
pub fn patch(body: &str) -> Call {
    let mut call = Call::of(Family::Edit);
    for line in body.lines() {
        let line = line.trim();
        if let Some(path) = line.strip_prefix("*** Update File:") {
            call.edited.push(path.trim().to_owned());
        } else if let Some(path) = line
            .strip_prefix("*** Add File:")
            .or_else(|| line.strip_prefix("*** Delete File:"))
            .or_else(|| line.strip_prefix("*** Move to:"))
        {
            call.written.push(path.trim().to_owned());
        }
    }
    if call.edited.is_empty() && !call.written.is_empty() {
        call.family = Family::Write;
    }
    call
}

fn words(segment: &str) -> Vec<String> {
    segment
        .split_whitespace()
        .map(|w| w.trim_matches(|c| c == '"' || c == '\'').to_owned())
        .filter(|w| !w.is_empty())
        .collect()
}

type Classified = (Family, Vec<String>, Vec<String>, Vec<String>);

fn classify_words(words: &[String]) -> Classified {
    let (args, written) = split_redirects(words);
    let mut rest = args.as_slice();
    while let Some((first, tail)) = rest.split_first() {
        let wrapper = matches!(first.as_str(), "sudo" | "env" | "command" | "time" | "nice" | "xargs")
            || first.contains('=') && !first.starts_with('-');
        let flag_of_wrapper = first.starts_with('-') && rest.len() < args.len();
        if wrapper || flag_of_wrapper {
            rest = tail;
        } else {
            break;
        }
    }
    let Some((program, args)) = rest.split_first() else {
        return (
            if written.is_empty() {
                Family::Other
            } else {
                Family::Write
            },
            Vec::new(),
            Vec::new(),
            written,
        );
    };
    let program = program.rsplit('/').next().unwrap_or(program);
    let operands = || operands(args);
    let (family, read, edited) = match program {
        "basemind" => (Family::Basemind, Vec::new(), Vec::new()),
        "rg" | "grep" | "egrep" | "fgrep" | "ag" | "ack" | "find" | "fd" | "sg" | "ast-grep" => {
            (Family::ShellSearch, Vec::new(), Vec::new())
        }
        "git" => (git(args), Vec::new(), Vec::new()),
        "cat" | "bat" | "nl" | "less" | "more" => (Family::ReadWhole, operands(), Vec::new()),
        "head" | "tail" => (Family::ReadRange, operands(), Vec::new()),
        "sed" => sed(args),
        "apply_patch" | "applypatch" => (Family::Edit, Vec::new(), Vec::new()),
        _ => (Family::Other, Vec::new(), Vec::new()),
    };
    let redirected = matches!(family, Family::Other | Family::ReadWhole | Family::ReadRange);
    let family = if redirected && !written.is_empty() {
        Family::Write
    } else {
        family
    };
    (family, read, edited, written)
}

/// Split `> path` / `>> path` / `>path` redirects off the words; `/dev/*` and `&N` targets are not
/// writes.
fn split_redirects(words: &[String]) -> (Vec<String>, Vec<String>) {
    let mut args = Vec::new();
    let mut written = Vec::new();
    let mut iter = words.iter();
    while let Some(word) = iter.next() {
        let target = word.trim_start_matches(|c: char| c.is_ascii_digit());
        if let Some(target) = target.strip_prefix(">>").or_else(|| target.strip_prefix('>')) {
            let target = if target.is_empty() {
                iter.next().map(String::as_str).unwrap_or("")
            } else {
                target
            };
            if !target.is_empty() && !target.starts_with("/dev/") && !target.starts_with('&') {
                written.push(target.to_owned());
            }
        } else if word.starts_with('<') {
            if word.len() == 1 {
                iter.next();
            }
        } else {
            args.push(word.clone());
        }
    }
    (args, written)
}

fn operands(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut skip_value = false;
    for arg in args {
        if skip_value {
            skip_value = false;
            continue;
        }
        if arg.starts_with('-') {
            skip_value = matches!(arg.as_str(), "-n" | "-c" | "--lines" | "--bytes");
            continue;
        }
        if arg.chars().all(|c| c.is_ascii_digit() || c == '+') {
            continue;
        }
        out.push(arg.clone());
    }
    out
}

fn git(args: &[String]) -> Family {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-C" | "-c" | "--git-dir" | "--work-tree" => {
                iter.next();
            }
            flag if flag.starts_with('-') => {}
            "log" | "blame" | "show" | "grep" | "shortlog" | "annotate" => return Family::ShellGit,
            _ => return Family::Other,
        }
    }
    Family::Other
}

/// `sed -i` edits its file operands; `sed -n` (or any other) reads them. The script is the first
/// operand unless `-e` / `-f` named it.
fn sed(args: &[String]) -> (Family, Vec<String>, Vec<String>) {
    let in_place = args
        .iter()
        .any(|a| a == "-i" || a.starts_with("-i") || a == "--in-place");
    let script_given = args.iter().any(|a| a == "-e" || a == "-f");
    let mut files = Vec::new();
    let mut script_seen = script_given;
    let mut skip_value = false;
    for arg in args {
        if skip_value {
            skip_value = false;
            continue;
        }
        if arg.starts_with('-') {
            skip_value = arg == "-e" || arg == "-f";
            continue;
        }
        if !script_seen {
            script_seen = true;
            continue;
        }
        files.push(arg.clone());
    }
    if in_place {
        (Family::Edit, Vec::new(), files)
    } else {
        (Family::ReadRange, files, Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searches_and_history_are_classified() {
        assert_eq!(classify("rg -n 'fn main' src").family, Family::ShellSearch);
        assert_eq!(classify("cd sub && grep -r x .").family, Family::ShellSearch);
        assert_eq!(classify("git -C . log --oneline -5").family, Family::ShellGit);
        assert_eq!(classify("git status").family, Family::Other);
        assert_eq!(classify("basemind code grep x").family, Family::Basemind);
        assert_eq!(
            classify("find . -name '*.rs' | xargs grep x").family,
            Family::ShellSearch
        );
    }

    #[test]
    fn reads_keep_their_operands() {
        let whole = classify("cat src/a.rs");
        assert_eq!(whole.family, Family::ReadWhole);
        assert_eq!(whole.read, vec!["src/a.rs"]);
        let ranged = classify("sed -n '10,40p' src/b.rs");
        assert_eq!(ranged.family, Family::ReadRange);
        assert_eq!(ranged.read, vec!["src/b.rs"]);
        assert_eq!(classify("head -n 20 src/c.rs").read, vec!["src/c.rs"]);
    }

    #[test]
    fn edits_and_writes_keep_their_targets() {
        let in_place = classify("sed -i '' 's/a/b/' src/a.rs");
        assert_eq!(in_place.family, Family::Edit);
        assert_eq!(in_place.edited, vec!["src/a.rs"]);
        let redirect = classify("cat > notes.txt <<EOF");
        assert_eq!(redirect.written, vec!["notes.txt"]);
        assert_eq!(classify("cargo test 2>&1 >/dev/null").written, Vec::<String>::new());
    }

    #[test]
    fn a_patch_lists_its_files() {
        let call = classify(
            "apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** Add File: src/new.rs\n+z\n*** End Patch\nEOF",
        );
        assert_eq!(call.family, Family::Edit);
        assert_eq!(call.edited, vec!["src/a.rs"]);
        assert_eq!(call.written, vec!["src/new.rs"]);
    }
}
