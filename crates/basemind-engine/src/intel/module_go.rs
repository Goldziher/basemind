//! Go file-level module resolution (ADR-0047 decision 4).
//!
//! An import path equal to, or under, the module path in the nearest `go.mod` names a package
//! directory of the repository; the import is one edge to each indexed non-test `.go` file in that
//! directory. Build constraints are ignored, so a package's files for every platform are targets.
//! A path under the module with no such file is unresolved; any other path (the standard library,
//! another module, `"C"`) is external. A module the repository holds besides the nearest one also
//! counts: a `use` directory of the nearest `go.work`, and a `replace` directive of the nearest
//! `go.mod` whose target is a local path (`./` or `../`). The longest module path that prefixes an
//! import wins. `vendor/` is not read.
//!
//! The package directory is recorded as a probe, so a file added to or removed from it re-stitches
//! the importer through `IndexView::importers_probing`'s ancestor lookup. A package at the
//! repository root is the probe `""`, which no path has as an ancestor; the resolve pass probes it
//! only for a top-level [`is_package_file`] (`ModuleResolver::lists_in_root`).

use ahash::AHashMap;

use crate::extract::Import;
use crate::index::keys::EdgeKinds;
use crate::intel::module_resolver::{ModuleImport, ModuleTarget, Probe, join, parent_dir, strip_dir};

/// Go module resolver. Caches `go.mod` module paths and package directory listings.
#[derive(Default)]
pub(crate) struct GoModules {
    /// `go.mod` path -> its module path; `None` when there is no such file or no `module` line.
    modules: AHashMap<String, Option<String>>,
    /// Package directory -> its indexed non-test `.go` files.
    packages: AHashMap<String, Vec<String>>,
    /// Module directory -> the other modules of the repository it can import: `(directory, module path)`.
    siblings: AHashMap<String, Vec<(String, String)>>,
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
                    kind: EdgeKinds::IMPORT,
                });
            }
        }
        out
    }

    fn resolve_path(&mut self, probe: &mut Probe<'_>, module: Option<&(String, String)>, path: &str) -> ModuleTarget {
        let Some(own) = module else {
            return ModuleTarget::External;
        };
        let siblings = self.siblings_of(probe, &own.0);
        let Some((module_dir, rest)) = std::iter::once(own)
            .chain(siblings.iter())
            .filter_map(|(dir, module_path)| Some((dir, module_path, module_subpath(path, module_path)?)))
            .max_by_key(|(_, module_path, _)| module_path.len())
            .map(|(dir, _, rest)| (dir.as_str(), rest))
        else {
            return ModuleTarget::External;
        };
        let package = join(module_dir, rest);
        probe.probes.insert_dir(&package);
        let (root, indexed) = (probe.root, probe.indexed);
        let files = self.packages.entry(package).or_insert_with_key(|package| {
            package_files(&root.join(package))
                .into_iter()
                .map(|name| join(package, &name))
                .filter(|file| indexed(file))
                .collect()
        });
        if files.is_empty() {
            ModuleTarget::Unresolved
        } else {
            ModuleTarget::Files(files.clone())
        }
    }

    /// The module path declared by the `go.mod` in `dir`; the file is a probe.
    fn module_at(&mut self, probe: &mut Probe<'_>, dir: &str) -> Option<String> {
        let manifest = join(dir, "go.mod");
        probe.record(&manifest);
        let root = probe.root;
        self.modules
            .entry(manifest)
            .or_insert_with_key(|manifest| {
                std::fs::read_to_string(root.join(manifest))
                    .ok()
                    .and_then(|text| module_directive(&text))
            })
            .clone()
    }

    /// The other modules of the repository the module in `module_dir` can import: the `use`
    /// directories of the nearest `go.work` above it and the local `replace` targets of its own
    /// `go.mod`. Every file read, found or not, is a probe.
    fn siblings_of(&mut self, probe: &mut Probe<'_>, module_dir: &str) -> Vec<(String, String)> {
        if let Some(known) = self.siblings.get(module_dir) {
            return known.clone();
        }
        let mut found = Vec::new();
        let manifest = join(module_dir, "go.mod");
        if let Ok(text) = std::fs::read_to_string(probe.root.join(&manifest)) {
            found.extend(replaced_modules(&text, module_dir));
        }
        let mut dir = module_dir;
        loop {
            let work = join(dir, "go.work");
            probe.record(&work);
            if let Ok(text) = std::fs::read_to_string(probe.root.join(&work)) {
                for used in workspace_dirs(&text, dir) {
                    if let Some(module_path) = self.module_at(probe, &used) {
                        found.push((used, module_path));
                    }
                }
                break;
            }
            if dir.is_empty() {
                break;
            }
            dir = parent_dir(dir);
        }
        self.siblings.insert(module_dir.to_string(), found.clone());
        found
    }

    /// The nearest `go.mod` above `importer` that declares a module: its directory and module path.
    /// Every candidate is a probe.
    fn module_of(&mut self, probe: &mut Probe<'_>, importer: &str) -> Option<(String, String)> {
        let mut dir = parent_dir(importer);
        loop {
            if let Some(module_path) = self.module_at(probe, dir) {
                return Some((dir.to_string(), module_path));
            }
            if dir.is_empty() {
                return None;
            }
            dir = parent_dir(dir);
        }
    }
}

/// The module path of a `go.mod`'s `module` directive, written as a line (`module example.com/x`) or,
/// as the go.mod grammar also allows, a block whose `(` ends its line and whose one line is the path
/// (`module (` newline `example.com/x` newline `)`). The one-line block `module ( example.com/x )`,
/// which `go` itself refuses, is read the same way.
fn module_directive(text: &str) -> Option<String> {
    let mut lines = text.lines().map(|line| line.split("//").next().unwrap_or("").trim());
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("module") else {
            continue;
        };
        let path = if let Some(block) = rest.trim_start().strip_prefix('(') {
            match block.trim() {
                "" => lines.find(|line| !line.is_empty())?,
                inline => inline.strip_suffix(')').unwrap_or(inline).trim(),
            }
        } else if rest.starts_with([' ', '\t']) {
            rest
        } else {
            continue;
        };
        let path = path.trim().trim_matches(['"', '`']);
        return (!path.is_empty() && path != ")").then(|| path.to_string());
    }
    None
}

/// The values of a `go.mod` / `go.work` directive written as lines (`use ./a`) or as a block
/// (`use (` newline one value per line newline `)`), comments removed.
fn directive_values<'a>(text: &'a str, keyword: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in text.lines().map(|line| line.split("//").next().unwrap_or("").trim()) {
        if in_block {
            match line {
                ")" => in_block = false,
                "" => {}
                value => out.push(value),
            }
            continue;
        }
        let Some(rest) = line.strip_prefix(keyword) else {
            continue;
        };
        if !rest.starts_with([' ', '\t', '(']) {
            continue;
        }
        match rest.trim() {
            "(" => in_block = true,
            "" => {}
            value => out.push(value),
        }
    }
    out
}

/// `dir` joined with the local path `rel` (`./x`, `../x`), lexically; `None` when it leaves the repository.
fn join_local(dir: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|part| !part.is_empty()).collect();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

/// The `use` directories of a `go.work` in `work_dir`, as repo-relative directories.
fn workspace_dirs(text: &str, work_dir: &str) -> Vec<String> {
    directive_values(text, "use")
        .into_iter()
        .filter_map(|value| join_local(work_dir, value.trim_matches(['"', '`'])))
        .collect()
}

/// The modules a `go.mod` in `module_dir` replaces with a local directory (`replace m => ../m`):
/// each one's directory and module path.
fn replaced_modules(text: &str, module_dir: &str) -> Vec<(String, String)> {
    directive_values(text, "replace")
        .into_iter()
        .filter_map(|value| {
            let (from, to) = value.split_once("=>")?;
            let module_path = from.split_whitespace().next()?.trim_matches(['"', '`']);
            let target = to.split_whitespace().next()?.trim_matches(['"', '`']);
            if !(target.starts_with("./") || target.starts_with("../")) {
                return None;
            }
            Some((join_local(module_dir, target)?, module_path.to_string()))
        })
        .collect()
}

/// The part of `path` below `module_path` (`""` for the module itself), or `None` when it is not under it.
fn module_subpath<'a>(path: &'a str, module_path: &str) -> Option<&'a str> {
    strip_dir(path, module_path).or((path == module_path).then_some(""))
}

/// The sorted non-test `.go` file names of a directory.
/// Whether a file named `name` belongs to its directory's package: a non-test `.go` file.
pub(crate) fn is_package_file(name: &str) -> bool {
    name.ends_with(".go") && !name.ends_with("_test.go")
}

fn package_files(dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| is_package_file(name))
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

    #[test]
    fn module_directive_reads_a_block_module_directive() {
        assert_eq!(
            module_directive("// header\nmodule (\n\t// the path\n\n\t\"example.com/app\" // trailing\n)\n\ngo 1.22\n")
                .as_deref(),
            Some("example.com/app")
        );
        assert_eq!(
            module_directive("module(\nexample.com/app\n)\n").as_deref(),
            Some("example.com/app")
        );
        assert_eq!(
            module_directive("module ( example.com/app )\n").as_deref(),
            Some("example.com/app")
        );
        assert_eq!(module_directive("module (\n)\n"), None);
        assert_eq!(module_directive("module ()\n"), None);
    }

    #[test]
    fn a_block_module_directive_resolves_in_module_imports() {
        let (resolved, _) = resolve(
            &[
                ("go.mod", "module (\n\texample.com/app\n)\n\ngo 1.22\n"),
                ("store/a.go", ""),
                ("cmd/main.go", ""),
            ],
            "cmd/main.go",
            "import \"example.com/app/store\"",
        );
        assert_eq!(
            resolved,
            [(
                "example.com/app/store".to_string(),
                ModuleTarget::Files(vec!["store/a.go".to_string()])
            )]
        );
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

    #[test]
    fn a_root_package_is_probed_as_the_root_directory() {
        let (resolved, probes) = resolve(
            &[
                ("go.mod", "module example.com/app\n"),
                ("app.go", ""),
                ("cmd/main.go", ""),
            ],
            "cmd/main.go",
            "import \"example.com/app\"",
        );
        assert_eq!(
            resolved,
            [(
                "example.com/app".to_string(),
                ModuleTarget::Files(vec!["app.go".to_string()])
            )]
        );
        assert!(probes.contains(""), "the root package directory must be a probe");
    }
    fn files(rels: &[&str]) -> ModuleTarget {
        ModuleTarget::Files(rels.iter().map(|rel| (*rel).to_string()).collect())
    }

    #[test]
    fn a_go_work_use_directory_is_a_module_of_the_repository() {
        let (resolved, probes) = resolve(
            &[
                ("go.work", "go 1.22\n\nuse (\n\t./a // first\n\t./b\n)\n"),
                ("a/go.mod", "module example.com/a\n"),
                ("a/main.go", ""),
                ("b/go.mod", "module example.com/b\n"),
                ("b/lib/lib.go", ""),
            ],
            "a/main.go",
            "import (\n\t\"example.com/b/lib\"\n\t\"example.com/c\"\n)",
        );
        assert_eq!(
            resolved,
            [
                ("example.com/b/lib".to_string(), files(&["b/lib/lib.go"])),
                ("example.com/c".to_string(), ModuleTarget::External),
            ]
        );
        assert!(probes.contains("go.work") && probes.contains("b/go.mod"));
    }

    #[test]
    fn a_local_replace_directive_is_a_module_of_the_repository() {
        let (resolved, _) = resolve(
            &[
                (
                    "a/go.mod",
                    "module example.com/a\n\nreplace example.com/b v1.0.0 => ../b\nreplace (\n\texample.com/c => ./vendored/c\n\texample.com/d => example.com/fork/d v2.0.0\n)\n",
                ),
                ("a/main.go", ""),
                ("b/b.go", ""),
                ("a/vendored/c/c.go", ""),
            ],
            "a/main.go",
            "import (\n\t\"example.com/b\"\n\t\"example.com/c\"\n\t\"example.com/d\"\n)",
        );
        assert_eq!(
            resolved,
            [
                ("example.com/b".to_string(), files(&["b/b.go"])),
                ("example.com/c".to_string(), files(&["a/vendored/c/c.go"])),
                ("example.com/d".to_string(), ModuleTarget::External),
            ]
        );
    }

    #[test]
    fn the_longest_module_path_wins_between_nested_modules() {
        let (resolved, _) = resolve(
            &[
                ("go.work", "use ./app\nuse ./app/tools\n"),
                ("app/go.mod", "module example.com/app\n"),
                ("app/main.go", ""),
                ("app/tools/go.mod", "module example.com/app/tools\n"),
                ("app/tools/t.go", ""),
                ("app/tools/sub/s.go", ""),
            ],
            "app/main.go",
            "import \"example.com/app/tools/sub\"",
        );
        assert_eq!(
            resolved,
            [("example.com/app/tools/sub".to_string(), files(&["app/tools/sub/s.go"]))]
        );
    }

    #[test]
    fn directive_values_read_lines_and_blocks_and_skip_other_keywords() {
        let text = "use ./a\nused ./nope\nuse (\n\t./b // note\n\n\t\"./c\"\n)\nuse(\n./d\n)\n";
        assert_eq!(directive_values(text, "use"), ["./a", "./b", "\"./c\"", "./d"]);
        assert_eq!(workspace_dirs(text, "w"), ["w/a", "w/b", "w/c", "w/d"]);
    }

    #[test]
    fn join_local_stays_inside_the_repository() {
        assert_eq!(join_local("a/b", "../c").as_deref(), Some("a/c"));
        assert_eq!(join_local("a", "./b/./c").as_deref(), Some("a/b/c"));
        assert_eq!(join_local("a", "../../x"), None);
    }
    #[test]
    fn a_go_work_workspace_resolves_an_import_between_its_modules() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/multi_root");
        let lookup = |rel: &str| root.join(rel).is_file();
        let mut probes = ProbeSet::default();
        let raw = "import \"example.com/b/store\"";
        let import = Import {
            module: None,
            raw: raw.to_string(),
            start_byte: 0,
            end_byte: raw.len() as u32,
        };
        let resolved = GoModules::default().resolve(
            &mut Probe {
                root: &root,
                indexed: &lookup,
                probes: &mut probes,
            },
            "go/a/cmd/main.go",
            &[import],
        );
        assert_eq!(resolved[0].target, files(&["go/b/store/store.go"]));
    }
}
