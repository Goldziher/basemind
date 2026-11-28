//! `code cycles` over synthetic graphs (selection, order, scope, size cut, language notes) and
//! through a scanned Rust crate (the chain with its lines, the check, the refusals).

use super::*;
use crate::engine::ops::graph_report::tests::{I, T, graph_of, paths};

fn selected(graph: &ImportGraph, tiers: Tiers, scope: Option<&str>, max: Option<u32>) -> (Vec<Vec<String>>, usize) {
    let (cycles, over) = select(graph, tiers, scope, max);
    (cycles.iter().map(|members| paths(graph, members)).collect(), over)
}

/// A 3-cycle, a 2-cycle, a self-import and a type-only 2-cycle.
fn knots() -> ImportGraph {
    graph_of(
        &[
            ("a/x.ts", "a/y.ts", I),
            ("a/y.ts", "a/z.ts", I),
            ("a/z.ts", "a/x.ts", I),
            ("b/p.ts", "b/q.ts", I),
            ("b/q.ts", "b/p.ts", I),
            ("c/self.ts", "c/self.ts", I),
            ("d/t.ts", "d/u.ts", I),
            ("d/u.ts", "d/t.ts", T),
        ],
        "typescript",
        true,
    )
}

#[test]
fn cycles_come_smallest_first_and_type_only_ones_only_when_asked() {
    let graph = knots();
    let (cycles, over) = selected(&graph, Tiers::RUNTIME, None, None);
    assert_eq!(
        cycles,
        [
            vec!["c/self.ts"],
            vec!["b/p.ts", "b/q.ts"],
            vec!["a/x.ts", "a/y.ts", "a/z.ts"]
        ]
    );
    assert_eq!(over, 0);
    let (with_types, _) = selected(&graph, tiers(true, false), None, None);
    assert_eq!(with_types.len(), 4);
    assert!(with_types.contains(&vec!["d/t.ts".to_string(), "d/u.ts".to_string()]));
}

#[test]
fn a_path_keeps_the_cycles_through_it_and_max_size_drops_the_large_ones() {
    let graph = knots();
    assert_eq!(
        selected(&graph, Tiers::RUNTIME, Some("a"), None).0,
        [vec!["a/x.ts", "a/y.ts", "a/z.ts"]]
    );
    assert_eq!(
        selected(&graph, Tiers::RUNTIME, Some("b/q.ts"), None).0,
        [vec!["b/p.ts", "b/q.ts"]]
    );
    assert!(
        selected(&graph, Tiers::RUNTIME, Some("a/x"), None).0.is_empty(),
        "a path is a file or a directory, not a prefix"
    );
    let (small, over) = selected(&graph, Tiers::RUNTIME, None, Some(2));
    assert_eq!(small, [vec!["c/self.ts"], vec!["b/p.ts", "b/q.ts"]]);
    assert_eq!(over, 1);
}

#[test]
fn the_closing_chain_of_a_cycle_starts_and_ends_at_its_first_file() {
    let graph = knots();
    let x = graph.node("a/x.ts").unwrap();
    let members: Vec<NodeId> = ["a/x.ts", "a/y.ts", "a/z.ts"]
        .iter()
        .map(|path| graph.node(path).unwrap())
        .collect();
    assert_eq!(
        paths(&graph, &closing_chain(&graph, &members, Tiers::RUNTIME)),
        ["a/x.ts", "a/y.ts", "a/z.ts", "a/x.ts"]
    );
    let me = graph.node("c/self.ts").unwrap();
    assert_eq!(
        paths(&graph, &closing_chain(&graph, &[me], Tiers::RUNTIME)),
        ["c/self.ts", "c/self.ts"]
    );
    assert_eq!(
        would_close(&graph, x, graph.node("a/y.ts").unwrap(), Tiers::RUNTIME).map(|c| paths(&graph, &c)),
        Some(vec!["a/y.ts".to_string(), "a/z.ts".to_string(), "a/x.ts".to_string()])
    );
    assert_eq!(
        would_close(&graph, x, graph.node("b/p.ts").unwrap(), Tiers::RUNTIME),
        None
    );
}

#[test]
fn a_cycle_note_says_what_the_language_makes_of_it() {
    assert_eq!(note([Some("rust"), Some("rust")].into_iter()), Some(RUST_CYCLE_NOTE));
    assert_eq!(note([Some("rust"), Some("go")].into_iter()), Some(GO_NOTE));
    assert_eq!(note([Some("rust"), Some("typescript")].into_iter()), None);
    assert_eq!(note([Some("rust"), None].into_iter()), None);
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
mod scanned {
    use super::*;
    use crate::engine::ops::graph_report::tests::{rust_crate, scanned_workspace};

    fn list(ws: &Workspace, p: CyclesParams) -> CyclesList {
        match ws.code_cycles(p).expect("cycles") {
            CyclesResponse::List(list) => list,
            CyclesResponse::Check(check) => panic!("a list was asked for: {check:?}"),
        }
    }

    fn check(ws: &Workspace, from: &str, to: &str) -> CycleCheck {
        let p = CyclesParams {
            check_from: Some(RelPath::from(from)),
            check_to: Some(RelPath::from(to)),
            ..CyclesParams::default()
        };
        match ws.code_cycles(p).expect("check") {
            CyclesResponse::Check(check) => check,
            CyclesResponse::List(list) => panic!("a check was asked for: {list:?}"),
        }
    }

    #[test]
    fn a_scanned_crate_reports_its_cycle_with_lines_and_checks_a_new_import() {
        crate::store::init_isolated_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        rust_crate(dir.path());
        let ws = scanned_workspace(dir.path(), false);

        let report = list(&ws, CyclesParams::default());
        assert_eq!(report.cycle_count, 1, "{report:?}");
        let cycle = &report.cycles[0];
        assert_eq!(cycle.files, [RelPath::from("src/a.rs"), RelPath::from("src/b.rs")]);
        assert_eq!(cycle.chain, "src/a.rs:1 -> src/b.rs:1 -> src/a.rs");
        assert_eq!(cycle.note, Some(RUST_CYCLE_NOTE));
        assert!(
            report
                .coverage
                .languages
                .iter()
                .any(|language| language.language == "rust")
        );

        let with_decls = list(
            &ws,
            CyclesParams {
                include_module_decls: true,
                ..CyclesParams::default()
            },
        );
        assert_eq!(
            with_decls.cycle_count, 1,
            "lib.rs declares, nothing uses it back: {with_decls:?}"
        );
        assert_eq!(with_decls.cycles[0].size, 2);

        let closing = check(&ws, "src/a.rs", "src/d.rs");
        assert!(closing.would_close_cycle);
        assert_eq!(
            closing.closing_chain.as_deref(),
            Some("src/d.rs:1 -> src/c.rs:1 -> src/a.rs")
        );
        let open = check(&ws, "src/d.rs", "src/a.rs");
        assert!(!open.would_close_cycle && open.closing_chain.is_none(), "{open:?}");
        assert!(
            check(&ws, "src/c.rs", "src/c.rs").would_close_cycle,
            "a file importing itself"
        );

        let scoped = list(
            &ws,
            CyclesParams {
                path: Some("src/c.rs".to_string()),
                ..CyclesParams::default()
            },
        );
        assert_eq!(scoped.cycle_count, 0);
        assert!(scoped.notes.is_empty(), "src/c.rs is in the graph: {:?}", scoped.notes);
        let nowhere = list(
            &ws,
            CyclesParams {
                path: Some("vendor".to_string()),
                ..CyclesParams::default()
            },
        );
        assert_eq!(nowhere.notes, ["no file of the import graph is at or under `vendor`"]);
    }

    #[test]
    fn a_line_is_left_out_once_its_file_changed_since_the_scan() {
        crate::store::init_isolated_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        rust_crate(dir.path());
        let ws = scanned_workspace(dir.path(), false);
        std::fs::write(
            dir.path().join("src/a.rs"),
            "\n\nuse crate::b::bee;\npub fn ay() { bee(); }\n",
        )
        .unwrap();
        let report = list(&ws, CyclesParams::default());
        assert_eq!(report.cycles[0].chain, "src/a.rs -> src/b.rs:1 -> src/a.rs");
    }

    #[test]
    fn a_malformed_request_is_refused() {
        crate::store::init_isolated_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        rust_crate(dir.path());
        let ws = scanned_workspace(dir.path(), false);
        let refused = |p: CyclesParams| match ws.code_cycles(p) {
            Err(EngineError::InvalidArgument(message)) => message,
            other => panic!("refused, got {other:?}"),
        };
        let half = refused(CyclesParams {
            check_from: Some(RelPath::from("src/a.rs")),
            ..CyclesParams::default()
        });
        assert!(half.contains("go together"), "{half}");
        let mixed = refused(CyclesParams {
            check_from: Some(RelPath::from("src/a.rs")),
            check_to: Some(RelPath::from("src/b.rs")),
            max_size: Some(3),
            ..CyclesParams::default()
        });
        assert!(mixed.contains("takes no"), "{mixed}");
        let zero = refused(CyclesParams {
            max_size: Some(0),
            ..CyclesParams::default()
        });
        assert!(zero.contains("at least 1"), "{zero}");
        let missing = refused(CyclesParams {
            check_from: Some(RelPath::from("src/a.rs")),
            check_to: Some(RelPath::from("src/nope.rs")),
            ..CyclesParams::default()
        });
        assert!(missing.contains("src/nope.rs"), "{missing}");
    }
}
