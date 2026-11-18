//! HTTP routes and literal-URL client calls at the extraction level (ADR-0048): each server
//! framework's routes become `Route` symbols named `METHOD /normalized/path`, and each client's
//! literal-URL requests become call rows with the same callee shape.

use basemind::extract::SymbolKind;
use basemind::extract::l1::extract_l1;
use basemind::extract::l2::extract_l2;

fn fixture(rel: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/http/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// `(name, handler)` of every route symbol, in document order.
fn routes(lang: &'static str, rel: &str) -> Vec<(String, Option<String>)> {
    extract_l1(lang, &fixture(rel))
        .expect("extract l1")
        .symbols
        .into_iter()
        .filter(|s| s.kind == SymbolKind::Route)
        .map(|s| (s.name, s.signature))
        .collect()
}

/// Every HTTP callee (`METHOD /path`) among the file's calls, in document order.
fn requests(lang: &'static str, rel: &str) -> Vec<String> {
    extract_l2(lang, &fixture(rel))
        .expect("extract l2")
        .calls
        .into_iter()
        .map(|c| c.callee)
        .filter(|c| c.contains(' '))
        .collect()
}

fn pairs(want: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
    want.iter()
        .map(|(n, h)| ((*n).to_string(), h.map(str::to_string)))
        .collect()
}

#[test]
fn express_routes_fold_literal_mounts() {
    assert_eq!(
        routes("javascript", "express/server.js"),
        pairs(&[
            ("GET /health", None),
            ("GET /api/users", Some("listUsers")),
            ("GET /api/users/:param", Some("users.show")),
            ("PUT /api/users/:param", Some("users.update")),
            ("DELETE /api/admin/cache/:param", Some("clearCache")),
            ("ANY /ping", Some("ping")),
        ])
    );
    assert_eq!(
        routes("typescript", "express/server.ts"),
        pairs(&[
            ("POST /orders", Some("createOrder")),
            ("PATCH /orders/:param", Some("createOrder")),
        ])
    );
}

#[test]
fn fetch_and_axios_literal_urls_are_call_rows() {
    assert_eq!(
        requests("typescript", "fetch/client.ts"),
        [
            "GET /users/:param",
            "POST /api/users",
            "GET /api/users/:param",
            "DELETE /api/cache/k",
            "PUT /api/users/7",
            "GET /orders",
        ]
    );
}

#[test]
fn fastapi_routes_fold_router_prefixes() {
    assert_eq!(
        routes("python", "fastapi/app.py"),
        pairs(&[
            ("GET /api/users/:param", Some("get_user")),
            ("POST /api/users", Some("create_user")),
            ("GET /ping", Some("ping")),
            ("HEAD /ping", Some("ping")),
            ("GET /items/:param", Some("read_item")),
        ])
    );
}

#[test]
fn flask_routes_read_methods_and_blueprint_prefixes() {
    assert_eq!(
        routes("python", "flask/app.py"),
        pairs(&[
            ("GET /", Some("index")),
            ("GET /login", Some("login")),
            ("POST /login", Some("login")),
            ("GET /posts/:param", Some("show")),
            ("DELETE /posts/:param", Some("remove")),
        ])
    );
}

#[test]
fn requests_and_httpx_literal_urls_are_call_rows() {
    assert_eq!(
        requests("python", "python_client/clients.py"),
        [
            "GET /users/:param",
            "POST /users",
            "PUT /users/7",
            "DELETE /users/:param/sessions",
            "GET /orders",
        ]
    );
}

#[test]
fn axum_method_routers_name_each_method() {
    assert_eq!(
        routes("rust", "axum/main.rs"),
        pairs(&[
            ("GET /users", Some("list_users")),
            ("POST /users", Some("create_user")),
            ("GET /users/:param", Some("show_user")),
            ("GET /health", None),
        ])
    );
}

#[test]
fn axum_route_starts_at_its_route_call() {
    let l1 = extract_l1("rust", &fixture("axum/main.rs")).expect("extract l1");
    let rows: Vec<u32> = l1
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Route)
        .map(|s| s.start_row)
        .collect();
    assert_eq!(rows, [17, 17, 18, 19], "0-based rows of each `.route(` line");
}

#[test]
fn reqwest_literal_urls_are_call_rows() {
    assert_eq!(
        requests("rust", "axum/client.rs"),
        ["GET /users", "POST /users/42/sessions", "PATCH /users/7"]
    );
}

#[test]
fn gin_routes_fold_nested_groups() {
    assert_eq!(
        routes("go", "gin/main.go"),
        pairs(&[
            ("GET /ping", None),
            ("GET /api/v1/users", Some("h.List")),
            ("GET /api/v1/users/:param", Some("showUser")),
            ("POST /api/v1/users", Some("h.List")),
            ("ANY /static/*", Some("h.List")),
        ])
    );
}

#[test]
fn net_http_literal_urls_are_call_rows() {
    assert_eq!(
        requests("go", "gin/client.go"),
        [
            "GET /api/v1/users",
            "POST /api/v1/users",
            "DELETE /api/v1/users/9",
            "PUT /api/v1/users/9",
            "GET /ping",
        ]
    );
}

#[test]
fn spring_routes_fold_the_class_mapping() {
    assert_eq!(
        routes("java", "spring/OwnerController.java"),
        pairs(&[
            ("GET /owners", Some("list")),
            ("GET /owners/:param", Some("show")),
            ("POST /owners/:param/pets", Some("addPet")),
            ("PUT /owners/:param/visits", Some("visits")),
            ("PATCH /owners/:param/visits", Some("visits")),
            ("DELETE /cache", Some("clear")),
        ])
    );
}

/// A server route and a client request for the same endpoint carry the same name.
#[test]
fn a_route_and_its_client_call_compare_equal() {
    let server: Vec<String> = routes("go", "gin/main.go").into_iter().map(|(n, _)| n).collect();
    for call in requests("go", "gin/client.go") {
        if call.starts_with("DELETE") || call.starts_with("PUT") || call == "GET /ping" {
            continue;
        }
        assert!(server.contains(&call), "{call} has no route in {server:?}");
    }
}

/// Every route and client call keeps the ordinary call rows it had: `fetch` is still a callee.
#[test]
fn ordinary_call_rows_are_kept() {
    let calls: Vec<String> = extract_l2("typescript", &fixture("fetch/client.ts"))
        .expect("extract l2")
        .calls
        .into_iter()
        .map(|c| c.callee)
        .collect();
    assert_eq!(calls.iter().filter(|c| *c == "fetch").count(), 3, "{calls:?}");
}
