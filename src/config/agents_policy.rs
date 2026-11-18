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

/// The operator's agent-launch policy file: `agents.toml` in basemind's platform configuration
/// directory (`$XDG_CONFIG_HOME/basemind` or `~/.config/basemind` on Linux,
/// `~/Library/Application Support/basemind` on macOS). `None` without a home directory.
pub fn policy_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "basemind").map(|dirs| dirs.config_dir().join(AGENTS_POLICY_FILE))
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
