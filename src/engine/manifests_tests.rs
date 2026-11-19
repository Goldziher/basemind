use std::collections::BTreeMap;

use super::*;

fn ecosystem_of(file_name: &str) -> Option<Ecosystem> {
    MANIFEST_NAMES
        .iter()
        .find(|(name, _)| *name == file_name)
        .map(|(_, e)| *e)
}

fn no_read(_: &str, _: usize) -> Option<String> {
    None
}

fn parse_with(path: &str, text: &str, files: &[&str], heads: &BTreeMap<&str, &str>) -> Manifest {
    let files = FileSet::new(files.iter().copied());
    let read = |p: &str, _max: usize| heads.get(p).map(|s| (*s).to_owned());
    let cx = Context {
        files: &files,
        read: &read,
        go_modules: &[],
    };
    let ecosystem = ecosystem_of(path.rsplit('/').next().unwrap()).expect("a manifest name");
    parse(path, ecosystem, text, &cx)
}

fn targets(m: &Manifest) -> Vec<(TargetKind, Option<&str>, Option<String>)> {
    m.targets
        .iter()
        .map(|t| (t.kind, t.name.as_deref(), t.path.as_ref().map(ToString::to_string)))
        .collect()
}

#[test]
fn cargo_reads_the_package_its_dependency_tiers_and_its_conventional_targets() {
    let text = r#"
[package]
name = "demo"
version = "0.3.1"

[[bin]]
name = "tool"
path = "src/tool.rs"

[dependencies]
serde = "1"
anyhow = "1"

[target.'cfg(unix)'.dependencies]
libc = "0.2"

[dev-dependencies]
tempfile = "3"

[build-dependencies]
cc = "1"
"#;
    let files = [
        "crates/demo/src/lib.rs",
        "crates/demo/src/main.rs",
        "crates/demo/src/tool.rs",
        "crates/demo/src/bin/extra.rs",
        "crates/demo/src/bin/multi/main.rs",
        "crates/demo/src/bin/multi/helper.rs",
        "crates/demo/tests/it.rs",
        "crates/demo/benches/speed.rs",
        "crates/demo/examples/hello.rs",
        "crates/demo/build.rs",
        "crates/other/src/lib.rs",
    ];
    let m = parse_with("crates/demo/Cargo.toml", text, &files, &BTreeMap::new());
    assert_eq!(m.error, None);
    assert_eq!(m.name.as_deref(), Some("demo"));
    assert_eq!(m.version.as_deref(), Some("0.3.1"));
    assert_eq!(m.dependency_names, ["anyhow", "libc", "serde"]);
    assert_eq!(
        m.dependencies,
        DependencyCounts {
            runtime: 3,
            dev: 1,
            build: 1,
            indirect: 0
        }
    );
    let p = |s: &str| Some(format!("crates/demo/{s}"));
    assert_eq!(
        targets(&m),
        [
            (TargetKind::Lib, Some("demo"), p("src/lib.rs")),
            (TargetKind::Bin, Some("demo"), p("src/main.rs")),
            (TargetKind::Bin, Some("extra"), p("src/bin/extra.rs")),
            (TargetKind::Bin, Some("multi"), p("src/bin/multi/main.rs")),
            (TargetKind::Bin, Some("tool"), p("src/tool.rs")),
            (TargetKind::Build, None, p("build.rs")),
            (TargetKind::Test, Some("it"), p("tests/it.rs")),
            (TargetKind::Bench, Some("speed"), p("benches/speed.rs")),
            (TargetKind::Example, Some("hello"), p("examples/hello.rs")),
        ]
    );
}

#[test]
fn a_virtual_cargo_workspace_reports_members_and_its_shared_dependencies() {
    let text = r#"
[workspace]
members = ["crates/*", "tools/gen"]

[workspace.dependencies]
tokio = "1"
serde = { version = "1" }
"#;
    let m = parse_with("Cargo.toml", text, &[], &BTreeMap::new());
    assert_eq!(m.name, None);
    assert_eq!(m.members, ["crates/*", "tools/gen"]);
    assert_eq!(m.dependency_names, ["serde", "tokio"]);
    assert!(m.targets.is_empty(), "a virtual manifest builds nothing itself");
}

#[test]
fn a_manifest_that_does_not_parse_says_why_and_nothing_else() {
    let m = parse_with("Cargo.toml", "[package\nname=", &[], &BTreeMap::new());
    assert!(
        m.error.as_deref().is_some_and(|e| e.starts_with("not valid TOML")),
        "{m:?}"
    );
    assert_eq!(m.name, None);
    let m = parse_with("web/package.json", "{", &[], &BTreeMap::new());
    assert!(
        m.error.as_deref().is_some_and(|e| e.starts_with("not valid JSON")),
        "{m:?}"
    );
}

#[test]
fn package_json_reads_main_bin_exports_scripts_and_workspaces() {
    let text = r#"{
  "name": "web",
  "version": "1.2.0",
  "main": "./dist/index.js",
  "bin": { "webctl": "bin/cli.js" },
  "exports": { ".": { "import": "./src/index.mjs", "require": "./dist/index.js" }, "./feature/*": "./src/feature/*.js" },
  "scripts": { "build": "tsc", "test": "vitest", "postinstall": "node install.js && echo ok" },
  "workspaces": { "packages": ["packages/*"] },
  "dependencies": { "react": "^18", "zod": "^3" },
  "devDependencies": { "vitest": "^1" },
  "peerDependencies": { "react-dom": "^18" }
}"#;
    let files = ["web/bin/cli.js", "web/install.js", "web/src/index.mjs"];
    let m = parse_with("web/package.json", text, &files, &BTreeMap::new());
    assert_eq!(m.name.as_deref(), Some("web"));
    assert_eq!(m.members, ["packages/*"]);
    assert_eq!(m.dependency_names, ["react", "zod"]);
    assert_eq!(m.dependencies.dev, 2);
    assert_eq!(
        targets(&m),
        [
            (TargetKind::Bin, Some("webctl"), Some("web/bin/cli.js".into())),
            (TargetKind::Main, None, None),
            (TargetKind::Export, None, None),
            (TargetKind::Export, None, Some("web/src/index.mjs".into())),
            (TargetKind::Script, Some("build"), None),
            (TargetKind::Script, Some("postinstall"), Some("web/install.js".into())),
            (TargetKind::Script, Some("test"), None),
        ],
        "an unindexed `main` (a build output) is declared but not resolved; a glob export is skipped"
    );
}

#[test]
fn pyproject_reads_pep621_and_poetry_and_resolves_scripts_to_their_module() {
    let text = r#"
[project]
name = "pkg"
version = "0.1.0"
dependencies = ["Requests>=2", "click"]

[project.optional-dependencies]
test = ["pytest"]

[project.scripts]
pkg-cli = "pkg.cli:main"

[dependency-groups]
dev = ["ruff", "mypy"]
"#;
    let files = ["src/pkg/__init__.py", "src/pkg/cli.py", "src/pkg/__main__.py"];
    let m = parse_with("pyproject.toml", text, &files, &BTreeMap::new());
    assert_eq!(m.dependency_names, ["click", "requests"]);
    assert_eq!(m.dependencies.dev, 3);
    assert_eq!(
        targets(&m),
        [
            (TargetKind::Main, None, Some("src/pkg/__main__.py".into())),
            (TargetKind::Script, Some("pkg-cli"), Some("src/pkg/cli.py".into())),
        ]
    );

    let poetry = r#"
[tool.poetry]
name = "legacy"
version = "2.0"
[tool.poetry.dependencies]
python = "^3.10"
Django = "^4"
[tool.poetry.group.dev.dependencies]
pytest = "*"
"#;
    let m = parse_with("pyproject.toml", poetry, &[], &BTreeMap::new());
    assert_eq!(m.name.as_deref(), Some("legacy"));
    assert_eq!(
        m.dependency_names,
        ["django"],
        "`python` is the interpreter, not a dependency"
    );
    assert_eq!(m.dependencies.dev, 1);
}

#[test]
fn go_mod_counts_direct_and_indirect_requirements_and_finds_main_packages() {
    let text = "module example.com/app\n\ngo 1.22\n\nrequire github.com/a/b v1.0.0\n\nrequire (\n\tgithub.com/c/d v0.2.0\n\tgolang.org/x/sys v0.1.0 // indirect\n)\n";
    let files = [
        "cmd/server/main.go",
        "cmd/server/main_test.go",
        "internal/lib/lib.go",
        "tools/gen/gen.go",
    ];
    let heads = BTreeMap::from([
        (
            "cmd/server/main.go",
            "// Copyright\n/* block\n comment */\npackage main\n",
        ),
        ("internal/lib/lib.go", "package lib\n"),
        (
            "tools/gen/gen.go",
            "//go:build tools\n\npackage main // the generator\n",
        ),
    ]);
    let m = parse_with("go.mod", text, &files, &heads);
    assert_eq!(m.name.as_deref(), Some("example.com/app"));
    assert_eq!(m.version.as_deref(), Some("1.22"));
    assert_eq!(m.dependency_names, ["github.com/a/b", "github.com/c/d"]);
    assert_eq!(m.dependencies.indirect, 1);
    assert_eq!(
        targets(&m),
        [
            (TargetKind::Bin, Some("gen"), Some("tools/gen".into())),
            (TargetKind::Bin, Some("server"), Some("cmd/server".into())),
        ]
    );
}

#[test]
fn a_nested_go_module_owns_its_own_main_packages() {
    let files = FileSet::new(["main.go", "sub/cmd/main.go"]);
    let read = |_: &str, _: usize| Some("package main\n".to_owned());
    let cx = Context {
        files: &files,
        read: &read,
        go_modules: &["", "sub"],
    };
    let root = parse("go.mod", Ecosystem::Go, "module a\n", &cx);
    assert_eq!(targets(&root), [(TargetKind::Bin, None, Some(".".into()))]);
    let sub = parse("sub/go.mod", Ecosystem::Go, "module a/sub\n", &cx);
    assert_eq!(targets(&sub), [(TargetKind::Bin, Some("cmd"), Some("sub/cmd".into()))]);
}

#[test]
fn pom_reads_the_project_not_its_parent_and_scopes_test_dependencies_as_dev() {
    let text = r#"<project>
  <!-- <artifactId>commented</artifactId> -->
  <parent><artifactId>parent-pom</artifactId><version>9</version></parent>
  <artifactId>service</artifactId>
  <version>1.4.0</version>
  <modules><module>core</module><module>api</module></modules>
  <dependencies>
    <dependency><groupId>g</groupId><artifactId>guava</artifactId></dependency>
    <dependency><artifactId>junit</artifactId><scope>test</scope></dependency>
  </dependencies>
  <build><plugins><plugin><configuration><mainClass>com.x.Main</mainClass></configuration></plugin></plugins></build>
</project>"#;
    let m = parse_with("pom.xml", text, &[], &BTreeMap::new());
    assert_eq!(m.name.as_deref(), Some("service"));
    assert_eq!(m.version.as_deref(), Some("1.4.0"));
    assert_eq!(m.members, ["core", "api"]);
    assert_eq!(m.dependency_names, ["guava"]);
    assert_eq!(m.dependencies.dev, 1);
    assert_eq!(targets(&m), [(TargetKind::Main, Some("com.x.Main"), None)]);
}

#[test]
fn gradle_reads_dependencies_main_class_and_its_settings() {
    let text = "plugins { id 'application' }\nversion = '3.1'\ndependencies {\n  implementation 'org.slf4j:slf4j-api:2.0'\n  implementation(\"com.squareup:okhttp:4\")\n  testImplementation 'junit:junit:4.13'\n}\napplication { mainClass = 'app.Main' }\n";
    let heads = BTreeMap::from([(
        "settings.gradle.kts",
        "rootProject.name = \"shop\"\ninclude(\":core\", \":web\")\n",
    )]);
    let m = parse_with("build.gradle", text, &[], &heads);
    assert_eq!(m.name.as_deref(), Some("shop"));
    assert_eq!(m.version.as_deref(), Some("3.1"));
    assert_eq!(m.members, [":core", ":web"]);
    assert_eq!(m.dependency_names, ["okhttp", "slf4j-api"]);
    assert_eq!(m.dependencies.dev, 1);
    assert_eq!(targets(&m), [(TargetKind::Main, Some("app.Main"), None)]);
}

#[test]
fn discovery_looks_beside_indexed_files_and_their_ancestors_only() {
    let files = FileSet::new(["crates/a/src/lib.rs", "web/src/app.ts"]);
    assert_eq!(
        files.directories(),
        ["", "crates", "crates/a", "crates/a/src", "web", "web/src"]
    );
    let on_disk = [
        "Cargo.toml",
        "crates/a/Cargo.toml",
        "web/package.json",
        "node_modules/x/package.json",
    ];
    let exists = |p: &str| on_disk.contains(&p);
    let read = |p: &str, _: usize| match p {
        "Cargo.toml" => Some("[workspace]\nmembers = [\"crates/*\"]\n".to_owned()),
        "crates/a/Cargo.toml" => Some("[package]\nname = \"a\"\n".to_owned()),
        "web/package.json" => Some("{\"name\":\"web\"}".to_owned()),
        _ => None,
    };
    let cx = Context {
        files: &files,
        read: &read,
        go_modules: &[],
    };
    let found = discover(&files.directories(), &exists, &cx);
    let paths: Vec<String> = found.iter().map(|m| m.path.to_string()).collect();
    assert_eq!(paths, ["Cargo.toml", "crates/a/Cargo.toml", "web/package.json"]);
    assert_eq!(
        targets(&found[1]),
        [(TargetKind::Lib, Some("a"), Some("crates/a/src/lib.rs".into()))]
    );
}

#[test]
fn an_unreadable_manifest_is_reported_not_skipped() {
    let files = FileSet::new(["src/lib.rs"]);
    let exists = |p: &str| p == "Cargo.toml";
    let cx = Context {
        files: &files,
        read: &no_read,
        go_modules: &[],
    };
    let found = discover(&files.directories(), &exists, &cx);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].error.as_deref(), Some("unreadable"));
}

#[test]
fn dependency_names_are_capped_and_the_count_is_not() {
    let deps: String = (0..40).map(|i| format!("d{i:02} = \"1\"\n")).collect();
    let m = parse_with(
        "Cargo.toml",
        &format!("[package]\nname = \"x\"\n[dependencies]\n{deps}"),
        &[],
        &BTreeMap::new(),
    );
    assert_eq!(m.dependencies.runtime, 40);
    assert_eq!(m.dependency_names.len(), DEPENDENCY_NAMES_MAX);
}

#[test]
fn joined_paths_never_leave_the_repository() {
    assert_eq!(join("a", "./b/c.js").as_deref(), Some("a/b/c.js"));
    assert_eq!(join("", "src/lib.rs").as_deref(), Some("src/lib.rs"));
    assert_eq!(join("a", "../b.js"), None);
    assert_eq!(join("a", "/etc/passwd"), None);
}
