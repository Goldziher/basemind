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
//! pool, HTTP `?root=`, CLI scan verbs, in-process rescan), never inside discovery: a root must
//! look like a project — a git repository, or a directory carrying `basemind.toml`. This is a
//! deliberate breaking change for anyone indexing a plain directory, softened by two escape hatches
//! ([`basemind init`] writes the marker; [`ALLOW_ANY_ROOT_ENV`] skips the check for that
//! invocation) and a grandfather clause for roots that already had a working-view index before the
//! guard existed.
//!
//! This is a MISCONFIGURATION guard, not an authorization boundary. It stops basemind pointing a
//! whole-filesystem walk at a directory nobody meant to index; it does not defend against a caller
//! who controls the process's environment or arguments.
//!
//! # Why the verdict returns a path
//!
//! [`workspace_root_verdict`] canonicalizes before it decides and hands the *resolved* path back.
//! A syntactic check cannot see through `ParentDir` or symlinks — `std::path` normalizes neither —
//! so `/..`, `/tmp/../..` and a symlink whose target is `/` all read as ordinary directories while
//! naming the filesystem root. Returning the resolved path (rather than `Ok(())`) is what stops a
//! caller checking one path and opening another: there is no un-resolved path left to pass on.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::CONFIG_FILE_NAME;

/// Escape hatch: set truthy to accept any directory as a workspace root **for that invocation**.
/// Mirrors the `BASEMIND_ALLOW_PRIVATE_HOSTS` style — an env-var opt-out for a safety default. It
/// does NOT override [`RootRefusal::FilesystemRoot`]; nothing does. Unsetting it re-refuses the
/// root: an index minted under the hatch does not grandfather itself in (see [`ROOT_ADMISSION_FILE`]).
pub const ALLOW_ANY_ROOT_ENV: &str = "BASEMIND_ALLOW_ANY_ROOT";

/// Sidecar in a root's workspace cache directory recording the guard's standing verdict for that
/// root, so the grandfather clause reads an explicit decision instead of inferring consent from a
/// side effect.
///
/// Without it, the mere existence of `views/working/index.msgpack` was proof of admission — which
/// made a single `BASEMIND_ALLOW_ANY_ROOT=1 basemind scan` a *permanent* whitelist for that root:
/// the scan it authorized left behind the very file the next, hatch-free run accepted as evidence.
pub const ROOT_ADMISSION_FILE: &str = "root-admission.json";

/// Why a candidate workspace root was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootRefusal {
    /// A directory that is neither a git repository nor marked with `basemind.toml`. Overridable.
    NotAProject,
    /// A filesystem or volume root (`/`, `C:\`, a UNC share root). Never overridable.
    FilesystemRoot,
    /// The path does not resolve to an existing filesystem object: it is relative, missing, or
    /// unreadable. The guard cannot decide about a path it cannot resolve, so it refuses.
    Unresolvable,
}

/// Why the guard admitted a root, persisted as [`ROOT_ADMISSION_FILE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionReason {
    /// The root already carried a working-view index the first time the guard saw it, so it
    /// predates the guard and keeps working. A standing admission: it survives without any env var.
    PreExistingIndex,
    /// The root was admitted solely because [`ALLOW_ANY_ROOT_ENV`] was set. Explicitly NOT a
    /// standing admission — the hatch must be set again on every invocation, and
    /// [`is_grandfathered`] refuses to treat the index this scan produces as evidence of consent.
    EnvHatch,
}

/// The persisted verdict. Deliberately tiny and best-effort, exactly like the neighbouring
/// `workspace.json` / `status.json` sidecars: a failed write only degrades the guard to its
/// pre-existing "an index means admitted" behaviour, and never fails a scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RootAdmission {
    reason: AdmissionReason,
    /// Unix-epoch seconds when the admission was recorded. Diagnostics only.
    admitted_unix: i64,
}

/// Decide whether `root` may be used as a workspace root, returning the **resolved** root to use.
///
/// The returned path is `root.canonicalize()`d — `..` segments collapsed and symlinks followed —
/// and callers MUST use it onward instead of the path they passed in. That is what makes the check
/// and the subsequent open refer to the same directory; checking a raw path and opening it is how
/// `/..` reached `Store::open` while the guard was looking at something it believed was a
/// subdirectory.
///
/// Evaluation order matters: the filesystem-root check runs FIRST (before and after resolution) so
/// no marker file, env var or pre-existing index can talk basemind into indexing an entire volume.
///
/// 1. `/`, `C:\`, a UNC share root — syntactically, then again after resolution →
///    [`RootRefusal::FilesystemRoot`], unconditionally.
/// 2. The path does not resolve → [`RootRefusal::Unresolvable`].
/// 3. `<root>/basemind.toml` exists → allowed. Reuses discovery's top-precedence marker, so the
///    documented durable escape hatch is simply `basemind init`.
/// 4. `root` is itself a git workdir → allowed.
/// 5. Grandfather: the root carries a standing admission (see [`ROOT_ADMISSION_FILE`]) → allowed.
///    Checked BEFORE the env hatch so a legitimately grandfathered root is never re-stamped as
///    hatch-admitted just because someone happened to have the variable set.
/// 6. [`ALLOW_ANY_ROOT_ENV`] is truthy → allowed for this invocation only, and recorded as such.
pub fn workspace_root_verdict(root: &Path) -> Result<PathBuf, RootRefusal> {
    // A volume root that cannot be canonicalized (an offline drive letter) still deserves the
    // specific refusal rather than the generic "does not resolve" one. ~keep
    if is_filesystem_root(root) {
        return Err(RootRefusal::FilesystemRoot);
    }
    let resolved = root.canonicalize().map_err(|_| RootRefusal::Unresolvable)?;
    if is_filesystem_root(&resolved) {
        return Err(RootRefusal::FilesystemRoot);
    }
    if resolved.join(CONFIG_FILE_NAME).is_file() || is_git_workdir(&resolved) || is_grandfathered(&resolved) {
        return Ok(resolved);
    }
    if allow_any_root() {
        record_admission(&resolved, AdmissionReason::EnvHatch);
        return Ok(resolved);
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
        RootRefusal::Unresolvable => format!(
            "refusing to use {root} as a basemind workspace root: it does not resolve to an existing \
             path.\n\
             \n\
             basemind resolves a root — collapsing `..` and following symlinks — before deciding \
             anything about it, so the path it checks is exactly the path it opens. A relative path \
             would resolve against whatever working directory this process happens to have, and a \
             missing one cannot be checked at all.\n\
             \n\
             Pass an absolute path to an existing project directory: `--root <path>`."
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
             \x20 - set {ALLOW_ANY_ROOT_ENV}=1 in the environment. That hatch applies to the \
             invocation that sets it and nothing else: the index it produces does not make the root \
             permanently acceptable, so unset it and this root is refused again."
        ),
    }
}

/// `/`, `C:\`, `\\server\share\` — a path made of nothing but a prefix and/or the root separator.
/// The `parent().is_none()` arm also catches the empty path, which is not a usable root either.
///
/// Purely syntactic, and that is only sound because [`workspace_root_verdict`] applies it to a
/// canonicalized path: `/..` has a parent and a `Normal`-free-but-not-empty component list, so an
/// unresolved path sails straight past this.
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

/// A root that carried a working-view index BEFORE the guard existed predates it and keeps working.
///
/// The evidence is the recorded verdict, not the index file. The first hatch-free sighting of an
/// unrecorded index stamps [`AdmissionReason::PreExistingIndex`] — that is the upgrade path — while
/// a root stamped [`AdmissionReason::EnvHatch`] is never grandfathered, because its index exists
/// only because the hatch authorized the scan that wrote it. Inferring consent from that file was a
/// permanent whitelist minted by one env var.
fn is_grandfathered(root: &Path) -> bool {
    let dir = crate::store::workspace_cache_dir(root);
    match read_admission(&dir).map(|record| record.reason) {
        Some(AdmissionReason::PreExistingIndex) => true,
        Some(AdmissionReason::EnvHatch) => false,
        None => {
            let has_index = dir
                .join(crate::store::VIEWS_DIR)
                .join(crate::store::VIEW_WORKING)
                .join(crate::store::INDEX_FILE)
                .exists();
            if has_index {
                write_admission(&dir, AdmissionReason::PreExistingIndex);
            }
            has_index
        }
    }
}

/// Record why `root` was admitted, if nothing is recorded yet. Never overwrites: the first verdict
/// is the standing one, so a hatch-set environment cannot downgrade a grandfathered root — and a
/// root grandfathered later cannot upgrade a hatch admission either, since
/// [`is_grandfathered`] short-circuits to `false` on an `EnvHatch` record.
fn record_admission(root: &Path, reason: AdmissionReason) {
    let dir = crate::store::workspace_cache_dir(root);
    if read_admission(&dir).is_some() {
        return;
    }
    write_admission(&dir, reason);
}

fn admission_path(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join(ROOT_ADMISSION_FILE)
}

fn read_admission(workspace_dir: &Path) -> Option<RootAdmission> {
    let bytes = std::fs::read(admission_path(workspace_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Best-effort, mirroring `ensure_workspace_marker`: an I/O failure leaves the root unrecorded,
/// which only means the next run re-derives the same verdict from the same inputs.
fn write_admission(workspace_dir: &Path, reason: AdmissionReason) {
    let record = RootAdmission {
        reason,
        admitted_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };
    let Ok(bytes) = serde_json::to_vec(&record) else {
        return;
    };
    if std::fs::create_dir_all(workspace_dir).is_err() {
        return;
    }
    let _ = crate::store_blob::write_bytes_atomic(admission_path(workspace_dir), &bytes);
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

        fn cleared() -> Self {
            let prior = std::env::var(ALLOW_ANY_ROOT_ENV).ok();
            // SAFETY: as above.
            unsafe { std::env::remove_var(ALLOW_ANY_ROOT_ENV) };
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

    fn git_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = plain_dir();
        let root = dir.path().canonicalize().expect("canonicalize");
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&root)
            .status()
            .expect("run git init");
        assert!(status.success(), "git init failed");
        (dir, root)
    }

    fn write_working_index(root: &Path) {
        let view_dir = crate::store::workspace_cache_dir(root)
            .join(crate::store::VIEWS_DIR)
            .join(crate::store::VIEW_WORKING);
        std::fs::create_dir_all(&view_dir).expect("mkdir view");
        std::fs::write(view_dir.join(crate::store::INDEX_FILE), b"").expect("write index");
    }

    #[test]
    fn filesystem_root_is_refused() {
        assert_eq!(workspace_root_verdict(Path::new("/")), Err(RootRefusal::FilesystemRoot));
    }

    /// The defect this signature change exists to close: `std::path` normalizes neither `..` nor
    /// symlinks, so every one of these names `/` while looking like an ordinary subdirectory to a
    /// syntactic check. The env hatch is SET throughout — it must not unlock any of them.
    #[test]
    fn paths_that_resolve_to_the_filesystem_root_are_refused_even_under_the_env_hatch() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = AllowAnyRoot::set("1");
        crate::store::init_isolated_cache();

        for spelling in ["/..", "/../..", "/usr/..", "/tmp/../.."] {
            assert_eq!(
                workspace_root_verdict(Path::new(spelling)),
                Err(RootRefusal::FilesystemRoot),
                "{spelling} resolves to the filesystem root"
            );
        }

        #[cfg(unix)]
        {
            let dir = plain_dir();
            let link = dir.path().join("slash");
            std::os::unix::fs::symlink("/", &link).expect("symlink to /");
            assert_eq!(
                workspace_root_verdict(&link),
                Err(RootRefusal::FilesystemRoot),
                "a symlink whose target is / is still the filesystem root"
            );
        }
    }

    /// The guard hands back the path it decided about, so no caller can check one path and open
    /// another. On macOS a tempdir also proves the symlink is followed (`/var` → `/private/var`).
    #[test]
    fn the_verdict_returns_the_resolved_root() {
        let (_dir, root) = git_repo();
        let unresolved = root.join("..").join(root.file_name().expect("leaf"));
        assert_eq!(
            workspace_root_verdict(&unresolved).expect("the repo is allowed"),
            root,
            "the `..` segment must be collapsed in the returned root"
        );
    }

    #[test]
    fn a_relative_or_missing_root_is_unresolvable() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = AllowAnyRoot::set("1");
        let dir = plain_dir();
        assert_eq!(
            workspace_root_verdict(&dir.path().join("no-such-child")),
            Err(RootRefusal::Unresolvable),
            "the env hatch cannot conjure a directory into existence"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_style_volume_and_unc_roots_are_refused() {
        for shape in [r"C:\", "C:/", r"\\?\C:\", r"\\server\share\", r"\\server\share"] {
            assert!(is_filesystem_root(Path::new(shape)), "{shape} is a volume/share root");
            assert_eq!(
                workspace_root_verdict(Path::new(shape)),
                Err(RootRefusal::FilesystemRoot),
                "{shape} must be refused"
            );
        }
        assert!(!is_filesystem_root(Path::new(r"C:\projects")));
        assert!(!is_filesystem_root(Path::new(r"\\server\share\projects")));
    }

    #[cfg(unix)]
    #[test]
    fn unix_root_shapes_are_classified() {
        // Windows spellings parse as `Normal` components on Unix, so only the Unix shapes can be
        // asserted here; the `#[cfg(windows)]` sibling covers `C:\` and UNC for real. ~keep
        assert!(is_filesystem_root(Path::new("/")));
        assert!(is_filesystem_root(Path::new("")));
        assert!(!is_filesystem_root(Path::new("/usr")));
        assert!(
            !is_filesystem_root(Path::new("/..")),
            "the syntactic predicate cannot see through `..` — that is why the verdict canonicalizes"
        );
    }

    #[test]
    fn a_git_repository_is_allowed() {
        let (_dir, root) = git_repo();
        assert_eq!(workspace_root_verdict(&root), Ok(root.clone()));
    }

    #[test]
    fn a_directory_with_basemind_toml_is_allowed() {
        let dir = plain_dir();
        let root = dir.path().canonicalize().expect("canonicalize");
        std::fs::write(root.join(CONFIG_FILE_NAME), "\"$schema\" = \"v1\"\n").expect("write marker");
        assert_eq!(workspace_root_verdict(&root), Ok(root.clone()));
    }

    #[test]
    fn a_plain_directory_is_refused() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = AllowAnyRoot::cleared();
        crate::store::init_isolated_cache();
        let dir = plain_dir();
        let root = dir.path().canonicalize().expect("canonicalize");
        assert_eq!(workspace_root_verdict(&root), Err(RootRefusal::NotAProject));
    }

    #[test]
    fn allow_any_root_env_permits_a_plain_directory_but_never_the_filesystem_root() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = AllowAnyRoot::set("1");
        crate::store::init_isolated_cache();
        let dir = plain_dir();
        let root = dir.path().canonicalize().expect("canonicalize");
        assert_eq!(workspace_root_verdict(&root), Ok(root.clone()));
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
        let _env = AllowAnyRoot::cleared();
        crate::store::init_isolated_cache();
        let dir = plain_dir();
        let root = dir.path().canonicalize().expect("canonicalize");
        assert_eq!(
            workspace_root_verdict(&root),
            Err(RootRefusal::NotAProject),
            "no index yet"
        );

        write_working_index(&root);
        assert_eq!(workspace_root_verdict(&root), Ok(root.clone()));
        assert_eq!(
            read_admission(&crate::store::workspace_cache_dir(&root)).map(|r| r.reason),
            Some(AdmissionReason::PreExistingIndex),
            "the upgrade path records an explicit standing verdict"
        );
    }

    /// The hatch is per-invocation. Using it once must not leave the root permanently acceptable —
    /// which is exactly what happened while `is_grandfathered` read the index file the hatched scan
    /// had just written.
    #[test]
    fn the_env_hatch_does_not_grandfather_the_index_it_produces() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::store::init_isolated_cache();
        let dir = plain_dir();
        let root = dir.path().canonicalize().expect("canonicalize");

        {
            let _env = AllowAnyRoot::set("1");
            assert_eq!(workspace_root_verdict(&root), Ok(root.clone()));
        }
        // Stand in for the scan the hatch authorized.
        write_working_index(&root);

        let _env = AllowAnyRoot::cleared();
        assert_eq!(
            workspace_root_verdict(&root),
            Err(RootRefusal::NotAProject),
            "unsetting the hatch must re-refuse a root it only ever admitted"
        );
    }

    /// The converse: a root grandfathered on its own merit must not be demoted to a hatch
    /// admission just because the variable happened to be set on some later run.
    #[test]
    fn a_grandfathered_root_is_not_downgraded_by_a_later_hatched_run() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::store::init_isolated_cache();
        let dir = plain_dir();
        let root = dir.path().canonicalize().expect("canonicalize");
        write_working_index(&root);

        {
            let _env = AllowAnyRoot::set("1");
            assert_eq!(workspace_root_verdict(&root), Ok(root.clone()));
        }
        let _env = AllowAnyRoot::cleared();
        assert_eq!(
            workspace_root_verdict(&root),
            Ok(root.clone()),
            "the pre-existing-index admission stands on its own"
        );
    }

    #[test]
    fn the_refusal_message_names_both_escape_hatches_and_the_hatchs_scope() {
        let message = refusal_message(Path::new("/home/dev/notes"), RootRefusal::NotAProject);
        assert!(message.contains("/home/dev/notes"));
        assert!(message.contains("basemind init"));
        assert!(message.contains(ALLOW_ANY_ROOT_ENV));
        assert!(message.contains("indexes every file beneath it"));
        assert!(
            message.contains("unset it and this root is refused again"),
            "the hatch's per-invocation scope must be stated, not implied: {message}"
        );

        let fs_root = refusal_message(Path::new("/"), RootRefusal::FilesystemRoot);
        assert!(fs_root.contains("no override"), "{fs_root}");

        let unresolvable = refusal_message(Path::new("relative/path"), RootRefusal::Unresolvable);
        assert!(unresolvable.contains("does not resolve"), "{unresolvable}");
    }
}
