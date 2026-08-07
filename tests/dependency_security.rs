//! Supply-chain invariants for temporary advisory exceptions in `deny.toml`.

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
fn quick_xml_037_advisory_ignore_is_confined_to_xberg_endnote_parser() {
    let lock: toml::Value = toml::from_str(include_str!("../Cargo.lock")).expect("parse Cargo.lock");
    assert_eq!(
        direct_dependents(&lock, "quick-xml 0.37.5"),
        [("biblib".to_string(), "0.4.3".to_string())],
        "the temporary quick-xml advisory ignore may cover only biblib 0.4.3"
    );
    assert_eq!(
        direct_dependents(&lock, "biblib"),
        [("xberg".to_string(), "1.0.14".to_string())],
        "the contained biblib parser may remain reachable only through xberg 1.0.14"
    );
}
