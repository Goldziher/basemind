use super::*;
use std::fs;

/// Build an `IndexFilter` rooted at a fresh temp dir, run `body` to populate the tree, and
/// return `(filter, root, tmp)`. `root` is canonicalized to match the absolute paths the filter
/// and the `ignore` walker compare against. The caller must keep `tmp` bound for the duration of
/// the test so the tree stays on disk while the filter walks it.
fn filter_for(body: impl FnOnce(&Path)) -> (IndexFilter, PathBuf, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize");
    fs::create_dir_all(root.join(".git")).expect("mkdir .git");
    body(&root);
    let config = crate::config::default_for_root(&root);
    let filter = IndexFilter::new(&root, &config).expect("build filter");
    (filter, root, tmp)
}

#[test]
fn should_reject_path_under_nested_gitignore_rule() {
    let (filter, root, _tmp) = filter_for(|root| {
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/.gitignore"), b"ignored.rs\n").unwrap();
        fs::write(root.join("sub/ignored.rs"), b"fn a() {}\n").unwrap();
        fs::write(root.join("sub/kept.rs"), b"fn b() {}\n").unwrap();
    });
    assert!(
        !filter.is_indexable(&root.join("sub/ignored.rs")),
        "a file matched by its own dir's nested .gitignore must be rejected"
    );
    assert!(
        filter.is_indexable(&root.join("sub/kept.rs")),
        "a tracked sibling must be kept"
    );
}

#[test]
fn should_reject_path_when_ancestor_directory_is_gitignored() {
    let (filter, root, _tmp) = filter_for(|root| {
        fs::write(root.join(".gitignore"), b"build/\n").unwrap();
        fs::create_dir_all(root.join("build/nested")).unwrap();
        fs::write(root.join("build/nested/out.rs"), b"fn c() {}\n").unwrap();
        fs::write(root.join("main.rs"), b"fn main() {}\n").unwrap();
    });
    assert!(
        !filter.is_indexable(&root.join("build/nested/out.rs")),
        "a file under an ancestor-gitignored directory must be rejected"
    );
    assert!(
        filter.is_indexable(&root.join("main.rs")),
        "a tracked top-level file must be kept"
    );
}

#[test]
fn should_reject_root_and_nested_basemind_via_default_exclude() {
    let (filter, root, _tmp) = filter_for(|root| {
        fs::create_dir_all(root.join(".basemind")).unwrap();
        fs::write(root.join(".basemind/x.msgpack"), b"\x00").unwrap();
        fs::create_dir_all(root.join("child/.basemind")).unwrap();
        fs::write(root.join("child/.basemind/y.msgpack"), b"\x00").unwrap();
        fs::write(root.join("child/real.rs"), b"fn d() {}\n").unwrap();
    });
    assert!(!filter.allows_glob(".basemind/x.msgpack"));
    assert!(!filter.allows_glob("child/.basemind/y.msgpack"));
    assert!(!filter.is_indexable(&root.join(".basemind/x.msgpack")));
    assert!(!filter.is_indexable(&root.join("child/.basemind/y.msgpack")));
    assert!(
        filter.is_indexable(&root.join("child/real.rs")),
        "a real source file beside a nested .basemind must still be kept"
    );
}

#[test]
fn embed_gates_compose_include_and_exclude_with_exclude_winning() {
    let mut config = crate::config::default_for_root(Path::new("."));
    let filters = Filters::build(&config, Vec::new()).expect("build filters");
    assert!(filters.code_embed_allowed("src/lib.rs"), "empty lists allow everything");
    assert!(filters.doc_embed_allowed("docs/a.pdf"));
    assert!(filters.doc_allowed("docs/a.pdf"));

    config.code_search.embed_include = vec!["src/**".to_string()];
    config.code_search.embed_exclude = vec!["**/generated/**".to_string(), "**/*.min.js".to_string()];
    config.documents.include = vec!["docs".to_string()];
    config.documents.exclude = vec!["**/draft*".to_string()];
    config.documents.embed_include = vec!["**/*.pdf".to_string()];
    config.documents.embed_exclude = vec!["docs/huge".to_string()];
    let filters = Filters::build(&config, Vec::new()).expect("build filters");

    assert!(filters.code_embed_allowed("src/lib.rs"));
    assert!(!filters.code_embed_allowed("tools/x.rs"), "outside embed_include");
    assert!(
        !filters.code_embed_allowed("src/generated/schema.rs"),
        "exclude beats include"
    );
    assert!(!filters.code_embed_allowed("src/bundle.min.js"));

    assert!(filters.doc_allowed("docs/guide.md"));
    assert!(!filters.doc_allowed("other/guide.md"), "outside documents.include");
    assert!(
        !filters.doc_allowed("docs/draft-1.md"),
        "documents.exclude beats include"
    );
    assert!(filters.doc_embed_allowed("docs/a.pdf"));
    assert!(!filters.doc_embed_allowed("docs/a.md"), "outside embed_include");
    assert!(
        !filters.doc_embed_allowed("docs/huge/a.pdf"),
        "bare embed_exclude covers the subtree"
    );
}

#[test]
fn invalid_globs_fail_filter_construction() {
    let mut config = crate::config::default_for_root(Path::new("."));
    config.documents.embed_exclude = vec!["a/[".to_string()];
    assert!(matches!(
        Filters::build(&config, Vec::new()),
        Err(ScanError::BadGlob(_))
    ));
    let mut config = crate::config::default_for_root(Path::new("."));
    config.documents.include = vec!["a/[".to_string()];
    assert!(matches!(
        Filters::build(&config, Vec::new()),
        Err(ScanError::BadGlob(_))
    ));
}

#[test]
fn bare_exclude_name_excludes_the_subtree_and_prunes_the_directory() {
    let mut config = crate::config::default_for_root(Path::new("."));
    config.scan.exclude = vec!["generated".to_string()];
    let filters = Filters::build(&config, Vec::new()).expect("build filters");
    assert!(!filters.allows("generated/schema.rs"));
    assert!(!filters.allows("pkg/generated/deep/schema.rs"));
    assert!(filters.allows("pkg/generated_code/schema.rs"));
    assert!(
        !filters.allows_dir("generated"),
        "the walker prunes the directory itself"
    );
    assert!(!filters.allows_dir("pkg/generated"));
}

#[test]
fn floor_allow_removes_named_floor_entries_but_never_git_or_basemind() {
    let mut config = crate::config::default_for_root(Path::new("."));
    let baseline = Filters::build(&config, Vec::new()).expect("build filters");
    assert!(!baseline.allows("build/gen.rs"));
    assert!(!baseline.allows("vendor/dep/lib.go"));

    config.scan.floor_allow = vec![
        "build".to_string(),
        "**/vendor/**".to_string(),
        ".git".to_string(),
        "no-such-entry".to_string(),
    ];
    let filters = Filters::build(&config, Vec::new()).expect("build filters");
    assert!(filters.allows("build/gen.rs"), "named by directory");
    assert!(filters.allows("vendor/dep/lib.go"), "named by floor pattern");
    assert!(
        filters.allows_dir("build"),
        "the walker descends into an allowed floor dir"
    );
    assert!(!filters.allows(".git/config"), ".git can never be allowed");
    assert!(!filters.allows("out/x.rs"), "untouched floor entries stay");
}

#[test]
fn secrets_are_excluded_by_default_and_floor_allow_opts_back_in() {
    let mut config = crate::config::default_for_root(Path::new("."));
    let filters = Filters::build(&config, Vec::new()).expect("build filters");
    for secret in [
        ".env",
        "svc/.env.production",
        ".aws/credentials",
        "home/.ssh/config",
        ".npmrc",
        "deploy/id_rsa",
        "certs/server.pem",
        "certs/tls.key",
    ] {
        assert!(!filters.allows(secret), "{secret} must not be indexed");
    }
    assert!(filters.allows("src/environment.rs"));
    assert!(filters.allows("keys.rs"));

    config.scan.floor_allow = vec![".env.*".to_string(), "*.pem".to_string()];
    let filters = Filters::build(&config, Vec::new()).expect("build filters");
    assert!(filters.allows("svc/.env.production"));
    assert!(filters.allows("certs/server.pem"));
    assert!(!filters.allows(".env"), "unlisted entries stay excluded");
}

#[test]
fn extra_root_files_match_globs_relative_to_the_root() {
    let config = crate::config::default_for_root(Path::new("."));
    let filters = Filters::build(&config, Vec::new()).expect("build filters");
    assert!(
        filters.allows_in_extra_root("src/lib.rs"),
        "root-relative path is judged on its own, not on where the root lives"
    );
    assert!(!filters.allows_in_extra_root("node_modules/x/index.js"));
    assert!(
        !filters.allows("/opt/build/ext/src/lib.rs"),
        "an absolute key under a floor-named directory is what the old check tripped on"
    );
}

#[test]
fn nested_extra_roots_scope_a_key_to_the_longest_matching_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canonicalize");
    for dir in ["ext", "ext/inner", "extra"] {
        fs::create_dir_all(base.join(dir)).expect("mkdir");
    }
    let mut config = crate::config::default_for_root(Path::new("."));
    config.scan.extra_roots = vec![base.join("ext"), base.join("ext/inner"), base.join("extra")];
    let filters = Filters::build(&config, Vec::new()).expect("build filters");
    let key = |rel: &str| format!("{}/{rel}", base.display());

    assert_eq!(filters.scoped(&key("ext/a.md")), "a.md");
    assert_eq!(
        filters.scoped(&key("ext/inner/a.md")),
        "a.md",
        "the nested root wins, not the first listed"
    );
    assert_eq!(filters.scoped(&key("extra/a.md")), "a.md", "/ext must not claim /extra");
}

#[test]
fn should_apply_exclude_floor_even_with_a_narrow_user_exclude() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize");
    fs::create_dir_all(root.join(".git")).expect("mkdir .git");
    let mut config = crate::config::default_for_root(&root);
    config.scan.exclude = vec!["**/mycustom/**".to_string()];
    let filter = IndexFilter::new(&root, &config).expect("build filter");

    assert!(
        !filter.allows_glob("node_modules/react/index.js"),
        "floor: node_modules"
    );
    assert!(!filter.allows_glob("target/debug/build.rs"), "floor: target");
    assert!(
        !filter.allows_glob("pkg/__pycache__/mod.pyc"),
        "floor: __pycache__ / *.pyc"
    );
    assert!(!filter.allows_glob("bazel-out/gen/x.go"), "floor: bazel-out");
    assert!(!filter.allows_glob("mycustom/thing.rs"), "user exclude honored");
    assert!(filter.allows_glob("src/lib.rs"), "real source file kept");
}

#[test]
fn should_reject_out_of_root_and_empty_rel() {
    let (filter, root, _tmp) = filter_for(|root| {
        fs::write(root.join("a.rs"), b"fn e() {}\n").unwrap();
    });
    assert!(!filter.is_indexable(&root));
    assert!(!filter.is_indexable(Path::new("/definitely/not/under/root.rs")));
}

/// `allows_dir` prunes directories by exclude globs only (no include-glob gate), so the
/// watcher can register inotify watches on kept directories and skip excluded or unreadable
/// ones — the fix for the "notify error: Permission denied" crash on unreadable trees.
#[test]
fn allows_dir_prunes_by_exclude_globs_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize");
    fs::create_dir_all(root.join(".git")).expect("mkdir .git");
    let mut config = crate::config::default_for_root(&root);
    config.scan.exclude = vec!["**/generated/**".to_string()];
    let filter = IndexFilter::new(&root, &config).expect("build filter");
    let filters = filter.filters();

    // Directories under the user exclude glob are pruned. The glob `**/generated/**` matches
    // subdirectories (a trailing segment is required), so we assert on nested paths.
    assert!(!filters.allows_dir("generated/schema"), "excluded nested dir pruned");
    assert!(
        !filters.allows_dir("generated/schema/types"),
        "deeply nested excluded dir pruned"
    );
    // Plain source directories are kept.
    assert!(filters.allows_dir("src"), "kept dir");
    assert!(filters.allows_dir("src/services"), "kept nested dir");
    assert!(!filters.allows_dir("node_modules/react"), "floor: node_modules/react");
    assert!(!filters.allows_dir("target/debug"), "floor: target/debug");
    // The bare directory itself: `**/X/**` never matches `X`, so the directory-level exclude
    // set carries the `/**`-stripped form. Without it the walker descends into `node_modules`
    // and stats every file inside only to throw them all away.
    for dir in ["node_modules", "target", "vendor", ".git", "dist", "generated"] {
        assert!(!filters.allows_dir(dir), "bare excluded dir pruned: {dir}");
    }
    assert!(
        !filters.allows_dir("packages/app/node_modules"),
        "nested bare excluded dir pruned"
    );
}

/// A `scan.exclude` entry with no `/**` suffix constrains files, not subtrees, so it must never
/// prune a directory. `**/generated` does not match `generated/schema.rs`, which is therefore
/// indexed today; pruning `generated/` would delete real files from the index — a data-loss bug
/// wearing a memory fix's clothes. This is the invariant [`dir_exclude_patterns`] exists to hold,
/// and the reason it drops every pattern outside the `P/**` family.
#[test]
fn a_file_shaped_exclude_never_prunes_the_directory_it_names() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize");
    fs::create_dir_all(root.join(".git")).expect("mkdir .git");
    let mut config = crate::config::default_for_root(&root);
    config.scan.exclude = vec!["**/generated".to_string(), "**/cache/**".to_string()];
    let filter = IndexFilter::new(&root, &config).expect("build filter");
    let filters = filter.filters();

    assert!(
        filters.allows("generated/schema.rs"),
        "a file under `generated/` is indexed today; the directory gate must not change that"
    );
    assert!(
        filters.allows_dir("generated"),
        "pruning `generated/` would drop the very file `allows` just admitted"
    );
    assert!(
        !filters.allows_dir("cache"),
        "the `/**` form still prunes, so the distinction is real and not an accident"
    );
}

/// The walk root is never pruned, whatever it is called: its relative path is `""` and a repo
/// that happens to be named `target` must still scan.
#[test]
fn dir_pruner_keeps_the_walk_root_even_when_named_target() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize").join("target");
    fs::create_dir_all(root.join(".git")).expect("mkdir .git");
    fs::create_dir_all(root.join("node_modules/pkg")).expect("mkdir node_modules");
    fs::write(root.join("a.rs"), b"fn a() {}\n").expect("write a.rs");
    fs::write(root.join("node_modules/pkg/i.js"), b"//\n").expect("write i.js");
    let config = crate::config::default_for_root(&root);
    let filters = Filters::build(&config, Vec::new()).expect("build filters");
    let pruner = filters.dir_pruner(Some(&root));

    let kept: Vec<String> = ignore_walk_builder(&root, false, false)
        .filter_entry(move |dent| pruner.keep(dent))
        .build()
        .flatten()
        .filter_map(|d| {
            d.path()
                .strip_prefix(&root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        })
        .collect();

    assert!(
        kept.iter().any(|p| p == "a.rs"),
        "root named `target` still walked: {kept:?}"
    );
    assert!(
        !kept.iter().any(|p| p.starts_with("node_modules")),
        "node_modules pruned at the directory, not per file: {kept:?}"
    );
}

/// The only thing `filter_entry(DirPruner)` changes is how much the walker *does*: the indexed
/// set was already identical without it, because `walk_candidates` dropped every `node_modules`
/// path through `Filters::allows` one stat later. Scan output therefore cannot observe this fix
/// — walk work can, so the two walks below differ by exactly the subtree the gate refuses to
/// descend into.
#[test]
fn the_dir_pruner_stops_the_walk_descending_into_a_non_gitignored_node_modules() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize");
    fs::create_dir_all(root.join("src")).expect("mkdir src");
    fs::write(root.join("src/a.rs"), b"fn a() {}\n").expect("write a.rs");
    for pkg in 0..4 {
        let pkg_dir = root.join(format!("node_modules/pkg{pkg}/lib"));
        fs::create_dir_all(&pkg_dir).expect("mkdir pkg");
        for file in 0..5 {
            fs::write(pkg_dir.join(format!("m{file}.js")), b"//\n").expect("write module");
        }
    }
    let config = crate::config::default_for_root(&root);
    let filters = Filters::build(&config, Vec::new()).expect("build filters");

    let visited = |pruner: Option<DirPruner>| -> Vec<String> {
        let mut builder = ignore_walk_builder(&root, false, false);
        if let Some(pruner) = pruner {
            builder.filter_entry(move |dent| pruner.keep(dent));
        }
        builder
            .build()
            .flatten()
            .filter_map(|d| {
                d.path()
                    .strip_prefix(&root)
                    .ok()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
            })
            .collect()
    };

    let ungated = visited(None);
    let pruned = visited(Some(filters.dir_pruner(Some(&root))));

    assert!(
        ungated.iter().filter(|p| p.starts_with("node_modules")).count() >= 20,
        "the ungated walk must actually descend, or the comparison proves nothing: {ungated:?}"
    );
    assert!(
        !pruned.iter().any(|p| p.starts_with("node_modules")),
        "the gated walk must not enter node_modules at all: {pruned:?}"
    );
    assert!(
        pruned.len() < ungated.len(),
        "pruning must be strictly less walk work: {} vs {}",
        pruned.len(),
        ungated.len()
    );
    assert!(
        pruned.iter().any(|p| p == "src/a.rs"),
        "real source is still walked: {pruned:?}"
    );
}
