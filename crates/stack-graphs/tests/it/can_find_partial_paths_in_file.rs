// ------------------------------------------------------------------------------------------------
// Copyright © 2021, stack-graphs authors.
// Licensed under either of Apache License, Version 2.0, or MIT license, at your option.
// Please see the LICENSE-APACHE or LICENSE-MIT files in this distribution for license details.
// ------------------------------------------------------------------------------------------------

use std::collections::BTreeSet;

use pretty_assertions::assert_eq;
use stack_graphs::NoCancellation;
use stack_graphs::graph::StackGraph;
use stack_graphs::partial::PartialPaths;
use stack_graphs::stitching::{ForwardPartialPathStitcher, StitcherConfig};

use crate::test_graphs;

fn check_partial_paths_in_file(graph: &StackGraph, file: &str, expected_paths: &[&str]) {
    let file = graph.get_file(file).expect("Missing file");
    let mut partials = PartialPaths::new();
    let mut results = BTreeSet::new();
    ForwardPartialPathStitcher::find_minimal_partial_path_set_in_file(
        graph,
        &mut partials,
        file,
        StitcherConfig::default(),
        &NoCancellation,
        |graph, partials, path| {
            results.insert(path.display(graph, partials).to_string());
        },
    )
    .expect("should never be cancelled");
    let expected_paths = expected_paths.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    assert_eq!(expected_paths, results, "failed in file {}", graph[file]);
}

pub(crate) static CLASS_FIELD_THROUGH_FUNCTION_PARAMETER_MAIN_PATHS: &[&str] = &[
    "<__main__,%1> ($1) [root] -> [main.py(0) definition __main__] <%1> ($1)",
    "<%1> ($1) [main.py(15) reference b] -> [root] <b,%1> ($1)",
    "<%1> ($1) [main.py(17) reference a] -> [root] <a,%1> ($1)",
    "<__main__.,%1> ($1) [main.py(0) definition __main__] -> [main.py(15) reference b] <b.,%1> ($1)",
    "<__main__.,%1> ($1) [main.py(0) definition __main__] -> [main.py(17) reference a] <a.,%1> ($1)",
    "<%1> ($1) [main.py(13) reference foo] -> [main.py(15) reference b] <b.foo,%1> ($1)",
    "<%1> ($1) [main.py(13) reference foo] -> [main.py(17) reference a] <a.foo,%1> ($1)",
    "<%1> ($1) [main.py(9) reference A] -> [main.py(15) reference b] <b.A,%1> ($1)",
    "<%1> ($1) [main.py(9) reference A] -> [main.py(17) reference a] <a.A,%1> ($1)",
    "<%1> ($1) [main.py(10) reference bar] -> [main.py(13) reference foo] <foo()/([main.py(7)],$1).bar,%1> ($1)",
    "<0,%1> ($1) [main.py(7) exported scope] -> [main.py(9) reference A] <A,%1> ($1)",
];

pub(crate) static CLASS_FIELD_THROUGH_FUNCTION_PARAMETER_A_PATHS: &[&str] = &[
    "<a,%1> ($1) [root] -> [a.py(0) definition a] <%1> ($1)",
    "<a.foo,%1> ($1) [a.py(0) definition a] -> [a.py(5) definition foo] <%1> ($1)",
    "<foo()/($2),%1> ($1) [a.py(5) definition foo] -> [a.py(8) reference x] <x,%1> ($2)",
    "<%1> ($1) [a.py(8) reference x] -> [a.py(14) definition x] <%1> ()",
    "<%1> ($1) [a.py(8) reference x] -> [jump to scope] <0,%1> ($1)",
    "<%1> ($1) [a.py(8) reference x] -> [jump to scope] <x,%1> ($1)",
];

pub(crate) static CLASS_FIELD_THROUGH_FUNCTION_PARAMETER_B_PATHS: &[&str] = &[
    "<b,%1> ($1) [root] -> [b.py(0) definition b] <%1> ($1)",
    "<b.A,%1> ($1) [b.py(0) definition b] -> [b.py(5) definition A] <%1> ($1)",
    "<A.bar,%1> ($1) [b.py(5) definition A] -> [b.py(8) definition bar] <%1> ($1)",
    "<A()/($2).bar,%1> ($1) [b.py(5) definition A] -> [b.py(8) definition bar] <%1> ($2)",
];

#[test]
fn class_field_through_function_parameter() {
    let graph = test_graphs::class_field_through_function_parameter::new();
    check_partial_paths_in_file(&graph, "main.py", CLASS_FIELD_THROUGH_FUNCTION_PARAMETER_MAIN_PATHS);
    check_partial_paths_in_file(&graph, "a.py", CLASS_FIELD_THROUGH_FUNCTION_PARAMETER_A_PATHS);
    check_partial_paths_in_file(&graph, "b.py", CLASS_FIELD_THROUGH_FUNCTION_PARAMETER_B_PATHS);
}

pub(crate) const CYCLIC_IMPORTS_PYTHON_MAIN_PATHS: &[&str] = &[
    "<__main__,%1> ($1) [root] -> [main.py(0) definition __main__] <%1> ($1)",
    "<%1> ($1) [main.py(8) reference a] -> [root] <a,%1> ($1)",
    "<__main__.,%1> ($1) [main.py(0) definition __main__] -> [main.py(8) reference a] <a.,%1> ($1)",
    "<%1> ($1) [main.py(6) reference foo] -> [main.py(8) reference a] <a.foo,%1> ($1)",
];

pub(crate) const CYCLIC_IMPORTS_PYTHON_A_PATHS: &[&str] = &[
    "<a,%1> ($1) [root] -> [a.py(0) definition a] <%1> ($1)",
    "<%1> ($1) [a.py(6) reference b] -> [root] <b,%1> ($1)",
    "<a.,%1> ($1) [a.py(0) definition a] -> [a.py(6) reference b] <b.,%1> ($1)",
];

pub(crate) const CYCLIC_IMPORTS_PYTHON_B_PATHS: &[&str] = &[
    "<b,%1> ($1) [root] -> [b.py(0) definition b] <%1> ($1)",
    "<%1> ($1) [b.py(8) reference a] -> [root] <a,%1> ($1)",
    "<b.,%1> ($1) [b.py(0) definition b] -> [b.py(8) reference a] <a.,%1> ($1)",
    "<b.foo,%1> ($1) [b.py(0) definition b] -> [b.py(6) definition foo] <%1> ($1)",
];

#[test]
fn cyclic_imports_python() {
    let graph = test_graphs::cyclic_imports_python::new();
    check_partial_paths_in_file(&graph, "main.py", CYCLIC_IMPORTS_PYTHON_MAIN_PATHS);
    check_partial_paths_in_file(&graph, "a.py", CYCLIC_IMPORTS_PYTHON_A_PATHS);
    check_partial_paths_in_file(&graph, "b.py", CYCLIC_IMPORTS_PYTHON_B_PATHS);
}

// NOTE: Because everything in this example is local to one file, there aren't any partial
pub(crate) const CYCLIC_IMPORTS_RUST_PATHS: &[&str] = &[
    "<%1> ($1) [test.rs(206) reference b] -> [test.rs(301) definition b] <%1> ($1)",
    "<a::,%1> ($1) [test.rs(201) definition a] -> [test.rs(206) reference b] <b::,%1> ($1)",
    "<a::BAR,%1> ($1) [test.rs(201) definition a] -> [test.rs(204) definition BAR] <%1> ($1)",
    "<%1> ($1) [test.rs(307) reference a] -> [test.rs(201) definition a] <%1> ($1)",
    "<b::,%1> ($1) [test.rs(301) definition b] -> [test.rs(307) reference a] <a::,%1> ($1)",
    "<b::FOO,%1> ($1) [test.rs(301) definition b] -> [test.rs(304) definition FOO] <%1> ($1)",
    "<FOO,%1> ($1) [test.rs(304) definition FOO] -> [test.rs(305) reference BAR] <BAR,%1> ($1)",
    "<%1> ($1) [test.rs(305) reference BAR] -> [test.rs(307) reference a] <a::BAR,%1> ($1)",
    "<%1> ($1) [test.rs(103) reference a] -> [test.rs(201) definition a] <%1> ($1)",
    "<%1> ($1) [test.rs(101) reference FOO] -> [test.rs(103) reference a] <a::FOO,%1> ($1)",
];

#[test]
fn cyclic_imports_rust() {
    let graph = test_graphs::cyclic_imports_rust::new();
    check_partial_paths_in_file(&graph, "test.rs", CYCLIC_IMPORTS_RUST_PATHS);
}

pub(crate) const SEQUENCED_IMPORT_STAR_MAIN_PATHS: &[&str] = &[
    "<__main__,%1> ($1) [root] -> [main.py(0) definition __main__] <%1> ($1)",
    "<%1> ($1) [main.py(8) reference a] -> [root] <a,%1> ($1)",
    "<__main__.,%1> ($1) [main.py(0) definition __main__] -> [main.py(8) reference a] <a.,%1> ($1)",
    "<%1> ($1) [main.py(6) reference foo] -> [main.py(8) reference a] <a.foo,%1> ($1)",
];

pub(crate) const SEQUENCED_IMPORT_STAR_A_PATHS: &[&str] = &[
    "<a,%1> ($1) [root] -> [a.py(0) definition a] <%1> ($1)",
    "<%1> ($1) [a.py(6) reference b] -> [root] <b,%1> ($1)",
    "<a.,%1> ($1) [a.py(0) definition a] -> [a.py(6) reference b] <b.,%1> ($1)",
];

pub(crate) const SEQUENCED_IMPORT_STAR_B_PATHS: &[&str] = &[
    "<b,%1> ($1) [root] -> [b.py(0) definition b] <%1> ($1)",
    "<b.foo,%1> ($1) [b.py(0) definition b] -> [b.py(5) definition foo] <%1> ($1)",
];

#[test]
fn sequenced_import_star() {
    let graph = test_graphs::sequenced_import_star::new();
    check_partial_paths_in_file(&graph, "main.py", SEQUENCED_IMPORT_STAR_MAIN_PATHS);
    check_partial_paths_in_file(&graph, "a.py", SEQUENCED_IMPORT_STAR_A_PATHS);
    check_partial_paths_in_file(&graph, "b.py", SEQUENCED_IMPORT_STAR_B_PATHS);
}
