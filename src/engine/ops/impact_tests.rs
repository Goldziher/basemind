//! `code impact` over a synthetic graph (the walk, cycle marks, files outside the graph) and through
//! a scanned Rust crate (file, symbol and diff targets, depth, refusals).

use super::*;
use crate::engine::ops::graph_report::tests::{I, graph_of};

fn rows(response: &ImpactResponse) -> Vec<(String, u8, String, bool)> {
    response
        .affected
        .iter()
        .map(|hit| (hit.path.to_string(), hit.depth, hit.via.to_string(), hit.in_cycle))
        .collect()
}

fn targets(paths: &[&str]) -> Targets {
    Targets {
        kind: "file",
        paths: paths.iter().map(|path| RelPath::from(*path)).collect(),
        definitions: Vec::new(),
        scope_files: None,
    }
}

/// `leaf` is imported by `mid`, which `top` and the cycle `p <-> q` import.
fn chain_graph() -> ImportGraph {
    graph_of(
        &[
            ("mid", "leaf", I),
            ("top", "mid", I),
            ("p", "mid", I),
            ("p", "q", I),
            ("q", "p", I),
        ],
        "rust",
        true,
    )
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
fn workspace_for_answers() -> (tempfile::TempDir, Workspace) {
    crate::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    crate::engine::ops::graph_report::tests::rust_crate(dir.path());
    let ws = crate::engine::ops::graph_report::tests::scanned_workspace(dir.path(), false);
    (dir, ws)
}

#[test]
fn the_walk_reports_each_file_once_at_its_depth_and_marks_cycles() {
    let graph = chain_graph();
    let one = answer(&graph, targets(&["leaf"]), WalkLimits::new(None), Tiers::RUNTIME);
    assert_eq!(rows(&one), [("mid".to_string(), 1, "leaf".to_string(), false)]);
    assert_eq!((one.truncated, one.truncated_reason), (true, Some("depth")));

    let deep = answer(
        &graph,
        targets(&["leaf", "gone"]),
        WalkLimits::new(Some(5)),
        Tiers::RUNTIME,
    );
    assert_eq!(
        rows(&deep),
        [
            ("mid".to_string(), 1, "leaf".to_string(), false),
            ("p".to_string(), 2, "mid".to_string(), true),
            ("top".to_string(), 2, "mid".to_string(), false),
            ("q".to_string(), 3, "p".to_string(), true),
        ]
    );
    assert_eq!(deep.affected_files, 4);
    assert!(!deep.truncated && deep.truncated_reason.is_none());
    assert_eq!(deep.not_in_graph, [RelPath::from("gone")]);

    let from_cycle = answer(&graph, targets(&["q"]), WalkLimits::new(None), Tiers::RUNTIME);
    assert_eq!(from_cycle.targets_in_cycles, [RelPath::from("q")]);
}

#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
mod scanned {
    use super::*;
    use crate::engine::ops::graph_report::tests::{git, rust_crate, scanned_workspace, write};

    fn affected(response: &ImpactResponse) -> Vec<(String, u8)> {
        response
            .affected
            .iter()
            .map(|hit| (hit.path.to_string(), hit.depth))
            .collect()
    }

    #[test]
    fn a_file_and_a_symbol_reach_their_importers_by_depth() {
        let (_dir, ws) = workspace_for_answers();
        let by_file = ws
            .code_impact(ImpactParams {
                path: Some(RelPath::from("src/c.rs")),
                ..ImpactParams::default()
            })
            .expect("impact");
        assert_eq!(by_file.target, "file");
        assert_eq!(affected(&by_file), [("src/d.rs".to_string(), 1)]);
        assert!(by_file.notes.is_empty(), "{:?}", by_file.notes);

        let by_symbol = ws
            .code_impact(ImpactParams {
                symbol: Some("ay".to_string()),
                depth: Some(2),
                ..ImpactParams::default()
            })
            .expect("impact");
        assert_eq!(by_symbol.target, "symbol");
        assert_eq!(by_symbol.targets, [RelPath::from("src/a.rs")]);
        assert_eq!(by_symbol.definitions.len(), 1);
        assert_eq!(by_symbol.definitions[0].kind, "function");
        assert_eq!(
            affected(&by_symbol),
            [
                ("src/b.rs".to_string(), 1),
                ("src/c.rs".to_string(), 1),
                ("src/d.rs".to_string(), 2)
            ]
        );
        assert_eq!(by_symbol.targets_in_cycles, [RelPath::from("src/a.rs")]);
        assert!(by_symbol.affected[0].in_cycle && !by_symbol.affected[1].in_cycle);

        let with_decls = ws
            .code_impact(ImpactParams {
                path: Some(RelPath::from("src/c.rs")),
                include_module_decls: true,
                ..ImpactParams::default()
            })
            .expect("impact");
        assert_eq!(
            affected(&with_decls),
            [("src/d.rs".to_string(), 1), ("src/lib.rs".to_string(), 1)]
        );
    }

    #[test]
    fn a_diff_walks_from_the_changed_files() {
        crate::store::init_isolated_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        rust_crate(root);
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "init"]);
        write(root, "src/c.rs", "use crate::a::ay;\npub fn sea() { ay(); ay(); }\n");
        let ws = scanned_workspace(root, true);

        let changed = ws
            .code_impact(ImpactParams {
                diff: Some(String::new()),
                ..ImpactParams::default()
            })
            .expect("impact");
        assert_eq!((changed.target, changed.scope_files), ("diff", Some(1)));
        assert_eq!(changed.targets, [RelPath::from("src/c.rs")]);
        assert_eq!(affected(&changed), [("src/d.rs".to_string(), 1)]);

        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "second"]);
        let nothing = ws
            .code_impact(ImpactParams {
                diff: Some("HEAD".to_string()),
                ..ImpactParams::default()
            })
            .expect("impact");
        assert_eq!(nothing.scope_files, Some(0));
        assert!(nothing.affected.is_empty());
        let range = ws
            .code_impact(ImpactParams {
                diff: Some("HEAD~1..HEAD".to_string()),
                ..ImpactParams::default()
            })
            .expect("impact");
        assert_eq!(range.targets, [RelPath::from("src/c.rs")]);
    }

    #[test]
    fn a_malformed_request_is_refused() {
        let (_dir, ws) = workspace_for_answers();
        let refused = |p: ImpactParams| match ws.code_impact(p) {
            Err(EngineError::InvalidArgument(message)) => message,
            other => panic!("refused, got {other:?}"),
        };
        assert!(refused(ImpactParams::default()).contains("exactly one target"));
        let both = refused(ImpactParams {
            path: Some(RelPath::from("src/c.rs")),
            symbol: Some("sea".to_string()),
            ..ImpactParams::default()
        });
        assert!(both.contains("exactly one target"), "{both}");
        let deep = refused(ImpactParams {
            path: Some(RelPath::from("src/c.rs")),
            depth: Some(6),
            ..ImpactParams::default()
        });
        assert!(deep.contains("1 to 5"), "{deep}");
        let unknown = refused(ImpactParams {
            symbol: Some("no_such_thing".to_string()),
            ..ImpactParams::default()
        });
        assert!(unknown.contains("no definition named `no_such_thing`"), "{unknown}");
        let missing = refused(ImpactParams {
            path: Some(RelPath::from("src/nope.rs")),
            ..ImpactParams::default()
        });
        assert!(missing.contains("src/nope.rs"), "{missing}");
        let expanded = refused(ImpactParams {
            diff: Some("HEAD+importers".to_string()),
            ..ImpactParams::default()
        });
        assert!(expanded.contains("+importers"), "{expanded}");
        match ws.code_impact(ImpactParams {
            diff: Some(String::new()),
            ..ImpactParams::default()
        }) {
            Err(EngineError::Unavailable(message)) => assert!(message.contains("git"), "{message}"),
            other => panic!("no repository, got {other:?}"),
        }
    }
}
