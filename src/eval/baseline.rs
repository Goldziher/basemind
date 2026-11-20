//! The "without basemind" side of a token-savings comparison: run the task's shell command(s) in
//! the workspace root and read its files, then charge every byte of output to the same tokenizer
//! that counts the basemind response.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::baseline_items::baseline_items;
use super::score::Item;
use super::task::BaselineSpec;

/// Result of running a baseline.
#[derive(Debug, Clone)]
pub struct BaselineRun {
    pub tokens: u64,
    pub elapsed_us: u64,
    /// Nothing but whitespace came out of the commands and the files read: the agent got no
    /// answer, which the gate counts as a baseline failure, not as a cheap one.
    pub empty: bool,
    /// The output read as items (`spec.items`), for scoring against the task's gold.
    pub items: Option<Vec<Item>>,
}

/// The environment every baseline command runs in, on top of the caller's, so what it prints, and
/// so its token count, is the same on every machine: no system or global git configuration (a
/// `color.ui`, `core.autocrlf`, pager or diff driver there changes what `git` prints) and the C
/// locale.
const BASELINE_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("LC_ALL", "C"),
];

/// Run `spec` in `root`. A non-zero exit from a grep-style command is tolerated (`git grep` exits
/// 1 on "no match", which is a legitimate empty output); failing to spawn, or a signal/exit >1,
/// is an error so a typo'd command cannot masquerade as a tiny baseline.
pub fn run_baseline(root: &Path, spec: &BaselineSpec, count: fn(&str) -> u64) -> Result<BaselineRun> {
    let started = std::time::Instant::now();
    let mut text = String::new();
    let mut files: Vec<(String, String)> = Vec::new();
    if let Some(cmds) = &spec.grep {
        for cmd in cmds.items() {
            let mut command = Command::new("sh");
            command
                .arg("-c")
                .arg(cmd)
                .current_dir(root)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .envs(BASELINE_ENV.iter().copied());
            // Git for Windows' `sh` rewrites an argument that looks like a POSIX path into a
            // Windows path before it runs a native program, so `git blame -L '/fn alpha/,/^}/'`
            // reached git as a path and exited 129. A baseline means its arguments verbatim.
            #[cfg(windows)]
            command.env("MSYS_NO_PATHCONV", "1").env("MSYS2_ARG_CONV_EXCL", "*");
            let out = command.output().with_context(|| format!("spawn baseline `{cmd}`"))?;
            match out.status.code() {
                Some(0 | 1) => {}
                other => bail!("baseline `{cmd}` failed (exit {other:?})"),
            }
            text.push_str(&String::from_utf8_lossy(&out.stdout));
        }
    }
    for rel in &spec.read {
        let joined = root.join(rel);
        let canonical = joined
            .canonicalize()
            .with_context(|| format!("baseline read `{rel}`"))?;
        let root_canonical = root.canonicalize().context("canonicalize root")?;
        if !canonical.starts_with(&root_canonical) {
            bail!("baseline read `{rel}` escapes the workspace root");
        }
        let raw = std::fs::read(&canonical).with_context(|| format!("baseline read `{rel}`"))?;
        files.push((rel.clone(), String::from_utf8_lossy(&raw).into_owned()));
    }
    let command_output = text.clone();
    for (_, body) in &files {
        text.push_str(body);
    }
    let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    Ok(BaselineRun {
        tokens: count(&text),
        elapsed_us,
        empty: text.trim().is_empty(),
        items: spec.items.map(|k| baseline_items(k, root, &command_output, &files)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::task::{BaselineItems, OneOrMany};

    fn count(s: &str) -> u64 {
        s.len() as u64
    }

    #[test]
    fn counts_command_output_and_file_reads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "12345").unwrap();
        let spec = BaselineSpec {
            grep: Some(OneOrMany::One("printf abc".into())),
            read: vec!["f.txt".into()],
            items: None,
        };
        let run = run_baseline(dir.path(), &spec, count).unwrap();
        assert_eq!(run.tokens, 8);
        assert!(!run.empty);
        assert!(run.items.is_none());
    }

    #[test]
    fn reads_items_from_command_output_only_unless_the_reader_is_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.rs"), "a\nb\n").unwrap();
        let grep = BaselineSpec {
            grep: Some(OneOrMany::One("printf 'f.rs:2:b\\n'".into())),
            read: vec!["f.rs".into()],
            items: Some(BaselineItems::GrepLines),
        };
        let items = run_baseline(dir.path(), &grep, count).unwrap().items.unwrap();
        assert_eq!(items, vec![Item::parse("f.rs:2")]);
        let read = BaselineSpec {
            items: Some(BaselineItems::Read),
            ..grep
        };
        let items = run_baseline(dir.path(), &read, count).unwrap().items.unwrap();
        assert_eq!(items, vec![Item::parse("f.rs:1"), Item::parse("f.rs:2")]);
    }

    #[test]
    fn commands_run_without_host_git_configuration_in_the_c_locale() {
        let dir = tempfile::tempdir().unwrap();
        let spec = BaselineSpec {
            grep: Some(OneOrMany::One(
                r#"test "$GIT_CONFIG_NOSYSTEM" = 1 && test "$GIT_CONFIG_GLOBAL" = /dev/null && test "$LC_ALL" = C && printf ok"#
                    .into(),
            )),
            read: vec![],
            items: None,
        };
        assert_eq!(run_baseline(dir.path(), &spec, count).unwrap().tokens, 2);
    }

    #[test]
    fn tolerates_no_match_but_not_broken_commands_or_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let no_match = BaselineSpec {
            grep: Some(OneOrMany::One("exit 1".into())),
            read: vec![],
            items: None,
        };
        let empty = run_baseline(dir.path(), &no_match, count).unwrap();
        assert_eq!((empty.tokens, empty.empty), (0, true));
        let blank = BaselineSpec {
            grep: Some(OneOrMany::One("printf '\\n  \\n'".into())),
            read: vec![],
            items: None,
        };
        assert!(
            run_baseline(dir.path(), &blank, count).unwrap().empty,
            "whitespace is no answer"
        );
        let broken = BaselineSpec {
            grep: Some(OneOrMany::One("exit 2".into())),
            read: vec![],
            items: None,
        };
        assert!(run_baseline(dir.path(), &broken, count).is_err());
        let escape = BaselineSpec {
            grep: None,
            read: vec!["../outside".into()],
            items: None,
        };
        assert!(run_baseline(dir.path(), &escape, count).is_err());
    }
}
