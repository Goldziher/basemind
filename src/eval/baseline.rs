//! The "without basemind" side of a token-savings comparison: run the task's shell command(s) in
//! the workspace root and read its files, then charge every byte of output to the same tokenizer
//! that counts the basemind response.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::task::BaselineSpec;

/// Result of running a baseline.
#[derive(Debug, Clone)]
pub struct BaselineRun {
    pub tokens: u64,
    pub elapsed_us: u64,
}

/// Run `spec` in `root`. A non-zero exit from a grep-style command is tolerated (`git grep` exits
/// 1 on "no match", which is a legitimate empty output); failing to spawn, or a signal/exit >1,
/// is an error so a typo'd command cannot masquerade as a tiny baseline.
pub fn run_baseline(root: &Path, spec: &BaselineSpec, count: fn(&str) -> u64) -> Result<BaselineRun> {
    let started = std::time::Instant::now();
    let mut text = String::new();
    if let Some(cmds) = &spec.grep {
        for cmd in cmds.items() {
            let out = Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(root)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .with_context(|| format!("spawn baseline `{cmd}`"))?;
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
        text.push_str(&String::from_utf8_lossy(&raw));
    }
    Ok(BaselineRun {
        tokens: count(&text),
        elapsed_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::task::OneOrMany;

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
        };
        assert_eq!(run_baseline(dir.path(), &spec, count).unwrap().tokens, 8);
    }

    #[test]
    fn tolerates_no_match_but_not_broken_commands_or_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let no_match = BaselineSpec {
            grep: Some(OneOrMany::One("exit 1".into())),
            read: vec![],
        };
        assert_eq!(run_baseline(dir.path(), &no_match, count).unwrap().tokens, 0);
        let broken = BaselineSpec {
            grep: Some(OneOrMany::One("exit 2".into())),
            read: vec![],
        };
        assert!(run_baseline(dir.path(), &broken, count).is_err());
        let escape = BaselineSpec {
            grep: None,
            read: vec!["../outside".into()],
        };
        assert!(run_baseline(dir.path(), &escape, count).is_err());
    }
}
