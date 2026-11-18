//! Go file-level module resolution (ADR-0047 decision 4).
//!
//! An import path equal to, or under, the module path in the nearest `go.mod` names a package
//! directory of the repository; the import is one edge to each indexed non-test `.go` file in that
//! directory. Build constraints are ignored, so a package's files for every platform are targets.
//! A path under the module with no such file is unresolved; any other path (the standard library,
//! another module, `"C"`) is external. `replace` directives, `go.work` and `vendor/` are not read.
//!
//! The package directory is recorded as a probe, so a file added to or removed from it re-stitches
//! the importer through `IndexView::importers_probing`'s ancestor lookup.

use ahash::AHashMap;

use crate::extract::Import;
use crate::intel::module_resolver::{ModuleImport, ModuleTarget, Probe, join, parent_dir, strip_dir};

/// Go module resolver. Caches `go.mod` module paths and package directory listings.
#[derive(Default)]
pub(crate) struct GoModules {
    /// `go.mod` path -> its module path; `None` when there is no such file or no `module` line.
    modules: AHashMap<String, Option<String>>,
    /// Package directory -> its non-test `.go` file names.
    packages: AHashMap<String, Vec<String>>,
}

impl GoModules {
    pub(crate) fn resolve(&mut self, probe: &mut Probe<'_>, importer: &str, imports: &[Import]) -> Vec<ModuleImport> {
        let mut out = Vec::new();
        let mut module: Option<Option<(String, String)>> = None;
        for import in imports {
            for (offset, path) in import_paths(&import.raw) {
                let module = module.get_or_insert_with(|| self.module_of(probe, importer));
                out.push(ModuleImport {
                    anchor: import.start_byte + offset,
                    specifier: path.to_string(),
                    target: self.resolve_path(probe, module.as_ref(), path),
                });
            }
        }
        out
    }

    fn resolve_path(&mut self, probe: &mut Probe<'_>, module: Option<&(String, String)>, path: &str) -> ModuleTarget {
        let Some((module_dir, module_path)) = module else {
            return ModuleTarget::External;
        };
        let Some(rest) = strip_dir(path, module_path).or((path == module_path).then_some("")) else {
            return ModuleTarget::External;
        };
        let package = join(module_dir, rest);
        probe.record(&package);
        let root = probe.root;
        let names = self
            .packages
            .entry(package.clone())
            .or_insert_with_key(|package| package_files(&root.join(package)));
        let files: Vec<String> = names
            .iter()
            .map(|name| join(&package, name))
            .filter(|file| (probe.indexed)(file))
            .collect();
        if files.is_empty() {
            ModuleTarget::Unresolved
        } else {
            ModuleTarget::Files(files)
        }
    }

    /// The nearest `go.mod` above `importer` that declares a module: its directory and module path.
    /// Every candidate is a probe.
    fn module_of(&mut self, probe: &mut Probe<'_>, importer: &str) -> Option<(String, String)> {
        let mut dir = parent_dir(importer);
        loop {
            let manifest = join(dir, "go.mod");
            probe.record(&manifest);
            let root = probe.root;
            let declared = self
                .modules
                .entry(manifest)
                .or_insert_with_key(|manifest| {
                    std::fs::read_to_string(root.join(manifest))
                        .ok()
                        .and_then(|text| module_directive(&text))
                })
                .clone();
            if let Some(module_path) = declared {
                return Some((dir.to_string(), module_path));
            }
            if dir.is_empty() {
                return None;
            }
            dir = parent_dir(dir);
        }
    }
}

/// The module path of a `go.mod`'s `module` directive.
fn module_directive(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.split("//").next()?.trim();
        let path = line.strip_prefix("module")?;
        if !path.starts_with([' ', '\t']) {
            return None;
        }
        let path = path.trim().trim_matches(['"', '`']);
        (!path.is_empty()).then(|| path.to_string())
    })
}

/// The sorted non-test `.go` file names of a directory.
fn package_files(dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".go") && !name.ends_with("_test.go"))
        .collect();
    names.sort_unstable();
    names
}

/// The quoted import paths of an import declaration with each one's byte offset in `raw`, skipping
/// comments: `import ( f "fmt"; _ "x/y" )` gives `fmt` and `x/y`.
fn import_paths(raw: &str) -> Vec<(u32, &str)> {
    let bytes = raw.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                at = memchr::memchr(b'\n', &bytes[at..]).map_or(bytes.len(), |end| at + end);
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at = memchr::memmem::find(&bytes[at + 2..], b"*/").map_or(bytes.len(), |end| at + 2 + end + 2);
            }
            quote @ (b'"' | b'`') => {
                let Some(len) = memchr::memchr(quote, &bytes[at + 1..]) else {
                    break;
                };
                out.push((at as u32, &raw[at + 1..at + 1 + len]));
                at += len + 2;
            }
            _ => at += 1,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use ahash::AHashSet;

    use super::*;
    use crate::intel::resolver::ProbeSet;

    #[test]
    fn import_paths_reads_single_and_grouped_declarations() {
        assert_eq!(import_paths("import \"fmt\""), [(7, "fmt")]);
        let grouped = "import (\n\tf \"fmt\" // \"not/this\"\n\t_ `x/y`\n\t/* \"nor/this\" */\n)";
        let paths: Vec<&str> = import_paths(grouped).into_iter().map(|(_, path)| path).collect();
        assert_eq!(paths, ["fmt", "x/y"]);
    }

    #[test]
    fn module_directive_reads_the_module_path() {
        assert_eq!(
            module_directive("// header\nmodule example.com/app // trailing\n\ngo 1.22\n").as_deref(),
            Some("example.com/app")
        );
        assert_eq!(module_directive("go 1.22\n"), None);
        assert_eq!(module_directive("modules x\n"), None);
    }

    fn resolve(files: &[(&str, &str)], importer: &str, raw: &str) -> (Vec<(String, ModuleTarget)>, ProbeSet) {
        let dir = tempfile::tempdir().unwrap();
        for (rel, body) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let indexed: AHashSet<String> = files
            .iter()
            .filter(|(rel, _)| rel.ends_with(".go"))
            .map(|(rel, _)| (*rel).to_string())
            .collect();
        let lookup = |rel: &str| indexed.contains(rel);
        let mut probes = ProbeSet::default();
        let import = Import {
            module: None,
            raw: raw.to_string(),
            start_byte: 100,
            end_byte: 100 + raw.len() as u32,
        };
        let out = GoModules::default()
            .resolve(
                &mut Probe {
                    root: dir.path(),
                    indexed: &lookup,
                    probes: &mut probes,
                },
                importer,
                &[import],
            )
            .into_iter()
            .map(|import| (import.specifier, import.target))
            .collect();
        (out, probes)
    }

    #[test]
    fn module_imports_target_every_non_test_file_of_the_package() {
        let raw = "import (\n\t\"fmt\"\n\t\"example.com/app/internal/store\"\n\t\"example.com/app\"\n\t\"example.com/app/gone\"\n\t\"example.com/other/x\"\n)";
        let (resolved, probes) = resolve(
            &[
                ("svc/go.mod", "module example.com/app\n"),
                ("svc/main.go", ""),
                ("svc/cmd/run/run.go", ""),
                ("svc/internal/store/a.go", ""),
                ("svc/internal/store/b.go", ""),
                ("svc/internal/store/b_test.go", ""),
                ("svc/internal/store/notes.md", ""),
            ],
            "svc/cmd/run/run.go",
            raw,
        );
        let files = |rels: &[&str]| ModuleTarget::Files(rels.iter().map(|rel| (*rel).to_string()).collect());
        assert_eq!(
            resolved,
            [
                ("fmt".to_string(), ModuleTarget::External),
                (
                    "example.com/app/internal/store".to_string(),
                    files(&["svc/internal/store/a.go", "svc/internal/store/b.go"])
                ),
                ("example.com/app".to_string(), files(&["svc/main.go"])),
                ("example.com/app/gone".to_string(), ModuleTarget::Unresolved),
                ("example.com/other/x".to_string(), ModuleTarget::External),
            ]
        );
        for probed in [
            "svc/cmd/run/go.mod",
            "svc/cmd/go.mod",
            "svc/go.mod",
            "svc/internal/store",
            "svc/gone",
        ] {
            assert!(probes.contains(probed), "{probed} must be a probe");
        }
        assert!(!probes.contains("go.mod"), "the walk stops at the nearest go.mod");
    }

    #[test]
    fn without_a_go_mod_every_import_is_external() {
        let (resolved, probes) = resolve(&[("a/a.go", "")], "a/a.go", "import \"example.com/app/b\"");
        assert_eq!(resolved, [("example.com/app/b".to_string(), ModuleTarget::External)]);
        assert!(probes.contains("a/go.mod") && probes.contains("go.mod"));
    }
}
