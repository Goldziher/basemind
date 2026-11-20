//! `BASEMIND_OFFLINE` and `BASEMIND_GRAMMAR_OFFLINE` as `basemind lang install` reads them (#208).
//!
//! Each run gets an empty grammar cache and a grammar manifest URL nothing listens on, so a run
//! that tries to download fails at the connection instead of reaching the network, and the error
//! tells an offline refusal from a failed download.

use std::process::{Command, Output};

const OFFLINE_REFUSAL: &str = "offline mode: missing grammars";
const NOWHERE: &str = "http://127.0.0.1:1/manifest.json";

fn lang_install(vars: &[(&str, &str)]) -> Output {
    let home = tempfile::tempdir().expect("data home");
    let comms = tempfile::tempdir().expect("comms dir");
    let grammars = tempfile::tempdir().expect("grammar cache");
    let cwd = tempfile::tempdir().expect("not a project");
    let mut command = Command::new(env!("CARGO_BIN_EXE_basemind"));
    command
        .args(["lang", "install"])
        .current_dir(cwd.path())
        .env("BASEMIND_DATA_HOME", home.path())
        .env("BASEMIND_COMMS_DIR", comms.path())
        .env("TREE_SITTER_LANGUAGE_PACK_CACHE_DIR", grammars.path())
        .env("TREE_SITTER_LANGUAGE_PACK_MANIFEST_URL", NOWHERE)
        .env_remove("BASEMIND_OFFLINE")
        .env_remove("BASEMIND_GRAMMAR_OFFLINE");
    command.envs(vars.iter().copied());
    command.output().expect("run basemind lang install")
}

#[test]
fn an_offline_switch_refuses_the_grammar_download_and_names_both_variables() {
    for vars in [
        [("BASEMIND_OFFLINE", "1")],
        [("BASEMIND_OFFLINE", "true")],
        [("BASEMIND_OFFLINE", "on")],
        [("BASEMIND_GRAMMAR_OFFLINE", "1")],
        [("BASEMIND_GRAMMAR_OFFLINE", "ON")],
        [("BASEMIND_GRAMMAR_OFFLINE", "yes")],
    ] {
        let output = lang_install(&vars);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{vars:?}: {stderr}");
        assert!(stderr.contains(OFFLINE_REFUSAL), "{vars:?}: {stderr}");
        assert!(
            stderr.contains("BASEMIND_OFFLINE or BASEMIND_GRAMMAR_OFFLINE is set"),
            "{vars:?}: {stderr}"
        );
    }
}

#[test]
fn a_false_offline_value_tries_the_download() {
    for vars in [
        [("BASEMIND_GRAMMAR_OFFLINE", "false")],
        [("BASEMIND_GRAMMAR_OFFLINE", "no")],
        [("BASEMIND_GRAMMAR_OFFLINE", "off")],
        [("BASEMIND_GRAMMAR_OFFLINE", "0")],
        [("BASEMIND_OFFLINE", "false")],
        [("BASEMIND_OFFLINE", "0")],
        [("BASEMIND_OFFLINE", "Off")],
    ] {
        let output = lang_install(&vars);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{vars:?}: the manifest URL is unreachable: {stderr}"
        );
        assert!(!stderr.contains(OFFLINE_REFUSAL), "{vars:?}: {stderr}");
        assert!(stderr.contains("grammar download failed"), "{vars:?}: {stderr}");
    }
}

#[test]
fn an_offline_switch_that_is_not_a_boolean_stops_the_command_naming_it() {
    for name in ["BASEMIND_OFFLINE", "BASEMIND_GRAMMAR_OFFLINE"] {
        let output = lang_install(&[(name, "maybe")]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{name}: {stderr}");
        assert!(
            stderr.contains(&format!(
                "{name}=\"maybe\" is not a boolean: use 1, true, yes, on or 0, false, no, off"
            )),
            "{name}: {stderr}"
        );
        assert!(!stderr.contains("grammar download failed"), "{name}: {stderr}");
    }
}
