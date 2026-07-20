//! Cross-file `goto_definition` through the real MCP tool.
//!
//! A use of an imported name must resolve to the definition in the file the import points at — not
//! stop at the local import binding. The shape mirrors the monorepo case that surfaced the gap: a
//! hook is exported from one module, imported into another, and *called* there.
//!
//! Two positions must both land in the defining file:
//! * the CALL SITE (`useCustomerSettings()`), and
//! * the IMPORT BINDING itself (`import { useCustomerSettings } ...`).
//!
//! The monorepo case is the one that regressed: imports go through a **tsconfig path alias**
//! (`@app/*` → `src/*`), not a relative path. With the alias unresolved, the cross-file stitch emits
//! no edge and `goto_definition` silently degrades — the call site lands on the import binding in the
//! SAME file, and the binding itself returns no definition at all.
//!
//! The negative cases are load-bearing: a same-named symbol in a file that was never imported must
//! NEVER be returned. A wrong jump is worse than no jump.
//!
//! Driven through `basemind serve` (not the library) because the MCP tool body performs a hop the
//! raw `query::definition_of` layer does not — the tool is the contract agents plan against.
#![cfg(feature = "code-intel-js")]

use std::fs;
use std::path::Path;

use basemind::config::ConfigV1;
use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::Command as AsyncCommand;

fn write(root: &Path, rel: &str, body: &str) {
    let abs = root.join(rel);
    fs::create_dir_all(abs.parent().unwrap()).unwrap();
    fs::write(abs, body).unwrap();
}

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

/// Byte column of the `nth` (0-based) occurrence of `needle` within `line`.
fn col_of(line: &str, needle: &str, nth: usize) -> u64 {
    line.match_indices(needle)
        .nth(nth)
        .unwrap_or_else(|| panic!("occurrence {nth} of {needle:?} in {line:?}"))
        .0 as u64
}

type Server = rmcp::service::RunningService<rmcp::RoleClient, ()>;

/// Scan `root`, then spawn `basemind serve` against it.
async fn serve(root: &Path) -> Server {
    basemind::store::init_isolated_cache();
    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    let cfg = ConfigV1::with_defaults();
    scan(root, &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Inline).unwrap();
    drop(store);

    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root").arg(root).arg("serve").arg("--view").arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    ().serve(transport).await.expect("rmcp handshake")
}

/// `goto_definition` at `path`:`line`:`column`; returns the `definition` object, if any.
async fn goto(service: &Server, path: &str, line: u64, column: u64) -> Option<Value> {
    let mut params = CallToolRequestParams::new("goto_definition");
    let args = json!({ "path": path, "line": line, "column": column });
    params = params.with_arguments(args.as_object().unwrap().clone());
    let result = service.call_tool(params).await.expect("goto_definition");
    decode_text(&result).get("definition").filter(|d| !d.is_null()).cloned()
}

/// A monorepo whose imports go through the tsconfig `@app/*` → `src/*` alias.
fn aliased_monorepo() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "tsconfig.json",
        "{\n  \"compilerOptions\": {\n    \"baseUrl\": \".\",\n    \"paths\": { \"@app/*\": [\"src/*\"] }\n  }\n}\n",
    );
    write(
        root,
        "src/hooks/useCustomerSettings.ts",
        "export function useCustomerSettings() {\n  return { theme: 'dark' };\n}\n",
    );
    write(
        root,
        "src/routes/SettingsRoutes.tsx",
        "import { useCustomerSettings } from '@app/hooks/useCustomerSettings';\n\
         \n\
         export function SettingsRoutes() {\n\
         \x20 const settings = useCustomerSettings();\n\
         \x20 return settings;\n\
         }\n",
    );
    write(
        root,
        "src/unrelated/useCustomerSettings.ts",
        "export function useCustomerSettings() {\n  return { theme: 'light' };\n}\n",
    );
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goto_definition_follows_an_aliased_import_from_the_call_site_to_the_defining_file() {
    let dir = aliased_monorepo();
    let service = serve(dir.path()).await;

    let call_col = col_of("  const settings = useCustomerSettings();", "useCustomerSettings", 0);
    let definition = goto(&service, "src/routes/SettingsRoutes.tsx", 4, call_col).await;

    let definition = definition.expect(
        "the call site must resolve to a definition — a tsconfig-aliased import is the norm in a \
         monorepo, not an exotic case",
    );
    assert_eq!(
        definition.get("path").and_then(Value::as_str),
        Some("src/hooks/useCustomerSettings.ts"),
        "the call site must resolve ACROSS the import into the defining file, not stop at the \
         import binding in SettingsRoutes.tsx: {definition}"
    );
    assert_eq!(
        definition.get("line").and_then(Value::as_u64),
        Some(1),
        "must land on the exported definition's line: {definition}"
    );
    assert_eq!(
        definition.get("name").and_then(Value::as_str),
        Some("useCustomerSettings"),
        "must land on the exported identifier: {definition}"
    );
    service.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goto_definition_resolves_the_aliased_import_binding_itself() {
    let dir = aliased_monorepo();
    let service = serve(dir.path()).await;

    let binding_col = col_of(
        "import { useCustomerSettings } from '@app/hooks/useCustomerSettings';",
        "useCustomerSettings",
        0,
    );
    let definition = goto(&service, "src/routes/SettingsRoutes.tsx", 1, binding_col).await;

    let definition =
        definition.expect("standing ON the import binding must resolve to what it names — not silently return nothing");
    assert_eq!(
        definition.get("path").and_then(Value::as_str),
        Some("src/hooks/useCustomerSettings.ts"),
        "the import binding must resolve to the definition it names: {definition}"
    );
    service.cancel().await.ok();
}

/// The negative that stops a "fix" which merely name-matches across the repo: an un-imported,
/// same-named export in an unrelated file must NEVER be the answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goto_definition_never_jumps_to_an_unimported_same_named_symbol() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "tsconfig.json",
        "{\n  \"compilerOptions\": {\n    \"baseUrl\": \".\",\n    \"paths\": { \"@app/*\": [\"src/*\"] }\n  }\n}\n",
    );
    write(
        root,
        "src/hooks/useCustomerSettings.ts",
        "export function useCustomerSettings() {\n  return 1;\n}\n",
    );
    write(
        root,
        "src/routes/Orphan.tsx",
        "export function Orphan() {\n  const settings = useCustomerSettings();\n  return settings;\n}\n",
    );
    let service = serve(root).await;

    let call_col = col_of("  const settings = useCustomerSettings();", "useCustomerSettings", 0);
    let definition = goto(&service, "src/routes/Orphan.tsx", 2, call_col).await;

    assert_eq!(
        definition, None,
        "an unbound call must resolve to NOTHING — never to a same-named symbol in a file that was \
         never imported. A wrong jump is worse than no jump: {definition:?}"
    );
    service.cancel().await.ok();
}

/// An alias that maps to no file on disk must stay unresolved. Guards against a resolver that
/// invents a target from the alias pattern alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goto_definition_leaves_an_alias_with_no_target_file_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "tsconfig.json",
        "{\n  \"compilerOptions\": {\n    \"baseUrl\": \".\",\n    \"paths\": { \"@app/*\": [\"src/*\"] }\n  }\n}\n",
    );
    write(
        root,
        "src/routes/Ghost.tsx",
        "import { missingHook } from '@app/hooks/missingHook';\n\
         \n\
         export function Ghost() {\n\
         \x20 return missingHook();\n\
         }\n",
    );
    let service = serve(root).await;

    let binding_col = col_of(
        "import { missingHook } from '@app/hooks/missingHook';",
        "missingHook",
        0,
    );
    let definition = goto(&service, "src/routes/Ghost.tsx", 1, binding_col).await;
    assert_eq!(
        definition, None,
        "an alias whose target file does not exist must stay unresolved: {definition:?}"
    );
    service.cancel().await.ok();
}

/// Plain relative imports must keep working — the tsconfig plumbing must not regress the case that
/// already resolved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goto_definition_still_follows_a_relative_import() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "hooks/useCustomerSettings.ts",
        "export function useCustomerSettings() {\n  return 1;\n}\n",
    );
    write(
        root,
        "routes/SettingsRoutes.tsx",
        "import { useCustomerSettings } from '../hooks/useCustomerSettings';\n\
         \n\
         export function SettingsRoutes() {\n\
         \x20 const settings = useCustomerSettings();\n\
         \x20 return settings;\n\
         }\n",
    );
    let service = serve(root).await;

    let call_col = col_of("  const settings = useCustomerSettings();", "useCustomerSettings", 0);
    let definition = goto(&service, "routes/SettingsRoutes.tsx", 4, call_col)
        .await
        .expect("a relative import's call site must resolve");
    assert_eq!(
        definition.get("path").and_then(Value::as_str),
        Some("hooks/useCustomerSettings.ts"),
        "relative-import call site must still cross the file boundary: {definition}"
    );
    service.cancel().await.ok();
}
