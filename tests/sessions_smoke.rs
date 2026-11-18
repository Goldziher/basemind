//! `basemind sessions` over synthetic transcripts (issue #128, ADR-0049): the counts per host, the
//! containment of the transcript roots, and that no transcript text reaches the output.
//!
//! The fixtures under `tests/fixtures/sessions/` imitate the Claude Code and Codex record shapes;
//! `{{REPO}}`, `{{WORKTREE}}` and `{{OTHER}}` are replaced with the temporary checkouts. Every
//! secret-bearing field holds a `SECRET-` marker, so any echo of transcript text fails the test.
//! The transcript roots always point into a temporary home, never the real one.

use std::path::{Path, PathBuf};
use std::process::Command;

use basemind::sessions::claude::slug;
use basemind::sessions::tally::{Counts, OutputTokens};
use basemind::sessions::{Host, Report, Scope, SessionRoots, collect};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sessions");

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
    worktree: PathBuf,
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// Copy fixture `name` to `dest`, substituting the checkout paths.
fn place(env: &Env, other: &Path, name: &str, dest: &Path) {
    let text = std::fs::read_to_string(Path::new(FIXTURES).join(name)).expect("read fixture");
    let text = text
        .replace("{{REPO}}", &env.repo.to_string_lossy())
        .replace("{{WORKTREE}}", &env.worktree.to_string_lossy())
        .replace("{{OTHER}}", &other.to_string_lossy());
    std::fs::create_dir_all(dest.parent().expect("parent")).expect("mkdir");
    std::fs::write(dest, text).expect("write fixture");
}

/// A repository with one linked worktree, another repository, and a home holding transcripts of
/// all three plus the escape attempts.
fn setup() -> Env {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = dir.path().canonicalize().expect("canonical tempdir");
    let repo = base.join("repo");
    let other = base.join("other");
    let worktree = base.join("wt-feature");
    let home = base.join("home");
    let outside = base.join("outside");
    for d in [&repo, &other, &home, &outside] {
        std::fs::create_dir_all(d).expect("mkdir");
    }
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "feature", &worktree.to_string_lossy()],
    );
    let env = Env {
        _dir: dir,
        home,
        repo,
        worktree,
    };

    let claude = env.home.join(".claude/projects");
    let repo_dir = claude.join(slug(&env.repo));
    place(&env, &other, "claude/main.jsonl", &repo_dir.join("s1.jsonl"));
    place(&env, &other, "claude/unsupported.jsonl", &repo_dir.join("s3.jsonl"));
    // A transcript of another directory that landed in this repository's project directory: read up
    // to its cwd, then skipped.
    place(&env, &other, "claude/other-repo.jsonl", &repo_dir.join("s4.jsonl"));
    place(
        &env,
        &other,
        "claude/worktree.jsonl",
        &claude.join(slug(&env.worktree)).join("s2.jsonl"),
    );
    // Another repository's own project directory is never opened.
    place(
        &env,
        &other,
        "claude/other-repo.jsonl",
        &claude.join(slug(&other)).join("s5.jsonl"),
    );

    let codex = env.home.join(".codex/sessions/2026/11/18");
    place(&env, &other, "codex/main.jsonl", &codex.join("rollout-a.jsonl"));
    place(&env, &other, "codex/other-repo.jsonl", &codex.join("rollout-b.jsonl"));
    place(&env, &other, "codex/pre-envelope.jsonl", &codex.join("rollout-c.jsonl"));
    place(&env, &other, "codex/unsupported.jsonl", &codex.join("rollout-d.jsonl"));

    // Escapes: a transcript symlinked to a file outside the root, and a day directory symlinked out.
    place(&env, &other, "claude/main.jsonl", &outside.join("escaped.jsonl"));
    place(&env, &other, "codex/main.jsonl", &outside.join("day/rollout-e.jsonl"));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.join("escaped.jsonl"), repo_dir.join("s6.jsonl")).expect("symlink");
        std::os::unix::fs::symlink(outside.join("day"), env.home.join(".codex/sessions/2026/11/19")).expect("symlink");
    }
    env
}

fn report(env: &Env, since: Option<&str>) -> Report {
    collect(&SessionRoots::under_home(&env.home), &Scope::discover(&env.repo), since)
}

#[test]
fn counts_each_host_and_keeps_to_this_repository() {
    let env = setup();
    let report = report(&env, None);
    let ids: Vec<(Host, &str, &str)> = report
        .sessions
        .iter()
        .map(|s| (s.host, s.id.as_str(), s.checkout.as_str()))
        .collect();
    assert_eq!(
        ids,
        vec![
            (Host::Claude, "s2", "wt-feature"),
            (Host::Codex, "rollout-a", "main"),
            (Host::Claude, "s1", "main"),
        ]
    );

    let claude = &report.sessions[2];
    assert_eq!(
        claude.counts,
        Counts {
            basemind: 1,
            search_tool: 1,
            shell_search: 1,
            shell_git: 1,
            read_whole: 5,
            read_range: 1,
            edit: 2,
            write: 1,
            other: 0,
            errors: 2,
            basemind_errors: 1,
            edits_without_read: 1,
            writes_without_search: 0,
            files_read_repeatedly: 1,
            outside_paths: 1,
            output_tokens: OutputTokens {
                read_search: 73,
                edit: 100,
                other: 5
            },
        }
    );
    assert_eq!(claude.adoption, Some(1.0 / 9.0));
    assert_eq!(claude.repeated_reads.len(), 1);
    assert_eq!(claude.repeated_reads[0].path, "src/main.rs");
    assert_eq!(claude.repeated_reads[0].reads, 4);
    assert_eq!(claude.started.as_deref(), Some("2026-11-18T10:00:00.000Z"));
    assert_eq!(claude.ended.as_deref(), Some("2026-11-18T11:00:00.000Z"));

    let worktree = &report.sessions[0];
    assert_eq!(
        (
            worktree.counts.edit,
            worktree.counts.basemind,
            worktree.counts.edits_without_read,
            worktree.counts.writes_without_search
        ),
        (1, 1, 1, 1)
    );

    let codex = &report.sessions[1];
    assert_eq!(
        (
            codex.counts.shell_search,
            codex.counts.basemind,
            codex.counts.edit,
            codex.counts.read_whole,
            codex.counts.errors,
            codex.counts.edits_without_read,
            codex.counts.writes_without_search,
        ),
        (1, 1, 1, 1, 1, 0, 0)
    );
    assert_eq!(
        codex.counts.output_tokens,
        OutputTokens {
            read_search: 30,
            edit: 50,
            other: 0
        }
    );
    assert_eq!(codex.format_version.as_deref(), Some("0.58.0"));

    assert_eq!(report.aggregate.sessions, 3);
    assert_eq!(report.aggregate.counts.basemind, 3);
}

#[test]
fn unknown_formats_are_reported_not_skipped() {
    let env = setup();
    let report = report(&env, None);
    let unsupported: Vec<(Host, &str, &str)> = report
        .unsupported
        .iter()
        .map(|u| (u.host, u.id.as_str(), u.reason.as_str()))
        .collect();
    assert_eq!(
        unsupported,
        vec![
            (Host::Claude, "s3", "unknown version 9.0.0"),
            (Host::Codex, "rollout-d", "unknown cli_version 7.1.0"),
        ]
    );
    assert_eq!(report.unsupported_unattributed, 1, "the pre-envelope Codex rollout");
}

#[test]
fn other_repositories_and_escapes_are_not_counted() {
    let env = setup();
    let report = report(&env, None);
    // The Claude transcript of another directory inside this repository's project directory, and the
    // Codex rollout of another directory. The other repository's own Claude project directory is not
    // walked, so it adds nothing.
    assert_eq!(report.other_directories, 2);
    #[cfg(unix)]
    {
        assert_eq!(
            report.refused, 2,
            "the symlinked transcript and the symlinked day directory"
        );
        assert_eq!(report.sessions.len(), 3, "neither escape became a session");
    }
}

#[test]
fn a_missing_root_is_reported_and_since_filters_by_day() {
    let env = setup();
    std::fs::remove_dir_all(env.home.join(".codex")).expect("remove codex root");
    let report = report(&env, Some("2026-11-18"));
    let states: Vec<(Host, &str)> = report.roots.iter().map(|r| (r.host, r.state.as_str())).collect();
    assert_eq!(states, vec![(Host::Claude, "present"), (Host::Codex, "missing")]);
    let ids: Vec<&str> = report.sessions.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["s1"], "the worktree session ended on 2026-11-17");
}

#[test]
fn a_repository_configuration_cannot_move_the_roots() {
    let env = setup();
    std::fs::write(
        env.repo.join("basemind.toml"),
        "[sessions]\nclaude_projects = \"/\"\ncodex_sessions = \"/\"\n",
    )
    .expect("write config");
    let report = report(&env, None);
    assert!(report.roots.iter().all(|r| r.state == "present"));
    assert_eq!(report.sessions.len(), 3);
}

#[cfg(unix)]
#[test]
fn the_command_prints_counts_and_never_transcript_text() {
    let env = setup();
    let scratch = tempfile::tempdir().expect("tempdir");
    let run = |json: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_basemind"));
        command
            .arg("--root")
            .arg(&env.repo)
            .arg("sessions")
            .env("HOME", &env.home)
            .env("BASEMIND_DATA_HOME", scratch.path().join("data"))
            .env("BASEMIND_COMMS_DIR", scratch.path().join("comms"))
            .env_remove("RUST_LOG");
        if json {
            command.arg("--json");
        }
        let output = command.output().expect("run basemind sessions");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
        let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
        for text in [&stdout, &stderr] {
            assert!(!text.contains("SECRET"), "transcript text leaked:\n{text}");
            assert!(!text.contains("--json has no effect"), "{text}");
        }
        stdout
    };

    let json: serde_json::Value = serde_json::from_str(&run(true)).expect("JSON report");
    assert_eq!(json["aggregate"]["sessions"], 3);
    assert_eq!(json["sessions"][2]["counts"]["edits_without_read"], 1);
    assert_eq!(json["sessions"][2]["repeated_reads"][0]["path"], "src/main.rs");
    assert_eq!(json["unsupported"].as_array().map(Vec::len), Some(2));

    let text = run(false);
    assert!(text.contains("claude transcripts: present"), "{text}");
    assert!(text.contains("src/main.rs  x4"), "{text}");
    assert!(
        text.contains("unsupported: codex rollout-d (unknown cli_version 7.1.0)"),
        "{text}"
    );
    assert!(text.contains("skipped 2 transcripts: refused"), "{text}");
}
