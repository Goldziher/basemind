//! The two tools only `basemind serve --lead` lists (ADR-0040), apart from the verb tools: their
//! own committed snapshot and byte ceiling, the description contract, and argument refusals for
//! every field a model must not be able to set.

use basemind::mcp::lead_tools::{self, AGENT_LAUNCH, AGENT_STOP, LEAD_TOOLS};
use basemind::mcp::params::{RETIRED_TOOLS, VERB_TOOLS};
use serde_json::{Value, json};

/// The committed `tools/list` entries of the lead tools.
const LEAD_TOOLS_SNAPSHOT: &str = "tests/fixtures/lead_tools_list.json";
/// What the lead tools may cost every lead session, serialized.
const LEAD_TOOLS_BYTE_CEILING: usize = 4_000;

fn snapshot() -> String {
    let mut text = serde_json::to_string_pretty(&lead_tools::tool_definitions()).expect("serialize");
    text.push('\n');
    text
}

#[test]
fn lead_tools_list_matches_the_committed_snapshot() {
    let served = snapshot();
    let committed = std::fs::read_to_string(LEAD_TOOLS_SNAPSHOT)
        .expect("read the lead tools snapshot")
        .replace("\r\n", "\n");
    assert!(
        served == committed,
        "the lead tools differ from {LEAD_TOOLS_SNAPSHOT}. If intended, run `cargo test --test \
         lead_tools_surface -- --ignored regenerate_lead_tools_list` and review the diff.\nserved:\n{served}"
    );
}

/// Rewrite the committed snapshot. Ignored so it only changes when asked to.
#[test]
#[ignore]
fn regenerate_lead_tools_list() {
    std::fs::write(LEAD_TOOLS_SNAPSHOT, snapshot()).expect("write the snapshot");
}

#[test]
fn the_lead_tools_stay_under_their_own_ceiling_and_off_the_verb_surface() {
    let definitions = lead_tools::tool_definitions();
    let bytes = serde_json::to_string(&definitions).expect("serialize").len();
    assert!(bytes <= LEAD_TOOLS_BYTE_CEILING, "{bytes} bytes");
    let names: Vec<&str> = definitions.iter().filter_map(|tool| tool["name"].as_str()).collect();
    assert_eq!(names, LEAD_TOOLS);
    for name in LEAD_TOOLS {
        assert!(!VERB_TOOLS.contains(name), "{name} is a verb tool");
        assert!(!RETIRED_TOOLS.contains(name), "{name}");
    }
}

#[test]
fn each_description_opens_with_use_and_ends_with_one_example_and_the_annotations_are_exact() {
    for tool in lead_tools::tool_definitions() {
        let name = tool["name"].as_str().unwrap();
        let description = tool["description"].as_str().unwrap();
        assert!(description.starts_with("Use "), "{name}");
        assert_eq!(description.matches("Example: {").count(), 1, "{name}");
        let example = description.rsplit("Example: ").next().unwrap();
        let example: Value = serde_json::from_str(example).expect("the example is JSON");
        let parsed = match name {
            AGENT_LAUNCH => lead_tools::parse_launch(example).map(drop),
            _ => lead_tools::parse_stop(example).map(drop),
        };
        assert!(parsed.is_ok(), "{name}: the example is refused: {parsed:?}");
        assert_eq!(tool["annotations"]["readOnlyHint"], false, "{name} writes");
        assert!(tool.get("_meta").is_none(), "{name} is not always-loaded");
        let schema = tool["inputSchema"].to_string();
        for construct in ["$ref", "$defs", "anyOf", "oneOf", "allOf"] {
            assert!(!schema.contains(construct), "{name}: {construct} in {schema}");
        }
    }
    let launch = &lead_tools::tool_definitions()[0];
    assert_eq!(
        launch["annotations"]["idempotentHint"], false,
        "a launch is not idempotent"
    );
    assert_eq!(launch["annotations"]["openWorldHint"], true);
}

/// No field a model sends can name what only the operator's file may: an executable, a permission
/// mode or opt-in, an environment value, a principal, an approval answer.
#[test]
fn a_launch_refuses_every_field_the_operator_owns() {
    let base = json!({ "harness": "claude", "brief": "Summarize", "working_dir": "w" });
    assert!(lead_tools::parse_launch(base.clone()).is_ok());
    for field in [
        "executable",
        "permission_mode",
        "bypass",
        "unverified_mode",
        "approval_policy",
        "launcher_may_allow",
        "env",
        "principal",
        "allow",
        "mcp_command",
    ] {
        let mut call = base.clone();
        call[field] = json!("x");
        let refused = lead_tools::parse_launch(call).expect_err(field);
        assert!(refused.contains(field), "{field}: {refused}");
    }
    let missing = lead_tools::parse_launch(json!({ "harness": "claude", "brief": "b" })).expect_err("working_dir");
    assert!(missing.contains("working_dir"), "{missing}");
    assert!(lead_tools::parse_stop(json!({ "session": 3 })).is_ok());
    let refused = lead_tools::parse_stop(json!({ "session": 3, "force": true })).expect_err("force");
    assert!(refused.contains("force"), "{refused}");
    assert_eq!(AGENT_STOP, LEAD_TOOLS[1]);
}
