//! Auto-approving basemind's own MCP tools in Claude Code's `permissions.allow`.
//!
//! `basemind init` can optionally add one glob entry to `.claude/settings.local.json` (recommended)
//! or the committed `.claude/settings.json` so Claude Code stops prompting for approval on
//! basemind's own read-only MCP tools. This is opt-in, not default-on: broadening auto-approved
//! permissions is a decision the user should make explicitly, so a non-interactive run skips it
//! unless `--settings-target` is passed.
//!
//! The merge is deliberately narrow: it reads the existing JSON (if any), touches only
//! `permissions.allow` (creating the path if absent, appending the entry once, never duplicating
//! it), and leaves every other top-level key — including its position in the file — untouched.
//! `serde_json`'s `preserve_order` feature (enabled on the crate in `Cargo.toml`) is load-bearing
//! here: without it, `serde_json::Map` is `BTreeMap`-backed and would alphabetically re-sort a
//! hand-ordered settings file on every write.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde_json::{Map, Value};

use super::init::{Change, InitArgs};

/// Permission glob covering every basemind MCP tool exposed through the Claude Code plugin.
/// Claude Code names plugin MCP tools `mcp__plugin_<pluginName>_<serverName>__<toolName>`; both
/// halves are `"basemind"` here per `.claude-plugin/plugin.json` (`"name": "basemind"`,
/// `mcpServers.basemind`), confirmed against this plugin's own live tool names
/// (`mcp__plugin_basemind_basemind__code`, `__git`, `__graph`, …).
const PERMISSION_ENTRY: &str = "mcp__plugin_basemind_basemind__*";

/// Where (if anywhere) to add basemind's MCP-tool permission entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SettingsTarget {
    /// Personal, gitignored `.claude/settings.local.json` (recommended).
    Local,
    /// Committed, shared `.claude/settings.json`.
    Shared,
    /// Do not touch either settings file.
    None,
}

/// Resolved settings-permissions plan, mirroring [`super::init::RulesPlan`]'s shape.
pub(crate) enum SettingsPlan {
    /// Leave both settings files alone.
    Skip,
    /// Merge the basemind permission entry into this file (creating it if absent).
    Write(PathBuf),
}

/// Decide the effective settings target. Precedence: an explicit `--settings-target` wins
/// verbatim; otherwise, in an interactive TTY, prompt (default: `Local`); otherwise (`--yes` /
/// piped, no explicit flag) skip — this step only ever runs unattended when asked for by name.
pub(crate) fn resolve_settings_target(args: &InitArgs) -> Result<SettingsTarget> {
    if let Some(target) = args.settings_target {
        return Ok(target);
    }
    if !args.yes && std::io::stdin().is_terminal() {
        return prompt_settings_target();
    }
    Ok(SettingsTarget::None)
}

/// Interactive prompt for the settings-permissions step. A blank answer accepts the recommended
/// default: the personal, gitignored `settings.local.json`.
fn prompt_settings_target() -> Result<SettingsTarget> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    println!("Auto-approve basemind's MCP tools in Claude Code?");
    println!("  1) .claude/settings.local.json — personal, gitignored (recommended)");
    println!("  2) .claude/settings.json       — committed, shared with everyone on the repo");
    println!("  3) none                        — skip this step");
    write!(stdout, "Choose [1-3, blank = 1]: ").context("write prompt")?;
    stdout.flush().context("flush prompt")?;
    let mut line = String::new();
    stdin.read_line(&mut line).context("read stdin")?;
    Ok(match line.trim() {
        "" | "1" => SettingsTarget::Local,
        "2" => SettingsTarget::Shared,
        "3" => SettingsTarget::None,
        // ~keep An unrecognized answer falls back to the safe, gitignored recommendation rather
        // ~keep than guessing the committed file.
        _ => SettingsTarget::Local,
    })
}

/// Resolve `target` into a concrete file plan under `root`.
pub(crate) fn resolve_settings_plan(root: &Path, target: SettingsTarget) -> SettingsPlan {
    match target {
        SettingsTarget::None => SettingsPlan::Skip,
        SettingsTarget::Local => SettingsPlan::Write(root.join(".claude").join("settings.local.json")),
        SettingsTarget::Shared => SettingsPlan::Write(root.join(".claude").join("settings.json")),
    }
}

/// Plan the settings write from an already-resolved `plan`. Always returns `Some` (mirroring
/// [`super::init::plan_rules_change`]'s shape) so `run()` can push it into the same `changes` vec
/// unconditionally.
pub(crate) fn plan_settings_change(plan: &SettingsPlan) -> Result<Option<Change>> {
    let path = match plan {
        SettingsPlan::Skip => {
            return Ok(Some(Change::NoOp {
                note: "settings: skipped (no --settings-target)".to_string(),
            }));
        }
        SettingsPlan::Write(path) => path,
    };
    let existing = match std::fs::read_to_string(path) {
        Ok(c) => Some(c),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow::Error::new(e).context(format!("read {}", path.display()))),
    };
    let (contents, changed) = merged_settings_contents(existing.as_deref())
        .with_context(|| format!("merge basemind permissions into {}", path.display()))?;
    if !changed {
        return Ok(Some(Change::NoOp {
            note: format!("settings: basemind permission already present ({})", path.display()),
        }));
    }
    Ok(Some(Change::Write {
        path: path.clone(),
        note: "added basemind's MCP tools to permissions.allow",
        contents,
    }))
}

/// Merge [`PERMISSION_ENTRY`] into `existing`'s `permissions.allow` array, returning the
/// pretty-printed (2-space indent) result and whether anything actually changed. Every other
/// top-level key — and its position — is preserved verbatim; only `permissions` (and, under it,
/// `allow`) is created or touched. Errors rather than clobbers when an existing `permissions` or
/// `permissions.allow` value is not the shape basemind expects (e.g. a string instead of an array),
/// since silently overwriting it would lose whatever the user already had there.
fn merged_settings_contents(existing: Option<&str>) -> Result<(String, bool)> {
    let mut root: Value = match existing {
        Some(s) if !s.trim().is_empty() => serde_json::from_str(s).context("parse existing settings JSON")?,
        _ => Value::Object(Map::new()),
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings file root is not a JSON object"))?;

    let permissions = obj.entry("permissions").or_insert_with(|| Value::Object(Map::new()));
    let permissions_obj = permissions
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("existing `permissions` is not a JSON object"))?;

    let allow = permissions_obj.entry("allow").or_insert_with(|| Value::Array(Vec::new()));
    let allow_arr = allow
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("existing `permissions.allow` is not a JSON array"))?;

    let already_present = allow_arr.iter().any(|v| v.as_str() == Some(PERMISSION_ENTRY));
    if !already_present {
        allow_arr.push(Value::String(PERMISSION_ENTRY.to_string()));
    }

    let mut rendered = serde_json::to_string_pretty(&root).context("render settings JSON")?;
    rendered.push('\n');
    Ok((rendered, !already_present))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_settings_plan_maps_targets_to_their_paths() {
        let root = Path::new("/repo");
        match resolve_settings_plan(root, SettingsTarget::Local) {
            SettingsPlan::Write(path) => assert_eq!(path, root.join(".claude").join("settings.local.json")),
            SettingsPlan::Skip => panic!("Local must not skip"),
        }
        match resolve_settings_plan(root, SettingsTarget::Shared) {
            SettingsPlan::Write(path) => assert_eq!(path, root.join(".claude").join("settings.json")),
            SettingsPlan::Skip => panic!("Shared must not skip"),
        }
        assert!(matches!(resolve_settings_plan(root, SettingsTarget::None), SettingsPlan::Skip));
    }

    #[test]
    fn merge_creates_permissions_allow_from_nothing() {
        let (contents, changed) = merged_settings_contents(None).expect("merge into empty file");
        assert!(changed);
        let parsed: Value = serde_json::from_str(&contents).expect("valid JSON");
        assert_eq!(
            parsed["permissions"]["allow"],
            Value::Array(vec![Value::String(PERMISSION_ENTRY.to_string())])
        );
    }

    #[test]
    fn merge_appends_without_touching_unrelated_top_level_keys() {
        // ~keep `skillOverrides` (this repo's own `.claude/settings.json` carries exactly this key,
        // ~keep per CLAUDE.md's "NEVER remove `init: off`") must survive untouched, in place.
        let existing = r#"{
  "skillOverrides": {
    "init": "off"
  },
  "otherKey": [1, 2, 3]
}"#;
        let (contents, changed) = merged_settings_contents(Some(existing)).expect("merge");
        assert!(changed);

        let parsed: Value = serde_json::from_str(&contents).expect("valid JSON");
        assert_eq!(parsed["skillOverrides"]["init"], "off", "unrelated key preserved");
        assert_eq!(parsed["otherKey"], Value::Array(vec![1.into(), 2.into(), 3.into()]));
        assert_eq!(
            parsed["permissions"]["allow"],
            Value::Array(vec![Value::String(PERMISSION_ENTRY.to_string())])
        );

        // ~keep Order, not just presence: `skillOverrides` was first in the source file and must
        // ~keep still be first in the output — preserve_order is what makes this true; a plain
        // ~keep BTreeMap-backed `serde_json::Map` would alphabetize `otherKey` before `permissions`
        // ~keep before `skillOverrides` and silently reorder the user's hand-authored file.
        let skill_pos = contents.find("skillOverrides").expect("skillOverrides present");
        let other_pos = contents.find("otherKey").expect("otherKey present");
        let permissions_pos = contents.find("\"permissions\"").expect("permissions present");
        assert!(skill_pos < other_pos, "skillOverrides must stay before otherKey");
        assert!(other_pos < permissions_pos, "the newly-added permissions key must land last");
    }

    #[test]
    fn merge_is_idempotent_when_the_entry_is_already_present() {
        let existing = format!(
            r#"{{
  "permissions": {{
    "allow": ["{PERMISSION_ENTRY}", "some/other/tool"]
  }}
}}"#
        );
        let (_, changed) = merged_settings_contents(Some(&existing)).expect("merge");
        assert!(!changed, "already present — no rewrite needed");
    }

    #[test]
    fn merge_dedupes_rather_than_appending_a_second_copy() {
        let existing = format!(r#"{{"permissions": {{"allow": ["{PERMISSION_ENTRY}"]}}}}"#);
        let (contents, changed) = merged_settings_contents(Some(&existing)).expect("merge");
        assert!(!changed);
        let parsed: Value = serde_json::from_str(&contents).expect("valid JSON");
        let allow = parsed["permissions"]["allow"].as_array().expect("array");
        assert_eq!(allow.len(), 1, "must not duplicate the entry");
    }

    #[test]
    fn plan_settings_change_skip_is_a_noop() {
        let change = plan_settings_change(&SettingsPlan::Skip).expect("plan");
        assert!(matches!(change, Some(Change::NoOp { .. })));
    }

    #[test]
    fn plan_settings_change_writes_a_fresh_local_settings_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(".claude").join("settings.local.json");
        let change = plan_settings_change(&SettingsPlan::Write(path.clone()))
            .expect("plan")
            .expect("a change");
        match change {
            Change::Write { path: written, contents, .. } => {
                assert_eq!(written, path);
                assert!(contents.contains(PERMISSION_ENTRY));
            }
            Change::NoOp { note } => panic!("expected a write, got no-op: {note}"),
        }
    }
}
