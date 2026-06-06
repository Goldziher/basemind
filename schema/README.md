# gitmind config schemas

Each file in this directory is a **versioned JSON Schema (Draft 2020-12)** that
describes a major version of the gitmind config format.

## Versioning policy

- New schema = new file. `gitmind-config-v1.schema.json` is immutable once
  shipped; v2 lands as `gitmind-config-v2.schema.json`.
- The TOML config carries a top-level `$schema` field. Validation picks the
  matching schema by that value.
- Migration between versions lives in `src/config/migrate.rs` as
  `migrate_v1_to_v2(toml: Value) -> Value` functions. `gitmind config migrate`
  runs the chain and rewrites the user's TOML in place.
- The Rust type for each version is generated from the schema by `build.rs`
  (typify). Hand-written code wraps the generated types with validate / load /
  migrate helpers.
