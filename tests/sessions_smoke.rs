//! `basemind sessions` over synthetic transcripts (issue #128, ADR-0049): the counts per host, the
//! subagents and their parents, sessions started in a subdirectory, the containment of the transcript
//! roots, and that no transcript text reaches the output.
//!
//! The fixtures under `tests/fixtures/sessions/` imitate the Claude Code and Codex record shapes
//! (`codex/main.jsonl` the 0.58-era shapes, `codex/current.jsonl` and `codex/subagent.jsonl` those
//! of the `openai/codex` sources of November 2026);
//! `{{REPO}}`, `{{WORKTREE}}` and `{{OTHER}}` are replaced with the temporary checkouts. Every
//! secret-bearing field holds a `SECRET-` marker, so any echo of transcript text fails the test.
//! The transcript roots always point into a temporary home, never the real one.

use std::path::{Path, PathBuf};
use std::process::Command;

use basemind::sessions::claude::slug;
use basemind::sessions::tally::{Counts, OutputTokens};
use basemind::sessions::{Host, Report, Scope, SessionRoots, collect};

/// The symlinks `setup` plants: the transcript, the day directory, and in the subagent tree a
/// transcript, a session directory, a `subagents` directory and a workflow-run directory.
#[cfg(unix)]
const PLANTED_SYMLINKS: u64 = 6;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sessions");

#[path = "sessions_smoke/worktrees.rs"]
mod worktrees;

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

/// Copy fixture `name` to `dest`, substituting the checkout paths, and `{{N}}` with the file stem so
/// one fixture placed twice carries distinct call ids.
fn place(env: &Env, other: &Path, name: &str, dest: &Path) {
    let text = std::fs::read_to_string(Path::new(FIXTURES).join(name)).expect("read fixture");
    let stem = dest.file_stem().expect("file stem").to_string_lossy();
    let text = fill(
        &text,
        &stem,
        &[
            ("{{REPO}}", &env.repo),
            ("{{WORKTREE}}", &env.worktree),
            ("{{OTHER}}", other),
        ],
    );
    std::fs::create_dir_all(dest.parent().expect("parent")).expect("mkdir");
    std::fs::write(dest, text).expect("write fixture");
}

/// A JSON string's contents for `text`, without the quotes.
fn json_escaped(text: &str) -> String {
    let quoted = serde_json::to_string(text).expect("encode a string");
    quoted[1..quoted.len() - 1].to_owned()
}

/// `text` with `{{N}}` replaced by `stem` and each placeholder by its path, escaped for the JSON
/// string it lands in: a Windows path's `\` is an escape, and inside a JSON-encoded string (a Codex
/// call's `arguments`, where the placeholder follows `\"`) it is escaped twice.
fn fill(text: &str, stem: &str, paths: &[(&str, &Path)]) -> String {
    let mut text = text.replace("{{N}}", stem);
    for (placeholder, path) in paths {
        let once = json_escaped(&path.to_string_lossy());
        let twice = json_escaped(&once);
        text = text
            .replace(&format!("\\\"{placeholder}"), &format!("\\\"{twice}"))
            .replace(placeholder, &once);
    }
    text
}

/// On Windows the checkouts are `C:\…` paths: every fixture line that is JSON, the JSON-encoded call
/// arguments inside one included, stays JSON once they are filled in, whatever the platform running
/// this. (Some lines are deliberately not JSON.)
#[test]
fn fixtures_filled_with_windows_paths_stay_valid_json() {
    let repo = Path::new(r"C:\Users\a\repo");
    let paths = [
        ("{{REPO}}", repo),
        ("{{WORKTREE}}", Path::new(r"C:\Users\a\wt-feature")),
        ("{{OTHER}}", Path::new(r"C:\Users\a\other")),
    ];
    let mut fixtures = Vec::new();
    for host in ["claude", "codex"] {
        for entry in std::fs::read_dir(Path::new(FIXTURES).join(host)).expect("read fixtures") {
            let path = entry.expect("entry").path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                fixtures.push(path);
            }
        }
    }
    let (mut checked, mut named) = (0, 0);
    for fixture in &fixtures {
        let text = std::fs::read_to_string(fixture).expect("read fixture");
        for raw in text.lines() {
            if serde_json::from_str::<serde_json::Value>(raw).is_err() {
                continue;
            }
            let line = fill(raw, "s", &paths);
            let record: serde_json::Value =
                serde_json::from_str(&line).unwrap_or_else(|e| panic!("{}: {e}\n{line}", fixture.display()));
            checked += 1;
            let arguments = record["payload"]["arguments"].as_str();
            if let Some(arguments) = arguments.filter(|a| a.contains("workdir")) {
                let arguments: serde_json::Value = serde_json::from_str(arguments)
                    .unwrap_or_else(|e| panic!("{} arguments: {e}\n{arguments}", fixture.display()));
                assert_eq!(arguments["workdir"], repo.to_string_lossy().as_ref());
                named += 1;
            }
        }
    }
    assert!(
        checked >= 60,
        "checked only {checked} JSON lines in {} fixtures",
        fixtures.len()
    );
    assert!(
        named >= 1,
        "no fixture placed a checkout inside JSON-encoded call arguments"
    );
}

/// A repository with one linked worktree, another repository, and a home holding transcripts of
/// all three plus the escape attempts.
fn setup() -> Env {
    let dir = tempfile::tempdir().expect("tempdir");
    // `std` canonicalizes a Windows path to its verbatim `\\?\` form, under which git cannot create
    // the worktree; `dunce` leaves the prefix off.
    let base = dunce::canonicalize(dir.path()).expect("canonical tempdir");
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
    // Another repository beside the checkouts: the directory holding the worktree holds more than
    // this repository, so a gone directory there (`repo-sibling`) is no removed worktree.
    git(&other, &["init", "-q"]);
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
    // A session started in a subdirectory of the checkout has a project directory of its own.
    place(
        &env,
        &other,
        "claude/subdir.jsonl",
        &claude.join(slug(&env.repo.join("crates/core"))).join("s7.jsonl"),
    );
    // A sibling whose slug extends this checkout's is opened, then dropped by its cwd.
    let sibling = base.join("repo-sibling");
    place(
        &env,
        &sibling,
        "claude/other-repo.jsonl",
        &claude.join(slug(&sibling)).join("s10.jsonl"),
    );
    // Subagents of s1, directly and in a workflow run; a journal beside them is not a transcript.
    place(
        &env,
        &other,
        "claude/subagent.jsonl",
        &repo_dir.join("s1/subagents/agent-a1.jsonl"),
    );
    place(
        &env,
        &other,
        "claude/subagent.jsonl",
        &repo_dir.join("s1/subagents/workflows/wf_1/agent-a2.jsonl"),
    );
    place(
        &env,
        &other,
        "claude/main.jsonl",
        &repo_dir.join("s1/subagents/workflows/wf_1/journal.jsonl"),
    );

    let codex = env.home.join(".codex/sessions/2026/11/18");
    place(&env, &other, "codex/main.jsonl", &codex.join("rollout-a.jsonl"));
    place(&env, &other, "codex/other-repo.jsonl", &codex.join("rollout-b.jsonl"));
    place(&env, &other, "codex/pre-envelope.jsonl", &codex.join("rollout-c.jsonl"));
    place(&env, &other, "codex/unsupported.jsonl", &codex.join("rollout-d.jsonl"));
    place(&env, &other, "codex/current.jsonl", &codex.join("rollout-f.jsonl"));
    place(&env, &other, "codex/subagent.jsonl", &codex.join("rollout-g.jsonl"));
    // A cwd that names the checkout lexically but resolves elsewhere.
    place(
        &env,
        &env.repo.join("..").join("other"),
        "codex/other-repo.jsonl",
        &codex.join("rollout-h.jsonl"),
    );

    // Escapes: a transcript symlinked to a file outside the root, and a day directory symlinked out.
    place(&env, &other, "claude/main.jsonl", &outside.join("escaped.jsonl"));
    place(&env, &other, "codex/main.jsonl", &outside.join("day/rollout-e.jsonl"));
    // And in the subagent tree: a session directory, a `subagents` directory and a workflow run
    // symlinked out, and a symlinked subagent transcript.
    // Two transcripts there, so a symlinked directory that was followed would show as two refused
    // files rather than one refused directory.
    for name in ["agent-x.jsonl", "agent-y.jsonl"] {
        place(
            &env,
            &other,
            "claude/subagent.jsonl",
            &outside.join("session/subagents").join(name),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(outside.join("escaped.jsonl"), repo_dir.join("s6.jsonl")).expect("symlink");
        symlink(outside.join("day"), env.home.join(".codex/sessions/2026/11/19")).expect("symlink");
        symlink(
            outside.join("escaped.jsonl"),
            repo_dir.join("s1/subagents/agent-link.jsonl"),
        )
        .expect("symlink");
        symlink(outside.join("session"), repo_dir.join("s9")).expect("symlink");
        std::fs::create_dir_all(repo_dir.join("s8")).expect("mkdir");
        symlink(outside.join("session/subagents"), repo_dir.join("s8/subagents")).expect("symlink");
        symlink(
            outside.join("session/subagents"),
            repo_dir.join("s1/subagents/workflows/wf_2"),
        )
        .expect("symlink");
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
            (Host::Codex, "rollout-f", "main"),
            (Host::Claude, "s1", "main"),
            (Host::Claude, "s7", "main"),
        ]
    );

    let claude = &report.sessions[3];
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

    assert_eq!(
        report.sessions[4].counts.search_tool, 1,
        "the session started in crates/core"
    );

    assert_eq!(report.aggregate.sessions, 5);
    assert_eq!(report.aggregate.subagents, 3);
    assert_eq!(report.aggregate.counts.basemind, 7, "sessions and subagents");
}

#[test]
fn the_current_codex_shapes_are_counted() {
    let env = setup();
    let report = report(&env, None);
    let current = report
        .sessions
        .iter()
        .find(|s| s.id == "rollout-f")
        .expect("the current-format rollout, recorded in crates/core");
    assert_eq!(current.format_version.as_deref(), Some("0.169.0"));
    assert_eq!(
        current.counts,
        Counts {
            basemind: 2,
            search_tool: 0,
            shell_search: 2,
            shell_git: 0,
            read_whole: 1,
            read_range: 0,
            edit: 1,
            write: 0,
            other: 2,
            errors: 2,
            basemind_errors: 0,
            edits_without_read: 0,
            writes_without_search: 0,
            files_read_repeatedly: 0,
            outside_paths: 0,
            output_tokens: OutputTokens {
                read_search: 40,
                edit: 25,
                other: 0
            },
        },
        "the code-mode script's exec_command and basemind calls, the namespaced MCP call, \
         exec_command, apply_patch, the collaboration tool, the tool search and the built-in in \
         the default namespace; the script and exec_command failures from their headers"
    );
}

#[test]
fn subagents_are_reported_and_attributed_to_their_session() {
    let env = setup();
    let report = report(&env, None);
    let subagents: Vec<(Host, &str, &str, bool)> = report
        .subagents
        .iter()
        .map(|s| (s.session.host, s.session.id.as_str(), s.parent.as_str(), s.attributed))
        .collect();
    assert_eq!(
        subagents,
        vec![
            (Host::Codex, "rollout-g", "rollout-f", true),
            (Host::Claude, "agent-a1", "s1", true),
            (Host::Claude, "agent-a2", "s1", true),
        ],
        "the journal beside agent-a2 is not a transcript"
    );

    let claude = &report.subagents[1].session;
    assert_eq!(
        (claude.counts.basemind, claude.counts.read_whole, claude.counts.edit),
        (1, 1, 1)
    );
    assert_eq!(claude.counts.edits_without_read, 0, "the subagent's own read counts");

    // The forked Codex subagent counts only what it did after its start ordinal.
    let codex = &report.subagents[0].session;
    assert_eq!((codex.counts.shell_search, codex.counts.edit), (1, 0));
    assert_eq!(codex.counts.output_tokens.read_search, 7);
    assert_eq!(codex.started.as_deref(), Some("2026-11-18T09:40:00.000Z"));

    let s1 = report.sessions.iter().find(|s| s.id == "s1").expect("s1");
    let totals = s1.subagents.as_ref().expect("s1 has subagents");
    assert_eq!(totals.transcripts, 2);
    assert_eq!(totals.counts.basemind, 2);
    assert_eq!(
        s1.counts.basemind, 1,
        "the session's own counts leave its subagents out"
    );
    let f = report.sessions.iter().find(|s| s.id == "rollout-f").expect("rollout-f");
    assert_eq!(f.subagents.as_ref().map(|t| t.transcripts), Some(1));
    assert!(
        report
            .sessions
            .iter()
            .find(|s| s.id == "s7")
            .expect("s7")
            .subagents
            .is_none()
    );
}

fn session<'a>(report: &'a Report, id: &str) -> &'a basemind::sessions::SessionReport {
    report
        .sessions
        .iter()
        .find(|s| s.id == id)
        .unwrap_or_else(|| panic!("session {id}"))
}

#[test]
fn relative_paths_resolve_against_the_sessions_working_directory() {
    let env = setup();
    let day = env.home.join(".codex/sessions/2026/11/20");
    place(&env, &env.repo, "codex/subdir.jsonl", &day.join("rollout-s.jsonl"));
    let report = report(&env, None);
    let s = session(&report, "rollout-s");
    let repeated: Vec<(&str, u32)> = s.repeated_reads.iter().map(|r| (r.path.as_str(), r.reads)).collect();
    assert_eq!(
        repeated,
        vec![("crates/core/src/lib.rs", 4)],
        "`cat src/lib.rs` in crates/core is crates/core/src/lib.rs; basemind's `src/lib.rs` is the root one"
    );
    assert_eq!(
        (s.counts.edits_without_read, s.counts.outside_paths),
        (0, 1),
        "`../../src/main.rs` is the src/main.rs basemind read; only the path above the checkout is outside"
    );
}

#[test]
fn a_compressed_codex_rollout_is_read() {
    let env = setup();
    let day = env.home.join(".codex/sessions/2026/11/20");
    let plain = day.join("rollout-z.jsonl");
    place(&env, &env.repo, "codex/subdir.jsonl", &plain);
    let text = std::fs::read(&plain).expect("read placed fixture");
    std::fs::remove_file(&plain).expect("remove plain rollout");
    // A corrupt one beside it is counted as unreadable, never dropped.
    std::fs::write(day.join("rollout-y.jsonl.zst"), b"not zstd").expect("write corrupt rollout");
    let compressed = zstd::encode_all(text.as_slice(), 3).expect("compress");
    std::fs::write(day.join("rollout-z.jsonl.zst"), compressed).expect("write compressed rollout");
    let report = report(&env, None);
    let z = session(&report, "rollout-z");
    assert_eq!(z.repeated_reads.len(), 1);
    assert_eq!(z.counts.shell_search + z.counts.read_whole, 6);
    assert_eq!(report.unreadable, 1, "the corrupt rollout");
}

#[test]
fn a_field_of_another_type_drops_the_field_not_the_record() {
    let env = setup();
    let day = env.home.join(".codex/sessions/2026/11/20");
    place(&env, &env.repo, "codex/loose.jsonl", &day.join("rollout-loose.jsonl"));
    let report = report(&env, None);
    let loose = session(&report, "rollout-loose");
    assert_eq!(
        (loose.counts.shell_search, loose.counts.basemind, loose.counts.errors),
        (1, 1, 1),
        "the call with a numeric namespace, and its failure read from the header item"
    );
    assert_eq!(report.skipped_lines, 1, "the line that is not JSON");
}

#[test]
fn a_fork_counts_only_what_it_did_after_the_fork() {
    let env = setup();
    let day = env.home.join(".codex/sessions/2026/11/20");
    place(&env, &env.repo, "codex/fork.jsonl", &day.join("rollout-fork.jsonl"));
    let report = report(&env, None);
    assert_eq!(
        session(&report, "rollout-fork").counts,
        Counts {
            shell_search: 1,
            output_tokens: OutputTokens {
                read_search: 5,
                ..OutputTokens::default()
            },
            ..Counts::default()
        },
        "the calls, outputs and token count copied from rollout-f are rollout-f's; the repeated token \
         count adds nothing"
    );
    let parent = session(&report, "rollout-f");
    assert_eq!(
        (parent.counts.basemind, parent.counts.read_whole, parent.counts.errors),
        (2, 1, 2)
    );
}

#[test]
fn a_reply_without_a_tool_call_after_a_fork_point_counts_once() {
    let env = setup();
    let day = env.home.join(".codex/sessions/2026/11/20");
    place(
        &env,
        &env.repo,
        "codex/fork-reply.jsonl",
        &day.join("rollout-fork-reply.jsonl"),
    );
    let report = report(&env, None);
    assert_eq!(
        session(&report, "rollout-fork-reply").counts,
        Counts {
            output_tokens: OutputTokens {
                other: 9,
                ..OutputTokens::default()
            },
            ..Counts::default()
        },
        "the copied calls and token count are rollout-f's; the text reply after the fork point is the \
         fork's, though the record before it was a copied call"
    );
    assert_eq!(
        session(&report, "rollout-f").counts.output_tokens,
        OutputTokens {
            read_search: 40,
            edit: 25,
            other: 0
        },
        "the parent's counts stay its own"
    );
}

#[test]
fn a_reverted_thread_is_one_session() {
    let env = setup();
    let day = env.home.join(".codex/sessions/2026/11/20");
    place(&env, &env.repo, "codex/revert.jsonl", &day.join("rollout-r.jsonl"));
    let report = report(&env, None);
    let ids: Vec<&str> = report.sessions.iter().map(|s| s.id.as_str()).collect();
    assert!(!ids.contains(&"rollout-r"), "{ids:?}");
    assert_eq!(report.aggregate.sessions, 5);
    let thread = session(&report, "rollout-f");
    assert_eq!(
        (thread.counts.basemind, thread.counts.shell_search),
        (2, 3),
        "the copied basemind call once, the new search added"
    );
    assert_eq!(thread.ended.as_deref(), Some("2026-11-18T10:10:01.000Z"));
    assert_eq!(thread.subagents.as_ref().map(|t| t.transcripts), Some(1));
}

#[test]
fn a_legacy_subagent_skips_its_inherited_history_by_position() {
    let env = setup();
    let day = env.home.join(".codex/sessions/2026/11/18");
    place(
        &env,
        &env.repo,
        "codex/subagent-legacy.jsonl",
        &day.join("rollout-l.jsonl"),
    );
    let report = report(&env, None);
    let legacy = report
        .subagents
        .iter()
        .find(|s| s.session.id == "rollout-l")
        .expect("the Legacy-mode subagent");
    let counts = &legacy.session.counts;
    assert_eq!((counts.shell_search, counts.output_tokens.read_search), (1, 7));
    assert_eq!(legacy.session.started.as_deref(), Some("2026-11-18T09:40:00.000Z"));
}

/// Set `path`'s modification time `secs` after the epoch.
fn touch(path: &Path, secs: u64) {
    let file = std::fs::File::options().write(true).open(path).expect("open");
    file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
        .expect("set mtime");
}

#[test]
fn a_resumed_claude_session_counts_only_its_new_calls() {
    let env = setup();
    let project = env.home.join(".claude/projects").join(slug(&env.repo));
    // Named to sort before s1, so only the write order tells the original apart.
    let resumed = project.join("s0.jsonl");
    place(&env, &env.repo, "claude/resumed.jsonl", &resumed);
    touch(&project.join("s1.jsonl"), 1_000);
    touch(&resumed, 2_000);
    let report = report(&env, None);
    let s0 = session(&report, "s0");
    assert_eq!(
        (
            s0.counts.basemind,
            s0.counts.search_tool,
            s0.counts.errors,
            s0.counts.output_tokens.read_search
        ),
        (0, 1, 0, 6),
        "the message, call and result copied from s1 are s1's"
    );
    assert_eq!(
        s0.counts.output_tokens.other, 3,
        "the reply right after the copied records called no tool"
    );
    let s1 = session(&report, "s1");
    assert_eq!((s1.counts.basemind, s1.counts.errors), (1, 2));
}

#[test]
fn a_subagent_is_attributed_through_its_direct_parent() {
    let env = setup();
    let day = env.home.join(".codex/sessions/2026/11/20");
    place(&env, &env.repo, "codex/fork.jsonl", &day.join("rollout-fork.jsonl"));
    place(
        &env,
        &env.repo,
        "codex/fork-subagent.jsonl",
        &day.join("rollout-fsub.jsonl"),
    );
    place(
        &env,
        &env.repo,
        "codex/nested-subagent.jsonl",
        &day.join("rollout-nested.jsonl"),
    );
    let report = report(&env, None);
    let parents: Vec<(&str, &str, bool)> = report
        .subagents
        .iter()
        .filter(|s| s.session.host == Host::Codex)
        .map(|s| (s.session.id.as_str(), s.parent.as_str(), s.attributed))
        .collect();
    assert_eq!(
        parents,
        vec![
            ("rollout-g", "rollout-f", true),
            ("rollout-nested", "rollout-f", true),
            ("rollout-fsub", "rollout-fork", true),
        ],
        "the fork's subagent names the root session too, but was spawned by the fork; the nested \
         subagent's parent is the subagent rollout-g"
    );
    let transcripts = |id: &str| session(&report, id).subagents.as_ref().map(|t| t.transcripts);
    assert_eq!(
        (transcripts("rollout-f"), transcripts("rollout-fork")),
        (Some(2), Some(1))
    );
}

#[test]
fn a_sidechain_transcript_beside_the_sessions_is_a_subagent() {
    let env = setup();
    let project = env.home.join(".claude/projects").join(slug(&env.repo));
    place(
        &env,
        &env.repo,
        "claude/subagent.jsonl",
        &project.join("agent-a3.jsonl"),
    );
    let report = report(&env, None);
    let ids: Vec<&str> = report.sessions.iter().map(|s| s.id.as_str()).collect();
    assert!(!ids.contains(&"agent-a3"), "{ids:?}");
    let a3 = report
        .subagents
        .iter()
        .find(|s| s.session.id == "agent-a3")
        .expect("the Claude Code 2.0 sidechain transcript");
    assert_eq!((a3.parent.as_str(), a3.attributed), ("s1", true));
    assert_eq!(
        session(&report, "s1").subagents.as_ref().map(|t| t.transcripts),
        Some(3)
    );
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
    // The Claude transcript of another directory inside this repository's project directory, the
    // sibling `repo-sibling` whose project directory shares the slug prefix, the Codex rollout of
    // another directory, and the one whose cwd climbs out with `..`. The other repository's own
    // Claude project directory is not walked, so it adds nothing.
    assert_eq!(report.other_directories, 4);
    #[cfg(unix)]
    {
        assert_eq!(report.refused, PLANTED_SYMLINKS, "every planted symlink");
        assert_eq!(
            (report.sessions.len(), report.subagents.len()),
            (5, 3),
            "no escape became a session or a subagent"
        );
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
    assert_eq!(ids, vec!["s1", "s7"], "the worktree session ended on 2026-11-17");
    assert_eq!(report.subagents.len(), 2, "the Claude subagents of 2026-11-18");
}

/// ADR-0049 rule 5: the repository's `basemind.toml` names other transcript roots, and the command
/// still reads only the ones under the home directory.
#[test]
fn a_repository_configuration_cannot_move_the_roots() {
    let env = setup();
    let decoy = env.repo.join("decoy");
    let claude = decoy.join("claude");
    let codex = decoy.join("codex");
    place(
        &env,
        &env.repo,
        "claude/main.jsonl",
        &claude.join(slug(&env.repo)).join("decoy-c.jsonl"),
    );
    place(
        &env,
        &env.repo,
        "codex/subdir.jsonl",
        &codex.join("2026/11/20/decoy-x.jsonl"),
    );
    let config = format!(
        "[sessions]\nclaude_projects = '{}'\ncodex_sessions = '{}'\n",
        claude.display(),
        codex.display()
    );
    std::fs::write(env.repo.join("basemind.toml"), config).expect("write config");
    let json: serde_json::Value = serde_json::from_str(&run_sessions(&env, true)).expect("JSON report");
    let states: Vec<&str> = json["roots"]
        .as_array()
        .expect("roots")
        .iter()
        .filter_map(|r| r["state"].as_str())
        .collect();
    assert_eq!(states, vec!["present", "present"]);
    let ids: Vec<&str> = json["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(!ids.iter().any(|id| id.starts_with("decoy")), "{ids:?}");
    assert_eq!(json["aggregate"]["sessions"], 5);
}

/// Run `basemind --root <repo> sessions [--json]` with the temporary home and fresh data and comms
/// directories, assert that no transcript text or control character reached stdout or stderr, and
/// return stdout.
fn run_sessions(env: &Env, json: bool) -> String {
    let scratch = tempfile::tempdir().expect("tempdir");
    let mut command = Command::new(env!("CARGO_BIN_EXE_basemind"));
    command
        .arg("--root")
        .arg(&env.repo)
        .arg("sessions")
        .env("HOME", &env.home)
        .env("USERPROFILE", &env.home)
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
        assert!(
            !text.chars().any(|c| c.is_control() && c != '\n'),
            "a control character reached the output:\n{text:?}"
        );
        assert!(!text.contains("--json has no effect"), "{text}");
    }
    stdout
}

#[test]
fn transcript_strings_are_printed_only_as_plain_tokens() {
    let env = setup();
    let project = env.home.join(".claude/projects").join(slug(&env.repo));
    let day = env.home.join(".codex/sessions/2026/11/20");
    place(
        &env,
        &env.repo,
        "claude/hostile-version.jsonl",
        &project.join("s12.jsonl"),
    );
    place(
        &env,
        &env.repo,
        "codex/hostile-version.jsonl",
        &day.join("rollout-hv.jsonl"),
    );
    place(
        &env,
        &env.repo,
        "codex/hostile-parent.jsonl",
        &day.join("rollout-hp.jsonl"),
    );
    let json: serde_json::Value = serde_json::from_str(&run_sessions(&env, true)).expect("JSON report");
    let s12 = json["sessions"]
        .as_array()
        .and_then(|list| list.iter().find(|s| s["id"] == "s12"))
        .expect("s12, a supported major");
    assert_eq!(s12["format_version"], serde_json::Value::Null);
    let hv = json["unsupported"]
        .as_array()
        .and_then(|list| list.iter().find(|u| u["id"] == "rollout-hv"))
        .expect("rollout-hv");
    assert_eq!(hv["reason"], "unparseable cli_version");
    let hp = json["subagents"]
        .as_array()
        .and_then(|list| list.iter().find(|s| s["id"] == "rollout-hp"))
        .expect("rollout-hp");
    assert_eq!(
        (&hp["parent"], &hp["attributed"]),
        (&"unparseable".into(), &false.into())
    );
    let text = run_sessions(&env, false);
    assert!(text.contains("rollout-hv (unparseable cli_version)"), "{text}");
}

#[test]
fn the_command_prints_counts_and_never_transcript_text() {
    let env = setup();
    let run = |json: bool| run_sessions(&env, json);

    let json: serde_json::Value = serde_json::from_str(&run(true)).expect("JSON report");
    assert_eq!(json["aggregate"]["sessions"], 5);
    assert_eq!(json["aggregate"]["subagents"], 3);
    assert_eq!(json["sessions"][3]["counts"]["edits_without_read"], 1);
    assert_eq!(json["sessions"][3]["repeated_reads"][0]["path"], "src/main.rs");
    assert_eq!(json["sessions"][3]["subagents"]["transcripts"], 2);
    assert_eq!(json["subagents"][1]["parent"], "s1");
    assert_eq!(json["subagents"][1]["id"], "agent-a1");
    assert_eq!(json["unsupported"].as_array().map(Vec::len), Some(2));

    let text = run(false);
    assert!(text.contains("claude transcripts: present"), "{text}");
    assert!(text.contains("src/main.rs  x4"), "{text}");
    assert!(
        text.contains("unsupported: codex rollout-d (unknown cli_version 7.1.0)"),
        "{text}"
    );
    assert!(text.contains("subagents:"), "{text}");
    assert!(text.contains("s1/agent-a1"), "{text}");
    #[cfg(unix)]
    assert!(
        text.contains(&format!("skipped {PLANTED_SYMLINKS} transcripts: refused")),
        "{text}"
    );
}
