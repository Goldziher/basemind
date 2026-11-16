//! Helpers shared by the adapter tests.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::event::ApprovalRequest;
use crate::harness::{Harness, HarnessKind};
use crate::identity::AgentIdentity;
use crate::spec::{ApprovalPolicy, ApprovalVerdict, LaunchSpec};

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

/// A default spec for `kind` rooted at `root`.
pub fn spec(kind: HarnessKind, root: &Path) -> LaunchSpec {
    LaunchSpec::new(
        Harness::new(kind),
        root,
        "Summarize the README.",
        AgentIdentity::new("launched-agent").unwrap(),
    )
}

/// The value following `flag` in an argument list.
pub fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let at = args.iter().position(|arg| arg == flag)?;
    args.get(at + 1).map(String::as_str)
}
