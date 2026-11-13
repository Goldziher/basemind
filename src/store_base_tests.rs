use super::*;

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.test")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn committed_base_is_atomic_reusable_and_independent_of_working_state() {
        crate::store::init_isolated_cache();
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        let file = root.join("example.rs");
        git(root, &["init", "-q"]);
        fs::write(&file, "pub fn original() {}").unwrap();
        git(root, &["add", "."]);
        git(root, &["-c", "commit.gpgsign=false", "commit", "-qm", "fixture"]);
        fs::write(&file, "pub fn uncommitted() {}").unwrap();
        fs::write(root.join("untracked.rs"), "pub fn untracked() {}").unwrap();

        let mut config = Config::with_defaults();
        config.documents.embed = true;
        config.code_search.embed = true;
        let BaseBuild::Ready(first) = prepare(root, &config).unwrap() else {
            panic!("committed base was not published");
        };
        assert_eq!(first.file_name().unwrap(), "gen-1");
        let index = crate::store::read_index(&first).unwrap().unwrap();
        assert_eq!(index.files.len(), 1);
        let entry = index.files.get(&crate::path::RelPath::from("example.rs")).unwrap();
        assert_eq!(entry.mtime, 0);
        let expected = blake3::hash(b"pub fn original() {}");
        assert_eq!(entry.hash_hex, expected.to_hex().to_string());
        assert!(!first.join("checkout").exists());
        assert!(!first.join("lance").exists());

        let BaseBuild::Ready(reused) = prepare(root, &config).unwrap() else {
            panic!("compatible generation was not reused");
        };
        assert_eq!(reused, first);

        let commit = Repo::discover(root).unwrap().resolve_rev("HEAD").unwrap();
        assert!(open(root, &config, 1, "wrong").is_err());
        let reader = open(root, &config, 1, &commit).unwrap();
        let key = crate::index::keys::symbols_by_path_prefix(&crate::path::RelPath::from("example.rs"));
        let (_, retained) = reader
            .prefix(crate::index::view::Part::SymbolsByPath, &key)
            .next()
            .unwrap()
            .unwrap();
        assert!(!retained.is_empty());
        drop(reader);
        assert!(!first.join(INDEX_DIR).exists());
        let group = repo_group_dir(root);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(group.join(BASE_LOCK_FILE))
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&lock).unwrap();
        assert!(matches!(prepare(root, &config).unwrap(), BaseBuild::Busy));
        drop(lock);

        assert!(open_clean_worktree(root, &config).unwrap().is_none());
        use crate::store_base_retention::retire_unpinned;
        use std::collections::BTreeSet;
        let generations = BTreeSet::from([1]);
        let retired = retire_unpinned(&group, &generations, || Ok(BTreeSet::new())).unwrap();
        assert_eq!(retired.active, [1]);
        assert!(first.is_dir());
        drop(retained);
        let retired = retire_unpinned(&group, &generations, || Ok(BTreeSet::new())).unwrap();
        assert_eq!(retired.removed, [1]);
        let BaseBuild::Ready(second) = prepare(root, &config).unwrap() else {
            panic!("invalid generation was not replaced");
        };
        assert_eq!(second.file_name().unwrap(), "gen-2");
    }
}

#[cfg(all(test, feature = "code-intel-js"))]
#[test]
fn published_overlay_restitches_unchanged_importers() {
    verify_moved_export(
        "ts",
        "export function target() {}\n",
        "import { target } from './target';\ntarget();\n",
        "// moved\nexport function target() {}\n",
    );
    verify_moved_export(
        "py",
        "def target():\n    return 1\n",
        "from target import target\ntarget()\n",
        "# moved\ndef target():\n    return 1\n",
    );
}

#[cfg(all(test, feature = "code-intel-js"))]
fn verify_moved_export(extension: &str, original: &str, importer: &str, changed: &str) {
    crate::store::init_isolated_cache();
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.test")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.test")
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    let target_name = format!("target.{extension}");
    let importer_name = format!("use.{extension}");
    fs::write(root.join(&target_name), original).unwrap();
    fs::write(root.join(&importer_name), importer).unwrap();
    git(&["add", &target_name, &importer_name]);
    git(&["-c", "commit.gpgsign=false", "commit", "-qm", "fixture"]);
    let mut config = Config::with_defaults();
    config.documents.enabled = false;
    config.documents.embed = false;
    config.code_search.enabled = false;
    config.code_search.embed = false;
    assert!(open_clean_worktree(root, &config).unwrap().is_some());
    fs::write(root.join(&target_name), changed).unwrap();
    let mut standalone = Store::open(root, "working").unwrap();
    scan(
        root,
        &mut standalone,
        &config,
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
    )
    .unwrap();
    let reference = standalone.index_view().unwrap();
    assert!(publish_dirty_worktree(root, &config, &reference).unwrap());
    let view = open_dirty_worktree(root, &config).unwrap().unwrap();
    let target = crate::path::RelPath::from(target_name);
    let offset = u32::try_from(changed.find("target").unwrap()).unwrap();
    let expected = reference.references_to(&target, offset);
    assert!(!expected.is_empty());
    assert_eq!(view.references_to(&target, offset), expected);
    drop(view);
    let group = repo_group_dir(root);
    let base = group.join(BASE_DIR);
    fs::create_dir(base.join("gen-2")).unwrap();
    fs::create_dir(base.join("gen-3")).unwrap();
    assert_eq!(retire_old_generations(root).unwrap(), 0);
    assert!(base.join("gen-1").is_dir());
    let pin = group
        .join("overlay-pins")
        .join(format!("{}-1", crate::store_layout::workspace_key(root)));
    fs::remove_file(pin).unwrap();
    assert_eq!(retire_old_generations(root).unwrap(), 1);
    assert!(!base.join("gen-1").exists());
}

#[cfg(all(test, not(any(feature = "code-intel-js", feature = "code-intel-stack"))))]
#[test]
fn reuses_a_base_when_new_commit_changes_less_than_two_percent() {
    crate::store::init_isolated_cache();
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.test")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.test")
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    for index in 0..51 {
        fs::write(
            root.join(format!("sample_{index}.rs")),
            format!("fn sample_{index}() {{ old(); }}"),
        )
        .unwrap();
    }
    git(&["add", "."]);
    git(&["-c", "commit.gpgsign=false", "commit", "-qm", "base"]);
    let mut config = Config::with_defaults();
    config.documents.enabled = false;
    config.documents.embed = false;
    config.code_search.enabled = false;
    config.code_search.embed = false;
    assert!(open_clean_worktree(root, &config).unwrap().is_some());
    fs::write(root.join("sample_0.rs"), "fn sample_0() { new(); }").unwrap();
    git(&["add", "sample_0.rs"]);
    git(&["-c", "commit.gpgsign=false", "commit", "-qm", "one file"]);
    assert!(scan_dirty_worktree(root, &config).unwrap());
    let base = repo_group_dir(root).join(BASE_DIR);
    assert!(base.join("gen-1").is_dir());
    assert!(!base.join("gen-2").exists());
    let view = open_dirty_worktree(root, &config).unwrap().unwrap();
    let mut reference = Store::open(root, "working").unwrap();
    scan(
        root,
        &mut reference,
        &config,
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
    )
    .unwrap();
    assert_eq!(reference.index.files.len(), 51);
    crate::index::verify_shared_base(&*view, &reference.index_view().unwrap()).unwrap();
}

#[cfg(all(test, feature = "code-intel-js"))]
#[test]
fn changing_a_typescript_config_restitches_its_unchanged_consumer() {
    crate::store::init_isolated_cache();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.test")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.test")
                .args(args)
                .status()
                .unwrap()
                .success()
        )
    };
    git(&["init", "-q"]);
    for (path, text) in [
        ("one/target.ts", "export const target = 1;\n"),
        ("two/target.ts", "export const target = 2;\n"),
        (
            "use.ts",
            "import { target } from '@lib/target';\nexport const v = target;\n",
        ),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib/*":["one/*"]}}}"#,
        ),
    ] {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    git(&["add", "."]);
    git(&["-c", "commit.gpgsign=false", "commit", "-qm", "fixture"]);
    let mut config = Config::with_defaults();
    config.documents.enabled = false;
    config.documents.embed = false;
    config.code_search.enabled = false;
    config.code_search.embed = false;
    assert!(open_clean_worktree(root, &config).unwrap().is_some());
    fs::write(
        root.join("tsconfig.json"),
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib/*":["two/*"]}}}"#,
    )
    .unwrap();
    let mut reference = Store::open(root, "working").unwrap();
    scan(
        root,
        &mut reference,
        &config,
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
    )
    .unwrap();
    assert!(publish_dirty_worktree(root, &config, &reference.index_view().unwrap()).unwrap());
    let view = open_dirty_worktree(root, &config).unwrap().unwrap();
    let target = crate::path::RelPath::from("two/target.ts");
    let expected = reference.index_view().unwrap().references_to(&target, 13);
    assert!(expected.iter().any(|(path, _)| path.as_str() == Some("use.ts")));
    assert_eq!(view.references_to(&target, 13), expected);
    let mut cold = Store::open_read_only_no_index(root, "working").unwrap();
    cold.index.files.clear();
    hydrate_cold_store(&mut cold, &config).unwrap();
    assert!(cold.index_db.is_none());
    assert_eq!(crate::query::resolved_references(&cold, &target, 13), expected);
    let (use_path, use_offset) = expected
        .iter()
        .find(|(path, _)| path.as_str() == Some("use.ts"))
        .unwrap();
    assert_eq!(
        crate::query::definition_of(&cold, use_path, *use_offset),
        Some((target, 13))
    );
}

#[cfg(all(test, feature = "code-intel-js"))]
#[test]
fn changing_a_typescript_barrel_restitches_its_unchanged_consumer() {
    crate::store::init_isolated_cache();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.test")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.test")
                .args(args)
                .status()
                .unwrap()
                .success()
        )
    };
    git(&["init", "-q"]);
    for (path, text) in [
        ("one.ts", "export function target() {}\n"),
        ("two.ts", "export function target() { return 2; }\n"),
        ("barrel.ts", "import { target } from './one'; export { target };\n"),
        ("use.ts", "import { target } from './barrel';\ntarget();\n"),
    ] {
        fs::write(root.join(path), text).unwrap();
    }
    git(&["add", "."]);
    git(&["-c", "commit.gpgsign=false", "commit", "-qm", "fixture"]);
    let mut config = Config::with_defaults();
    config.documents.enabled = false;
    config.documents.embed = false;
    config.code_search.enabled = false;
    config.code_search.embed = false;
    assert!(open_clean_worktree(root, &config).unwrap().is_some());
    fs::write(
        root.join("barrel.ts"),
        "import { target } from './two'; export { target };\n",
    )
    .unwrap();
    let mut reference = Store::open(root, "working").unwrap();
    scan(
        root,
        &mut reference,
        &config,
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
    )
    .unwrap();
    assert!(publish_dirty_worktree(root, &config, &reference.index_view().unwrap()).unwrap());
    let view = open_dirty_worktree(root, &config).unwrap().unwrap();
    let target = crate::path::RelPath::from("two.ts");
    let expected = reference.index_view().unwrap().references_to(&target, 16);
    assert!(expected.iter().any(|(path, _)| path.as_str() == Some("use.ts")));
    assert_eq!(view.references_to(&target, 16), expected);
}

#[cfg(all(test, not(any(feature = "code-intel-js", feature = "code-intel-stack"))))]
#[test]
fn rebase_refuses_delta_between_initial_and_post_rebase_file_limits() {
    crate::store::init_isolated_cache();
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.test")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.test")
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    for index in 0..8 {
        fs::write(root.join(format!("keep_{index}.rs")), format!("fn keep_{index}() {{}}")).unwrap();
    }
    git(&["add", "."]);
    git(&["-c", "commit.gpgsign=false", "commit", "-qm", "base"]);
    let mut config = Config::with_defaults();
    config.documents.enabled = false;
    config.documents.embed = false;
    config.code_search.enabled = false;
    config.code_search.embed = false;
    assert!(open_clean_worktree(root, &config).unwrap().is_some());
    fs::write(root.join("keep_0.rs"), "fn overlay() {}").unwrap();
    assert!(scan_dirty_worktree(root, &config).unwrap());
    fs::write(root.join("keep_1.rs"), "fn new_commit() {}").unwrap();
    git(&["add", "keep_1.rs"]);
    git(&["-c", "commit.gpgsign=false", "commit", "-qm", "advance"]);
    assert!(!scan_dirty_worktree(root, &config).unwrap());
    assert!(repo_group_dir(root).join("base/gen-2").is_dir());
    assert!(!scan_dirty_worktree(root, &config).unwrap());
    assert!(open_dirty_worktree(root, &config).unwrap().is_none());
}
