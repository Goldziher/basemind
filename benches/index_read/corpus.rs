//! Deterministic synthetic repository group for the index read benchmark.
//!
//! One group is a git repository plus `versions - 1` linked worktrees of it. Every worktree holds
//! the same paths; a linked worktree differs from the main checkout in a fixed fraction of its
//! files, and each changed file is unique to its worktree. So a group of `V` worktrees over `N`
//! paths holds `N + (V - 1) * changed` distinct file contents: the "content versions" the benchmark
//! is parameterised by.
//!
//! Nothing here depends on how the index is laid out. Real git worktrees are used because the
//! repository group is derived from the shared git common dir.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

/// Callees every file may call, with their share of all call sites in per-mille. The remainder is
/// split between file-local helpers and a long tail of rarely repeated names.
pub const HOT_CALLEES: [(&str, u32); 5] = [
    ("get", 120),
    ("dispatch_event", 60),
    ("acquire_lock", 30),
    ("get_or_insert", 25),
    ("record_metric", 15),
];
/// The trait every generated type implements.
pub const HOT_TRAIT: &str = "Handler";
/// A trait every [`RENDERABLE_EVERY`]th type also implements.
pub const WARM_TRAIT: &str = "Renderable";
pub const RENDERABLE_EVERY: usize = 4;
/// A function name defined once in every file.
pub const HOT_SYMBOL: &str = "process_widget";
/// A literal carried by every [`RARE_LITERAL_EVERY`]th file.
pub const RARE_LITERAL: &str = "rare_token_zebra_quartz";
pub const RARE_LITERAL_EVERY: usize = 100;
/// A literal no file carries.
pub const ABSENT_LITERAL: &str = "absent_token_xylophone";
/// Distinct names in the long tail of cross-file callees.
const TAIL_NAMES: u64 = 997;
/// Files per directory.
const FILES_PER_DIR: usize = 50;
/// How far in the past file mtimes are set. The scanner records an mtime inside its racy window as
/// "unknown", and the grep bloom never skips a file with an unknown stamp.
const SETTLED_AGE: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Generated source files per worktree, split evenly over Rust, TypeScript and Python.
    pub files: usize,
    /// Functions per file besides the fixed ones.
    pub helpers_per_file: usize,
    /// Call statements per function body.
    pub calls_per_fn: usize,
    /// Files a linked worktree changes, per mille of `files`.
    pub changed_per_mille: u32,
    pub seed: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Rust,
    TypeScript,
    Python,
}

impl Lang {
    fn of(index: usize) -> Self {
        match index % 3 {
            0 => Self::Rust,
            1 => Self::TypeScript,
            _ => Self::Python,
        }
    }

    fn dir(self) -> &'static str {
        match self {
            Self::Rust => "rs",
            Self::TypeScript => "ts",
            Self::Python => "py",
        }
    }

    fn ext(self) -> &'static str {
        self.dir()
    }
}

/// What the generator wrote into one worktree, so the benchmark can check that a read returned the
/// rows that are there.
#[derive(Clone, Debug, Default)]
pub struct Expected {
    /// Call sites per hot callee name.
    pub calls: BTreeMap<&'static str, usize>,
    /// Types implementing [`HOT_TRAIT`].
    pub hot_trait_impls: usize,
    /// Types implementing [`WARM_TRAIT`].
    pub warm_trait_impls: usize,
    /// Files carrying [`RARE_LITERAL`].
    pub rare_literal_files: usize,
}

pub struct WorktreeSpec {
    pub root: PathBuf,
    /// Generated source files, repo-relative.
    pub paths: Vec<String>,
    /// Files whose content differs from the main checkout.
    pub changed: usize,
    pub expected: Expected,
}

/// Repo-relative path of generated file `index`.
pub fn file_path(index: usize) -> String {
    let lang = Lang::of(index);
    format!(
        "src/{}/mod_{}/widget_{index}.{}",
        lang.dir(),
        index / FILES_PER_DIR,
        lang.ext()
    )
}

/// Name of the file-local helper `helper` in file `index`.
pub fn helper_name(index: usize, helper: usize) -> String {
    format!("helper_{index}_{helper}")
}

/// Name of the constructor function unique to file `index`.
pub fn builder_name(index: usize) -> String {
    format!("build_widget_{index}")
}

/// Repo-relative path of the file defining the hot callees for `lang`.
pub fn core_path(lang: Lang) -> String {
    format!("src/{}/core/events.{}", lang.dir(), lang.ext())
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn mix(seed: u64, a: u64, b: u64) -> u64 {
    let mut state = seed ^ a.wrapping_mul(0xA24B_AED4_963E_E407) ^ b.wrapping_mul(0x9FB2_1C65_1E98_DF25);
    splitmix(&mut state)
}

/// Whether linked worktree `worktree` (1-based) changes file `index`.
fn is_changed(params: &Params, worktree: usize, index: usize) -> bool {
    worktree > 0 && mix(params.seed, worktree as u64, index as u64) % 1000 < u64::from(params.changed_per_mille)
}

enum Callee {
    Hot(&'static str),
    Local(usize),
    Tail(u64),
}

fn pick_callee(rng: &mut u64, helpers: usize) -> Callee {
    let roll = (splitmix(rng) % 1000) as u32;
    let mut floor = 0;
    for (name, share) in HOT_CALLEES {
        floor += share;
        if roll < floor {
            return Callee::Hot(name);
        }
    }
    if splitmix(rng).is_multiple_of(2) {
        Callee::Local((splitmix(rng) % helpers as u64) as usize)
    } else {
        Callee::Tail(splitmix(rng) % TAIL_NAMES)
    }
}

/// One call expression. `get` is a method call, as it is in real code; the rest are free functions.
fn call_expr(lang: Lang, index: usize, callee: &Callee, arg: usize) -> String {
    let name = match callee {
        Callee::Hot("get") => {
            return match lang {
                Lang::Rust => format!("STORE.get(x + {arg})"),
                Lang::TypeScript | Lang::Python => format!("store.get(x + {arg})"),
            };
        }
        Callee::Hot(name) => (*name).to_string(),
        Callee::Local(helper) => helper_name(index, *helper),
        Callee::Tail(n) => format!("shared_util_{n}"),
    };
    format!("{name}(x + {arg})")
}

struct Body<'a> {
    lang: Lang,
    index: usize,
    params: &'a Params,
    rng: u64,
    expected: &'a mut Expected,
}

impl Body<'_> {
    /// Append `calls_per_fn` call statements at `indent`.
    fn calls(&mut self, out: &mut String, indent: &str) {
        for n in 0..self.params.calls_per_fn {
            let callee = pick_callee(&mut self.rng, self.params.helpers_per_file);
            self.statement(out, indent, &callee, n);
        }
    }

    fn statement(&mut self, out: &mut String, indent: &str, callee: &Callee, n: usize) {
        if let Callee::Hot(name) = callee {
            *self.expected.calls.entry(name).or_default() += 1;
        }
        let expr = call_expr(self.lang, self.index, callee, n);
        let _ = match self.lang {
            Lang::Rust => writeln!(out, "{indent}let v{n} = {expr};"),
            Lang::TypeScript => writeln!(out, "{indent}const v{n} = {expr};"),
            Lang::Python => writeln!(out, "{indent}v{n} = {expr}"),
        };
    }
}

/// Source of generated file `index` as worktree `worktree` holds it. A changed file gains one
/// function at the top, which shifts every later offset and so changes every row the file produces.
fn file_source(params: &Params, worktree: usize, index: usize, expected: &mut Expected) -> String {
    let lang = Lang::of(index);
    let changed = is_changed(params, worktree, index);
    let renderable = index.is_multiple_of(RENDERABLE_EVERY);
    let rare = index % RARE_LITERAL_EVERY == 7;
    expected.hot_trait_impls += 1;
    expected.warm_trait_impls += usize::from(renderable);
    expected.rare_literal_files += usize::from(rare);
    let mut body = Body {
        lang,
        index,
        params,
        rng: mix(params.seed, 0, index as u64),
        expected,
    };
    let mut out = String::with_capacity(4096);
    match lang {
        Lang::Rust => rust_source(&mut out, &mut body, worktree, changed, renderable, rare),
        Lang::TypeScript => ts_source(&mut out, &mut body, worktree, changed, renderable, rare),
        Lang::Python => py_source(&mut out, &mut body, worktree, changed, renderable, rare),
    }
    out
}

/// The two calls a changed file adds, so a changed file also changes what a hot-name scan returns.
fn variant_calls(body: &mut Body<'_>, out: &mut String, indent: &str) {
    body.statement(out, indent, &Callee::Hot("acquire_lock"), 0);
    body.statement(out, indent, &Callee::Hot("get"), 1);
}

fn rust_source(out: &mut String, body: &mut Body<'_>, worktree: usize, changed: bool, renderable: bool, rare: bool) {
    let index = body.index;
    out.push_str("use crate::core::events::{acquire_lock, dispatch_event, get_or_insert, record_metric};\n");
    out.push_str("use crate::core::events::{Handler, Renderable, STORE};\n\n");
    if rare {
        let _ = writeln!(out, "pub const MARKER: &str = \"{RARE_LITERAL}\";\n");
    }
    if changed {
        let _ = writeln!(out, "pub fn variant_marker_{worktree}_{index}(x: u64) -> u64 {{");
        variant_calls(body, out, "    ");
        out.push_str("    x\n}\n\n");
    }
    let _ = writeln!(out, "pub struct Widget{index} {{\n    pub id: u64,\n}}\n");
    let _ = writeln!(
        out,
        "impl Handler for Widget{index} {{\n    fn handle(&self, x: u64) -> u64 {{"
    );
    body.calls(out, "        ");
    out.push_str("        x\n    }\n}\n\n");
    if renderable {
        let _ = writeln!(
            out,
            "impl Renderable for Widget{index} {{\n    fn render(&self) -> u64 {{\n        self.id\n    }}\n}}\n"
        );
    }
    let _ = writeln!(
        out,
        "pub fn {}() -> Widget{index} {{\n    Widget{index} {{ id: {index} }}\n}}\n",
        builder_name(index)
    );
    let _ = writeln!(out, "pub fn {HOT_SYMBOL}(x: u64) -> u64 {{");
    body.calls(out, "    ");
    out.push_str("    x\n}\n\n");
    for helper in 0..body.params.helpers_per_file {
        let _ = writeln!(out, "fn {}(x: u64) -> u64 {{", helper_name(index, helper));
        body.calls(out, "    ");
        out.push_str("    x\n}\n\n");
    }
}

fn ts_source(out: &mut String, body: &mut Body<'_>, worktree: usize, changed: bool, renderable: bool, rare: bool) {
    let index = body.index;
    out.push_str("import { acquire_lock, dispatch_event, get_or_insert, record_metric } from \"../core/events\";\n");
    out.push_str("import { Handler, Renderable, store } from \"../core/events\";\n\n");
    if rare {
        let _ = writeln!(out, "export const MARKER = \"{RARE_LITERAL}\";\n");
    }
    if changed {
        let _ = writeln!(
            out,
            "export function variant_marker_{worktree}_{index}(x: number): number {{"
        );
        variant_calls(body, out, "  ");
        out.push_str("  return x;\n}\n\n");
    }
    let implements = if renderable { "Handler, Renderable" } else { "Handler" };
    let _ = writeln!(out, "export class Widget{index} implements {implements} {{");
    let _ = writeln!(out, "  id: number = {index};\n\n  handle(x: number): number {{");
    body.calls(out, "    ");
    out.push_str("    return x;\n  }\n");
    if renderable {
        out.push_str("\n  render(): number {\n    return this.id;\n  }\n");
    }
    out.push_str("}\n\n");
    let _ = writeln!(
        out,
        "export function {}(): Widget{index} {{\n  return new Widget{index}();\n}}\n",
        builder_name(index)
    );
    let _ = writeln!(out, "export function {HOT_SYMBOL}(x: number): number {{");
    body.calls(out, "  ");
    out.push_str("  return x;\n}\n\n");
    for helper in 0..body.params.helpers_per_file {
        let _ = writeln!(out, "function {}(x: number): number {{", helper_name(index, helper));
        body.calls(out, "  ");
        out.push_str("  return x;\n}\n\n");
    }
}

fn py_source(out: &mut String, body: &mut Body<'_>, worktree: usize, changed: bool, renderable: bool, rare: bool) {
    let index = body.index;
    out.push_str("from core.events import acquire_lock, dispatch_event, get_or_insert, record_metric\n");
    out.push_str("from core.events import Handler, Renderable, store\n\n\n");
    if rare {
        let _ = writeln!(out, "MARKER = \"{RARE_LITERAL}\"\n\n");
    }
    if changed {
        let _ = writeln!(out, "def variant_marker_{worktree}_{index}(x):");
        variant_calls(body, out, "    ");
        out.push_str("    return x\n\n\n");
    }
    let bases = if renderable { "Handler, Renderable" } else { "Handler" };
    let _ = writeln!(out, "class Widget{index}({bases}):\n    def handle(self, x):");
    body.calls(out, "        ");
    out.push_str("        return x\n");
    if renderable {
        out.push_str("\n    def render(self):\n        return self.id\n");
    }
    let _ = writeln!(
        out,
        "\n\ndef {}():\n    return Widget{index}()\n\n",
        builder_name(index)
    );
    let _ = writeln!(out, "def {HOT_SYMBOL}(x):");
    body.calls(out, "    ");
    out.push_str("    return x\n\n\n");
    for helper in 0..body.params.helpers_per_file {
        let _ = writeln!(out, "def {}(x):", helper_name(index, helper));
        body.calls(out, "    ");
        out.push_str("    return x\n\n\n");
    }
}

/// The file defining the hot callees and traits for `lang`. Identical in every worktree.
fn core_source(lang: Lang) -> String {
    let free = ["dispatch_event", "acquire_lock", "get_or_insert", "record_metric"];
    let mut out = String::new();
    match lang {
        Lang::Rust => {
            out.push_str("pub trait Handler {\n    fn handle(&self, x: u64) -> u64;\n}\n\n");
            out.push_str("pub trait Renderable {\n    fn render(&self) -> u64;\n}\n\n");
            out.push_str("pub struct Store;\n\npub static STORE: Store = Store;\n\n");
            out.push_str("impl Store {\n    pub fn get(&self, x: u64) -> u64 {\n        x\n    }\n}\n\n");
            for name in free {
                let _ = writeln!(out, "pub fn {name}(x: u64) -> u64 {{\n    x\n}}\n");
            }
        }
        Lang::TypeScript => {
            out.push_str("export interface Handler {\n  handle(x: number): number;\n}\n\n");
            out.push_str("export interface Renderable {\n  render(): number;\n}\n\n");
            out.push_str("export class Store {\n  get(x: number): number {\n    return x;\n  }\n}\n\n");
            out.push_str("export const store = new Store();\n\n");
            for name in free {
                let _ = writeln!(out, "export function {name}(x: number): number {{\n  return x;\n}}\n");
            }
        }
        Lang::Python => {
            out.push_str("class Handler:\n    def handle(self, x):\n        return x\n\n\n");
            out.push_str("class Renderable:\n    def render(self):\n        return 0\n\n\n");
            out.push_str("class Store:\n    def get(self, x):\n        return x\n\n\n");
            out.push_str("store = Store()\n\n\n");
            for name in free {
                let _ = writeln!(out, "def {name}(x):\n    return x\n\n");
            }
        }
    }
    out
}

fn write_file(root: &Path, rel: &str, text: &str, mtime: SystemTime) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create source dir");
    }
    std::fs::write(&path, text).expect("write source file");
    settle(&path, mtime);
}

fn settle(path: &Path, mtime: SystemTime) {
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open source file");
    file.set_modified(mtime).expect("set source mtime");
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(cwd)
        .args([
            "-c",
            "user.name=index-read-bench",
            "-c",
            "user.email=bench@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "gc.auto=0",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdout(std::process::Stdio::null())
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed in {}", cwd.display());
}

/// Build one repository group under `dir`: a main checkout and `versions - 1` linked worktrees.
pub fn build_group(dir: &Path, params: &Params, versions: usize) -> Vec<WorktreeSpec> {
    assert!(versions >= 1, "a group has at least its main checkout");
    let mtime = SystemTime::now() - SETTLED_AGE;
    let paths: Vec<String> = (0..params.files).map(file_path).collect();
    let cores = [Lang::Rust, Lang::TypeScript, Lang::Python];

    let main = dir.join("main");
    std::fs::create_dir_all(&main).expect("create main checkout");
    let mut expected = Expected::default();
    for lang in cores {
        write_file(&main, &core_path(lang), &core_source(lang), mtime);
    }
    for (index, rel) in paths.iter().enumerate() {
        write_file(&main, rel, &file_source(params, 0, index, &mut expected), mtime);
    }
    git(&main, &["init", "-q", "-b", "main"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-q", "-m", "synthetic corpus"]);

    let mut group = vec![WorktreeSpec {
        root: main.clone(),
        paths: paths.clone(),
        changed: 0,
        expected,
    }];
    for worktree in 1..versions {
        let root = dir.join(format!("wt-{worktree}"));
        let root_arg = root.to_str().expect("utf-8 temp path");
        git(&main, &["worktree", "add", "-q", "--detach", root_arg]);
        let mut expected = Expected::default();
        let mut changed = 0;
        for lang in cores {
            settle(&root.join(core_path(lang)), mtime);
        }
        for (index, rel) in paths.iter().enumerate() {
            let text = file_source(params, worktree, index, &mut expected);
            if is_changed(params, worktree, index) {
                changed += 1;
                write_file(&root, rel, &text, mtime);
            } else {
                settle(&root.join(rel), mtime);
            }
        }
        group.push(WorktreeSpec {
            root,
            paths: paths.clone(),
            changed,
            expected,
        });
    }
    group
}
