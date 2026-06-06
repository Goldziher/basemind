use gitmind::config::{self, ConfigError};

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
    // Defaults applied.
    assert!(cfg.scan.respect_gitignore);
    assert!(!cfg.scan.include.is_empty());
}

#[test]
fn full_url_schema_form_is_accepted() {
    let toml = r#"
"$schema" = "https://gitmind.dev/schema/v1.json"
"#;
    let cfg = config::parse_str(toml).expect("must parse");
    assert_eq!(cfg.schema, "https://gitmind.dev/schema/v1.json");
}
