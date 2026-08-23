//! Stable broker response conversions.

use crate::comms::protocol::CommsResponse;

/// Map a registry persistence failure into a stable-token error response.
pub(super) fn registry_error(error: crate::registry::RegistryError) -> CommsResponse {
    CommsResponse::Error {
        code: "registry_error".to_string(),
        message: error.to_string(),
    }
}
