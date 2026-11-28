//! Starlark `load()` resolution.
//!
//! A `load("//pkg:file.bzl", "sym")` names a file by Bazel label: `//pkg:file.bzl` is
//! `pkg/file.bzl` under the repository root, `:file.bzl` is a file beside the importer, and `//:f.bzl`
//! is a file at the root. A label into another repository (`@rules_python//...`, `@@repo//...`) is
//! external; `@//pkg:f.bzl` names the main repository and reads as `//pkg:f.bzl`. A label whose
//! file is not indexed is unresolved. The candidate is recorded as a probe.

use crate::extract::Import;
use crate::index::keys::EdgeKinds;
use crate::intel::module_resolver::{ModuleImport, ModuleTarget, Probe, join, parent_dir};

/// Resolves the `load()` statements of Starlark files.
#[derive(Default)]
pub struct StarlarkLoads;

impl StarlarkLoads {
    pub(crate) fn resolve(&mut self, probe: &mut Probe<'_>, importer: &str, imports: &[Import]) -> Vec<ModuleImport> {
        imports
            .iter()
            .filter_map(|import| {
                let label = import.module.as_deref()?;
                Some(ModuleImport {
                    anchor: import.start_byte,
                    specifier: label.to_string(),
                    target: resolve_label(probe, importer, label),
                    kind: EdgeKinds::IMPORT,
                })
            })
            .collect()
    }
}

fn resolve_label(probe: &mut Probe<'_>, importer: &str, label: &str) -> ModuleTarget {
    let main_repository;
    let label = match label.strip_prefix("@//") {
        Some(rest) => {
            main_repository = format!("//{rest}");
            main_repository.as_str()
        }
        None => label,
    };
    if label.starts_with('@') {
        return ModuleTarget::External;
    }
    let Some(file) = label_file(importer, label) else {
        return ModuleTarget::Unresolved;
    };
    if probe.indexed(&file) {
        ModuleTarget::Files(vec![file])
    } else {
        ModuleTarget::Unresolved
    }
}

/// The repo-relative path a label names, `None` for a label that names no file.
fn label_file(importer: &str, label: &str) -> Option<String> {
    let (package, name) = match label.strip_prefix("//") {
        Some(rest) => match rest.split_once(':') {
            Some((package, name)) => (package, name),
            None => (rest, rest.rsplit('/').next().unwrap_or_default()),
        },
        None => (parent_dir(importer), label.strip_prefix(':').unwrap_or(label)),
    };
    (!name.is_empty() && !name.split('/').any(|part| part == "..")).then(|| join(package, name))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ahash::AHashSet;

    use super::*;
    use crate::intel::resolver::ProbeSet;

    fn resolve(indexed: &[&str], importer: &str, labels: &[&str]) -> (Vec<ModuleTarget>, ProbeSet) {
        let files: AHashSet<String> = indexed.iter().map(|file| file.to_string()).collect();
        let lookup = |rel: &str| files.contains(rel);
        let imports: Vec<Import> = labels
            .iter()
            .map(|label| Import {
                module: Some(label.to_string()),
                raw: format!("load(\"{label}\")"),
                start_byte: 0,
                end_byte: 0,
            })
            .collect();
        let mut probes = ProbeSet::default();
        let targets = StarlarkLoads
            .resolve(
                &mut Probe {
                    root: Path::new("."),
                    indexed: &lookup,
                    probes: &mut probes,
                },
                importer,
                &imports,
            )
            .into_iter()
            .map(|import| import.target)
            .collect();
        (targets, probes)
    }

    fn files(path: &str) -> ModuleTarget {
        ModuleTarget::Files(vec![path.to_string()])
    }

    #[test]
    fn absolute_and_relative_labels_resolve_to_indexed_files() {
        let indexed = ["tools/defs.bzl", "defs.bzl", "pkg/local.bzl", "pkg/sub/deep.bzl"];
        let (targets, probes) = resolve(
            &indexed,
            "pkg/BUILD",
            &[
                "//tools:defs.bzl",
                "//:defs.bzl",
                ":local.bzl",
                "//pkg/sub:deep.bzl",
                "local.bzl",
            ],
        );
        assert_eq!(
            targets,
            [
                files("tools/defs.bzl"),
                files("defs.bzl"),
                files("pkg/local.bzl"),
                files("pkg/sub/deep.bzl"),
                files("pkg/local.bzl"),
            ]
        );
        assert!(probes.contains("tools/defs.bzl"));
    }

    #[test]
    fn the_main_repository_label_reads_as_an_absolute_one() {
        let (targets, _) = resolve(&["tools/defs.bzl"], "BUILD", &["@//tools:defs.bzl"]);
        assert_eq!(targets, [files("tools/defs.bzl")]);
    }

    #[test]
    fn other_repositories_are_external_and_a_missing_file_is_unresolved() {
        let (targets, probes) = resolve(
            &[],
            "pkg/BUILD",
            &[
                "@rules_python//python:defs.bzl",
                "@@bazel_tools//tools:x.bzl",
                "//tools:gone.bzl",
                "//x:../y.bzl",
            ],
        );
        assert_eq!(
            targets,
            [
                ModuleTarget::External,
                ModuleTarget::External,
                ModuleTarget::Unresolved,
                ModuleTarget::Unresolved
            ]
        );
        assert!(probes.contains("tools/gone.bzl"));
    }
}
