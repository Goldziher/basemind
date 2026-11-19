//! `code explain`: section parsing, the doc join, owners and the budget cut, then briefings on a
//! scanned Rust crate (symbol, `path:line` and file targets, limits, the budget, refusals).

use super::*;
use crate::extract::DocComment;

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
fn a_doc_joins_only_when_nothing_but_whitespace_separates_it() {
    let source = b"/// Adds.\nfn add() {}\n/// Lost.\n#[inline]\nfn sub() {}\n";
    let docs = [
        DocComment {
            text: "/// Adds.".to_string(),
            start_byte: 0,
            end_byte: 9,
        },
        DocComment {
            text: "/// Lost.".to_string(),
            start_byte: 22,
            end_byte: 31,
        },
    ];
    assert_eq!(
        doc_above(&docs, &symbol("add", 10, 21), source).as_deref(),
        Some("/// Adds.")
    );
    assert_eq!(doc_above(&docs, &symbol("sub", 42, 53), source), None);
    assert_eq!(doc_above(&docs, &symbol("first", 0, 5), source), None);
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
        next: AHashMap::from_iter([("callees", "code_read {}".to_string())]),
    };
    fit(&mut briefing, 150);
    let response = &briefing.response;
    assert!(response.body.is_none());
    assert!(tokens(response) <= 150, "{}", tokens(response));
    let callees = response.callees.as_ref().unwrap().len();
    assert!(callees < 40, "{callees}");
    let cuts: Vec<(&str, usize, Option<usize>, &str, &str)> = response
        .truncated_sections
        .iter()
        .map(|c| (c.section, c.shown, c.total, c.reason, c.next.as_str()))
        .collect();
    assert_eq!(cuts[0], ("body", 0, None, "budget", ""));
    assert_eq!(cuts[1], ("callees", callees, Some(40), "budget", "code_read {}"));

    let mut roomy = Briefing {
        response: ExplainResponse {
            target: "symbol",
            body: Some("x".to_string()),
            ..ExplainResponse::default()
        },
        next: AHashMap::new(),
    };
    fit(&mut roomy, 6_000);
    assert!(roomy.response.body.is_some() && roomy.response.truncated_sections.is_empty());
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
mod scanned {
    use super::*;
    use crate::engine::ops::graph_report::tests::{rust_crate, scanned_workspace, write};

    fn workspace() -> (tempfile::TempDir, Workspace) {
        crate::store::init_isolated_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        rust_crate(dir.path());
        write(
            dir.path(),
            "src/b.rs",
            "use crate::a::ay;\n/// Bee.\npub fn bee() {}\npub fn _x() { ay(); }\n",
        );
        let ws = scanned_workspace(dir.path(), false);
        (dir, ws)
    }

    fn explain(ws: &Workspace, target: &str) -> ExplainResponse {
        ws.code_explain(ExplainParams {
            target: target.to_string(),
            ..ExplainParams::default()
        })
        .unwrap_or_else(|error| panic!("{target}: {error}"))
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
        assert_eq!(ay.callees.as_deref(), Some(&["bee".to_string()][..]));
        assert_eq!(paths(&ay.importers), ["src/b.rs", "src/c.rs"]);
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
    fn a_file_briefing_holds_its_outline_imports_and_importers() {
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
        assert!(file.definition.is_none() && file.callers.is_none());
    }

    #[test]
    fn limits_sections_and_no_code_shape_the_answer_and_every_cut_is_listed() {
        let (_dir, ws) = workspace();
        let cut = ws
            .code_explain(ExplainParams {
                target: "ay".to_string(),
                limit: Some(1),
                no_code: true,
                ..ExplainParams::default()
            })
            .unwrap();
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

        let only = ws
            .code_explain(ExplainParams {
                target: "ay".to_string(),
                sections: Some("callees".to_string()),
                ..ExplainParams::default()
            })
            .unwrap();
        assert!(only.callers.is_none() && only.body.is_none() && only.importers.is_none());
        assert!(only.callees.is_some() && only.definition.is_some());
    }

    #[test]
    fn a_target_it_cannot_brief_is_refused_with_the_next_step() {
        let (_dir, ws) = workspace();
        let refuse = |target: &str, limit: Option<u32>| {
            ws.code_explain(ExplainParams {
                target: target.to_string(),
                limit,
                ..ExplainParams::default()
            })
            .unwrap_err()
            .to_string()
        };
        assert!(refuse("nowhere", None).contains("--fuzzy"));
        assert!(refuse("src/a.rs:9", None).contains("holds line 9"));
        assert!(refuse("src/gone.rs", None).contains("src/gone.rs"));
        assert!(refuse("ay", Some(0)).contains("`limit`"));
        assert!(refuse("  ", None).contains("empty"));
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
