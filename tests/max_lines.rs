//! Enforces basemind's module-size cap: every `src/**/*.rs` file stays at or under
//! [`MAX_LINES`] lines. Small modules stay reviewable; when a file approaches the cap,
//! split it into submodules (see the `module-size-cap` rule) rather than raising the cap.
//!
//! This is the real enforcement behind that rule. poly cannot count lines — its custom
//! rules are ast-grep pattern matches — so the cap lives here, in the standard `cargo test`
//! gate CI runs on every matrix leg (feature-agnostic: it only reads files).

use std::path::{Path, PathBuf};

/// The per-file line cap for every `src/**/*.rs` module.
const MAX_LINES: usize = 1000;

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

#[test]
fn no_source_file_exceeds_the_module_line_cap() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let src = Path::new(manifest_dir).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);
    assert!(!files.is_empty(), "found no .rs files under {}", src.display());

    let mut offenders: Vec<(PathBuf, usize)> = Vec::new();
    for file in &files {
        let contents = std::fs::read_to_string(file).unwrap_or_else(|error| panic!("read {}: {error}", file.display()));
        let lines = contents.lines().count();
        if lines > MAX_LINES {
            offenders.push((file.clone(), lines));
        }
    }
    offenders.sort_by_key(|offender| std::cmp::Reverse(offender.1));

    let report = offenders
        .iter()
        .map(|(path, lines)| {
            let shown = path.strip_prefix(manifest_dir).unwrap_or(path);
            format!("  {} ({lines} lines)", shown.display())
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        offenders.is_empty(),
        "these src/**/*.rs files exceed the {MAX_LINES}-line module cap — split them into submodules:\n{report}",
    );
}
