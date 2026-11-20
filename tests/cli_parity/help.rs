//! Every command's long help (`basemind <command> --help`, issue #163): when to use it, what it
//! returns, one example and what to call next, with the example and every quoted command checked
//! against the live tree the way the manual's are.

use std::process::Command;

use crate::{cli_tree, manual};

/// The parts every long help has, in this order, each starting a line.
const PARTS: [&str; 4] = ["When to use: ", "Returns: ", "Example:\n  basemind ", "Next: "];

fn long_help(path: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_basemind"))
        .args(path.split(' '))
        .arg("--help")
        .output()
        .unwrap_or_else(|e| panic!("spawn `basemind {path} --help`: {e}"));
    assert!(
        output.status.success(),
        "`basemind {path} --help` exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Check the long help `text` of the command at `path`; `Err` says what is wrong.
fn check_long_help(tree: &cli_tree::CliTree, path: &str, text: &str) -> Result<(), String> {
    let mut from = 0;
    for part in PARTS {
        let at = text[from..]
            .find(&format!("\n{part}"))
            .ok_or_else(|| format!("no `{}` part after the options", part.trim_end()))?;
        from += at + 1;
    }
    let example_at = text.find("\nExample:\n  ").expect("found above") + "\nExample:\n  ".len();
    let example = text[example_at..].lines().next().unwrap_or_default();
    let own = format!("basemind {path}");
    if example != own && !example.starts_with(&format!("{own} ")) {
        return Err(format!("the example `{example}` does not run `{own}`"));
    }
    manual::check_command(tree, example).map_err(|why| format!("example `{example}`: {why}"))?;
    let after = &text[text.find("\nWhen to use: ").expect("found above")..];
    for span in after.split('`').skip(1).step_by(2) {
        if span.starts_with("basemind ") {
            manual::check_command(tree, span).map_err(|why| format!("`{span}`: {why}"))?;
        }
    }
    Ok(())
}

#[test]
fn every_command_has_a_long_help_with_a_valid_example() {
    let tree = cli_tree::load();
    let mut failures = Vec::new();
    let mut checked = 0;
    for command in tree.leaves() {
        checked += 1;
        if let Err(why) = check_long_help(tree, &command.path, &long_help(&command.path)) {
            failures.push(format!("basemind {}: {why}", command.path));
        }
    }
    assert!(
        checked >= 60,
        "only {checked} commands walked; the tree came back short"
    );
    assert!(
        failures.is_empty(),
        "commands whose --help lacks a valid long help (src/cli/help.rs, src/cli/help_code.rs):\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn a_missing_part_or_a_wrong_example_is_caught() {
    let tree = cli_tree::load();
    let help = |example: &str, next: &str| {
        format!("Options:\n  -h\n\nWhen to use: a.\n\nReturns: b.\n\nExample:\n  {example}\n\nNext: {next}")
    };
    let valid = help("basemind code outline src/a.rs", "`basemind code read <path>`.");
    assert_eq!(check_long_help(tree, "code outline", &valid), Ok(()));
    assert!(check_long_help(tree, "code outline", &valid.replace("\n\nNext: ", "\n\nThen: ")).is_err());
    assert!(check_long_help(tree, "code outline", &valid.replace("Returns: ", "Gives: ")).is_err());
    assert!(check_long_help(tree, "code outline", &help("basemind code symbols x", "c.")).is_err());
    assert!(check_long_help(tree, "code outline", &help("basemind code outline --bogus", "c.")).is_err());
    assert!(
        check_long_help(
            tree,
            "code outline",
            &help("basemind code outline a.rs", "`basemind code reed x`.")
        )
        .is_err()
    );
}
