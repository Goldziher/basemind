//! `shell:exec` — run a shell command in the repository root.
//!
//! Gated by an exec permission claim on the command string. Output (stdout then stderr) is returned
//! to the model; a non-zero exit is reported as a tool error, not a turn abort.

use std::path::Path;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Tool, ToolCtx, ToolOutput};
use crate::error::{AgentError, Result};
use crate::permission::PermissionClaim;

/// `shell:exec` tool.
pub struct ShellTool;

/// Arguments for [`ShellTool`].
#[derive(Deserialize, JsonSchema)]
pub struct ShellArgs {
    /// The shell command to run (executed via `sh -c`).
    pub command: String,
}

#[async_trait]
impl Tool for ShellTool {
    type Args = ShellArgs;

    fn name(&self) -> &'static str {
        "shell:exec"
    }

    fn description(&self) -> &'static str {
        "Run a shell command in the repository root via `sh -c`. Returns combined stdout and stderr."
    }

    fn permission(&self, args: &ShellArgs) -> PermissionClaim {
        PermissionClaim::exec(args.command.clone())
    }

    async fn execute(&self, args: ShellArgs, ctx: &ToolCtx) -> Result<ToolOutput> {
        run_command(&args.command, &ctx.root).await
    }
}

/// Run `command` in `root`, returning combined stdout+stderr. Server-free so it is directly
/// testable. A non-zero exit sets `is_error` but still returns the output for the model to read.
async fn run_command(command: &str, root: &Path) -> Result<ToolOutput> {
    // `kill_on_drop` so that when the turn-loop drops this future to cancel the tool, the child is
    // actually killed rather than orphaned. (A `sh -c` pipeline's grandchildren can still outlive
    // the killed `sh`; a process group would reap those, deferred as a later refinement.)
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(AgentError::Io)?;

    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    Ok(ToolOutput {
        text,
        is_error: !output.status.success(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_requires_an_exec_claim() {
        let claim = ShellTool.permission(&ShellArgs {
            command: "ls -a".into(),
        });
        assert_eq!(claim, PermissionClaim::exec("ls -a"));
    }

    #[tokio::test]
    async fn run_command_captures_stdout() {
        let out = run_command("echo hello", Path::new(".")).await.expect("runs");
        assert_eq!(out.text.trim(), "hello");
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn run_command_flags_nonzero_exit() {
        let out = run_command("exit 3", Path::new(".")).await.expect("runs");
        assert!(out.is_error);
    }
}
