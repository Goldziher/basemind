// Migration chain across schema versions.
//
// Each function rewrites a parsed TOML/JSON Value from version N → N+1.
// `migrate_to_latest` walks the chain. v1 is current, so the chain is empty.

use serde_json::Value;

use super::ConfigError;

#[allow(dead_code)] // wired in when v2 lands
pub fn migrate_to_latest(value: Value) -> Result<Value, ConfigError> {
    Ok(value)
}
