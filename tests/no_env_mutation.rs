//! No `src/**/*.rs` code (of the `basemind` crate or `crates/basemind-engine`) mutates the process environment outside the places that do it before
//! any other thread exists. libtest runs a binary's `#[test]` functions on parallel threads, and
//! `std::env::set_var` racing a `getenv` is undefined behaviour on glibc, so a lib test that sets a
//! variable corrupts every test running beside it. Pass the value as a parameter instead, or move
//! the test into its own integration binary (as `tests/comms_no_autospawn.rs` does).

use std::path::{Path, PathBuf};

/// The functions that mutate the process environment.
const MUTATORS: [&str; 2] = ["set_var", "remove_var"];

/// Whether `code` calls `mutator`: the name as a whole word, then `(` after any whitespace, or
/// nothing more on the line, for a call rustfmt split before its arguments.
fn calls(code: &str, mutator: &str) -> bool {
    code.match_indices(mutator).any(|(at, _)| {
        let before = code[..at].chars().next_back();
        let rest = code[at + mutator.len()..].trim_start();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_') && (rest.is_empty() || rest.starts_with('('))
    })
}

/// A file that may mutate the environment, and the top-level function it may do so in (`None`:
/// anywhere in the file).
const ALLOWED: [(&str, Option<&str>); 3] = [
    (
        "crates/basemind-engine/src/store_layout.rs",
        Some("pub fn init_isolated_cache("),
    ),
    ("src/cli/env_config.rs", Some("pub unsafe fn export_model_hub_offline(")),
    ("src/cli/comms_daemon.rs", None),
];

/// Collect every `.rs` file under `dir`, recursing into subdirectories.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|error| panic!("read_dir {}: {error}", dir.display()));
    for entry in entries {
        let path = entry.expect("read directory entry").path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The 1-based lines of `contents` (the file at the `/`-separated `rel` path) that mutate the
/// environment outside its allowance. Comment lines are skipped; an allowed function spans from its
/// signature to the first line that is a lone `}` at column 0, the rustfmt shape of its end.
fn offending_lines(rel: &str, contents: &str) -> Vec<usize> {
    let allowance = ALLOWED
        .iter()
        .find(|(path, _)| *path == rel)
        .map(|(_, function)| *function);
    if allowance == Some(None) {
        return Vec::new();
    }
    let allowed_function = allowance.flatten();
    let mut inside_allowed = false;
    let mut offenders = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        if allowed_function.is_some_and(|signature| line.starts_with(signature)) {
            inside_allowed = true;
        } else if inside_allowed && line == "}" {
            inside_allowed = false;
        }
        let code = line.trim_start();
        if inside_allowed || code.starts_with("//") {
            continue;
        }
        if MUTATORS.iter().any(|mutator| calls(code, mutator)) {
            offenders.push(index + 1);
        }
    }
    offenders
}

#[test]
fn no_source_file_mutates_the_process_environment() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for src in [
        manifest_dir.join("src"),
        manifest_dir.join("crates/basemind-engine/src"),
    ] {
        let before = files.len();
        collect_rs_files(&src, &mut files);
        assert!(files.len() > before, "found no .rs files under {}", src.display());
    }

    let mut report = Vec::new();
    for file in &files {
        let contents = std::fs::read_to_string(file).unwrap_or_else(|error| panic!("read {}: {error}", file.display()));
        let rel = file
            .strip_prefix(manifest_dir)
            .expect("under the manifest dir")
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        report.extend(
            offending_lines(&rel, &contents)
                .into_iter()
                .map(|line| format!("  {rel}:{line}")),
        );
    }
    assert!(
        report.is_empty(),
        "these lines mutate the process environment, which races every parallel test; pass the value \
         as a parameter or move the test into its own integration binary:\n{}",
        report.join("\n")
    );
}

#[test]
fn a_mutation_outside_an_allowance_is_reported_and_a_comment_is_not() {
    let contents = "fn f() {\n    // a `set_var(` mention\n    unsafe { std::env::set_var(\"K\", \"v\") };\n    \
                    unsafe { std::env::remove_var(\"K\") };\n}\n";
    assert_eq!(offending_lines("src/other.rs", contents), vec![3, 4]);
}

#[test]
fn only_the_allowed_function_of_an_allowed_file_may_mutate() {
    let contents = "pub fn init_isolated_cache() {\n    unsafe { std::env::set_var(\"K\", \"v\") };\n}\n\nfn \
                    after() {\n    unsafe { std::env::set_var(\"K\", \"v\") };\n}\n";
    assert_eq!(
        offending_lines("crates/basemind-engine/src/store_layout.rs", contents),
        vec![6]
    );
    assert_eq!(
        offending_lines("src/cli/comms_daemon.rs", contents),
        Vec::<usize>::new()
    );
}

#[test]
fn a_call_split_before_its_arguments_is_reported() {
    let contents = "fn f() {\n    unsafe { std::env::set_var\n        (\"K\", \"v\") };\n    unsafe {\n        \
                    std::env::remove_var\n            (\"K\")\n    };\n    let reset_var = 1;\n}\n";
    assert_eq!(offending_lines("src/other.rs", contents), vec![2, 5]);
}
