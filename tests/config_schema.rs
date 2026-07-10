use basemind::config::{self, ConfigError, ConfigLayers, ConfigSource, ConfigV1, DocumentsCliOverrides, merge_layers};

#[cfg(feature = "full")]
const SCHEMA_PATH: &str = "schema/basemind-config-v1.schema.json";

#[cfg(feature = "full")]
fn generate_schema_text() -> String {
    let schema = schemars::schema_for!(ConfigV1);
    let mut s = serde_json::to_string_pretty(&schema).expect("schema serializes");
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

#[cfg(feature = "full")]
#[test]
fn schema_snapshot_matches_derived() {
    let derived = generate_schema_text();
    let committed = std::fs::read_to_string(SCHEMA_PATH).expect("read committed schema");
    if derived != committed {
        let derived_lines: Vec<&str> = derived.lines().collect();
        let committed_lines: Vec<&str> = committed.lines().collect();
        eprintln!("--- committed (head) ---");
        for line in committed_lines.iter().take(20) {
            eprintln!("{line}");
        }
        eprintln!("--- derived (head) ---");
        for line in derived_lines.iter().take(20) {
            eprintln!("{line}");
        }
        panic!(
            "schema/basemind-config-v1.schema.json is out of sync with `schemars::schema_for!(ConfigV1)`. \
             Run `cargo test --test config_schema -- --ignored regenerate_schema` to update the snapshot."
        );
    }
}

/// Regenerate the committed snapshot. Gated behind `#[ignore]` so updating the
/// schema is always an explicit, audited step. Same `full`-feature gate as
/// the assertion above so regen and assert see the same dep graph.
#[cfg(feature = "full")]
#[test]
#[ignore]
fn regenerate_schema() {
    let derived = generate_schema_text();
    std::fs::write(SCHEMA_PATH, derived).expect("write schema");
    eprintln!("wrote {SCHEMA_PATH}");
}

#[test]
fn precedence_cli_beats_env_beats_file() {
    let defaults = ConfigV1::with_defaults();

    let mut file_cfg = ConfigV1::with_defaults();
    file_cfg.documents.reranker.preset = "x".to_string();

    let env = DocumentsCliOverrides {
        reranker_preset: Some("y".to_string()),
        ..DocumentsCliOverrides::default()
    };
    let cli = DocumentsCliOverrides {
        reranker_preset: Some("z".to_string()),
        ..DocumentsCliOverrides::default()
    };

    let loaded = merge_layers(
        defaults,
        ConfigLayers {
            toml_file: Some(file_cfg),
            env: Some(env),
            cli: Some(cli),
        },
    );

    assert_eq!(loaded.config.documents.reranker.preset, "z");
    assert_eq!(
        loaded.provenance.get("documents.reranker.preset"),
        Some(&ConfigSource::Cli)
    );
}

#[test]
fn precedence_env_wins_when_no_cli() {
    let defaults = ConfigV1::with_defaults();
    let env = DocumentsCliOverrides {
        reranker_preset: Some("from-env".to_string()),
        ..DocumentsCliOverrides::default()
    };
    let loaded = merge_layers(
        defaults,
        ConfigLayers {
            toml_file: None,
            env: Some(env),
            cli: None,
        },
    );
    assert_eq!(loaded.config.documents.reranker.preset, "from-env");
    assert_eq!(
        loaded.provenance.get("documents.reranker.preset"),
        Some(&ConfigSource::Env)
    );
}

#[test]
fn defaults_layer_yields_default_provenance() {
    let loaded = config::defaults_only();
    assert_eq!(
        loaded.provenance.get("documents.reranker.preset"),
        Some(&ConfigSource::Default)
    );
    assert_eq!(
        loaded.provenance.get("documents.output.format"),
        Some(&ConfigSource::Default)
    );
}

#[test]
fn missing_schema_field_is_rejected() {
    let toml = r#"
[scan]
include = ["**/*.rs"]
"#;
    let err = config::parse_str(toml).expect_err("must fail");
    assert!(matches!(err, ConfigError::MissingSchema));
}

#[test]
fn unknown_schema_version_is_rejected() {
    let toml = r#"
"$schema" = "v99"
"#;
    let err = config::parse_str(toml).expect_err("must fail");
    assert!(matches!(err, ConfigError::UnknownSchema(_)));
}

#[test]
fn schema_validation_surfaces_json_pointer() {
    let toml = r#"
"$schema" = "v1"

[scan]
max_file_bytes = 100
"#;
    let err = config::parse_str(toml).expect_err("must fail");
    match err {
        ConfigError::SchemaValidation(msg) => {
            assert!(
                msg.contains("/scan/max_file_bytes"),
                "expected JSON Pointer path in error, got: {msg}"
            );
        }
        other => panic!("expected SchemaValidation, got {other:?}"),
    }
}

#[test]
fn additional_properties_are_rejected() {
    let toml = r#"
"$schema" = "v1"
who_is_this = "field"
"#;
    let err = config::parse_str(toml).expect_err("must fail");
    assert!(matches!(err, ConfigError::SchemaValidation(_)));
}

#[test]
fn minimal_valid_config_parses() {
    let toml = r#"
"$schema" = "v1"
"#;
    let cfg = config::parse_str(toml).expect("must parse");
    assert_eq!(cfg.schema, "v1");
    assert!(cfg.scan.respect_gitignore);
    assert!(!cfg.scan.include.is_empty());
}

#[test]
fn full_url_schema_form_is_accepted() {
    let toml = r#"
"$schema" = "https://basemind.dev/schema/v1.json"
"#;
    let cfg = config::parse_str(toml).expect("must parse");
    assert_eq!(cfg.schema, "https://basemind.dev/schema/v1.json");
}
