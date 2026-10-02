//! Supply-chain invariants for dependency security fixes.

fn packages(lock: &toml::Value) -> &[toml::Value] {
    lock.get("package")
        .and_then(toml::Value::as_array)
        .map(Vec::as_slice)
        .expect("Cargo.lock package array")
}

fn direct_dependents(lock: &toml::Value, dependency: &str) -> Vec<(String, String)> {
    let mut matches: Vec<_> = packages(lock)
        .iter()
        .filter(|package| {
            package
                .get("dependencies")
                .and_then(toml::Value::as_array)
                .is_some_and(|dependencies| dependencies.iter().any(|item| item.as_str() == Some(dependency)))
        })
        .map(|package| {
            let name = package["name"].as_str().expect("package name").to_string();
            let version = package["version"].as_str().expect("package version").to_string();
            (name, version)
        })
        .collect();
    matches.sort();
    matches
}

fn version_at_least(version: &str, minimum: (u64, u64)) -> bool {
    let mut parts = version
        .split('.')
        .map(|part| part.parse::<u64>().expect("numeric version component"));
    let major = parts.next().expect("major version");
    let minor = parts.next().expect("minor version");
    (major, minor) >= minimum
}

#[test]
fn xberg_11_removes_the_vulnerable_quick_xml_line() {
    let lock: toml::Value = toml::from_str(include_str!("../Cargo.lock")).expect("parse Cargo.lock");

    // The vulnerable quick-xml 0.37 line must be absent, whether it is referenced as a bare name or
    // as the `name version` form Cargo.lock uses when several versions coexist.
    let vulnerable: Vec<_> = packages(&lock)
        .iter()
        .filter(|package| package["name"].as_str() == Some("quick-xml"))
        .filter_map(|package| package["version"].as_str())
        .filter(|version| version.starts_with("0.37."))
        .collect();
    assert!(
        vulnerable.is_empty(),
        "the vulnerable quick-xml 0.37 line must not remain in the dependency graph: {vulnerable:?}"
    );

    // xberg 1.1 is the release that dropped that line; any later release must keep it dropped, so
    // assert a floor rather than an exact version that every routine bump would invalidate.
    let xberg_versions: Vec<_> = packages(&lock)
        .iter()
        .filter(|package| package["name"].as_str() == Some("xberg"))
        .map(|package| package["version"].as_str().expect("package version"))
        .collect();
    assert!(!xberg_versions.is_empty(), "xberg must be locked");
    for version in &xberg_versions {
        assert!(
            version_at_least(version, (1, 1)),
            "xberg {version} predates 1.1, which removed the vulnerable quick-xml line"
        );
    }

    // biblib must remain reachable only through the reviewed xberg dependency.
    let dependents: Vec<String> = direct_dependents(&lock, "biblib")
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        dependents,
        ["xberg"],
        "biblib must remain reachable only through the reviewed xberg dependency"
    );
}
