//! Allow-list guard for workspace roots (issue #62).
//!
//! [`discover_root_with_basemind`](super::discover_root_with_basemind) falls back to `start`
//! unchanged when there is neither a `basemind.toml` marker nor an enclosing git repository. That
//! fallback is load-bearing for the CLI, but it means an MCP host launched with cwd `/` hands the
//! daemon `/` as a workspace root — and basemind then opens `/` read-write and walks the entire
//! filesystem. This has already happened in the wild (a workspace cache entry whose
//! `workspace.json` read `{"root":"/"}`).
//!
//! The fix is an ADDITIVE predicate applied where a root is *consumed* (client pre-flight, daemon
//! pool, HTTP `?root=`, CLI scan verbs), never inside discovery: a root must look like a project —
//! a git repository, or a directory carrying `basemind.toml`. This is a deliberate breaking change
//! for anyone indexing a plain directory, softened by two escape hatches ([`basemind init`] writes
//! the marker; [`ALLOW_ANY_ROOT_ENV`] skips the check) and a grandfather clause for roots that
//! already have a working-view index on disk.

use std::path::{Component, Path};

use super::CONFIG_FILE_NAME;

/// Escape hatch: set truthy to accept any directory as a workspace root. Mirrors the
/// `BASEMIND_ALLOW_PRIVATE_HOSTS` style — an env-var opt-out for a safety default. It does NOT
/// override [`RootRefusal::FilesystemRoot`]; nothing does.
pub const ALLOW_ANY_ROOT_ENV: &str = "BASEMIND_ALLOW_ANY_ROOT";

/// Why a candidate workspace root was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootRefusal {
    /// A directory that is neither a git repository nor marked with `basemind.toml`. Overridable.
    NotAProject,
    /// A filesystem or volume root (`/`, `C:\`, a UNC share root). Never overridable.
    FilesystemRoot,
}

/// Decide whether `root` may be used as a workspace root.
///
/// Evaluation order matters: the filesystem-root check runs FIRST so no marker file, env var or
/// pre-existing index can talk basemind into indexing an entire volume.
///
/// 1. `/`, `C:\`, a UNC share root → [`RootRefusal::FilesystemRoot`], unconditionally.
/// 2. `<root>/basemind.toml` exists → allowed. Reuses discovery's top-precedence marker, so the
///    documented escape hatch is simply `basemind init`.
/// 3. `root` is itself a git workdir → allowed.
/// 4. [`ALLOW_ANY_ROOT_ENV`] is truthy → allowed.
/// 5. Grandfather: a working-view index already exists in this root's cache → allowed, so an
///    upgrade never breaks someone who is already successfully indexing a plain directory.
pub fn workspace_root_verdict(root: &Path) -> Result<(), RootRefusal> {
    if is_filesystem_root(root) {
        return Err(RootRefusal::FilesystemRoot);
    }
    if root.join(CONFIG_FILE_NAME).is_file() || is_git_workdir(root) || allow_any_root() || is_grandfathered(root) {
        return Ok(());
    }
    Err(RootRefusal::NotAProject)
}

/// The operator-facing refusal text. Names what was refused, why it matters, and every way out.
pub fn refusal_message(root: &Path, refusal: RootRefusal) -> String {
    let root = root.display();
    match refusal {
        RootRefusal::FilesystemRoot => format!(
            "refusing to use {root} as a basemind workspace root: that is a filesystem/volume root.\n\
             \n\
             basemind opens a workspace root read-write and indexes every file beneath it. Rooted at \
             a whole volume that means walking the entire machine — it will exhaust memory and disk \
             long before it finishes.\n\
             \n\
             There is no override for this case. Point basemind at a project directory instead: pass \
             `--root <path>`, or launch the MCP host with its working directory inside the project."
        ),
        RootRefusal::NotAProject => format!(
            "refusing to use {root} as a basemind workspace root: it is neither a git repository nor \
             a directory containing {CONFIG_FILE_NAME}.\n\
             \n\
             basemind opens a workspace root read-write and indexes every file beneath it, so an \
             accidentally-inherited root — `/`, your home directory, or wherever an MCP host happened \
             to start — becomes a whole-filesystem scan that exhausts memory.\n\
             \n\
             If this really is a project you want indexed, either:\n\
             \x20 - run `basemind init` in {root} (writes {CONFIG_FILE_NAME}, the durable marker), or\n\
             \x20 - set {ALLOW_ANY_ROOT_ENV}=1 in the environment to skip this check."
        ),
    }
}

/// `/`, `C:\`, `\\server\share\` — a path made of nothing but a prefix and/or the root separator.
/// The `parent().is_none()` arm also catches the empty path, which is not a usable root either.
fn is_filesystem_root(root: &Path) -> bool {
    root.parent().is_none()
        || root
            .components()
            .all(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
}

/// True when git discovery from `root` lands on `root` itself. A subdirectory of a repo is NOT a
/// project root by this test — discovery already walked callers up to the workdir, so anything
/// deeper reaching here is a root that was never resolved through it.
fn is_git_workdir(root: &Path) -> bool {
    let Ok(repo) = crate::git::Repo::discover(root) else {
        return false;
    };
    let workdir = repo.workdir();
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    workdir == root || canonical(workdir) == canonical(root)
}

fn allow_any_root() -> bool {
    std::env::var(ALLOW_ANY_ROOT_ENV).is_ok_and(|value| {
        let value = value.trim();
        value.eq_ignore_ascii_case("1") || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
    })
}

/// A root that already carries a working-view index predates this guard and keeps working. Only
/// the on-disk index counts (not merely a cache directory), so a refused-then-abandoned root does
/// not grandfather itself in on the next attempt.
fn is_grandfathered(root: &Path) -> bool {
    crate::store::workspace_cache_dir(root)
        .join(crate::store::VIEWS_DIR)
        .join(crate::store::VIEW_WORKING)
        .join(crate::store::INDEX_FILE)
        .exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The env hatch is process-global, so the tests that touch it run under one mutex and always
    /// restore the prior value.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct AllowAnyRoot(Option<String>);

    impl AllowAnyRoot {
        fn set(value: &str) -> Self {
            let prior = std::env::var(ALLOW_ANY_ROOT_ENV).ok();
            // SAFETY: serialized by ENV_LOCK, which every env-touching test in this module holds.
            unsafe { std::env::set_var(ALLOW_ANY_ROOT_ENV, value) };
            Self(prior)
        }
    }

    impl Drop for AllowAnyRoot {
        fn drop(&mut self) {
            // SAFETY: as above.
            unsafe {
                match self.0.take() {
                    Some(prior) => std::env::set_var(ALLOW_ANY_ROOT_ENV, prior),
                    None => std::env::remove_var(ALLOW_ANY_ROOT_ENV),
                }
            }
        }
    }

    fn plain_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn filesystem_root_is_refused() {
        assert_eq!(workspace_root_verdict(Path::new("/")), Err(RootRefusal::FilesystemRoot));
    }

    #[test]
    fn windows_style_volume_and_unc_roots_are_refused() {
        // Parsed as `Normal` components on Unix, so assert the component predicate directly on the
        // shapes `std` would produce on Windows.
        assert!(is_filesystem_root(Path::new("/")));
        assert!(!is_filesystem_root(Path::new("/usr")));
        assert!(is_filesystem_root(Path::new("")));
    }

    #[test]
    fn a_git_repository_is_allowed() {
        let dir = plain_dir();
        let root = dir.path().canonicalize().expect("canonicalize");
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&root)
            .status()
            .expect("run git init");
        assert!(status.success(), "git init failed");
        assert_eq!(workspace_root_verdict(&root), Ok(()));
    }

    #[test]
    fn a_directory_with_basemind_toml_is_allowed() {
        let dir = plain_dir();
        let root = dir.path().canonicalize().expect("canonicalize");
        std::fs::write(root.join(CONFIG_FILE_NAME), "\"$schema\" = \"v1\"\n").expect("write marker");
        assert_eq!(workspace_root_verdict(&root), Ok(()));
    }

    #[test]
    fn a_plain_directory_is_refused() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = AllowAnyRoot(std::env::var(ALLOW_ANY_ROOT_ENV).ok());
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::remove_var(ALLOW_ANY_ROOT_ENV) };
        crate::store::init_isolated_cache();
        let dir = plain_dir();
        let root = dir.path().canonicalize().expect("canonicalize");
        assert_eq!(workspace_root_verdict(&root), Err(RootRefusal::NotAProject));
    }

    #[test]
    fn allow_any_root_env_permits_a_plain_directory_but_never_the_filesystem_root() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = AllowAnyRoot::set("1");
        let dir = plain_dir();
        let root = dir.path().canonicalize().expect("canonicalize");
        assert_eq!(workspace_root_verdict(&root), Ok(()));
        assert_eq!(
            workspace_root_verdict(Path::new("/")),
            Err(RootRefusal::FilesystemRoot),
            "the filesystem-root refusal is not overridable"
        );
    }

    #[test]
    fn allow_any_root_env_accepts_true_and_yes_but_not_arbitrary_values() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for truthy in ["1", "true", "TRUE", "yes"] {
            let _env = AllowAnyRoot::set(truthy);
            assert!(allow_any_root(), "{truthy:?} is truthy");
        }
        for falsy in ["0", "false", "no", ""] {
            let _env = AllowAnyRoot::set(falsy);
            assert!(!allow_any_root(), "{falsy:?} is not truthy");
        }
    }

    #[test]
    fn an_existing_working_index_grandfathers_a_plain_directory() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = AllowAnyRoot(std::env::var(ALLOW_ANY_ROOT_ENV).ok());
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::remove_var(ALLOW_ANY_ROOT_ENV) };
        crate::store::init_isolated_cache();
        let dir = plain_dir();
        let root = dir.path().canonicalize().expect("canonicalize");
        assert_eq!(
            workspace_root_verdict(&root),
            Err(RootRefusal::NotAProject),
            "no index yet"
        );

        let view_dir = crate::store::workspace_cache_dir(&root)
            .join(crate::store::VIEWS_DIR)
            .join(crate::store::VIEW_WORKING);
        std::fs::create_dir_all(&view_dir).expect("mkdir view");
        std::fs::write(view_dir.join(crate::store::INDEX_FILE), b"").expect("write index");
        assert_eq!(workspace_root_verdict(&root), Ok(()));
    }

    #[test]
    fn the_refusal_message_names_both_escape_hatches() {
        let message = refusal_message(Path::new("/home/dev/notes"), RootRefusal::NotAProject);
        assert!(message.contains("/home/dev/notes"));
        assert!(message.contains("basemind init"));
        assert!(message.contains(ALLOW_ANY_ROOT_ENV));
        assert!(message.contains("indexes every file beneath it"));

        let fs_root = refusal_message(Path::new("/"), RootRefusal::FilesystemRoot);
        assert!(fs_root.contains("no override"), "{fs_root}");
    }
}
