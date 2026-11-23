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
    let filter = IndexFilter::new(&root, &config, crate::scan_retrieval::Retrieval::inert()).expect("build filter");
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
    let filters =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");
    assert!(filters.code_embed_allowed("src/lib.rs"), "empty lists allow everything");
    assert!(filters.doc_embed_allowed("docs/a.pdf"));
    assert!(filters.doc_allowed("docs/a.pdf"));

    config.code_search.embed_include = vec!["src/**".to_string()];
    config.code_search.embed_exclude = vec!["**/generated/**".to_string(), "**/*.min.js".to_string()];
    config.documents.include = vec!["docs".to_string()];
    config.documents.exclude = vec!["**/draft*".to_string()];
    config.documents.embed_include = vec!["**/*.pdf".to_string()];
    config.documents.embed_exclude = vec!["docs/huge".to_string()];
    let filters =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");

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
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()),
        Err(ScanError::BadGlob(_))
    ));
    let mut config = crate::config::default_for_root(Path::new("."));
    config.documents.include = vec!["a/[".to_string()];
    assert!(matches!(
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()),
        Err(ScanError::BadGlob(_))
    ));
}

#[test]
fn bare_exclude_name_excludes_the_subtree_and_prunes_the_directory() {
    let mut config = crate::config::default_for_root(Path::new("."));
    config.scan.exclude = vec!["generated".to_string()];
    let filters =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");
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
    let baseline =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");
    assert!(!baseline.allows("build/gen.rs"));
    assert!(!baseline.allows("vendor/dep/lib.go"));

    config.scan.floor_allow = vec![
        "build".to_string(),
        "**/vendor/**".to_string(),
        ".git".to_string(),
        "no-such-entry".to_string(),
    ];
    let filters =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");
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
fn secrets_are_excluded_by_default_and_opt_back_in_needs_the_grant() {
    let config = crate::config::default_for_root(Path::new("."));
    let filters =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");
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

    // A repository's own `floor_allow` cannot remove credential entries without the operator grant.
    let entries = vec![".env.*".to_string(), "*.pem".to_string()];
    let without = effective_floor(&entries, false);
    assert!(
        without.contains(&"**/.env.*") && without.contains(&"**/*.pem"),
        "credentials stay in the floor without the grant"
    );

    // With the grant, the same entries are removed.
    let with = effective_floor(&entries, true);
    assert!(
        !with.contains(&"**/.env.*") && !with.contains(&"**/*.pem"),
        "the operator grant re-includes the named credential entries"
    );

    // A non-credential floor entry never needs the grant.
    let build = effective_floor(&["build".to_string()], false);
    assert!(!build.contains(&"**/build/**"), "build artifacts need no grant");
}

#[test]
fn credential_floor_is_a_subset_of_the_exclude_floor() {
    for pattern in FLOOR_CREDENTIALS {
        assert!(
            FLOOR_EXCLUDES.contains(pattern),
            "{pattern} is a credential-floor entry and must also be in FLOOR_EXCLUDES"
        );
    }
}

#[test]
fn extra_root_files_match_globs_relative_to_the_root() {
    let config = crate::config::default_for_root(Path::new("."));
    let filters =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");
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

// Keys are built by joining a canonical base with `/`; Windows canonical paths carry a `\\?\` prefix.
#[cfg(unix)]
#[test]
fn nested_extra_roots_scope_a_key_to_the_longest_matching_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canonicalize");
    for dir in ["ext", "ext/inner", "extra"] {
        fs::create_dir_all(base.join(dir)).expect("mkdir");
    }
    let mut config = crate::config::default_for_root(Path::new("."));
    config.scan.extra_roots = vec![base.join("ext"), base.join("ext/inner"), base.join("extra")];
    let filters =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");
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
    let filter = IndexFilter::new(&root, &config, crate::scan_retrieval::Retrieval::inert()).expect("build filter");

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

fn default_filters() -> Filters {
    let config = crate::config::default_for_root(Path::new("."));
    Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters")
}

#[test]
fn cache_directories_and_files_are_in_the_exclude_floor() {
    let filters = default_filters();
    let excluded = [
        ".cache/pip/a.py",
        "app/.cache/x.json",
        "infra/.terragrunt-cache/abc/main.tf",
        ".turbo/log.json",
        "web/.parcel-cache/data.json",
        ".nox/py312/lib/m.py",
        ".hypothesis/examples/a",
        ".pnpm-store/v3/files/a.js",
        ".yarn/cache/left-pad-npm-1.0.0.zip",
        ".angular/cache/a.js",
        ".sass-cache/a.scss",
        ".ccache/tmp/a.o",
        "pkg/.dart_tool/package_config.json",
        "__pypackages__/3.12/lib/m.py",
        ".eggs/a.py",
        "src/mypkg.egg-info/PKG-INFO",
        "nb/.ipynb_checkpoints/a-checkpoint.ipynb",
        ".cargo/registry/src/a/lib.rs",
        "site/.docusaurus/routes.js",
        "web/.vite/deps/a.js",
        ".cache-loader/a.js",
        "web/.eslintcache",
        ".coverage",
        "pkg/.coverage",
    ];
    for path in excluded {
        assert!(!filters.allows(path), "{path} must be excluded by the floor");
    }
    let kept = [
        "src/cache.rs",
        "src/cache/store.rs",
        "docs/cache-notes.md",
        "src/hypothesis.py",
        ".yarn/releases/yarn-4.0.0.cjs",
        ".yarn/plugins/plugin.cjs",
        ".cargo/config.toml",
        "src/mypkg_egg-info.md",
        "src/coverage.py",
        "src/eslintcache.ts",
        "src/vite.config.ts",
        ".github/workflows/ci.yml",
    ];
    for path in kept {
        assert!(filters.allows(path), "{path} must stay indexable");
    }
}

#[test]
fn lockfiles_are_in_the_exclude_floor() {
    let filters = default_filters();
    let excluded = [
        ".terraform.lock.hcl",
        "infra/prod/.terraform.lock.hcl",
        "a/b/foo.lock",
        "a/bun.lockb",
        "a/x.lockfile",
        "a/gradle.lockfile",
        "a/schema.proto.lock.json",
        "a/deps.lock.hcl",
        "a/deps.lock.yaml",
        "package-lock.json",
        "a/npm-shrinkwrap.json",
        "a/pnpm-lock.yaml",
        "yarn.lock",
        "a/bun.lock",
        "a/go.sum",
        "Cargo.lock",
        "a/poetry.lock",
        "a/uv.lock",
        "a/Pipfile.lock",
        "a/composer.lock",
        "a/Gemfile.lock",
        "charts/app/Chart.lock",
        "a/flake.lock",
        "a/pdm.lock",
        "a/mix.lock",
        "a/pubspec.lock",
        "ios/Podfile.lock",
        "a/packages.lock.json",
        "a/deno.lock",
    ];
    for path in excluded {
        assert!(!filters.allows(path), "{path} must be excluded by the floor");
    }
    let kept = [
        "go.mod",
        "lockdown.py",
        "block.lock.py",
        "src/lock.rs",
        "src/locks/mod.rs",
        "package.json",
        "pnpm-workspace.yaml",
        "a/main.tf",
        "a/Chart.yaml",
        "a/requirements.txt",
        "a/sum.go",
    ];
    for path in kept {
        assert!(filters.allows(path), "{path} must stay indexable");
    }
}

#[test]
fn floor_allow_re_includes_a_lockfile_and_a_cache_directory() {
    let mut config = crate::config::default_for_root(Path::new("."));
    config.scan.floor_allow = vec!["go.sum".to_string(), ".cache".to_string(), "*.lock".to_string()];
    let filters =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");
    assert!(filters.allows("a/go.sum"));
    assert!(filters.allows(".cache/a.py"));
    assert!(filters.allows("a/foo.lock"));
    assert!(
        !filters.allows("a/yarn.lock"),
        "an unnamed lockfile entry stays excluded"
    );
    assert!(
        !filters.allows(".turbo/a.json"),
        "an unnamed cache entry stays excluded"
    );
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
    let filter = IndexFilter::new(&root, &config, crate::scan_retrieval::Retrieval::inert()).expect("build filter");
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
    let filter = IndexFilter::new(&root, &config, crate::scan_retrieval::Retrieval::inert()).expect("build filter");
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
    let filters =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");
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
    let filters =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");

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

#[test]
fn default_document_excludes_drop_vectors_lockfiles_and_test_data() {
    let config = crate::config::default_for_root(Path::new("."));
    let filters =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");
    for dropped in [
        "public/assets/svg/logo.svg",
        "icon.svg",
        "pnpm-lock.yaml",
        "web/package-lock.json",
        "yarn.lock",
        "Cargo.lock",
        "tools/poetry.lock",
        "Gemfile.lock",
        "svc/test_data/get_devices_valid.yaml",
        "svc/testdata/a.json",
        "pkg/fixtures/b.json",
        "ui/__snapshots__/c.snap",
    ] {
        assert!(!filters.doc_allowed(dropped), "{dropped} is excluded by default");
    }
    for kept in ["README.md", "docs/guide.md", "config/settings.yaml", "data/report.csv"] {
        assert!(filters.doc_allowed(kept), "{kept} stays indexable");
    }
}

#[test]
fn default_embed_scope_indexes_structured_data_but_embeds_only_prose() {
    let config = crate::config::default_for_root(Path::new("."));
    let filters =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");
    for (path, embedded) in [
        ("data/report.json", false),
        ("config/tsconfig.jsonc", false),
        ("config/settings.json5", false),
        ("maps/regions.geojson", false),
        ("logs/events.jsonl", false),
        ("logs/events.ndjson", false),
        ("config/settings.yaml", false),
        (".github/workflows/ci.yml", false),
        ("pyproject.toml", false),
        ("data/report.csv", false),
        ("data/report.tsv", false),
        ("pom.xml", false),
        ("README.md", true),
        ("docs/guide.rst", true),
        ("notes/readme.txt", true),
        ("docs/manual.pdf", true),
        ("docs/spec.docx", true),
        ("site/index.html", true),
        ("notebooks/demo.ipynb", true),
    ] {
        assert!(filters.doc_allowed(path), "{path} stays indexed as a document");
        assert_eq!(
            filters.doc_embed_allowed(path),
            embedded,
            "default embed decision for {path}"
        );
    }
}

#[test]
fn an_empty_embed_exclude_opts_structured_data_back_into_embedding() {
    let mut config = crate::config::default_for_root(Path::new("."));
    config.documents.embed_exclude = Vec::new();
    let filters =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");
    for path in [
        "data/report.json",
        "config/settings.yaml",
        "pyproject.toml",
        "data/report.csv",
        "pom.xml",
    ] {
        assert!(filters.doc_embed_allowed(path), "{path} is embedded once opted in");
    }

    config.documents.embed_exclude = vec!["*.csv".to_string()];
    let filters =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");
    assert!(
        !filters.doc_embed_allowed("data/report.csv"),
        "a custom list keeps its own entries"
    );
    assert!(
        filters.doc_embed_allowed("data/report.json"),
        "unlisted defaults are replaced, not merged"
    );
}

#[test]
fn user_document_excludes_replace_the_defaults_so_they_can_reinclude() {
    let mut config = crate::config::default_for_root(Path::new("."));
    config.documents.exclude = Vec::new();
    let filters =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");
    assert!(filters.doc_allowed("svc/test_data/a.yaml"));

    config.documents.exclude = vec!["*.svg".to_string()];
    let filters =
        Filters::build(&config, Vec::new(), crate::scan_retrieval::Retrieval::inert()).expect("build filters");
    assert!(
        !filters.doc_allowed("icon.svg"),
        "a custom list can repeat a default entry"
    );
    assert!(
        filters.doc_allowed("pnpm-lock.yaml"),
        "unlisted defaults are replaced, not merged"
    );
}

/// The Linux watcher re-arms a watch on each directory created after start. One created under a
/// gitignored tree, an excluded tree or `.basemind` must not get one: a `cargo build` would add a
/// watch per directory under `target/`, none ever removed.
#[test]
fn only_a_directory_the_scan_would_descend_into_is_watchable() {
    let (filter, root, _tmp) = filter_for(|root| {
        fs::write(root.join(".gitignore"), b"target/\n").unwrap();
        fs::create_dir_all(root.join("target/debug/deps")).unwrap();
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::create_dir_all(root.join(".basemind/cache")).unwrap();
        fs::write(root.join("src/lib.rs"), b"fn a() {}\n").unwrap();
    });
    assert!(filter.is_watchable_dir(&root.join("src")));
    assert!(filter.is_watchable_dir(&root.join("src/nested")));
    assert!(!filter.is_watchable_dir(&root.join("target")), "gitignored");
    assert!(
        !filter.is_watchable_dir(&root.join("target/debug/deps")),
        "under a gitignored dir"
    );
    assert!(!filter.is_watchable_dir(&root.join(".basemind/cache")));
    assert!(
        !filter.is_watchable_dir(&root.join("src/lib.rs")),
        "a file is not a directory"
    );
    assert!(!filter.is_watchable_dir(&root.join("gone")), "a vanished directory");
}
