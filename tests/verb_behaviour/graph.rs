//! `code_refs` over the file-level import graph: `importers` with `path` (a file's importers, by
//! depth) and `cycles`. The graph holds edges only where a resolver is compiled in; without one the
//! answer is empty and says why.

use serde_json::json;

use super::{call, started_with};

/// One crate: `a` and `b` import each other, `c` imports `a`, `d` imports `c`; `p`, `q` and `r` form
/// a component whose shortest loop from `p` is `p -> q`, so it is listed with a `chain`. `lib.rs`
/// only declares the modules, which the default tier does not follow.
const CRATE: [(&str, &str); 9] = [
    (
        "g/Cargo.toml",
        "[package]\nname = \"g\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    ),
    (
        "g/src/lib.rs",
        "pub mod a;\npub mod b;\npub mod c;\npub mod d;\npub mod p;\npub mod q;\npub mod r;\n",
    ),
    ("g/src/a.rs", "use crate::b::B;\npub struct A(pub Option<B>);\n"),
    ("g/src/b.rs", "use crate::a::A;\npub struct B(pub Box<Option<A>>);\n"),
    ("g/src/c.rs", "use crate::a::A;\npub struct C(pub A);\n"),
    ("g/src/d.rs", "use crate::c::C;\npub struct D(pub C);\n"),
    ("g/src/p.rs", "use crate::q::Q;\npub struct P(pub Box<Option<Q>>);\n"),
    (
        "g/src/q.rs",
        "use crate::p::P;\nuse crate::r::R;\npub struct Q(pub Box<Option<P>>, pub Box<Option<R>>);\n",
    ),
    ("g/src/r.rs", "use crate::q::Q;\npub struct R(pub Box<Option<Q>>);\n"),
];

/// Direct importers by default; `depth` 2 adds the importers of those, and `in_cycle` names the
/// importers the file reaches back.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_importers_of_a_path_walk_the_import_graph_by_depth() {
    let (_dir, client) = started_with(&CRATE).await;

    let direct = call(
        &client,
        "code_refs",
        json!({ "relation": "importers", "path": "g/src/a.rs" }),
    )
    .await;
    assert_eq!(
        direct,
        json!({
            "total": 2,
            "by_depth": { "1": ["g/src/b.rs", "g/src/c.rs"] },
            "in_cycle": ["g/src/b.rs"],
            "truncated": true,
            "truncated_reason": "depth",
        }),
        "{direct}"
    );

    let two = call(
        &client,
        "code_refs",
        json!({ "relation": "importers", "path": "g/src/a.rs", "depth": 2 }),
    )
    .await;
    assert_eq!(
        two,
        json!({
            "total": 3,
            "by_depth": { "1": ["g/src/b.rs", "g/src/c.rs"], "2": ["g/src/d.rs"] },
            "in_cycle": ["g/src/b.rs"],
        }),
        "{two}"
    );

    let first = call(
        &client,
        "code_refs",
        json!({ "relation": "importers", "path": "g/src/a.rs", "depth": 2, "limit": 2 }),
    )
    .await;
    assert_eq!(
        first["by_depth"],
        json!({ "1": ["g/src/b.rs", "g/src/c.rs"] }),
        "{first}"
    );
    let cursor = first["next_cursor"].as_str().expect("a second page").to_string();
    let rest = call(
        &client,
        "code_refs",
        json!({ "relation": "importers", "path": "g/src/a.rs", "depth": 2, "limit": 2, "cursor": cursor }),
    )
    .await;
    assert_eq!(rest["by_depth"], json!({ "2": ["g/src/d.rs"] }), "{rest}");
    assert!(rest.get("next_cursor").is_none(), "{rest}");

    let leaf = call(
        &client,
        "code_refs",
        json!({ "relation": "importers", "path": "g/src/d.rs" }),
    )
    .await;
    assert_eq!(leaf, json!({ "total": 0, "by_depth": {} }), "{leaf}");
}

/// Every cycle, smallest first, a component the shortest loop does not cover listed with its
/// `chain`, and with `path` only the cycle holding that file.
#[cfg(any(feature = "code-intel-js", feature = "code-intel-stack"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_cycles_lists_each_import_cycle_with_a_closing_chain() {
    let (_dir, client) = started_with(&CRATE).await;
    let rust_note = "rust: modules of one crate may import each other; a cycle is legal and informational";

    let mut all = call(&client, "code_refs", json!({ "relation": "cycles" })).await;
    // The base fixture's Python file is named here when this build resolves no Python imports.
    if let Some(coverage) = all.as_object_mut().and_then(|answer| answer.remove("coverage")) {
        assert!(
            coverage
                .as_str()
                .is_some_and(|note| note.contains("python") && !note.contains("rust")),
            "{coverage}"
        );
    }
    assert_eq!(
        all,
        json!({
            "total": 2,
            "cycles": [
                { "files": ["g/src/a.rs", "g/src/b.rs"] },
                {
                    "files": ["g/src/p.rs", "g/src/q.rs", "g/src/r.rs"],
                    "chain": ["g/src/p.rs", "g/src/q.rs"],
                },
            ],
            "notes": [rust_note],
        }),
        "{all}"
    );

    let one = call(
        &client,
        "code_refs",
        json!({ "relation": "cycles", "path": "g/src/r.rs" }),
    )
    .await;
    assert_eq!(one["total"], 1, "{one}");
    assert_eq!(
        one["cycles"][0]["files"],
        json!(["g/src/p.rs", "g/src/q.rs", "g/src/r.rs"]),
        "{one}"
    );

    let none = call(
        &client,
        "code_refs",
        json!({ "relation": "cycles", "path": "g/src/d.rs" }),
    )
    .await;
    assert_eq!(none, json!({ "total": 0, "cycles": [] }), "{none}");

    let page = call(&client, "code_refs", json!({ "relation": "cycles", "limit": 1 })).await;
    assert_eq!(page["cycles"].as_array().map(Vec::len), Some(1), "{page}");
    assert!(page["next_cursor"].is_string(), "{page}");
}

/// Without a compiled-in resolver the graph has no edges, and the answer says so instead of
/// reading as "no importers" or "no cycles".
#[cfg(not(any(feature = "code-intel-js", feature = "code-intel-stack")))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_refs_graph_relations_name_the_missing_resolver() {
    let (_dir, client) = started_with(&CRATE).await;

    let importers = call(
        &client,
        "code_refs",
        json!({ "relation": "importers", "path": "g/src/a.rs" }),
    )
    .await;
    assert_eq!(importers["total"], 0, "{importers}");
    let coverage = importers["coverage"].as_str().unwrap_or_default();
    assert!(
        coverage.contains("no import resolver for rust") && coverage.contains("{name, relation:\"importers\"}"),
        "{importers}"
    );

    let cycles = call(&client, "code_refs", json!({ "relation": "cycles" })).await;
    assert_eq!(cycles["total"], 0, "{cycles}");
    assert!(
        cycles["coverage"]
            .as_str()
            .is_some_and(|note| note.contains("no cycle is known")),
        "{cycles}"
    );
}
