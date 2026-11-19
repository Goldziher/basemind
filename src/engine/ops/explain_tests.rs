//! `code explain`: section parsing, the doc join, macros, owners and the budget cut, then briefings
//! on a scanned Rust crate (symbol, `path:line` and file targets, ambiguity and qualifiers, stale and
//! unreadable files, edgeless and unresolved languages, callers by depth, calls by file, limits, the
//! budget, refusals).

use super::file::{line_count, owners};
use super::pick::doc_above;
use super::*;
use crate::engine::types::explain::ExplainDefinition;
use crate::extract::{DocComment, Symbol, SymbolKind};

fn symbol(name: &str, start_byte: u32, end_byte: u32) -> Symbol {
    Symbol {
        name: name.to_string(),
        kind: SymbolKind::Function,
        start_byte,
        end_byte,
        start_row: 0,
        start_col: 0,
        signature: None,
        decorators: Vec::new(),
    }
}

/// The doc comment `/// <text>` at the start of `source`.
fn doc_at_start(source: &str) -> DocComment {
    let end = source.find('\n').expect("a doc line");
    DocComment {
        text: source[..end].to_string(),
        start_byte: 0,
        end_byte: end as u32,
    }
}

/// The doc `doc_above` finds for the `fn` that `source` ends with.
fn doc_of(source: &str) -> Option<String> {
    let start = source
        .rfind("fn ")
        .or_else(|| source.rfind("def "))
        .expect("a definition") as u32;
    let docs = [doc_at_start(source)];
    doc_above(&docs, &symbol("f", start, source.len() as u32), source.as_bytes()).map(|(text, _)| text)
}

#[test]
fn sections_default_to_every_known_one_and_refuse_an_unknown_one() {
    assert_eq!(sections(None, SYMBOL_SECTIONS).unwrap(), SYMBOL_SECTIONS);
    assert_eq!(
        sections(Some("callers, body,"), SYMBOL_SECTIONS).unwrap(),
        ["callers", "body"]
    );
    let refused = sections(Some("outline"), SYMBOL_SECTIONS).unwrap_err().to_string();
    assert!(
        refused.contains("no section `outline`") && refused.contains("callers"),
        "{refused}"
    );
}

#[test]
fn a_doc_joins_across_whitespace_attributes_and_decorators_but_not_code() {
    assert_eq!(doc_of("/// Adds.\nfn add() {}").as_deref(), Some("/// Adds."));
    assert_eq!(
        doc_of("/// Kept.\n#[inline]\n#[must_use]\nfn sub() {}").as_deref(),
        Some("/// Kept.")
    );
    assert_eq!(
        doc_of("/// Spans.\n#[cfg(any(\n    unix,\n    windows = \"]\"\n))]\nfn f() {}").as_deref(),
        Some("/// Spans.")
    );
    assert_eq!(
        doc_of("/// Decorated.\n@app.route(\"/x\",\n    methods=[\"GET\"])\ndef f(): pass").as_deref(),
        Some("/// Decorated.")
    );
    assert_eq!(doc_of("/// Lost.\nlet x = 1;\nfn f() {}"), None);
    assert_eq!(doc_of("/// Open.\n#[cfg(unix\nfn f() {}"), None);
    let docs = [doc_at_start("/// Here.\nfn f() {}")];
    assert_eq!(doc_above(&docs, &symbol("first", 0, 5), b"fn f() {}"), None);
}

#[test]
fn a_macro_invocation_is_told_from_a_call() {
    let source = b"println!(\"x\"); bee(); vec ![1];";
    assert!(is_macro(source, 0, "println"));
    assert!(!is_macro(source, 15, "bee"));
    assert!(!is_macro(source, 22, "vec"));
    assert!(!is_macro(source, 400, "gone"));
}

#[test]
fn owners_rank_by_commits_then_name_and_lines_count_an_open_last_line() {
    let ranked = owners(["b", "a", "b", "c", "a", "b"].into_iter());
    let rows: Vec<(&str, u32)> = ranked.iter().map(|o| (o.author.as_str(), o.commits)).collect();
    assert_eq!(rows, [("b", 3), ("a", 2), ("c", 1)]);
    assert_eq!(line_count(b"a\nb\n"), 2);
    assert_eq!(line_count(b"a\nb"), 2);
    assert_eq!(line_count(b""), 0);
}

fn cuts(response: &ExplainResponse) -> Vec<(&str, usize, Option<usize>, &str, &str)> {
    response
        .truncated_sections
        .iter()
        .map(|c| (c.section, c.shown, c.total, c.reason, c.next.as_str()))
        .collect()
}

#[test]
fn the_budget_drops_the_body_then_halves_the_longest_list_and_says_so() {
    let mut briefing = Briefing {
        response: ExplainResponse {
            target: "symbol",
            body: Some("x".repeat(400)),
            callees: Some((0..40).map(|i| format!("callee_{i}")).collect()),
            importers: Some(vec![RelPath::from("a.rs"), RelPath::from("b.rs")]),
            ..ExplainResponse::default()
        },
        next: AHashMap::from_iter([("callees", "code_explain {}".to_string())]),
    };
    fit(&mut briefing, 150, None).unwrap();
    let response = &briefing.response;
    assert!(response.body.is_none());
    assert!(tokens(response) <= 150, "{}", tokens(response));
    let callees = response.callees.as_ref().unwrap().len();
    assert!(callees < 40, "{callees}");
    let cuts = cuts(response);
    assert_eq!(cuts[0], ("body", 0, None, "budget", ""));
    assert_eq!(cuts[1], ("callees", callees, Some(40), "budget", "code_explain {}"));

    let mut roomy = Briefing {
        response: ExplainResponse {
            target: "symbol",
            body: Some("x".to_string()),
            ..ExplainResponse::default()
        },
        next: AHashMap::new(),
    };
    fit(&mut roomy, 6_000, None).unwrap();
    assert!(roomy.response.body.is_some() && roomy.response.truncated_sections.is_empty());
}

/// Every list holds at most one row, and the doc, signature and notes alone are over the budget.
fn over_budget_with_short_lists() -> Briefing {
    Briefing {
        response: ExplainResponse {
            target: "symbol",
            definition: Some(ExplainDefinition {
                path: RelPath::from("src/min.rs"),
                name: "id".to_string(),
                kind: "function",
                line: 1,
                end_line: 1,
                signature: Some(format!("fn id() {{ {} }}", "x".repeat(2_000))),
                doc: Some("/// ".to_string() + &"d".repeat(1_000)),
                stale: false,
            }),
            callers: Some(vec![("src/a.rs".to_string(), json!({"line": 3}))]),
            also_defined: vec!["src/b.rs:1".to_string()],
            notes: (0..30).map(|i| format!("note {i}: {}", "n".repeat(60))).collect(),
            ..ExplainResponse::default()
        },
        next: AHashMap::from_iter([
            ("signature", "code_read {\"line\":1}".to_string()),
            ("notes", "code_explain {\"max_tokens\":24000}".to_string()),
        ]),
    }
}

#[test]
fn the_budget_cuts_scalars_and_notes_when_every_list_is_short() {
    let mut briefing = over_budget_with_short_lists();
    fit(&mut briefing, 300, None).unwrap();
    let response = &briefing.response;
    assert!(tokens(response) <= 300, "{}", tokens(response));
    let definition = response.definition.as_ref().unwrap();
    assert!(definition.signature.is_none() && definition.doc.is_none());
    assert!(response.callers.as_ref().unwrap().is_empty() && response.also_defined.is_empty());
    let sections: Vec<&str> = response.truncated_sections.iter().map(|c| c.section).collect();
    for section in ["callers", "also_defined", "doc", "signature", "notes"] {
        assert!(sections.contains(&section), "{section} not listed: {sections:?}");
    }
    let signature = response
        .truncated_sections
        .iter()
        .find(|c| c.section == "signature")
        .unwrap();
    assert_eq!(signature.next, "code_read {\"line\":1}");
}

#[test]
fn the_budget_collapses_to_one_row_then_refuses_below_the_bare_answer() {
    let mut briefing = over_budget_with_short_lists();
    briefing.response.notes = vec!["n".repeat(4_000)];
    fit(&mut briefing, 80, Some("callers")).unwrap();
    let response = &briefing.response;
    assert!(tokens(response) <= 80, "{}", tokens(response));
    let rows = cuts(response);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (section, shown, total, reason, next) = rows[0];
    assert_eq!((section, shown, total, reason), ("all", 0, None, "budget"));
    assert!(
        next.starts_with("code_explain {")
            && next.contains("\"target\":\"src/min.rs:1\"")
            && next.contains("\"sections\":\"callers\""),
        "{next}"
    );
    assert!(response.notes.is_empty() && response.callers.is_none());

    let mut tiny = over_budget_with_short_lists();
    let refused = fit(&mut tiny, 10, None).unwrap_err().to_string();
    assert!(refused.contains("pass at least"), "{refused}");
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
mod scanned {
    use super::*;
    use crate::engine::ops::graph_report::tests::{rust_crate, scanned_workspace, write};

    fn workspace_with(extra: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
        crate::store::init_isolated_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        rust_crate(dir.path());
        write(
            dir.path(),
            "src/b.rs",
            "use crate::a::ay;\n/// Bee.\npub fn bee() {}\npub fn _x() { ay(); }\n",
        );
        for (path, body) in extra {
            write(dir.path(), path, body);
        }
        let ws = scanned_workspace(dir.path(), false);
        (dir, ws)
    }

    fn workspace() -> (tempfile::TempDir, Workspace) {
        workspace_with(&[])
    }

    fn explain_with(ws: &Workspace, p: ExplainParams) -> ExplainResponse {
        let target = p.target.clone();
        ws.code_explain(p).unwrap_or_else(|error| panic!("{target}: {error}"))
    }

    fn explain(ws: &Workspace, target: &str) -> ExplainResponse {
        explain_with(
            ws,
            ExplainParams {
                target: target.to_string(),
                ..ExplainParams::default()
            },
        )
    }

    fn refuse(ws: &Workspace, target: &str) -> String {
        ws.code_explain(ExplainParams {
            target: target.to_string(),
            ..ExplainParams::default()
        })
        .unwrap_err()
        .to_string()
    }

    fn caller_lines(response: &ExplainResponse) -> Vec<String> {
        response
            .callers
            .iter()
            .flatten()
            .map(|(path, row)| format!("{path}:{}", row["line"]))
            .collect()
    }

    fn paths(files: &Option<Vec<RelPath>>) -> Vec<String> {
        files.iter().flatten().map(ToString::to_string).collect()
    }

    fn at(response: &ExplainResponse) -> String {
        let definition = response.definition.as_ref().expect("definition");
        format!("{}:{}", definition.path, definition.line)
    }

    #[test]
    fn a_symbol_briefing_holds_its_definition_callers_callees_and_importers() {
        let (_dir, ws) = workspace();
        let ay = explain(&ws, "ay");
        assert_eq!(ay.target, "symbol");
        let definition = ay.definition.as_ref().expect("definition");
        assert_eq!(
            (
                definition.path.to_string(),
                definition.line,
                definition.end_line,
                definition.kind
            ),
            ("src/a.rs".to_string(), 2, 2, "function")
        );
        assert_eq!(ay.body.as_deref(), Some("pub fn ay() { bee(); }"));
        assert_eq!(caller_lines(&ay), ["src/b.rs:4", "src/c.rs:2"]);
        assert_eq!(ay.callers_total, Some(2));
        assert!(ay.callers_resolution_default.is_some());
        assert_eq!(ay.callees.as_deref(), Some(&["bee".to_string()][..]));
        assert_eq!(paths(&ay.importers), ["src/b.rs", "src/c.rs"]);
        assert!(ay.indirect_callers.is_none());
        assert!(ay.truncated_sections.is_empty(), "{:?}", ay.truncated_sections);
        assert!(
            ay.notes.iter().any(|note| note.starts_with("history:")),
            "{:?}",
            ay.notes
        );

        let bee = explain(&ws, "bee");
        assert_eq!(bee.definition.as_ref().unwrap().doc.as_deref(), Some("/// Bee."));

        let at = explain(&ws, "src/b.rs:4");
        assert_eq!(at.definition.as_ref().unwrap().name, "_x");
        assert_eq!(at.callees.as_deref(), Some(&["ay".to_string()][..]));
    }

    #[test]
    fn callees_leave_macros_out_and_a_doc_above_an_attribute_is_kept() {
        let (_dir, ws) = workspace_with(&[(
            "src/m.rs",
            "use crate::b::bee;\n/// Documented.\n#[inline]\n#[cfg(any(\n    unix,\n    windows\n))]\npub fn mac() {\n    \
             println!(\"{}\", 1);\n    let _v = vec![1];\n    bee();\n}\n",
        )]);
        let mac = explain(&ws, "mac");
        assert_eq!(mac.callees.as_deref(), Some(&["bee".to_string()][..]));
        assert_eq!(mac.definition.as_ref().unwrap().doc.as_deref(), Some("/// Documented."));
    }

    #[test]
    fn depth_follows_only_resolved_callers_and_is_refused_where_it_cannot_apply() {
        let (_dir, ws) = workspace();
        let deep = explain_with(
            &ws,
            ExplainParams {
                target: "ay".to_string(),
                depth: Some(2),
                ..ExplainParams::default()
            },
        );
        // Rust call sites carry no resolution facts, so no caller is proven to bind and none is walked.
        assert_eq!(deep.indirect_callers.as_deref().map(<[_]>::len), Some(0));
        assert!(
            deep.notes.iter().any(|note| note.starts_with("callers beyond depth 1")),
            "{:?}",
            deep.notes
        );
        let file = refuse_depth_on_a_file(&ws);
        assert!(file.contains("`depth`"), "{file}");
        let too_deep = ws
            .code_explain(ExplainParams {
                target: "ay".to_string(),
                depth: Some(4),
                ..ExplainParams::default()
            })
            .unwrap_err()
            .to_string();
        assert!(too_deep.contains("1 to 3"), "{too_deep}");
    }

    #[cfg(feature = "code-intel-js")]
    #[test]
    fn depth_walks_callers_of_callers_over_resolved_edges() {
        crate::store::init_isolated_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "basemind.toml", "");
        write(root, "a.ts", "export function ay() {}\n");
        write(
            root,
            "c.ts",
            "import { ay } from \"./a\";\nexport function sea() {\n  ay();\n}\n",
        );
        write(
            root,
            "d.ts",
            "import { sea } from \"./c\";\nexport function dee() {\n  sea();\n}\n",
        );
        write(
            root,
            "e.ts",
            "import { dee } from \"./d\";\nexport function eee() {\n  dee();\n}\n",
        );
        let ws = scanned_workspace(root, false);
        let deep = explain_with(
            &ws,
            ExplainParams {
                target: "ay".to_string(),
                depth: Some(3),
                ..ExplainParams::default()
            },
        );
        assert_eq!(caller_lines(&deep), ["c.ts:3"]);
        let rows: Vec<(u32, String, String, String)> = deep
            .indirect_callers
            .iter()
            .flatten()
            .map(|row| {
                (
                    row.depth,
                    format!("{}:{}", row.path, row.line),
                    row.callee.clone(),
                    row.callee_at.clone(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                (2, "d.ts:3".to_string(), "sea".to_string(), "c.ts:2".to_string()),
                (3, "e.ts:3".to_string(), "dee".to_string(), "d.ts:2".to_string()),
            ],
            "{:?}",
            deep.notes
        );
        let file = explain(&ws, "d.ts");
        let calls_out: Vec<(String, u32, u32)> = file
            .calls_out
            .iter()
            .flatten()
            .map(|row| (row.path.to_string(), row.calls, row.resolved))
            .collect();
        assert_eq!(calls_out, [("c.ts".to_string(), 1, 1)], "{:?}", file.notes);
    }

    fn refuse_depth_on_a_file(ws: &Workspace) -> String {
        ws.code_explain(ExplainParams {
            target: "src/a.rs".to_string(),
            depth: Some(2),
            ..ExplainParams::default()
        })
        .unwrap_err()
        .to_string()
    }

    #[test]
    fn a_file_briefing_holds_its_outline_imports_importers_and_calls_by_file() {
        let (_dir, ws) = workspace();
        let file = explain(&ws, "./src/a.rs");
        assert_eq!(file.target, "file");
        assert_eq!(file.path.as_ref().map(ToString::to_string).as_deref(), Some("src/a.rs"));
        assert_eq!(file.lines, Some(2));
        let outline: Vec<(&str, u32)> = file
            .outline
            .iter()
            .flatten()
            .map(|row| (row.name.as_str(), row.line))
            .collect();
        assert_eq!(outline, [("ay", 2)]);
        assert_eq!(paths(&file.imports), ["src/b.rs"]);
        assert_eq!(paths(&file.importers), ["src/b.rs", "src/c.rs"]);
        let calls_in: Vec<(String, u32, Vec<String>)> = file
            .calls_in
            .iter()
            .flatten()
            .map(|row| (row.path.to_string(), row.calls, row.names.clone()))
            .collect();
        assert_eq!(
            calls_in,
            [
                ("src/b.rs".to_string(), 1, vec!["ay".to_string()]),
                ("src/c.rs".to_string(), 1, vec!["ay".to_string()])
            ]
        );
        // No Rust resolution facts: `bee` counts toward the one file defining it, unresolved.
        let calls_out: Vec<(String, u32, u32)> = file
            .calls_out
            .iter()
            .flatten()
            .map(|row| (row.path.to_string(), row.calls, row.resolved))
            .collect();
        assert_eq!(calls_out, [("src/b.rs".to_string(), 1, 0)], "{:?}", file.notes);
        assert_eq!(file.calls_out_unresolved, Some(0));
        assert!(file.definition.is_none() && file.callers.is_none());
    }

    #[test]
    fn an_edgeless_file_has_no_neighbours_and_an_unresolved_language_says_so() {
        let (_dir, ws) = workspace_with(&[
            ("src/lone.rs", "pub fn lone() {}\n"),
            ("src/native.c", "int native(void) { return 0; }\n"),
        ]);
        let lone = explain(&ws, "src/lone.rs");
        assert_eq!(lone.imports.as_deref(), Some(&[][..]));
        assert_eq!(lone.importers.as_deref(), Some(&[][..]));
        assert!(
            !lone.notes.iter().any(|note| note.contains("resolver")),
            "{:?}",
            lone.notes
        );
        let symbol = explain(&ws, "lone");
        assert_eq!(symbol.importers.as_deref(), Some(&[][..]));

        let native = explain(&ws, "src/native.c");
        assert!(native.imports.is_none() && native.importers.is_none());
        assert!(
            native
                .notes
                .iter()
                .any(|note| note.contains("no import resolver reads c")),
            "{:?}",
            native.notes
        );
    }

    #[test]
    fn several_definitions_brief_the_only_callable_and_list_the_others() {
        let (_dir, ws) = workspace_with(&[
            ("src/e.rs", "pub fn helper() {}\n"),
            ("src/f.rs", "pub struct helper;\n"),
            ("src/g.rs", "pub const helper: u8 = 1;\n"),
            ("src/h.rs", "pub fn twice() {}\n"),
            ("src/i.rs", "pub fn twice() {}\n"),
        ]);
        let helper = explain(&ws, "helper");
        assert_eq!(at(&helper), "src/e.rs:1");
        assert_eq!(helper.also_defined, ["src/f.rs:1", "src/g.rs:1"]);
        let refused = refuse(&ws, "twice");
        assert!(
            refused.contains("names 2 definitions")
                && refused.contains("src/h.rs:1 (function)")
                && refused.contains("src/i.rs:1 (function)"),
            "{refused}"
        );
    }

    #[test]
    fn an_ambiguity_error_lists_twenty_definitions_and_counts_the_rest() {
        let files: Vec<(String, &str)> = (0..22)
            .map(|i| (format!("src/many_{i:02}.rs"), "pub fn many() {}\n"))
            .collect();
        let extra: Vec<(&str, &str)> = files.iter().map(|(path, body)| (path.as_str(), *body)).collect();
        let (_dir, ws) = workspace_with(&extra);
        let refused = refuse(&ws, "many");
        assert!(refused.contains("names 22 definitions"), "{refused}");
        assert!(refused.contains("src/many_19.rs:1") && !refused.contains("src/many_20.rs:1"));
        assert!(refused.contains("and 2 more"), "{refused}");
    }

    #[test]
    fn a_qualifier_picks_the_definition_inside_its_type_or_impl() {
        let (_dir, ws) = workspace_with(&[(
            "src/q.rs",
            "pub struct Foo;\nimpl Foo {\n    pub fn run(&self) {}\n}\npub struct Bar<T>(T);\nimpl<T> Bar<T> {\n    \
             pub fn run(&self) {}\n}\nimpl std::fmt::Display for Foo {\n    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) \
             -> std::fmt::Result {\n        Ok(())\n    }\n}\n\npub fn tail() {}\n",
        )]);
        assert_eq!(at(&explain(&ws, "Foo::run")), "src/q.rs:3");
        assert_eq!(at(&explain(&ws, "Bar::run")), "src/q.rs:7");
        assert_eq!(at(&explain(&ws, "Foo.fmt")), "src/q.rs:10");
        assert!(refuse(&ws, "run").contains("names 2 definitions"));
        let between = refuse(&ws, "src/q.rs:9");
        assert!(
            between.contains("inside `impl") && between.contains("`fmt` line 10"),
            "{between}"
        );
    }

    #[test]
    fn a_stale_target_is_briefed_from_the_working_tree() {
        let (dir, ws) = workspace();
        write(
            dir.path(),
            "src/a.rs",
            "use crate::b::bee;\n\n// moved down\npub fn ay() {\n    bee();\n}\n",
        );
        let ay = explain(&ws, "ay");
        let definition = ay.definition.as_ref().unwrap();
        assert!(definition.stale);
        assert_eq!((definition.line, definition.end_line), (4, 6));
        assert_eq!(ay.body.as_deref(), Some("pub fn ay() {\n    bee();\n}"));
    }

    #[test]
    fn an_unreadable_definition_is_skipped_for_the_next_one() {
        let (dir, ws) = workspace_with(&[
            ("src/old.rs", "pub fn moved() {}\n"),
            ("src/new.rs", "pub struct moved;\n"),
        ]);
        std::fs::remove_file(dir.path().join("src/old.rs")).unwrap();
        let moved = explain(&ws, "moved");
        assert_eq!(at(&moved), "src/new.rs:1");
        assert!(
            moved
                .notes
                .iter()
                .any(|note| note.starts_with("skipped `src/old.rs:1`")),
            "{:?}",
            moved.notes
        );
    }

    #[test]
    fn an_empty_index_is_named_rather_than_reported_as_no_definition() {
        crate::store::init_isolated_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        // A git repository is a workspace root without a config file, which would be indexed.
        crate::engine::ops::graph_report::tests::git(dir.path(), &["init", "-q"]);
        let ws = scanned_workspace(dir.path(), false);
        let refused = refuse(&ws, "anything");
        assert!(refused.contains("empty_index"), "{refused}");
    }

    #[test]
    fn limits_sections_and_no_code_shape_the_answer_and_every_cut_is_listed() {
        let (_dir, ws) = workspace();
        let cut = explain_with(
            &ws,
            ExplainParams {
                target: "ay".to_string(),
                limit: Some(1),
                no_code: true,
                ..ExplainParams::default()
            },
        );
        assert!(cut.body.is_none());
        assert_eq!(caller_lines(&cut), ["src/b.rs:4"]);
        let callers = &cut.truncated_sections[0];
        assert_eq!(
            (callers.section, callers.shown, callers.total, callers.reason),
            ("callers", 1, Some(2), "limit")
        );
        assert_eq!(
            callers.next,
            r#"code_refs {"name":"ay","path":"src/a.rs","line":2,"limit":1000}"#
        );
        assert_eq!(cut.truncated_sections[1].section, "importers");

        let only = explain_with(
            &ws,
            ExplainParams {
                target: "ay".to_string(),
                sections: Some("callees".to_string()),
                ..ExplainParams::default()
            },
        );
        assert!(only.callers.is_none() && only.body.is_none() && only.importers.is_none());
        assert!(only.callees.is_some() && only.definition.is_some());
    }

    #[test]
    fn a_target_it_cannot_brief_is_refused_with_the_next_step() {
        let (dir, ws) = workspace();
        write(dir.path(), "Makefile", "all:\n");
        let refuse_limit = |target: &str, limit: Option<u32>| {
            ws.code_explain(ExplainParams {
                target: target.to_string(),
                limit,
                ..ExplainParams::default()
            })
            .unwrap_err()
            .to_string()
        };
        assert!(refuse(&ws, "nowhere").contains("--fuzzy"));
        assert!(refuse(&ws, "src/a.rs:9").contains("holds line 9"));
        assert!(refuse(&ws, "src/gone.rs").contains("src/gone.rs"));
        assert!(refuse(&ws, "Makefile").contains("Makefile"));
        assert!(!refuse(&ws, "Makefile").contains("--fuzzy"));
        assert!(refuse_limit("ay", Some(0)).contains("`limit`"));
        assert!(refuse(&ws, "  ").contains("empty"));
        let sections = ws
            .code_explain(ExplainParams {
                target: "src/a.rs".to_string(),
                sections: Some("callers".to_string()),
                ..ExplainParams::default()
            })
            .unwrap_err()
            .to_string();
        assert!(sections.contains("outline"), "{sections}");
    }
}
