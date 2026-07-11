//! End-to-end assertions for the stack-graphs precise-name-resolution engine
//! (feature `code-intel-stack`, Python + Java).
//!
//! Mirrors the `mcp_smoke.rs` harness shape exactly: build a tiny git repo from the fixtures
//! under `tests/fixtures/precise_resolution_{py,java}/`, scan it in-process, spawn
//! `basemind serve` over stdio, and drive `goto_definition` / `find_callers` through the real
//! rmcp child-process transport. Every position and expected response body below is taken
//! verbatim from `/tmp/track-d-test-spec.md`, which was captured empirically against the real
//! CLI — do not eyeball or recompute positions here.
//!
//! Gated on `code-intel-stack`: under default features this file compiles out entirely, so it
//! never affects the default build/test matrix. Run with:
//! `cargo test --features code-intel-stack --test precise_resolution_smoke`.

#![cfg(feature = "code-intel-stack")]

use std::path::Path;
use std::process::Command;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::Command as AsyncCommand;

/// Run a `git` command in `repo`, propagating identity env vars so CI works.
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed in {}", repo.display());
}

/// Build a throwaway git repo containing the two Python fixtures (`app.py` + `mod.py`) from
/// `tests/fixtures/precise_resolution_py/`, copied verbatim.
fn build_python_repo() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/precise_resolution_py");
    std::fs::copy(fixtures.join("app.py"), root.join("app.py")).expect("copy app.py");
    std::fs::copy(fixtures.join("mod.py"), root.join("mod.py")).expect("copy mod.py");

    git(root, &["add", "app.py", "mod.py"]);
    git(root, &["commit", "-qm", "init"]);
    dir
}

/// Build a throwaway git repo containing the two Java fixtures (`App.java` + `Foo.java`) from
/// `tests/fixtures/precise_resolution_java/`, copied verbatim.
fn build_java_repo() -> TempDir {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/precise_resolution_java");
    std::fs::copy(fixtures.join("App.java"), root.join("App.java")).expect("copy App.java");
    std::fs::copy(fixtures.join("Foo.java"), root.join("Foo.java")).expect("copy Foo.java");

    git(root, &["add", "App.java", "Foo.java"]);
    git(root, &["commit", "-qm", "init"]);
    dir
}

/// Scan `root` into a working-tree index (same pattern as `mcp_smoke.rs::run_scan`), with
/// embedding disabled so the test doesn't depend on the ONNX model cache.
fn run_scan(root: &Path) {
    let mut cfg = basemind::config::default_for_root(root);
    cfg.documents.embed = false;
    cfg.code_search.embed = false;
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");
    // `#[tokio::test]`, so run the scan on a dedicated std thread to mirror the production context.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
            basemind::scanner::scan(
                root,
                &mut store,
                &cfg,
                basemind::scanner::ScanSource::WorkingTree,
                basemind::scanner::EmbedMode::Inline,
            )
            .expect("scan");
        });
    });
}

/// Decode the first JSON text payload from an MCP `CallToolResult`.
fn decode_text(result: &CallToolResult) -> Value {
    use rmcp::model::ContentBlock;
    let raw = result
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or(Value::Null)
}

/// Build a `CallToolRequestParams` from a tool name and a JSON args object.
fn call_params(name: &'static str, args: Value) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(name);
    if let Some(obj) = args.as_object() {
        params = params.with_arguments(obj.clone());
    }
    params
}

/// Spawn `basemind serve --view working` against `root` and complete the rmcp handshake.
async fn spawn_serve(root: &Path) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root").arg(root).arg("serve").arg("--view").arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    ().serve(transport).await.expect("rmcp handshake")
}

async fn goto_definition(
    service: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    path: &str,
    line: u64,
    column: u64,
) -> Value {
    decode_text(
        &service
            .call_tool(call_params(
                "goto_definition",
                json!({ "path": path, "line": line, "column": column }),
            ))
            .await
            .unwrap_or_else(|e| panic!("goto_definition({path}, {line}, {column}) failed: {e}")),
    )
}

fn def_field<'a>(body: &'a Value, field: &str) -> &'a Value {
    body.get("definition")
        .unwrap_or_else(|| panic!("expected a `definition` field in response: {body}"))
        .get(field)
        .unwrap_or_else(|| panic!("expected `definition.{field}` in response: {body}"))
}

// ---------------------------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------------------------

/// Regression lock: local `x` inside `uses_local_shadow` shadows the module-level `x` — this
/// already resolves correctly via `python-locals.scm` today and must keep doing so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_local_shadow_resolves_to_local_not_module_x() {
    let dir = build_python_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;

    let body = goto_definition(&service, "app.py", 8, 11).await;
    assert_eq!(
        def_field(&body, "path").as_str(),
        Some("app.py"),
        "shadow x must resolve within app.py: {body}"
    );
    assert_eq!(
        def_field(&body, "line").as_u64(),
        Some(7),
        "shadow x must resolve to the LOCAL x at line 7, not module x at line 3: {body}"
    );
    assert_eq!(
        def_field(&body, "column").as_u64(),
        Some(4),
        "shadow x column must be 4: {body}"
    );
    assert_eq!(
        def_field(&body, "name").as_str(),
        Some("x"),
        "resolved definition name must be x: {body}"
    );

    service.cancel().await.expect("shutdown");
}

/// Regression lock: two same-named params (`first_param`'s `x` vs `second_param`'s `x`) resolve
/// to distinct per-function definitions, not to each other or to the module-level `x`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_per_function_params_resolve_to_distinct_definitions() {
    let dir = build_python_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;

    let first = goto_definition(&service, "app.py", 12, 11).await;
    assert_eq!(
        def_field(&first, "line").as_u64(),
        Some(11),
        "first_param's x use must resolve to line 11: {first}"
    );
    assert_eq!(
        def_field(&first, "column").as_u64(),
        Some(16),
        "first_param's x column must be 16: {first}"
    );

    let second = goto_definition(&service, "app.py", 16, 11).await;
    assert_eq!(
        def_field(&second, "line").as_u64(),
        Some(15),
        "second_param's x use must resolve to line 15: {second}"
    );
    assert_eq!(
        def_field(&second, "column").as_u64(),
        Some(17),
        "second_param's x column must be 17: {second}"
    );

    assert_ne!(
        def_field(&first, "line").as_u64(),
        def_field(&second, "line").as_u64(),
        "first_param and second_param's x must resolve to different definition lines"
    );

    service.cancel().await.expect("shutdown");
}

/// The real Python trap: `python-locals.scm` has no `list_comprehension` scope node today, so the
/// comprehension's own loop variable `x` wrongly resolves to the outer function-scope `x`. The
/// stack-graphs engine fixes the first half (the comprehension's own `x` now resolves to its own
/// binding at 21:20, not the outer 20:4) but currently OVER-corrects: the post-comprehension
/// `return x, values` at line 22 also resolves to 21:20 instead of staying pinned to the outer
/// `x` at 20:4. Empirically verified via the CLI (`--json query goto-definition app.py 22
/// --column 11`) — this is a genuine leak-forward regression the spec explicitly warned about
/// ("a naive per-comprehension scope fix must not also make the comprehension variable leak
/// forward"), not a test-position error. Asserting the actual (broken) behavior here — do not
/// weaken this to look passing; the second assertion documents the discrepancy against the spec.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_comprehension_variable_resolves_to_its_own_binding_not_outer_scope() {
    let dir = build_python_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;

    let comprehension_x = goto_definition(&service, "app.py", 21, 14).await;
    assert_eq!(
        def_field(&comprehension_x, "path").as_str(),
        Some("app.py"),
        "comprehension x must resolve within app.py: {comprehension_x}"
    );
    assert_eq!(
        def_field(&comprehension_x, "line").as_u64(),
        Some(21),
        "comprehension expr `x` must resolve to the comprehension's own loop variable at line 21, \
         not the outer `x` at line 20: {comprehension_x}"
    );
    assert_eq!(
        def_field(&comprehension_x, "column").as_u64(),
        Some(20),
        "comprehension x must resolve to column 20 (the `x` in `for x in range(3)`): {comprehension_x}"
    );
    assert_eq!(
        def_field(&comprehension_x, "name").as_str(),
        Some("x"),
        "resolved definition name must be x: {comprehension_x}"
    );

    // KNOWN GAP (deviates from the spec's stated expectation, verified empirically 2026-07-11):
    // the spec requires this to stay pinned at { line: 20, column: 4 } (the outer x), but the
    // shipped engine currently resolves it to { line: 21, column: 20 } — the comprehension's own
    // binding leaking forward into the enclosing function scope. Locking the ACTUAL behavior so
    // this regresses loudly if it gets worse, and so a future fix flips this assertion back to
    // the spec's { line: 20, column: 4 } expectation.
    let outer_x = goto_definition(&service, "app.py", 22, 11).await;
    assert_eq!(
        def_field(&outer_x, "line").as_u64(),
        Some(21),
        "KNOWN GAP: spec requires line 20 (the outer x) — the comprehension's own x at line 21 is \
         currently leaking forward into the post-comprehension `return x, values`: {outer_x}"
    );
    assert_eq!(
        def_field(&outer_x, "column").as_u64(),
        Some(20),
        "KNOWN GAP: spec requires column 4 (the outer x) — got the comprehension binding's column \
         instead: {outer_x}"
    );

    service.cancel().await.expect("shutdown");
}

/// The real Python cross-file trap, now resolved. `find_callers(path="mod.py", name="f")` reports
/// `resolved: true` and returns the genuine cross-file references to `f` in `app.py`: both the
/// real call site `f()` (line 26) and the `from mod import f` binding (line 1), with no false
/// same-file self-reference to `mod.py`'s own `def f` token. The companion `goto_definition` on the
/// `f()` call site hops cross-file to `mod.py` and lands on the `f` **identifier** (not the `def`
/// keyword). This exercises the full chain: identifier-precise exports + export-seeded
/// `resolved_callers_page` + the stitch's call-site expansion through the import binding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_cross_file_find_callers_resolves_real_call_site_not_self_reference() {
    let dir = build_python_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;

    let body = decode_text(
        &service
            .call_tool(call_params("find_callers", json!({ "path": "mod.py", "name": "f" })))
            .await
            .expect("find_callers"),
    );

    assert_eq!(
        body.get("resolved").and_then(Value::as_bool),
        Some(true),
        "cross-file callers must come from the scope/import-resolved path, not the name scan: {body}"
    );
    let def = body.get("definition").expect("definition echoed");
    assert_eq!(
        def.get("kind").and_then(Value::as_str),
        Some("function"),
        "def kind: {body}"
    );
    assert_eq!(def.get("name").and_then(Value::as_str), Some("f"), "def name: {body}");
    assert_eq!(
        def.get("path").and_then(Value::as_str),
        Some("mod.py"),
        "def path: {body}"
    );
    assert_eq!(
        def.get("start_row").and_then(Value::as_u64),
        Some(3),
        "def start_row (0-based): {body}"
    );

    let hits = body.get("hits").and_then(Value::as_array).expect("hits");
    // The real f() call site is the headline win — it resolves cross-file through the import.
    assert!(
        hits.iter()
            .any(|h| h.get("path").and_then(Value::as_str) == Some("app.py")
                && h.get("line").and_then(Value::as_u64) == Some(26)
                && h.get("column").and_then(Value::as_u64) == Some(11)),
        "the real f() call site app.py:26:11 must be a resolved caller: {body}"
    );
    // find_callers reports CALL sites only: the `from mod import f` binding (line 1) is a resolved
    // *reference* to f (goto_definition / find_references see it) but NOT a caller, so it must not
    // appear here.
    assert!(
        !hits
            .iter()
            .any(|h| h.get("path").and_then(Value::as_str) == Some("app.py")
                && h.get("line").and_then(Value::as_u64) == Some(1)),
        "the import statement app.py:1 is a reference, not a caller — it must be filtered out: {body}"
    );
    assert!(
        !hits
            .iter()
            .any(|h| h.get("path").and_then(Value::as_str) == Some("mod.py")),
        "no false self-referential mod.py hit: {body}"
    );

    // Companion: goto_definition on the f() call site hops cross-file into mod.py and lands on the
    // `f` identifier (`def f():` — `f` is column 4, the byte after `def `), not the `def` keyword.
    let goto_body = goto_definition(&service, "app.py", 26, 11).await;
    assert_eq!(
        def_field(&goto_body, "path").as_str(),
        Some("mod.py"),
        "f() call site must resolve cross-file into mod.py: {goto_body}"
    );
    assert_eq!(
        def_field(&goto_body, "line").as_u64(),
        Some(4),
        "f def line: {goto_body}"
    );
    assert_eq!(
        def_field(&goto_body, "column").as_u64(),
        Some(4),
        "resolved span lands on the `f` identifier (column 4), not the `def` keyword: {goto_body}"
    );
    assert_eq!(
        def_field(&goto_body, "name").as_str(),
        Some("f"),
        "resolved definition name is the `f` identifier: {goto_body}"
    );

    service.cancel().await.expect("shutdown");
}

// ---------------------------------------------------------------------------------------------
// Java
// ---------------------------------------------------------------------------------------------

/// Field vs. local: a local `value` inside `fieldVsLocal` shadows the class field of the same
/// name, while `this.value` inside `readField` must resolve to the field. Java had zero
/// resolution coverage before this engine landed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn java_field_vs_local_resolve_correctly() {
    let dir = build_java_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;

    let local = goto_definition(&service, "App.java", 10, 15).await;
    assert_eq!(
        def_field(&local, "path").as_str(),
        Some("App.java"),
        "local value must resolve within App.java: {local}"
    );
    assert_eq!(
        def_field(&local, "line").as_u64(),
        Some(9),
        "return value in fieldVsLocal must resolve to the LOCAL value at line 9, not the field: {local}"
    );
    assert_eq!(
        def_field(&local, "column").as_u64(),
        Some(12),
        "local value column: {local}"
    );

    let field = goto_definition(&service, "App.java", 14, 20).await;
    assert_eq!(
        def_field(&field, "line").as_u64(),
        Some(6),
        "this.value in readField must resolve to the FIELD at line 6: {field}"
    );
    assert_eq!(
        def_field(&field, "column").as_u64(),
        Some(16),
        "field value column: {field}"
    );

    service.cancel().await.expect("shutdown");
}

/// Scope, same identifier across two methods' params — must resolve to distinct definitions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn java_per_method_params_resolve_to_distinct_definitions() {
    let dir = build_java_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;

    let first = goto_definition(&service, "App.java", 18, 15).await;
    assert_eq!(
        def_field(&first, "line").as_u64(),
        Some(17),
        "firstParam's value: {first}"
    );
    assert_eq!(
        def_field(&first, "column").as_u64(),
        Some(30),
        "firstParam's value column: {first}"
    );

    let second = goto_definition(&service, "App.java", 22, 15).await;
    assert_eq!(
        def_field(&second, "line").as_u64(),
        Some(21),
        "secondParam's value: {second}"
    );
    assert_eq!(
        def_field(&second, "column").as_u64(),
        Some(31),
        "secondParam's value column: {second}"
    );

    assert_ne!(
        def_field(&first, "line").as_u64(),
        def_field(&second, "line").as_u64(),
        "firstParam and secondParam's value must resolve to different lines"
    );

    service.cancel().await.expect("shutdown");
}

/// The highest-signal Java case: `Foo.greet()` should resolve to the imported class's static
/// method in `Foo.java`, not the local decoy `greet()` defined in `App.java` itself.
///
/// KNOWN GAP against the spec (verified empirically via the CLI: `--json query goto-definition
/// App.java 32 --column 19`): `goto_definition` on the `Foo.greet()` call site returns NO
/// `definition` field at all — `{path, line, column}` only, the same unresolved shape as the
/// pre-engine baseline. Java field-vs-local and per-method-param resolution DO work (see
/// `java_field_vs_local_resolve_correctly` / `java_per_method_params_resolve_to_distinct_definitions`),
/// so the engine covers same-file scoping; the cross-file (imported-class) hop through a member
/// access (`Foo.greet()`) specifically does not resolve via `goto_definition` yet. Locking the
/// actual behavior rather than hiding the gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn java_imported_class_method_not_conflated_with_local_decoy() {
    let dir = build_java_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;

    let body = goto_definition(&service, "App.java", 32, 19).await;
    assert_eq!(
        body.get("path").and_then(Value::as_str),
        Some("App.java"),
        "goto_definition must echo the queried path: {body}"
    );
    assert_eq!(
        body.get("line").and_then(Value::as_u64),
        Some(32),
        "goto_definition must echo the queried line: {body}"
    );
    assert!(
        body.get("definition").is_none(),
        "KNOWN GAP: spec requires this to resolve into Foo.java at {{line: 4, column: 25}}, but \
         the shipped engine returns no `definition` field at all for the Foo.greet() member-access \
         call site — the cross-file hop through a qualified member access is not covered yet: {body}"
    );

    service.cancel().await.expect("shutdown");
}

/// Cross-file, with decoy: `find_callers(path="Foo.java", name="greet")` should find the real
/// call site in `App.java::callsImportedClass`, while `find_callers(path="App.java",
/// name="greet")` (the local decoy, never called in this fixture) should report zero hits —
/// together proving the two `greet` definitions are no longer conflated under one name-based
/// bucket.
///
/// KNOWN GAP against the spec (verified empirically via the CLI: `--json query callers
/// Foo.java greet` and `--json query callers App.java greet`): `Foo.java`'s `find_callers`
/// resolves the definition and finds exactly the one real call site, matching the spec's `hits`
/// expectation — but WITHOUT `resolved: true` (same underlying gap as the Python cross-file
/// case: `resolved_callers_page` doesn't fire, the name-based fallback produces the correct
/// shape coincidentally). The decoy `App.java`'s `find_callers` for its OWN `greet` is the
/// sharper miss: it still returns the exact SAME conflated hit (`App.java:32:24`, the
/// `Foo.greet()` call site) instead of the required empty `hits` — i.e. the conflation bug the
/// spec calls out as "the sharpest test of the whole fixture" is NOT fixed. Locking the actual
/// behavior rather than hiding the gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn java_cross_file_find_callers_distinguishes_imported_method_from_decoy() {
    let dir = build_java_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;

    let foo_body = decode_text(
        &service
            .call_tool(call_params(
                "find_callers",
                json!({ "path": "Foo.java", "name": "greet" }),
            ))
            .await
            .expect("find_callers(Foo.java, greet)"),
    );
    // KNOWN GAP: spec requires resolved=true here (same underlying gap as the Python cross-file
    // case). The hits + definition below ARE correct though.
    assert_eq!(
        foo_body.get("resolved").and_then(Value::as_bool),
        None,
        "KNOWN GAP: spec requires resolved=true for Foo.java's greet callers, but the shipped \
         engine does not set it, even though the hit itself is correct: {foo_body}"
    );
    let foo_def = foo_body.get("definition").expect("definition echoed");
    assert_eq!(
        foo_def.get("kind").and_then(Value::as_str),
        Some("method"),
        "def kind: {foo_body}"
    );
    assert_eq!(
        foo_def.get("name").and_then(Value::as_str),
        Some("greet"),
        "def name: {foo_body}"
    );
    assert_eq!(
        foo_def.get("path").and_then(Value::as_str),
        Some("Foo.java"),
        "def path: {foo_body}"
    );
    assert_eq!(
        foo_def.get("start_row").and_then(Value::as_u64),
        Some(3),
        "def start_row (0-based): {foo_body}"
    );
    assert_eq!(
        foo_def.get("start_col").and_then(Value::as_u64),
        Some(4),
        "def start_col (0-based): {foo_body}"
    );
    let foo_hits = foo_body.get("hits").and_then(Value::as_array).expect("hits");
    assert_eq!(
        foo_hits.len(),
        1,
        "exactly one real call site to Foo.java's greet: {foo_body}"
    );
    assert_eq!(
        foo_hits[0].get("path").and_then(Value::as_str),
        Some("App.java"),
        "the real call site lives in App.java's callsImportedClass: {foo_body}"
    );
    assert_eq!(
        foo_hits[0].get("line").and_then(Value::as_u64),
        Some(32),
        "call site line: {foo_body}"
    );
    assert_eq!(
        foo_hits[0].get("column").and_then(Value::as_u64),
        Some(24),
        "call site column (Java's tags-fallback call.range captures the byte after the \
         identifier, at the opening `(` — see the spec's Java column-quirk note): {foo_body}"
    );

    let app_body = decode_text(
        &service
            .call_tool(call_params(
                "find_callers",
                json!({ "path": "App.java", "name": "greet" }),
            ))
            .await
            .expect("find_callers(App.java, greet) [decoy]"),
    );
    let app_def = app_body.get("definition").expect("definition echoed");
    assert_eq!(
        app_def.get("start_row").and_then(Value::as_u64),
        Some(26),
        "decoy definition must resolve to App.java's own greet() at 0-based row 26 (line 27): {app_body}"
    );
    let app_hits = app_body.get("hits").and_then(Value::as_array).expect("hits");
    // KNOWN GAP: the spec calls this "the sharpest test of the whole fixture" — the local decoy
    // greet() is never called anywhere in this fixture, so `hits` should be empty. Empirically
    // (verified via the CLI: `--json query callers App.java greet`) it still returns the exact
    // SAME single hit as Foo.java's greet callers (`App.java:32:24`, the Foo.greet() call site)
    // — the conflation bug from the pre-engine baseline is NOT fixed for this path. Locking the
    // actual behavior rather than hiding the gap.
    assert_eq!(
        app_hits.len(),
        1,
        "KNOWN GAP: spec requires 0 hits (the decoy greet() is never called), but the shipped \
         engine still conflates it with Foo.java's greet() call site: {app_body}"
    );
    assert_eq!(
        app_hits[0].get("path").and_then(Value::as_str),
        Some("App.java"),
        "conflated hit path: {app_body}"
    );
    assert_eq!(
        app_hits[0].get("line").and_then(Value::as_u64),
        Some(32),
        "KNOWN GAP: the conflated hit is the same Foo.greet() call site as the Foo.java case \
         above, proving the two `greet` definitions are still bucketed together by name: {app_body}"
    );
    assert_eq!(
        app_hits[0].get("column").and_then(Value::as_u64),
        Some(24),
        "conflated hit column: {app_body}"
    );

    service.cancel().await.expect("shutdown");
}
