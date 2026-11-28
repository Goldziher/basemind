//! Symbols and calls in a group index (ADR-0028 rule 7): every view of a repository group shares
//! the rows of a content, and each view's references, call sites and symbol lookups equal those of
//! a standalone index of the same tree, over every page, with each page sorted by path.

use std::fs;
use std::path::{Path, PathBuf};

use super::{budget_call_page, calls_in_content_fjall, scan_calls_by_name};
use crate::config::ConfigV1;
use crate::engine::refs::matching::Matching;
use crate::index::view::IndexView;
use crate::path::RelPath;
use crate::scanner::{EmbedMode, ScanSource, scan};
use crate::store::{Store, VIEW_WORKING};

const CALLEES: [(&str, Matching); 5] = [
    ("render_frame", Matching::Exact),
    ("spawn_task", Matching::Exact),
    ("helper", Matching::Substring),
    ("only_linked", Matching::Substring),
    ("absent_callee", Matching::Exact),
];
/// Exact names: a symbol lookup matches the whole name.
const SYMBOLS: [&str; 6] = [
    "process_widget_0",
    "process_widget_1",
    "common_helper_2",
    "only_linked_helper_1",
    "zebra_helper_2",
    "absent",
];
/// Small enough that every name spans several pages.
const PAGE: usize = 3;

fn git(args: &[&str], cwd: &Path) {
    let out = crate::testing::git_command()
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn source(i: usize, flavour: &str) -> String {
    format!(
        "pub fn process_widget_{i}() {{\n    render_frame();\n    spawn_task();\n    {flavour}_helper_{i}(1);\n}}\n\n\
         pub fn {flavour}_helper_{i}(queue: u32) -> u32 {{\n    render_frame();\n    queue + {i}\n}}\n"
    )
}

/// A repository with two linked worktrees: one edits two files and adds one, one drops a file.
/// Every tree holds `dup.rs`, a copy of `f0.rs`, so one content has two holders in each view.
fn group(tmp: &Path) -> [PathBuf; 3] {
    let main = tmp.join("main");
    fs::create_dir(&main).expect("main");
    git(&["init", "-q", "-b", "main"], &main);
    git(&["config", "user.email", "t@example.com"], &main);
    git(&["config", "user.name", "Test"], &main);
    git(&["config", "core.autocrlf", "false"], &main);
    // The fixture must not depend on the user's signing setup. ~keep
    git(&["config", "commit.gpgsign", "false"], &main);
    for i in 0..10 {
        fs::write(main.join(format!("f{i}.rs")), source(i, "common")).expect("write");
    }
    fs::write(main.join("dup.rs"), source(0, "common")).expect("write");
    git(&["add", "."], &main);
    git(&["commit", "-qm", "init"], &main);
    let linked = tmp.join("linked");
    let pruned = tmp.join("pruned");
    for (dir, branch) in [(&linked, "linked"), (&pruned, "pruned")] {
        git(
            &["worktree", "add", "-q", dir.to_str().expect("utf-8"), "-b", branch],
            &main,
        );
    }
    fs::write(linked.join("f1.rs"), source(1, "only_linked")).expect("write");
    fs::write(linked.join("f2.rs"), source(2, "zebra")).expect("write");
    fs::write(linked.join("extra.rs"), source(99, "only_linked")).expect("write");
    fs::remove_file(pruned.join("f3.rs")).expect("remove");
    [main, linked, pruned]
}

fn config() -> ConfigV1 {
    let mut config = ConfigV1::with_defaults();
    config.documents.enabled = false;
    config
}

fn scan_working(root: &Path) -> Store {
    let mut store = Store::open(root, VIEW_WORKING).expect("open");
    scan(
        root,
        &mut store,
        &config(),
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        crate::retrieval_wiring::backend(),
    )
    .expect("scan");
    store
}

/// The source files of `root` (no `.git`) copied outside the repository: a group of one.
fn standalone_copy(root: &Path, to: &Path) -> PathBuf {
    fs::create_dir_all(to).expect("standalone dir");
    for entry in fs::read_dir(root).expect("read worktree") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            fs::copy(&path, to.join(path.file_name().expect("name"))).expect("copy");
        }
    }
    to.to_path_buf()
}

/// One call-site hit: `(path, line, column, callee)`.
type Hit = (String, u32, u32, String);

/// Every page of a name scan, as the `references` tool pages it.
fn pages(view: &dyn IndexView, name: &str, matching: Matching) -> Vec<Vec<Hit>> {
    let mut pages = Vec::new();
    let mut cursor: Option<Vec<u8>> = None;
    loop {
        let page = scan_calls_by_name(view, name, matching, PAGE, cursor.as_deref()).expect("scan");
        assert!(!page.total_is_partial, "{name}: the fixture is far below every cap");
        let page = budget_call_page(page, name, None);
        pages.push(
            page.hits
                .iter()
                .map(|hit| (hit.path.to_string(), hit.line, hit.column, hit.callee.clone()))
                .collect(),
        );
        match page.next_cursor {
            Some(next) => cursor = Some(next.decode_fjall().expect("cursor")),
            None => return pages,
        }
        assert!(pages.len() < 100, "{name}: paging must terminate");
    }
}

/// What one view answers: every name scan's hits (all pages, sorted), its symbol lookups, and the
/// call sites of each of its files.
#[derive(Debug, PartialEq)]
struct Answers {
    references: Vec<Vec<Hit>>,
    symbols: Vec<Vec<(String, String, u32)>>,
    calls: Vec<FileCalls>,
}

/// A file's call sites: `(path, [(line, column, callee)])`.
type FileCalls = (String, Vec<(u32, u32, String)>);

fn answers(store: &Store) -> Answers {
    let view = store.index_view().expect("a writer holds its index");
    let references = CALLEES
        .iter()
        .map(|(name, matching)| {
            let pages = pages(&view, name, *matching);
            for page in &pages {
                assert!(
                    page.is_sorted_by(|a, b| a.0 <= b.0),
                    "{name}: each page is sorted by path: {page:?}"
                );
            }
            let mut all: Vec<Hit> = pages.into_iter().flatten().collect();
            all.sort();
            let listed = all.len();
            all.dedup();
            assert_eq!(all.len(), listed, "{name}: no hit is listed on two pages");
            all
        })
        .collect();
    let symbols = SYMBOLS
        .iter()
        .map(|name| {
            let mut found: Vec<(String, String, u32)> = view
                .symbols_by_name_lookup(name, 1_000)
                .into_iter()
                .map(|(name, _, path, start)| (name, path.to_string(), start))
                .collect();
            found.sort();
            found
        })
        .collect();
    let mut paths: Vec<&RelPath> = store.index.files.keys().collect();
    paths.sort();
    let calls = paths
        .into_iter()
        .map(|path| {
            let sites = calls_in_content_fjall(&view, view.live_keys().key_of(path))
                .expect("calls")
                .into_iter()
                .map(|call| (call.line, call.column, call.callee))
                .collect();
            (path.to_string(), sites)
        })
        .collect();
    Answers {
        references,
        symbols,
        calls,
    }
}

/// A call site's place: `(path, line, column)`.
type Site = (String, u32, u32);

/// The call sites of `callee` in the `.rs` files of `root`, read from their text: the oracle the
/// index answers are held to, independent of every scan and paging path. A call is the callee
/// followed by `(`, not preceded by an identifier character; its column is its 0-based byte column.
fn sites_in_text(root: &Path, callee: &str) -> Vec<Site> {
    let needle = format!("{callee}(");
    let mut sites = Vec::new();
    for entry in fs::read_dir(root).expect("read tree") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let name = path.file_name().expect("name").to_string_lossy().into_owned();
        let text = fs::read_to_string(&path).expect("read source");
        for (row, line) in text.lines().enumerate() {
            for (column, _) in line.match_indices(&needle) {
                let bound = line[..column]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
                if bound {
                    let (row, column) = (u32::try_from(row).expect("row"), u32::try_from(column).expect("column"));
                    sites.push((name.clone(), row + 1, column));
                }
            }
        }
    }
    sites.sort();
    sites
}

/// The exact-name answers of `view` against the text of `root`.
fn assert_matches_the_text(view: &Answers, root: &Path, label: &str) {
    for (at, (name, matching)) in CALLEES.iter().enumerate() {
        if *matching != Matching::Exact {
            continue;
        }
        let listed: Vec<Site> = view.references[at]
            .iter()
            .map(|(path, line, column, _)| (path.clone(), *line, *column))
            .collect();
        assert_eq!(listed, sites_in_text(root, name), "{label}: {name} against the text");
    }
    assert!(
        !sites_in_text(root, "render_frame").is_empty(),
        "{label}: the oracle sees call sites, or it proves nothing"
    );
}

fn assert_equivalent(group_view: &Store, standalone: &Store, label: &str) {
    let (group, alone) = (answers(group_view), answers(standalone));
    assert_matches_the_text(&group, &group_view.root, label);
    assert!(
        alone.references.iter().filter(|hits| !hits.is_empty()).count() >= 3,
        "{label}: the fixture must produce call hits"
    );
    assert!(
        alone.symbols.iter().filter(|found| !found.is_empty()).count() >= 2,
        "{label}: the fixture must produce symbol hits"
    );
    assert!(
        alone.references[0].iter().any(|hit| hit.0 == "dup.rs"),
        "{label}: a content with two holders is listed under both"
    );
    assert_eq!(group, alone, "{label}");
}

#[test]
fn every_view_of_a_group_answers_like_a_standalone_index_of_its_tree() {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let roots = group(tmp.path());

    let views: Vec<Store> = roots.iter().map(|root| scan_working(root)).collect();
    for (i, root) in roots.iter().enumerate() {
        let alone = scan_working(&standalone_copy(root, &tmp.path().join(format!("alone-{i}"))));
        assert_equivalent(&views[i], &alone, &format!("view {i}"));
    }

    // An incremental pass in the main worktree drops a content the linked one still holds.
    drop(views);
    fs::remove_file(roots[0].join("f0.rs")).expect("remove");
    let mut main = Store::open(&roots[0], VIEW_WORKING).expect("open");
    crate::scanner::scan_paths(
        &roots[0],
        &mut main,
        &config(),
        &[roots[0].join("f0.rs")],
        EmbedMode::Inline,
        crate::retrieval_wiring::backend(),
    )
    .expect("incremental rescan");
    drop(main);
    for (i, root) in roots.iter().enumerate() {
        let view = scan_working(root);
        let alone = scan_working(&standalone_copy(root, &tmp.path().join(format!("again-{i}"))));
        assert_equivalent(&view, &alone, &format!("view {i} after a sibling's incremental pass"));
    }

    // An edit inside one view: the earlier content's rows stay stored, and must not be read. ~keep
    fs::write(
        roots[0].join("f4.rs"),
        format!("{}\nfn more() {{\n    render_frame();\n}}\n", source(4, "edited")),
    )
    .expect("edit");
    let main = scan_working(&roots[0]);
    let alone = scan_working(&standalone_copy(&roots[0], &tmp.path().join("edited")));
    assert_equivalent(&main, &alone, "main after an edit in place");
    let view = main.index_view().expect("index");
    assert!(
        view.symbols_by_name_lookup("common_helper_4", 10).is_empty(),
        "the edited file's earlier symbols are not read"
    );
    assert_eq!(view.symbols_by_name_lookup("edited_helper_4", 10).len(), 1);
}
