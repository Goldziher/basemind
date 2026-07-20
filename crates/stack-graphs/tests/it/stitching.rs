// ------------------------------------------------------------------------------------------------
// Copyright © 2023, stack-graphs authors.
// Licensed under either of Apache License, Version 2.0, or MIT license, at your option.
// Please see the LICENSE-APACHE or LICENSE-MIT files in this distribution for license details.
// ------------------------------------------------------------------------------------------------

use itertools::Itertools;
use stack_graphs::arena::Handle;
use stack_graphs::cycles::Appendables;
use stack_graphs::cycles::AppendingCycleDetector;
use stack_graphs::graph::Degree;
use stack_graphs::graph::Node;
use stack_graphs::graph::StackGraph;
use stack_graphs::partial::PartialPath;
use stack_graphs::partial::PartialPaths;
use stack_graphs::partial::PartialScopeStack;
use stack_graphs::paths::PathResolutionError;
use stack_graphs::stitching::Database;
use stack_graphs::stitching::DatabaseCandidates;
use stack_graphs::stitching::ForwardPartialPathStitcher;

use crate::util::create_drop_scopes_node;
use crate::util::create_partial_path_and_edges;
use crate::util::create_pop_symbol_node;
use crate::util::create_push_symbol_node;
use crate::util::create_scope_node;

fn test_foo_bar_root_candidate_paths(symbols: &[&str], variable: bool) -> usize {
    let mut graph = StackGraph::new();
    let file = graph.add_file("test").unwrap();
    let mut partials = PartialPaths::new();

    let r = StackGraph::root_node();
    let foo_def = create_pop_symbol_node(&mut graph, file, "foo", true);
    let bar_def = create_pop_symbol_node(&mut graph, file, "bar", true);

    let path_with_variable = create_partial_path_and_edges(&mut graph, &mut partials, &[r, foo_def, bar_def]).unwrap();

    let mut path_without_variable = path_with_variable.clone();
    path_without_variable.eliminate_precondition_stack_variables(&mut partials);

    let mut db = Database::new();
    db.add_partial_path(&graph, &mut partials, path_with_variable);
    db.add_partial_path(&graph, &mut partials, path_without_variable);

    let r = StackGraph::root_node();
    let refs = symbols
        .iter()
        .map(|r| create_push_symbol_node(&mut graph, file, r, true))
        .chain(std::iter::once(r))
        .collect_vec();
    let mut path = create_partial_path_and_edges(&mut graph, &mut partials, &refs).unwrap();
    if !variable {
        path.eliminate_precondition_stack_variables(&mut partials);
    }

    let mut results = Vec::new();
    db.find_candidate_partial_paths_from_root(
        &graph,
        &mut partials,
        Some(path.symbol_stack_postcondition),
        &mut results,
    );

    results.len()
}

#[test]
fn find_candidates_for_exact_symbol_stack_with_variable() {
    let results = test_foo_bar_root_candidate_paths(&["bar", "foo"], true);
    assert_eq!(2, results);
}

#[test]
fn find_candidates_for_exact_symbol_stack_without_variable() {
    let results = test_foo_bar_root_candidate_paths(&["bar", "foo"], false);
    assert_eq!(2, results);
}

#[test]
fn find_candidates_for_longer_symbol_stack_with_variable() {
    let results = test_foo_bar_root_candidate_paths(&["quz", "bar", "foo"], true);
    assert_eq!(1, results);
}

#[test]
fn find_candidates_for_longer_symbol_stack_without_variable() {
    let results = test_foo_bar_root_candidate_paths(&["quz", "bar", "foo"], false);
    assert_eq!(1, results);
}

#[test]
fn find_candidates_for_shorter_symbol_stack_with_variable() {
    let results = test_foo_bar_root_candidate_paths(&["foo"], true);
    assert_eq!(2, results);
}

#[test]
fn find_candidates_for_shorter_symbol_stack_without_variable() {
    let results = test_foo_bar_root_candidate_paths(&["foo"], false);
    assert_eq!(0, results);
}

/// A partial path that starts and ends at the same _drop scopes_ node, and whose scope-stack
/// precondition is a concrete scope stack with no variable.
///
/// This is the shape that trips the cycle detector.  The detector tests a suspected cycle by
/// replaying its appendages against a path freshly minted at the cycle's end node — and
/// `PartialPath::from_node` on a _drop scopes_ node yields an **empty, variable-free** scope-stack
/// postcondition.  Unifying that with a precondition that demands concrete scopes is unsatisfiable,
/// so the replay legitimately fails even though the original path was built without error.
fn drop_scopes_self_loop() -> (StackGraph, PartialPaths, PartialPath, Handle<Node>) {
    let mut graph = StackGraph::new();
    let file = graph.add_file("test").unwrap();
    let mut partials = PartialPaths::new();

    let drop_scopes = create_drop_scopes_node(&mut graph, file);
    let exported_scope = create_scope_node(&mut graph, file, true);

    let mut path = create_partial_path_and_edges(&mut graph, &mut partials, &[drop_scopes, drop_scopes]).unwrap();
    let mut required_scopes = PartialScopeStack::empty();
    required_scopes.push_back(&mut partials, exported_scope);
    path.scope_stack_precondition = required_scopes;

    (graph, partials, path, drop_scopes)
}

#[test]
fn cycle_detector_reports_unsatisfiable_replay_as_an_error() {
    let (graph, mut partials, path, _) = drop_scopes_self_loop();

    let mut database = Database::new();
    let handle = database.add_partial_path(&graph, &mut partials, path.clone());

    let mut appendables = Appendables::new();
    let mut detector: AppendingCycleDetector<Handle<PartialPath>> =
        AppendingCycleDetector::from(&mut appendables, path);
    detector.append(&mut appendables, handle);

    let result = detector.is_cyclic(&graph, &mut partials, &database, &mut appendables);
    assert!(
        matches!(result, Err(PathResolutionError::ScopeStackUnsatisfied)),
        "expected ScopeStackUnsatisfied, got {result:?}",
    );
}

#[test]
fn stitcher_discontinues_path_when_cycle_test_is_unsatisfiable_instead_of_panicking() {
    let (graph, mut partials, path, drop_scopes) = drop_scopes_self_loop();

    let mut database = Database::new();
    database.add_partial_path(&graph, &mut partials, path.clone());

    let mut candidates = Vec::new();
    database.find_candidate_partial_paths_from_node(&graph, &mut partials, drop_scopes, &mut candidates);
    assert_eq!(1, candidates.len());

    // Upstream `.expect()`ed the cycle test here, so this phase aborted the process.
    let mut stitcher = ForwardPartialPathStitcher::from_partial_paths(&graph, &mut partials, vec![path]);
    let mut extended = 0usize;
    while !stitcher.is_complete() {
        stitcher.process_next_phase(
            &mut DatabaseCandidates::new(&graph, &mut partials, &mut database),
            |_, _, _| true,
        );
        extended += stitcher.previous_phase_partial_paths().count();
    }

    assert_eq!(0, extended);
}

#[test]
fn incoming_path_degree_is_zero_for_a_node_no_partial_path_ends_at() {
    let mut graph = StackGraph::new();
    let file = graph.add_file("test").unwrap();
    let mut partials = PartialPaths::new();

    let scope = create_scope_node(&mut graph, file, true);
    let foo_def = create_pop_symbol_node(&mut graph, file, "foo", true);
    let path = create_partial_path_and_edges(&mut graph, &mut partials, &[scope, foo_def]).unwrap();

    let mut database = Database::new();
    database.add_partial_path(&graph, &mut partials, path);

    let unseen = create_scope_node(&mut graph, file, true);
    assert_eq!(Degree::Zero, database.get_incoming_path_degree(unseen));
    assert_eq!(Degree::Zero, database.get_incoming_path_degree(StackGraph::root_node()));
    assert_eq!(Degree::One, database.get_incoming_path_degree(foo_def));
}

#[test]
fn stitcher_extends_a_path_whose_end_node_the_database_never_reaches() {
    let mut graph = StackGraph::new();
    let file = graph.add_file("test").unwrap();
    let mut partials = PartialPaths::new();

    let scope = create_scope_node(&mut graph, file, true);
    let foo_def = create_pop_symbol_node(&mut graph, file, "foo", true);
    let later_scope = create_scope_node(&mut graph, file, true);
    let bar_def = create_pop_symbol_node(&mut graph, file, "bar", true);

    let known = create_partial_path_and_edges(&mut graph, &mut partials, &[scope, foo_def]).unwrap();
    let extension = create_partial_path_and_edges(&mut graph, &mut partials, &[bar_def, foo_def]).unwrap();

    let mut database = Database::new();
    database.add_partial_path(&graph, &mut partials, known);
    database.add_partial_path(&graph, &mut partials, extension);

    let seed = create_partial_path_and_edges(&mut graph, &mut partials, &[later_scope, bar_def]).unwrap();
    assert_ne!(
        seed.start_node, seed.end_node,
        "start == end would short-circuit the degree lookup"
    );

    let mut stitcher = ForwardPartialPathStitcher::from_partial_paths(&graph, &mut partials, vec![seed]);
    stitcher.set_check_only_join_nodes(true);
    let mut extended = 0usize;
    while !stitcher.is_complete() {
        stitcher.process_next_phase(
            &mut DatabaseCandidates::new(&graph, &mut partials, &mut database),
            |_, _, _| true,
        );
        extended += stitcher.previous_phase_partial_paths().count();
    }

    assert_eq!(1, extended);
}
