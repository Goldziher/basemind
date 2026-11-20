//! Where the operator's agent-launch policy lives, and the rule that it lives nowhere else.
//!
//! Launching agents (`basemind serve --lead`, ADR-0040) is configured only in one user-level file,
//! [`policy_path`], which the operator owns. A repository's `basemind.toml` is written by whoever
//! wrote the repository, and a prompt-injected agent with an edit tool can write it too: an
//! `[agents]` table there is dropped with a warning before the file is validated, so it can neither
//! turn launching on nor break the rest of the file. No environment variable, CLI flag or MCP
//! override names the table or the file's path: [`ConfigV1`](super::ConfigV1) has no field for it.

use std::path::PathBuf;

/// The policy file's name inside basemind's user-level configuration directory.
pub const AGENTS_POLICY_FILE: &str = "agents.toml";

/// The top-level table that holds the policy (`[agents.launch]`).
pub const AGENTS_TABLE: &str = "agents";

/// basemind's user-level configuration directory under the account's home directory
/// (`~/.config/basemind` on Linux and the other Unixes, `~/Library/Application Support/basemind` on
/// macOS). The home directory is the user database's entry for this process's user, never `HOME` or
/// `XDG_CONFIG_HOME`: an MCP host that sets its servers' environment from a project's settings would
/// otherwise choose the directory (ADR-0044). `None` without such an entry.
#[cfg(unix)]
pub fn user_config_dir() -> Option<PathBuf> {
    #[cfg(feature = "test-support")]
    if let Some(home) = std::env::var_os(TEST_ACCOUNT_HOME_ENV) {
        return Some(user_config_dir_in(std::path::Path::new(&home)));
    }
    account_home().map(|home| user_config_dir_in(&home))
}

/// The home directory [`user_config_dir`] uses in place of the account's, for tests that run the
/// binary as a lead. Builds without the `test-support` feature carry neither the variable nor the
/// branch, so a shipped binary takes the directory from the user database only.
#[cfg(all(unix, feature = "test-support"))]
pub const TEST_ACCOUNT_HOME_ENV: &str = "BASEMIND_TEST_ACCOUNT_HOME";

/// Without a user database, the platform configuration directory.
#[cfg(not(unix))]
pub fn user_config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "basemind").map(|dirs| dirs.config_dir().to_path_buf())
}

/// The operator's agent-launch policy file: `agents.toml` in [`user_config_dir`].
pub fn policy_path() -> Option<PathBuf> {
    user_config_dir().map(|dir| dir.join(AGENTS_POLICY_FILE))
}

/// basemind's configuration directory under the home directory `home`.
#[cfg(unix)]
fn user_config_dir_in(home: &std::path::Path) -> PathBuf {
    let dir = if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support")
    } else {
        home.join(".config")
    };
    dir.join("basemind")
}

/// The home directory of this process's effective user, from the user database.
#[cfg(unix)]
fn account_home() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut buffer = vec![0_u8; 4096];
    loop {
        // SAFETY: passwd is plain data that getpwuid_r fills in; all-zero is a valid value of it.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: the buffer outlives the call and the reads of `entry` below; its length is
        // passed with it.
        let status = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                &mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        };
        if status == libc::ERANGE && buffer.len() < 1 << 20 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if status != 0 || found.is_null() || entry.pw_dir.is_null() {
            return None;
        }
        // SAFETY: on success `pw_dir` points at a NUL-terminated string inside `buffer`.
        let dir = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) };
        let dir = PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes()));
        return dir.is_absolute().then_some(dir);
    }
}

/// Drop the `[agents]` table from a repository config, warning once with the file it belongs in.
pub(super) fn strip_repo_agents(config: &mut serde_json::Value) {
    let Some(table) = config.as_object_mut() else {
        return;
    };
    if table.remove(AGENTS_TABLE).is_none() {
        return;
    }
    let file = policy_path().map_or_else(|| AGENTS_POLICY_FILE.to_owned(), |path| path.display().to_string());
    super::trust::warn_once(format!(
        "ignoring [{AGENTS_TABLE}] in basemind.toml: agent launching is configured only in the operator's \
         own {file}"
    ));
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Set in a rerun of [`the_policy_path_ignores_the_environment`]: where it writes what it saw.
    const OUT: &str = "BASEMIND_TEST_POLICY_PATH_OUT";

    #[test]
    fn the_policy_file_lies_in_basemind_s_directory_under_the_home() {
        let expected = if cfg!(target_os = "macos") {
            "/home/u/Library/Application Support/basemind/agents.toml"
        } else {
            "/home/u/.config/basemind/agents.toml"
        };
        assert_eq!(
            user_config_dir_in(std::path::Path::new("/home/u")).join(AGENTS_POLICY_FILE),
            PathBuf::from(expected)
        );
    }

    /// Review 3 N2: `HOME` and `XDG_CONFIG_HOME` name no other file. The test reruns itself with
    /// both pointed elsewhere and compares the path the rerun resolved with its own.
    #[test]
    fn the_policy_path_ignores_the_environment() {
        let here = policy_path().expect("this user has a home directory");
        if let Some(out) = std::env::var_os(OUT) {
            std::fs::write(out, here.as_os_str().as_encoded_bytes()).unwrap();
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("seen");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "config::agents_policy::tests::the_policy_path_ignores_the_environment",
                "--test-threads=1",
            ])
            .env(OUT, &out)
            .env_remove("BASEMIND_TEST_ACCOUNT_HOME")
            .env("HOME", dir.path())
            .env("XDG_CONFIG_HOME", dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        // A rerun that matched no test writes nothing, and fails here.
        let seen = std::fs::read(&out).expect("the rerun ran the test");
        assert_eq!(seen, here.as_os_str().as_encoded_bytes());
        assert!(!here.starts_with(dir.path()));
    }
}
