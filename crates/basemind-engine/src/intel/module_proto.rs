//! Protocol Buffers file-level import resolution.
//!
//! An `import "acme/common.proto";` names a path relative to a proto source root, which is the
//! repository root in some repositories and a directory such as `proto/` or `api/` in others. The
//! importer's own directory and each ancestor of it are tried as the root, nearest first; the first
//! one under which the path is an indexed file is the target. A path under `google/` (the bundled
//! well-known types and the Google API protos) that no root holds is external; any other path no
//! root holds is unresolved. Every candidate is recorded as a probe, so adding or removing one
//! re-stitches the importer.

use crate::extract::Import;
use crate::index::keys::EdgeKinds;
use crate::intel::module_resolver::{ModuleImport, ModuleTarget, Probe, join, parent_dir};

/// Import paths under this prefix come from a dependency, not from the repository.
const EXTERNAL_PREFIX: &str = "google/";

/// Resolves the `import` statements of `.proto` files.
#[derive(Default)]
pub struct ProtoImports;

impl ProtoImports {
    pub(crate) fn resolve(&mut self, probe: &mut Probe<'_>, importer: &str, imports: &[Import]) -> Vec<ModuleImport> {
        imports
            .iter()
            .filter_map(|import| {
                let path = import.module.as_deref()?;
                Some(ModuleImport {
                    anchor: import.start_byte,
                    specifier: path.to_string(),
                    target: resolve_path(probe, importer, path),
                    kind: EdgeKinds::IMPORT,
                })
            })
            .collect()
    }
}

fn resolve_path(probe: &mut Probe<'_>, importer: &str, path: &str) -> ModuleTarget {
    if path.is_empty() || path.starts_with('/') || path.split('/').any(|part| part == "..") {
        return ModuleTarget::Unresolved;
    }
    let mut dir = parent_dir(importer);
    loop {
        let candidate = join(dir, path);
        if probe.indexed(&candidate) {
            return ModuleTarget::Files(vec![candidate]);
        }
        if dir.is_empty() {
            break;
        }
        dir = parent_dir(dir);
    }
    if path.starts_with(EXTERNAL_PREFIX) {
        ModuleTarget::External
    } else {
        ModuleTarget::Unresolved
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ahash::AHashSet;

    use super::*;
    use crate::intel::resolver::ProbeSet;

    fn resolve(indexed: &[&str], importer: &str, paths: &[&str]) -> (Vec<ModuleTarget>, ProbeSet) {
        let files: AHashSet<String> = indexed.iter().map(|file| file.to_string()).collect();
        let lookup = |rel: &str| files.contains(rel);
        let imports: Vec<Import> = paths
            .iter()
            .map(|path| Import {
                module: Some(path.to_string()),
                raw: format!("import \"{path}\";"),
                start_byte: 0,
                end_byte: 0,
            })
            .collect();
        let mut probes = ProbeSet::default();
        let targets = ProtoImports
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
    fn a_path_resolves_from_the_repository_root() {
        let (targets, _) = resolve(
            &["acme/common.proto"],
            "svc/billing/v1/billing.proto",
            &["acme/common.proto"],
        );
        assert_eq!(targets, [files("acme/common.proto")]);
    }

    #[test]
    fn a_path_resolves_from_a_source_root_above_the_importer() {
        let (targets, probes) = resolve(
            &["proto/acme/common.proto"],
            "proto/svc/billing.proto",
            &["acme/common.proto"],
        );
        assert_eq!(targets, [files("proto/acme/common.proto")]);
        assert!(probes.contains("proto/svc/acme/common.proto"));
        assert!(probes.contains("proto/acme/common.proto"));
    }

    #[test]
    fn the_nearest_root_wins_over_a_farther_one() {
        let indexed = ["proto/acme/common.proto", "acme/common.proto"];
        let (targets, _) = resolve(&indexed, "proto/billing.proto", &["acme/common.proto"]);
        assert_eq!(targets, [files("proto/acme/common.proto")]);
    }

    #[test]
    fn a_missing_path_is_unresolved_and_a_google_path_is_external() {
        let (targets, probes) = resolve(
            &[],
            "svc/a.proto",
            &["acme/gone.proto", "google/protobuf/timestamp.proto", "../x.proto"],
        );
        assert_eq!(
            targets,
            [
                ModuleTarget::Unresolved,
                ModuleTarget::External,
                ModuleTarget::Unresolved
            ]
        );
        assert!(probes.contains("svc/acme/gone.proto"));
        assert!(probes.contains("acme/gone.proto"));
    }

    #[test]
    fn a_google_path_in_the_repository_resolves() {
        let (targets, _) = resolve(&["google/api/x.proto"], "a.proto", &["google/api/x.proto"]);
        assert_eq!(targets, [files("google/api/x.proto")]);
    }
}
