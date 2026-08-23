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

#[test]
fn xberg_11_removes_the_vulnerable_quick_xml_line() {
    let lock: toml::Value = toml::from_str(include_str!("../Cargo.lock")).expect("parse Cargo.lock");
    assert_eq!(
        direct_dependents(&lock, "quick-xml 0.37.5"),
        [],
        "the vulnerable quick-xml release must not remain in the dependency graph"
    );
    assert_eq!(
        direct_dependents(&lock, "biblib"),
        [("xberg".to_string(), "1.1.0".to_string())],
        "biblib must remain reachable only through the reviewed xberg 1.1 dependency"
    );
}
