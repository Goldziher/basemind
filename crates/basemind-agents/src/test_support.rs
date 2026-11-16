//! Helpers shared by the adapter tests.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::event::ApprovalRequest;
use crate::harness::{Harness, HarnessKind};
use crate::identity::AgentIdentity;
use crate::spec::{ApprovalPolicy, ApprovalVerdict, LaunchConfig, LaunchRequest, LaunchSpec};

/// The frames an adapter wrote, one JSON value per line.
pub fn written(out: &[u8]) -> Vec<Value> {
    out.split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("the adapter wrote a line that is not JSON"))
        .collect()
}

/// A policy that answers every request with `verdict`.
pub fn always(verdict: ApprovalVerdict) -> Option<Arc<dyn ApprovalPolicy>> {
    Some(Arc::new(move |_: &ApprovalRequest| verdict))
}

/// An executable that exists outside every test root. Plans name it; no test that only
/// plans runs it, so no real harness is ever started. Absolute on this platform: a rooted path
/// without a drive is refused on Windows, where it resolves against the current drive.
#[cfg(not(windows))]
pub const EXECUTABLE: &str = "/bin/sh";
#[cfg(windows)]
pub const EXECUTABLE: &str = r"C:\Windows\System32\cmd.exe";
/// A stand-in for the MCP server's command, under the same terms.
#[cfg(not(windows))]
pub const MCP_COMMAND: &str = "/bin/echo";
#[cfg(windows)]
pub const MCP_COMMAND: &str = r"C:\Windows\System32\where.exe";
/// Absolute directories outside every test root, for search paths.
#[cfg(not(windows))]
pub const BIN_DIRS: [&str; 2] = ["/usr/bin", "/bin"];
#[cfg(windows)]
pub const BIN_DIRS: [&str; 2] = [r"C:\Windows\System32", r"C:\Windows"];

/// The default config for `kind` rooted at `root`.
pub fn config(kind: HarnessKind, root: &Path) -> LaunchConfig {
    LaunchConfig::new(
        Harness::new(kind).with_executable(EXECUTABLE),
        root,
        AgentIdentity::new("launched-agent").unwrap(),
    )
}

/// A default spec for `kind` rooted at `root`.
pub fn spec(kind: HarnessKind, root: &Path) -> LaunchSpec {
    LaunchSpec::new(&config(kind, root), &LaunchRequest::new("Summarize the README.")).unwrap()
}

/// The value following `flag` in an argument list.
pub fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let at = args.iter().position(|arg| arg == flag)?;
    args.get(at + 1).map(String::as_str)
}
