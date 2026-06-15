# basemind config schemas

Each file in this directory is a **versioned JSON Schema (Draft 2020-12)**
snapshot for a major version of the basemind config format. The schemas are
**derived from the Rust types**, not the other way around.

## Source of truth

The Rust structs in `src/config/v1.rs` (and the sub-configs under
`src/config/documents.rs`) carry `schemars::JsonSchema` derives. The snapshot
in `basemind-config-v1.schema.json` is regenerated from
`schemars::schema_for!(ConfigV1)` and asserted byte-equal by
`tests/config_schema.rs`. Drift in either direction fails CI.

Hand-editing the schema file is forbidden. To update the snapshot after a
config change, run:

```sh
cargo test --test config_schema -- --ignored regenerate_schema
```

## Versioning policy

- New schema = new file. `basemind-config-v1.schema.json` is immutable once
  shipped; v2 lands as `basemind-config-v2.schema.json`.
- The TOML config carries a top-level `$schema` field. Validation picks the
  matching schema by that value.
- Migration between versions lives in `src/config/migrate.rs` as
  `migrate_v1_to_v2(toml: Value) -> Value` functions. `basemind config migrate`
  runs the chain and rewrites the user's TOML in place.
