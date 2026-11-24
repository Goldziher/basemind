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

    let down = answer(&graph, targets(&["top"]), WalkLimits::new(Some(5)), Tiers::RUNTIME);
    let imports: Vec<(String, u8, String)> = down
        .dependencies
        .iter()
        .map(|hit| (hit.path.to_string(), hit.depth, hit.via.to_string()))
        .collect();
    assert_eq!(
        imports,
        [
            ("mid".to_string(), 1, "top".to_string()),
            ("leaf".to_string(), 2, "mid".to_string())
        ]
    );
    assert_eq!(down.dependencies_truncated_reason, None);
    let shallow = answer(&graph, targets(&["top"]), WalkLimits::new(None), Tiers::RUNTIME);
    assert_eq!(shallow.dependencies_truncated_reason, Some("depth"));

    let from_cycle = answer(&graph, targets(&["q"]), WalkLimits::new(None), Tiers::RUNTIME);
    assert_eq!(from_cycle.targets_in_cycles, [RelPath::from("q")]);
}

#[test]
fn the_answer_buckets_its_impacted_files_and_orders_the_work() {
    let graph = graph_of(
        &[
            ("mid", "leaf", I),
            ("tests/top_test.rs", "mid", I),
            ("vendor/v.rs", "mid", I),
        ],
        "rust",
        true,
    );
    let mut response = answer(&graph, targets(&["leaf"]), WalkLimits::new(Some(2)), Tiers::RUNTIME);
    super::impact_order::finish(&mut response);
    assert_eq!(response.impacted_files, 3);
    assert_eq!(
        response.buckets,
        ImpactBuckets {
            prod: 1,
            config: 0,
            test: 1,
            external: 1
        }
    );
    let order: Vec<(String, u8)> = response
        .order
        .iter()
        .map(|step| (step.path.to_string(), step.depth))
        .collect();
    assert_eq!(
        order,
        [
            ("leaf".to_string(), 0),
            ("mid".to_string(), 1),
            ("tests/top_test.rs".to_string(), 2),
            ("vendor/v.rs".to_string(), 2)
        ]
    );
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
    fn a_symbol_lists_graded_callers_and_what_it_depends_on() {
        let (_dir, ws) = workspace_for_answers();
        let impact = ws
            .code_impact(ImpactParams {
                symbol: Some("ay".to_string()),
                depth: Some(2),
                ..ImpactParams::default()
            })
            .expect("impact");
        let direct: Vec<_> = impact
            .callers
            .iter()
            .filter(|caller| caller.depth == 1)
            .map(|caller| {
                (
                    caller.path.to_string(),
                    caller.line,
                    caller.symbol.clone().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            direct,
            [
                ("src/b.rs".to_string(), 3, "_x".to_string()),
                ("src/c.rs".to_string(), 2, "sea".to_string())
            ]
        );
        assert!(
            impact
                .callers
                .iter()
                .all(|caller| caller.calls == "ay" || caller.depth == 2)
        );
        for caller in &impact.callers {
            assert_eq!(
                caller.proven,
                matches!(caller.resolution, "same_file" | "imported"),
                "{caller:?}"
            );
        }
        // A caller is followed to the next depth only when its edge is proven.
        let sea_proven = impact
            .callers
            .iter()
            .any(|caller| caller.path == RelPath::from("src/c.rs") && caller.proven);
        let indirect: Vec<_> = impact
            .callers
            .iter()
            .filter(|caller| caller.depth == 2)
            .map(|caller| (caller.path.to_string(), caller.line, caller.calls.as_str()))
            .collect();
        if sea_proven {
            assert_eq!(indirect, [("src/d.rs".to_string(), 2, "sea")]);
        } else {
            assert!(indirect.is_empty(), "{indirect:?}");
        }
        assert_eq!(impact.callers_truncated_reason, None);

        assert_eq!(impact.calls.len(), 1, "{:?}", impact.calls);
        let callee = &impact.calls[0];
        assert_eq!(
            (callee.from.to_string(), callee.name.as_str()),
            ("src/a.rs".to_string(), "bee")
        );
        assert_eq!(callee.proven, callee.path == Some(RelPath::from("src/b.rs")));
        assert_eq!(callee.resolution, if callee.proven { "imported" } else { "name_only" });

        let imports: Vec<_> = impact
            .dependencies
            .iter()
            .map(|hit| (hit.path.to_string(), hit.depth, hit.in_cycle))
            .collect();
        assert_eq!(imports, [("src/b.rs".to_string(), 1, true)]);
        assert_eq!(impact.targets_in_cycles, [RelPath::from("src/a.rs")]);

        let shallow = ws
            .code_impact(ImpactParams {
                symbol: Some("ay".to_string()),
                ..ImpactParams::default()
            })
            .expect("impact");
        assert!(shallow.callers.iter().all(|caller| caller.depth == 1));
        assert_eq!(
            shallow.callers_truncated_reason,
            sea_proven.then_some("depth"),
            "depth 1 stops short of the proven caller `sea`'s callers"
        );
    }

    fn workspace_with_files(extra: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
        crate::store::init_isolated_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        rust_crate(dir.path());
        for (path, body) in extra {
            write(dir.path(), path, body);
        }
        let ws = scanned_workspace(dir.path(), false);
        (dir, ws)
    }

    fn symbol_impact(ws: &Workspace, name: &str) -> ImpactResponse {
        ws.code_impact(ImpactParams {
            symbol: Some(name.to_string()),
            ..ImpactParams::default()
        })
        .expect("impact")
    }

    #[test]
    fn depth_is_reported_only_when_a_boundary_definition_has_callers() {
        let (_dir, ws) = workspace_for_answers();
        // `dee` calls `sea` and nothing calls `dee`: depth 1 already lists every caller.
        let complete = symbol_impact(&ws, "sea");
        assert_eq!(complete.callers_truncated_reason, None, "{:?}", complete.callers);
    }

    #[test]
    fn an_unbound_call_to_the_definitions_own_name_is_self_recursion_and_listed_nowhere() {
        let (_dir, ws) = workspace_with_files(&[
            ("src/e.rs", "pub fn bee() {\n    crate::b::bee();\n    bee();\n}\n"),
            ("src/lib.rs", "mod a;\nmod b;\nmod c;\nmod d;\nmod e;\n"),
        ]);
        let callees =
            crate::engine::ops::explain::callees_of(&ws, &RelPath::from("src/e.rs"), "bee", 1).expect("callees");
        let listed: Vec<_> = callees
            .iter()
            .map(|callee| (callee.name.as_str(), callee.definition.clone()))
            .collect();
        // The facts do not bind a path-qualified call here, so the name decides: no callee.
        assert!(listed.is_empty(), "{listed:?}");
    }

    #[test]
    fn callees_of_two_definitions_on_one_line_follow_the_name_asked() {
        let (_dir, ws) = workspace_with_files(&[
            ("src/f.rs", "pub fn one() { alpha(); } pub fn two() { omega(); }\n"),
            ("src/lib.rs", "mod a;\nmod b;\nmod c;\nmod d;\nmod f;\n"),
        ]);
        let names = |name: &str| -> Vec<String> {
            crate::engine::ops::explain::callees_of(&ws, &RelPath::from("src/f.rs"), name, 1)
                .expect("callees")
                .into_iter()
                .map(|callee| callee.name)
                .collect()
        };
        assert_eq!(names("one"), ["alpha"]);
        assert_eq!(names("two"), ["omega"]);
    }

    #[test]
    fn a_definition_that_moved_since_the_scan_lists_no_calls_and_says_so() {
        let (dir, ws) = workspace_for_answers();
        write(
            dir.path(),
            "src/a.rs",
            "use crate::b::bee;\npub fn other() { bee(); }\npub fn ay() { bee(); }\n",
        );
        let impact = symbol_impact(&ws, "ay");
        assert!(impact.calls.is_empty(), "{:?}", impact.calls);
        assert!(
            impact
                .notes
                .iter()
                .any(|note| note.contains("changed since the scan and no longer holds `ay` at line 2")),
            "{:?}",
            impact.notes
        );
    }

    #[test]
    fn callees_of_a_definition_that_left_its_line_says_the_file_changed_and_asks_for_a_rescan() {
        let (dir, ws) = workspace_for_answers();
        write(
            dir.path(),
            "src/a.rs",
            "use crate::b::bee;\n\n\n\n\npub fn ay() { bee(); }\n",
        );
        let reason = crate::engine::ops::explain::callees_of(&ws, &RelPath::from("src/a.rs"), "ay", 2)
            .err()
            .expect("the definition is no longer at line 2");
        assert_eq!(
            reason,
            "`src/a.rs` changed since the scan and no longer holds `ay` at line 2: rescan"
        );
    }

    #[test]
    fn a_file_target_lists_its_imports_and_no_call_level_rows() {
        let (_dir, ws) = workspace_for_answers();
        let impact = ws
            .code_impact(ImpactParams {
                path: Some(RelPath::from("src/c.rs")),
                ..ImpactParams::default()
            })
            .expect("impact");
        let imports: Vec<_> = impact
            .dependencies
            .iter()
            .map(|hit| (hit.path.to_string(), hit.depth))
            .collect();
        assert_eq!(imports, [("src/a.rs".to_string(), 1)]);
        assert!(impact.callers.is_empty() && impact.calls.is_empty());
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
