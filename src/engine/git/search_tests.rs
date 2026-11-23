//! Typo-tolerant search over the git-history index, on a real tempdir repository: a misspelled
//! word, the start of a word, identifier parts, a mistyped author, and a query combined with a
//! path and a time range.

use std::fs;
use std::path::Path;

use basemind::git::{ChangeKind, CommitInfo, Repo};
use basemind::git_history::GitHistoryIndex;
use basemind::git_history::builder::{self, RebuildOutcome};
use basemind::git_history::fts::FtsScope;
use basemind::git_history::fuzzy::{CommitQuery, SearchHit};
use basemind::path::RelPath;

use super::{touches, without_empty_components};

fn git(repo: &Path, args: &[&str], author: (&str, &str), date: &str) {
    let status = basemind::testing::git_command()
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", author.0)
        .env("GIT_AUTHOR_EMAIL", author.1)
        .env("GIT_COMMITTER_NAME", author.0)
        .env("GIT_COMMITTER_EMAIL", author.1)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed");
}

/// Commit `path` by `author` at noon of `day`, with message `subject` and an optional `body`.
fn commit(root: &Path, path: &str, author: (&str, &str), day: &str, subject: &str, body: &str) -> String {
    let file = root.join(path);
    fs::create_dir_all(file.parent().expect("parent")).unwrap();
    fs::write(&file, format!("// {subject}\n")).unwrap();
    let date = format!("{day}T12:00:00+00:00");
    git(root, &["add", path], author, &date);
    let message = if body.is_empty() {
        subject.to_string()
    } else {
        format!("{subject}\n\n{body}")
    };
    git(root, &["commit", "-q", "-m", &message], author, &date);
    let head = basemind::testing::git_command()
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .expect("git in PATH");
    String::from_utf8(head.stdout).expect("utf8").trim().to_string()
}

const ADA: (&str, &str) = ("Ada Lovelace", "ada@calc.example");
const BJORN: (&str, &str) = ("Bjorn Iron", "bjorn@forge.example");
const NAAMAN: (&str, &str) = ("Naaman Hirschfeld", "n@home.example");

struct Fixture {
    root: tempfile::TempDir,
    collector: String,
    parser: String,
    loader: String,
    notes: String,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");
    git(
        root.path(),
        &["init", "-q", "-b", "main"],
        ADA,
        "2025-01-01T00:00:00+00:00",
    );
    git(
        root.path(),
        &["config", "commit.gpgsign", "false"],
        ADA,
        "2025-01-01T00:00:00+00:00",
    );
    let at = root.path();
    let collector = commit(
        at,
        "src/collector/bulk.rs",
        ADA,
        "2025-01-10",
        "feat: bulk collector config API",
        "",
    );
    let parser = commit(
        at,
        "src/parser/null.rs",
        BJORN,
        "2025-02-10",
        "fix: null deref in parser",
        "",
    );
    let loader = commit(
        at,
        "src/collector/snmp.rs",
        ADA,
        "2025-03-10",
        "refactor: BulkConfig loader",
        "Tracked in AMVP-165576.",
    );
    let notes = commit(
        at,
        "docs/readme.md",
        NAAMAN,
        "2025-04-10",
        "docs: bulk_config notes",
        "",
    );
    Fixture {
        root,
        collector,
        parser,
        loader,
        notes,
    }
}

fn sync(root: &Path) -> (GitHistoryIndex, RebuildOutcome) {
    let repo = Repo::discover(root).expect("repo discover");
    let dir = root.join(".basemind");
    fs::create_dir_all(&dir).unwrap();
    let index = GitHistoryIndex::open(&dir).expect("open git-history index");
    let outcome = builder::sync(&index, &repo, &dir).expect("sync");
    (index, outcome)
}

fn query(pattern: &str, scope: FtsScope) -> CommitQuery<'_> {
    CommitQuery {
        pattern,
        scope,
        author: None,
        path: None,
        since: None,
        until: None,
    }
}

/// The shas of `hits`, each with whether it matched only through a tolerance.
fn shas(hits: &[SearchHit]) -> Vec<(&str, bool)> {
    hits.iter().map(|hit| (hit.commit.sha.as_str(), hit.fuzzy)).collect()
}

fn search(index: &GitHistoryIndex, query: &CommitQuery<'_>) -> Vec<SearchHit> {
    index.search_commits_fuzzy(query, 0, 100)
}

#[test]
fn a_misspelled_query_finds_the_commit_and_says_it_was_loose() {
    let fixture = fixture();
    let (index, _) = sync(fixture.root.path());
    let hits = search(&index, &query("collectr bulk confg", FtsScope::Message));
    assert_eq!(shas(&hits), vec![(fixture.collector.as_str(), true)]);
    assert!(hits[0].commit.body.is_empty());
}

#[test]
fn a_query_typed_right_is_not_flagged_and_ranks_before_the_loose_matches() {
    let fixture = fixture();
    let near = commit(
        fixture.root.path(),
        "src/parser/cache.rs",
        BJORN,
        "2025-05-10",
        "perf: parsr cache",
        "",
    );
    let (index, _) = sync(fixture.root.path());
    let hits = search(&index, &query("parser", FtsScope::Message));
    assert_eq!(
        shas(&hits),
        vec![(fixture.parser.as_str(), false), (near.as_str(), true)],
        "the exact match is older and still first"
    );
}

#[test]
fn the_last_word_matches_by_prefix_and_an_earlier_one_does_not() {
    let fixture = fixture();
    let (index, _) = sync(fixture.root.path());
    let hits = search(&index, &query("bulk collec", FtsScope::Message));
    assert_eq!(shas(&hits), vec![(fixture.collector.as_str(), true)]);
    assert!(search(&index, &query("collec bulk", FtsScope::Message)).is_empty());
}

#[test]
fn the_last_word_keeps_its_prefix_match_when_an_earlier_word_repeats_it() {
    let fixture = fixture();
    let (index, _) = sync(fixture.root.path());
    let hits = search(&index, &query("collec bulk collec", FtsScope::Message));
    assert_eq!(shas(&hits), vec![(fixture.collector.as_str(), true)]);
}

#[test]
fn a_nearby_word_does_not_hide_the_commits_holding_the_run_together_spelling() {
    let root = tempfile::tempdir().expect("tempdir");
    let at = root.path();
    git(at, &["init", "-q", "-b", "main"], ADA, "2025-01-01T00:00:00+00:00");
    git(
        at,
        &["config", "commit.gpgsign", "false"],
        ADA,
        "2025-01-01T00:00:00+00:00",
    );
    let apart = commit(at, "a.rs", ADA, "2025-01-10", "feat: collector bulk", "");
    let near = commit(at, "b.rs", ADA, "2025-02-10", "chore: collectorbulks", "");
    let (index, _) = sync(at);
    let mut found: Vec<String> = search(&index, &query("collectorbulk", FtsScope::Message))
        .into_iter()
        .map(|hit| hit.commit.sha)
        .collect();
    found.sort();
    let mut expected = vec![apart, near];
    expected.sort();
    assert_eq!(found, expected);
}

#[test]
fn identifiers_match_by_their_parts_the_whole_and_a_run_together_spelling() {
    let fixture = fixture();
    let (index, _) = sync(fixture.root.path());
    let by_parts = search(&index, &query("BulkConfig", FtsScope::Message));
    assert_eq!(
        shas(&by_parts),
        vec![
            (fixture.notes.as_str(), false),
            (fixture.loader.as_str(), false),
            (fixture.collector.as_str(), false)
        ],
        "`BulkConfig` and `bulk_config` and `bulk ... config` all hold both parts"
    );
    let whole = search(&index, &query("bulkconfig", FtsScope::Message));
    assert_eq!(shas(&whole), vec![(fixture.loader.as_str(), false)]);
    let joined = search(&index, &query("collectorbulk", FtsScope::Message));
    assert_eq!(shas(&joined), vec![(fixture.collector.as_str(), true)]);
    let ticket = search(&index, &query("amvp165576", FtsScope::Message));
    assert_eq!(shas(&ticket), vec![(fixture.loader.as_str(), false)]);
}

#[test]
fn an_author_is_found_by_a_typo_and_by_a_subsequence() {
    let fixture = fixture();
    let (index, _) = sync(fixture.root.path());
    let typo = search(&index, &query("hirschfld", FtsScope::Author));
    assert_eq!(shas(&typo), vec![(fixture.notes.as_str(), true)]);
    let gaps = search(&index, &query("nmn", FtsScope::Author));
    assert_eq!(shas(&gaps), vec![(fixture.notes.as_str(), true)]);
    let exact = search(&index, &query("lovelace", FtsScope::Author));
    assert_eq!(
        shas(&exact),
        vec![(fixture.loader.as_str(), false), (fixture.collector.as_str(), false)]
    );
    let elsewhere = search(&index, &query("hirschfeld", FtsScope::Message));
    assert!(elsewhere.is_empty(), "a message-scoped query never reads the author");
}

#[test]
fn an_author_narrows_a_message_query_on_its_own_field() {
    let fixture = fixture();
    let (index, _) = sync(fixture.root.path());
    let by_ada = CommitQuery {
        author: Some("ada"),
        ..query("bulk", FtsScope::Message)
    };
    assert_eq!(
        shas(&search(&index, &by_ada)),
        vec![(fixture.loader.as_str(), false), (fixture.collector.as_str(), false)]
    );
    let mistyped = CommitQuery {
        author: Some("lovelase"),
        ..query("bulk", FtsScope::Message)
    };
    assert_eq!(
        shas(&search(&index, &mistyped)),
        vec![(fixture.loader.as_str(), true), (fixture.collector.as_str(), true)]
    );
}

/// 2025-02-01T00:00:00Z.
const FEBRUARY_FIRST: i64 = 1_738_368_000;
/// 2025-03-31T23:59:59Z.
const END_OF_MARCH: i64 = 1_743_465_599;

#[test]
fn a_query_combines_with_a_path_and_a_time_range_in_one_search() {
    let fixture = fixture();
    let (index, _) = sync(fixture.root.path());
    let directory = RelPath::from("src/collector/");
    let in_range = CommitQuery {
        path: Some(&directory),
        since: Some(FEBRUARY_FIRST),
        until: Some(END_OF_MARCH),
        ..query("bulk", FtsScope::Message)
    };
    assert_eq!(shas(&search(&index, &in_range)), vec![(fixture.loader.as_str(), false)]);

    let whole_directory = CommitQuery {
        since: None,
        until: None,
        ..in_range
    };
    assert_eq!(
        shas(&search(&index, &whole_directory)),
        vec![(fixture.loader.as_str(), false), (fixture.collector.as_str(), false)]
    );

    let file = RelPath::from("src/collector/bulk.rs");
    let one_file = CommitQuery {
        path: Some(&file),
        ..whole_directory
    };
    assert_eq!(
        shas(&search(&index, &one_file)),
        vec![(fixture.collector.as_str(), false)]
    );

    let elsewhere = RelPath::from("docs/");
    let outside = CommitQuery {
        path: Some(&elsewhere),
        ..query("collectr", FtsScope::Message)
    };
    assert!(
        search(&index, &outside).is_empty(),
        "the typo's commit touched no file under docs/"
    );

    let unknown = RelPath::from("src/missing.rs");
    let nothing = CommitQuery {
        path: Some(&unknown),
        ..query("bulk", FtsScope::Message)
    };
    assert!(search(&index, &nothing).is_empty());
}

#[test]
fn a_page_continues_across_the_exact_and_the_loose_matches() {
    let fixture = fixture();
    let near = commit(
        fixture.root.path(),
        "src/parser/cache.rs",
        BJORN,
        "2025-05-10",
        "perf: parsr cache",
        "",
    );
    let (index, _) = sync(fixture.root.path());
    let pages: Vec<Vec<(String, bool)>> = (0..3)
        .map(|skip| {
            index
                .search_commits_fuzzy(&query("parser", FtsScope::Message), skip, 1)
                .into_iter()
                .map(|hit| (hit.commit.sha, hit.fuzzy))
                .collect()
        })
        .collect();
    assert_eq!(
        pages,
        vec![
            vec![(fixture.parser.clone(), false)],
            vec![(near.clone(), true)],
            Vec::new()
        ]
    );
}

#[test]
fn an_appended_commit_is_found_by_a_typo_without_a_rebuild() {
    let fixture = fixture();
    let (index, _) = sync(fixture.root.path());
    drop(index);
    let added = commit(
        fixture.root.path(),
        "src/net/socket.rs",
        BJORN,
        "2025-06-10",
        "feat: reconnect the websocket",
        "",
    );
    let (index, outcome) = sync(fixture.root.path());
    assert_eq!(outcome, RebuildOutcome::Incremental { added: 1 });
    let hits = search(&index, &query("websockt reconect", FtsScope::Message));
    assert_eq!(shas(&hits), vec![(added.as_str(), true)]);
}

#[test]
fn the_paths_of_the_history_are_listed_for_a_nearest_path_hint() {
    let fixture = fixture();
    let (index, _) = sync(fixture.root.path());
    let mut paths: Vec<String> = index
        .history_paths()
        .iter()
        .map(|path| path.to_str_lossy().into_owned())
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "docs/readme.md",
            "src/collector/bulk.rs",
            "src/collector/snmp.rs",
            "src/parser/null.rs"
        ]
    );
}

#[test]
fn a_search_path_drops_empty_components_and_keeps_the_directory_marker() {
    let cases = [
        ("src/config/loader.rs", "src/config/loader.rs"),
        ("src//config/loader.rs", "src/config/loader.rs"),
        ("/src/config/", "src/config/"),
        ("src//config//", "src/config/"),
        ("src//config", "src/config"),
    ];
    for (given, expected) in cases {
        assert_eq!(
            without_empty_components(&crate::path::RelPath::from(given)),
            crate::path::RelPath::from(expected),
            "{given}"
        );
    }
    let changed = crate::git::CommitInfo {
        sha: "0".repeat(40),
        short_sha: "0".repeat(7),
        summary: String::new(),
        author: String::new(),
        author_email: String::new(),
        author_time_unix: 0,
        body: String::new(),
        files: vec![(
            crate::path::RelPath::from("src/config/loader.rs"),
            crate::git::ChangeKind::Modified,
        )],
    };
    assert!(touches(
        &changed,
        &without_empty_components(&crate::path::RelPath::from("src//config/"))
    ));
}
