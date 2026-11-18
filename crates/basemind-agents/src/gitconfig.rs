//! The repository's own git configuration, read before a launch. A harness runs git on the
//! repository by itself (Codex 0.169 ran `git status` when a turn started), outside any sandbox it
//! applies to the agent, and some keys there make git run a command on such a read.

use std::path::{Path, PathBuf};

use crate::error::LaunchError;

/// Keys, as `section.key` with any subsection left out, through which a read-only git command
/// runs a program: a filter's clean side runs on `git status` (seen with Codex 0.169), a diff
/// driver or an external diff on `git diff`, and an include pulls in a file this check does not
/// read. `core.fsmonitor` is not here: every child gets it switched off through the environment.
const COMMAND_KEYS: &[(&str, &str)] = &[
    ("filter", "clean"),
    ("filter", "smudge"),
    ("filter", "process"),
    ("diff", "textconv"),
    ("diff", "command"),
    ("diff", "external"),
    ("include", "path"),
    ("includeif", "path"),
];

/// What [`command_key`] reports for a section header it cannot parse.
const UNPARSEABLE_HEADER: &str = "an unparseable section header";

/// Refuses a launch whose working directory lies in a repository (or a worktree of one) whose
/// `config` or `config.worktree` sets one of [`COMMAND_KEYS`]. A file that exists and cannot be
/// read, or holds a section header that cannot be parsed, counts as setting one. User and system
/// git configuration are the user's own and are not read.
pub(crate) fn check_repository(cwd: &Path) -> Result<(), LaunchError> {
    let Some(git_dir) = git_dir(cwd) else {
        return Ok(());
    };
    let common = std::fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|common| git_dir.join(common.trim()));
    let files = [
        common.as_deref().unwrap_or(&git_dir).join("config"),
        git_dir.join("config"),
        git_dir.join("config.worktree"),
    ];
    for path in files {
        let text = match std::fs::read(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                return Err(LaunchError::GitConfig {
                    key: "an unreadable configuration".to_owned(),
                    path,
                });
            }
        };
        if let Some(key) = command_key(&String::from_utf8_lossy(&text)) {
            return Err(LaunchError::GitConfig { key, path });
        }
    }
    Ok(())
}

/// The git directory of the repository `cwd` lies in: the nearest `.git` directory, or the
/// directory a `.git` file names (`gitdir: <path>`, relative to the file's directory).
fn git_dir(cwd: &Path) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        let dot_git = dir.join(".git");
        let Ok(metadata) = dot_git.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            return Some(dot_git);
        }
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let target = text.trim().strip_prefix("gitdir:")?.trim();
        return Some(dir.join(target));
    }
    None
}

/// The first of [`COMMAND_KEYS`] `text` sets, as `section.*.key` or `section.key`, or
/// [`UNPARSEABLE_HEADER`] for a header this check cannot read. Sections and keys are compared
/// without case, as git does. Headers follow git's grammar: a leading byte-order mark is
/// skipped, a quoted subsection may hold `]` and `\"`, and a header may be followed on its line
/// by another header or by a key. A line continued with a backslash is read as lines of its own,
/// which can only find a key that is not there, never miss one.
fn command_key(text: &str) -> Option<String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut section = String::new();
    let mut subsection = false;
    for line in text.lines() {
        let mut line = line.trim_start();
        while let Some(header) = line.strip_prefix('[') {
            let Some((name, sub, rest)) = parse_header(header) else {
                // git refuses such a file, unless it reads a header this parser does not; the
                // keys under it cannot be told apart, so the launch is refused.
                return Some(UNPARSEABLE_HEADER.to_owned());
            };
            section = name;
            subsection = sub;
            line = rest.trim_start();
        }
        if line.starts_with(['#', ';']) {
            continue;
        }
        let key = line
            .split(|c: char| c == '=' || c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if COMMAND_KEYS.iter().any(|(s, k)| *s == section && *k == key) {
            return Some(if subsection {
                format!("{section}.*.{key}")
            } else {
                format!("{section}.{key}")
            });
        }
    }
    None
}

/// A header after its `[`: the lower-case section name, whether a subsection follows it (quoted,
/// or the legacy `[section.sub]`), and the rest of the line after the closing `]`.
fn parse_header(header: &str) -> Option<(String, bool, &str)> {
    let end = header.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '.'))?;
    let (name, mut rest) = header.split_at(end);
    let (name, mut subsection) = match name.split_once('.') {
        Some((name, _)) => (name, true),
        None => (name, false),
    };
    // git's `isspace` (` `, `\t`, `\r`, `\n`); a lone `\r` is not a line end to git.
    rest = rest.trim_start_matches([' ', '\t', '\r']);
    if let Some(quoted) = rest.strip_prefix('"') {
        subsection = true;
        let mut chars = quoted.char_indices();
        let close = loop {
            match chars.next()? {
                (_, '\\') => {
                    chars.next()?;
                }
                (at, '"') => break at,
                _ => {}
            }
        };
        rest = &quoted[close + 1..];
    }
    let rest = rest.strip_prefix(']')?;
    Some((name.to_ascii_lowercase(), subsection, rest))
}

#[cfg(test)]
#[path = "gitconfig_tests.rs"]
mod tests;
