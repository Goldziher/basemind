use super::*;
use crate::engine::refs::routes::{RouteLinker, score};

const METHODS: [&str; 3] = ["GET", "POST", "ANY"];

/// `services * 50` routes in the shapes services expose: collections, members, nested members,
/// a literal action, plus per-service static mounts and a few catch-alls.
fn synthetic(services: usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for s in 0..services {
        let file = format!("svc{s}/routes.rs");
        for r in 0..10 {
            let method = METHODS[(s + r) % METHODS.len()];
            for path in [
                format!("/svc{s}/res{r}"),
                format!("/svc{s}/res{r}/:param"),
                format!("/svc{s}/res{r}/:param/items"),
                format!("/res{r}/:param/action{s}"),
                format!("/v{}/res{r}/:param.json", s % 3),
            ] {
                out.push((file.clone(), format!("{method} {path}")));
            }
        }
        if s % 50 == 0 {
            out.push((file.clone(), format!("GET /static{s}/*")));
            out.push((file.clone(), "GET /:param".to_string()));
            out.push((file, "GET /*".to_string()));
        }
    }
    out
}

/// Distinct requests: exact, behind a base URL, a client-built parameter, and misses.
fn requests(services: usize, n: usize) -> Vec<String> {
    (0..n)
        .map(|i| {
            let s = (i * 7) % services;
            let r = i % 10;
            let method = ["GET", "POST", "DELETE"][i % 3];
            match i % 6 {
                0 => format!("{method} /svc{s}/res{r}/{i}"),
                1 => format!("{method} /api/svc{s}/res{r}/{i}/items"),
                2 => format!("{method} /res{r}/:param/action{s}"),
                3 => format!("{method} /cdn/static{}/a/{i}.css", s - s % 50),
                4 => format!("{method} /v{}/res{r}/{i}.json", s % 3),
                _ => format!("{method} /nothing/{i}"),
            }
        })
        .collect()
}

fn table(names: &[(String, String)]) -> Routes {
    let pairs: Vec<(&str, &str)> = names.iter().map(|(p, n)| (p.as_str(), n.as_str())).collect();
    Routes::from_names(&pairs)
}

/// The routes matching `call` best, scoring every route: what the table answered before it was
/// bucketed.
fn every_route(routes: &Routes, call: &Endpoint) -> Vec<(String, u32)> {
    let scored: Vec<_> = routes
        .defs
        .iter()
        .filter_map(|route| Some((score(&route.endpoint, call)?, route)))
        .collect();
    let Some(best) = scored.iter().map(|(score, _)| *score).max() else {
        return Vec::new();
    };
    let mut out: Vec<(String, u32)> = scored
        .into_iter()
        .filter(|(score, _)| *score == best)
        .map(|(_, route)| (route.path.to_string(), route.line))
        .collect();
    out.sort();
    out
}

fn bucketed(routes: &Routes, call: &Endpoint) -> Vec<(String, u32)> {
    let mut out: Vec<(String, u32)> = routes
        .matching(call)
        .into_iter()
        .map(|route| (route.path.to_string(), route.line))
        .collect();
    out.sort();
    out
}

#[test]
fn the_buckets_answer_what_scoring_every_route_answers() {
    let names = synthetic(120);
    let routes = table(&names);
    assert_eq!(routes.len(), names.len());
    let mut matched = 0;
    for request in requests(120, 3_000) {
        let call = Endpoint::parse(&request).expect("a request");
        let want = every_route(&routes, &call);
        matched += usize::from(!want.is_empty());
        assert_eq!(bucketed(&routes, &call), want, "{request}");
    }
    assert!(matched > 1_500, "the requests exercise matches, not misses: {matched}");
}

#[test]
fn a_handler_is_found_by_the_last_component_of_its_name() {
    let def = |path: &str, name: &str, handler: &str| RouteDef {
        path: RelPath::from(path),
        name: name.to_string(),
        endpoint: Endpoint::parse(name).expect("a route name"),
        handler: Some(handler.to_string()),
        line: 1,
        column: 0,
        start_byte: 0,
    };
    let routes = Routes::from_defs(vec![
        def("server.rs", "GET /users/:param", "show_user"),
        def("server.js", "GET /api/users/:param", "users.show"),
        def("main.rs", "PUT /users/:param", "handlers::show"),
    ]);
    let named = |name: &str| -> Vec<String> { routes.naming(name).map(|route| route.name.clone()).collect() };
    assert_eq!(named("show_user"), ["GET /users/:param"]);
    assert_eq!(named("show"), ["GET /api/users/:param", "PUT /users/:param"]);
    assert!(named("users").is_empty(), "the qualifier is not the handler");
}

/// The cost S3 of the review measured: a request the target matches is judged against the other
/// routes, and a `definition` lookup ranks every route for its call. Before, every route was scored
/// per request; the buckets score only those that can match. A catch-all target matches every
/// request, so each is judged. Run with `--ignored --nocapture` to print the timings.
#[test]
#[ignore = "timing; prints numbers"]
fn bucketed_linking_is_faster_than_scoring_every_route() {
    let names = synthetic(400);
    let started = std::time::Instant::now();
    let routes = table(&names);
    let build = started.elapsed();
    let requests = requests(400, 5_000);
    let calls: Vec<Endpoint> = requests
        .iter()
        .map(|r| Endpoint::parse(r).expect("a request"))
        .collect();

    let started = std::time::Instant::now();
    let linear: Vec<_> = calls.iter().map(|call| every_route(&routes, call)).collect();
    let linear_time = started.elapsed();
    let started = std::time::Instant::now();
    let buckets: Vec<_> = calls.iter().map(|call| bucketed(&routes, call)).collect();
    let buckets_time = started.elapsed();
    assert_eq!(linear, buckets, "both rank the same routes best");

    let target = Endpoint::parse("GET /*").expect("a route");
    let started = std::time::Instant::now();
    let mut linear_kept = 0usize;
    for call in &calls {
        let Some(own) = score(&target, call) else {
            continue;
        };
        linear_kept += usize::from(
            !routes
                .defs
                .iter()
                .any(|route| score(&route.endpoint, call).is_some_and(|other| other > own)),
        );
    }
    let linear_link = started.elapsed();
    let started = std::time::Instant::now();
    let mut linker = RouteLinker::new(&routes, target);
    let kept = requests.iter().filter(|request| linker.keep(request).is_some()).count();
    let buckets_link = started.elapsed();
    assert_eq!(kept, linear_kept, "both judge the same requests callers");

    println!(
        "routes {}, distinct requests {}: table build {build:?}; best routes per request: every route \
         {linear_time:?}, buckets {buckets_time:?}; catch-all callers: every route {linear_link:?}, buckets \
         {buckets_link:?}",
        routes.len(),
        requests.len(),
    );
    assert!(buckets_time < linear_time, "{buckets_time:?} vs {linear_time:?}");
}
