//! `--help-json` (issue #332) against the built binary: each command describes its positionals and
//! flags from the definition it parses with, without its required arguments, and without touching
//! a workspace or the cache.

use std::process::Command;

use serde_json::{Value, json};

/// Run `basemind <args> --help-json` outside any project with an empty data home; assert it exits
/// 0, prints nothing on stderr and writes nothing to the data home, and return the JSON.
fn help(args: &[&str]) -> Value {
    let home = tempfile::tempdir().expect("data home");
    let comms = tempfile::tempdir().expect("comms dir");
    let cwd = tempfile::tempdir().expect("not a project");
    let out = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(args)
        .arg("--help-json")
        .current_dir(cwd.path())
        .env("BASEMIND_DATA_HOME", home.path())
        .env("BASEMIND_COMMS_DIR", comms.path())
        .output()
        .expect("run basemind");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{args:?}: {stderr}");
    assert_eq!(stderr, "", "{args:?}");
    let written: Vec<_> = std::fs::read_dir(home.path()).expect("data home").collect();
    assert!(written.is_empty(), "{args:?} wrote into the data home: {written:?}");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{args:?} is not JSON: {e}"))
}

fn flag<'a>(help: &'a Value, long: &str) -> &'a Value {
    help["flags"]
        .as_array()
        .and_then(|flags| flags.iter().find(|flag| flag["long"] == long))
        .unwrap_or_else(|| panic!("no {long} in {help}"))
}

/// A flag's shape, without its help text.
fn shape(flag: &Value) -> Value {
    let mut flag = flag.clone();
    let help = flag.as_object_mut().and_then(|map| map.remove("help"));
    assert!(help.is_some_and(|help| help.is_string()), "{flag}");
    flag
}

#[test]
fn scan_and_rescan_take_different_positionals() {
    let scan = help(&["scan"]);
    assert_eq!(scan["format"], 1);
    assert_eq!(scan["command"], "basemind scan");
    assert_eq!(scan["path"], json!(["scan"]));
    assert_eq!(scan["positional_count"], json!({ "min": 0, "max": 0 }));
    assert_eq!(scan["positionals"], json!([]));

    let rescan = help(&["rescan"]);
    assert_eq!(rescan["positional_count"], json!({ "min": 0, "max": null }));
    let paths = &rescan["positionals"][0];
    assert_eq!(
        (&paths["name"], &paths["value_name"], &paths["required"]),
        (&json!("paths"), &json!("PATH"), &json!(false))
    );
}

#[test]
fn a_flag_says_whether_it_takes_a_value() {
    let scan = help(&["scan"]);
    assert_eq!(
        shape(flag(&scan, "--verbose")),
        json!({
            "name": "verbose",
            "long": "--verbose",
            "short": "-v",
            "aliases": [],
            "takes_value": false,
            "value_name": null,
            "required": false,
            "repeatable": false,
            "global": true,
            "default": [],
            "possible_values": [],
            "env": null,
        })
    );
    assert_eq!(
        shape(flag(&scan, "--rev")),
        json!({
            "name": "rev",
            "long": "--rev",
            "short": null,
            "aliases": [],
            "takes_value": true,
            "value_name": "REV",
            "required": false,
            "repeatable": false,
            "global": false,
            "default": [],
            "possible_values": [],
            "env": null,
        })
    );
    assert_eq!(flag(&scan, "--root")["takes_value"], true);
    assert_eq!(flag(&scan, "--help-json")["takes_value"], false);
}

/// `code grep` requires a pattern; asking for its help without one still answers.
#[test]
fn a_command_describes_itself_without_its_required_arguments() {
    let grep = help(&["code", "grep"]);
    assert_eq!(grep["command"], "basemind code grep");
    assert_eq!(grep["positional_count"], json!({ "min": 1, "max": 1 }));
    assert_eq!(grep["positionals"][0]["name"], "pattern");
    assert_eq!(grep["positionals"][0]["required"], true);
    assert_eq!(flag(&grep, "--max-tokens")["takes_value"], true);
    assert_eq!(flag(&grep, "--regex")["takes_value"], false);
}

/// An alias addresses its command; a hidden command is not listed.
#[test]
fn aliases_and_hidden_commands_follow_help() {
    assert_eq!(help(&["code", "expand"])["command"], "basemind code read");
    let memory = help(&["memory"]);
    let listed: Vec<&str> = memory["subcommands"]
        .as_array()
        .expect("subcommands")
        .iter()
        .filter_map(|sub| sub["name"].as_str())
        .collect();
    assert!(listed.contains(&"put"), "{listed:?}");
    assert!(!listed.contains(&"documents"), "{listed:?}");
    assert_eq!(help(&["memory", "documents"])["command"], "basemind memory documents");
}
