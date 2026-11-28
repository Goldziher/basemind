use super::*;

fn endpoint(name: &str) -> Endpoint {
    Endpoint::parse(name).unwrap_or_else(|| panic!("`{name}` is a route name"))
}

/// The routes `call` reaches, as `path name`.
fn reached(routes: &Routes, call: &str) -> Vec<String> {
    routes
        .matching(&endpoint(call))
        .into_iter()
        .map(|route| format!("{} {}", route.path, route.name))
        .collect()
}

#[test]
fn a_route_name_parses_in_any_case_and_normalizes_its_path() {
    assert_eq!(endpoint("get /users/{id}").name(), "GET /users/:param");
    assert_eq!(endpoint("POST /").name(), "POST /");
    assert!(Endpoint::parse("FETCH /users").is_none(), "not a method");
    assert!(Endpoint::parse("GET users").is_none(), "the path starts with `/`");
    assert!(Endpoint::parse("open_store").is_none());
}

#[test]
fn an_exact_match_beats_a_suffix_and_a_literal_beats_a_parameter() {
    let routes = Routes::from_names(&[
        ("api.rs", "GET /users/:param"),
        ("api.rs", "GET /users/me"),
        ("api.rs", "GET /users"),
        ("api.rs", "ANY /health"),
        ("files.rs", "GET /static/*"),
    ]);
    assert_eq!(reached(&routes, "GET /users/42"), ["api.rs GET /users/:param"]);
    assert_eq!(reached(&routes, "GET /users/me"), ["api.rs GET /users/me"]);
    assert_eq!(
        reached(&routes, "GET /users/:param"),
        ["api.rs GET /users/:param"],
        "a value the client builds matches only a parameter"
    );
    assert_eq!(reached(&routes, "HEAD /health"), ["api.rs ANY /health"]);
    assert_eq!(reached(&routes, "GET /static/css/site.css"), ["files.rs GET /static/*"]);
    assert!(reached(&routes, "POST /users").is_empty(), "the method must agree");
}

#[test]
fn a_suffix_links_a_client_base_url_or_a_mount_in_another_file() {
    let routes = Routes::from_names(&[("api.rs", "GET /api/v1/orders/:param"), ("svc.rs", "GET /items")]);
    assert_eq!(
        reached(&routes, "GET /orders/7"),
        ["api.rs GET /api/v1/orders/:param"],
        "the client's base URL holds `/api/v1`"
    );
    assert_eq!(
        reached(&routes, "GET /shop/items"),
        ["svc.rs GET /items"],
        "the router is mounted at `/shop` elsewhere"
    );
    assert!(
        reached(&routes, "GET /v2/orders/7").is_empty(),
        "neither path is a suffix of the other"
    );
    let bare = Routes::from_names(&[("a.rs", "GET /:param")]);
    assert!(
        reached(&bare, "GET /x/y").is_empty(),
        "a suffix of parameters alone links nothing"
    );
}

#[test]
fn the_longest_suffix_wins() {
    let routes = Routes::from_names(&[("a.rs", "GET /users/:param/posts"), ("b.rs", "GET /posts")]);
    assert_eq!(
        reached(&routes, "GET /api/users/3/posts"),
        ["a.rs GET /users/:param/posts"]
    );
}

#[test]
fn the_linker_lists_a_call_once_no_route_matches_it_better_and_flags_a_tie() {
    let routes = || {
        Routes::from_names(&[
            ("a/server.rs", "GET /users/:param"),
            ("b/server.rs", "GET /users/:param"),
            ("a/server.rs", "GET /users/me"),
            ("a/server.rs", "POST /orders"),
        ])
    };
    let table = routes();
    let mut users = RouteLinker::new(&table, endpoint("GET /users/:param"));
    assert_eq!(
        users.keep("GET /users/42"),
        Some(Kept {
            resolution: Resolution::PathMatch,
            ambiguous: true
        }),
        "two services expose the path"
    );
    assert_eq!(users.keep("GET /users/me"), None, "the literal route answers it");
    assert_eq!(users.keep("POST /users/42"), None);
    assert_eq!(users.keep("not_a_request"), None);

    let mut orders = RouteLinker::new(&table, endpoint("POST /orders"));
    assert_eq!(
        orders.keep("POST /api/orders"),
        Some(Kept {
            resolution: Resolution::PathMatch,
            ambiguous: false
        })
    );
    let mut unindexed = RouteLinker::new(&table, endpoint("DELETE /cache/:param"));
    assert_eq!(
        unindexed.keep("DELETE /cache/k").map(|kept| kept.ambiguous),
        Some(false),
        "a name no route has still lists the calls it matches"
    );
}

#[test]
fn a_name_no_route_has_is_ambiguous_when_one_route_ties_with_it() {
    let table = Routes::from_names(&[("a.rs", "GET /users/:param")]);
    let mut by_name = RouteLinker::new(&table, endpoint("GET /users/{id}"));
    assert_eq!(
        by_name.keep("GET /users/42").map(|kept| kept.ambiguous),
        Some(false),
        "the name is the indexed route"
    );
    let mut other = RouteLinker::new(&table, endpoint("GET /:param/:param"));
    assert_eq!(
        other.keep("GET /users/42"),
        None,
        "the indexed route names a segment of the path, the name does not"
    );
    let table = Routes::from_names(&[("a.rs", "GET /users/:param/posts")]);
    let mut suffix = RouteLinker::new(&table, endpoint("GET /:param/posts"));
    assert_eq!(
        suffix.keep("GET /users/7/posts"),
        None,
        "the indexed route matches more literal text"
    );
    let mut tied = RouteLinker::new(&table, endpoint("GET /users/:param/posts"));
    assert_eq!(tied.keep("GET /users/7/posts").map(|kept| kept.ambiguous), Some(false));
    let table = Routes::from_names(&[("a.rs", "GET /v1/users/:param")]);
    let mut unindexed = RouteLinker::new(&table, endpoint("GET /v2/users/:param"));
    assert_eq!(
        unindexed.keep("GET /users/7").map(|kept| kept.ambiguous),
        Some(true),
        "the name and the one indexed route match the request equally well"
    );
}

#[test]
fn a_catch_all_never_takes_a_request_from_a_route_naming_its_path() {
    let routes = Routes::from_names(&[
        ("server.js", "GET /*"),
        ("server.js", "GET /:param"),
        ("orders.js", "GET /orders"),
        ("users.js", "GET /users/:param"),
    ]);
    assert_eq!(
        reached(&routes, "GET /api/orders"),
        ["orders.js GET /orders"],
        "the router is mounted at `/api` in another file"
    );
    assert_eq!(reached(&routes, "GET /orders"), ["orders.js GET /orders"]);
    assert_eq!(reached(&routes, "GET /api/users/7"), ["users.js GET /users/:param"]);
    assert_eq!(
        reached(&routes, "GET /about"),
        ["server.js GET /*", "server.js GET /:param"],
        "a page the SPA serves: neither catch-all names its path"
    );
    assert_eq!(reached(&routes, "GET /a/b/c"), ["server.js GET /*"]);

    let table = routes;
    let mut orders = RouteLinker::new(&table, endpoint("GET /orders"));
    assert_eq!(orders.keep("GET /api/orders").map(|kept| kept.ambiguous), Some(false));
    let mut spa = RouteLinker::new(&table, endpoint("GET /*"));
    assert_eq!(spa.keep("GET /api/orders"), None, "the literal route answers it");
    assert!(spa.keep("GET /a/b/c").is_some());
}

#[test]
fn a_wildcard_route_matches_inside_a_request_after_a_base_url() {
    let routes = Routes::from_names(&[("files.rs", "GET /static/*"), ("api.rs", "GET /js/:param")]);
    assert_eq!(
        reached(&routes, "GET /cdn/static/css/site.css"),
        ["files.rs GET /static/*"]
    );
    assert_eq!(reached(&routes, "GET /static/a.css"), ["files.rs GET /static/*"]);
    assert_eq!(
        reached(&Routes::from_names(&[("a.rs", "GET /*")]), "GET /x/y"),
        ["a.rs GET /*"],
        "a bare catch-all matches every path exactly"
    );
}
