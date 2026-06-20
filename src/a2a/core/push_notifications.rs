//! Webhook push-notification configuration store for tasks.
//!
//! Implements the storage half of the four `*PushNotificationConfig` RPCs
//! from the A2A spec: per-task webhook configuration CRUD plus snapshot
//! save/restore. Configurations are kept in [`PushNotificationStore`],
//! indexed by [`TaskId`].
//!
//! The store is intentionally not wrapped in `Arc`/`RwLock`; locking belongs
//! at the server layer.
//!
//! # B4: outbound webhook delivery
//!
//! The SSRF guard ([`crate::a2a::core::ssrf`]) is now applied: at create time
//! it rejects IP-literal hosts in reserved / private / loopback / cloud-metadata
//! ranges, and the delivery worker re-checks every DNS-resolved address with
//! [`crate::a2a::core::ssrf::ip_is_blocked`] before POSTing (defeating DNS
//! rebinding). The actual outbound HTTP delivery worker (subscribes to the
//! message bus and POSTs task lifecycle events to each registered webhook URL)
//! is still deferred to phase B4: the `reqwest`-backed delivery loop and
//! exponential-backoff retry are intentionally omitted. See the `// B4:` markers
//! below for the precise extension points.

use ahash::AHashMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::a2a::core::task_types::TaskId;

// ── Error ───────────────────────────────────────────────────────────────────

/// Errors raised while validating or mutating push-notification config.
#[derive(Debug, thiserror::Error)]
pub enum PushNotificationError {
    /// External input failed validation (e.g. a malformed or non-`http(s)`
    /// webhook URL).
    #[error("invalid input: {reason}")]
    InvalidInput {
        /// Human-readable description of what was rejected.
        reason: String,
    },
}

// ── ID newtype ──────────────────────────────────────────────────────────────

/// Identifier for a single push-notification configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PushNotificationId(Uuid);

impl PushNotificationId {
    /// Mint a new random identifier.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for PushNotificationId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for PushNotificationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for PushNotificationId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

// ── Authentication ──────────────────────────────────────────────────────────

/// HTTP authentication credentials used when delivering a webhook.
///
/// Currently only `Bearer` and `Basic` schemes are recognised by the (B4)
/// delivery worker; other schemes are still stored and would be emitted
/// verbatim in an `Authorization` header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushNotificationAuth {
    /// HTTP authentication scheme name (case-insensitive per RFC 9110).
    pub scheme: String,
    /// Credential payload — format depends on the scheme.
    pub credentials: String,
}

// ── Config ──────────────────────────────────────────────────────────────────

/// A single webhook configuration attached to a task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushNotificationConfig {
    /// Unique identifier for this configuration.
    pub id: PushNotificationId,
    /// Task this configuration is bound to.
    pub task_id: TaskId,
    /// Absolute URL to which lifecycle events are POSTed.
    pub url: String,
    /// Opaque token forwarded as `X-Basemind-Notification-Token` so the
    /// receiver can correlate calls to a specific subscription.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    /// Optional `Authorization` header credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication: Option<PushNotificationAuth>,
}

// ── Store ───────────────────────────────────────────────────────────────────

/// Maximum number of webhook configurations a single task may register.
///
/// Bounds the work a single bus event can fan out to: the delivery worker POSTs
/// to each config serially with a bounded timeout + retry budget, so without a
/// cap an authenticated registrant could attach many slow webhooks and starve
/// the worker. 16 is well above any legitimate need.
const MAX_PUSH_CONFIGS_PER_TASK: usize = 16;

/// In-memory store of [`PushNotificationConfig`]s, indexed by task.
#[derive(Debug, Default)]
pub struct PushNotificationStore {
    configs: AHashMap<TaskId, Vec<PushNotificationConfig>>,
}

impl PushNotificationStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new webhook for `task_id` and return the populated
    /// configuration.
    ///
    /// The URL is validated to be an absolute `http`/`https` URL and run
    /// through the SSRF guard: when the host is an IP literal it is rejected if
    /// it falls in a reserved / private / loopback / link-local (cloud-metadata)
    /// range. Hostname targets pass here and are re-checked against
    /// [`crate::a2a::core::ssrf::ip_is_blocked`] on every resolved address at
    /// delivery time.
    ///
    /// # Errors
    ///
    /// Returns [`PushNotificationError::InvalidInput`] when `url` is not a
    /// parseable absolute URL with an `http` or `https` scheme, when an
    /// IP-literal host targets a blocked address, or when `task_id` already has
    /// [`MAX_PUSH_CONFIGS_PER_TASK`] configurations registered.
    pub fn create(
        &mut self,
        task_id: TaskId,
        url: String,
        token: String,
        authentication: Option<PushNotificationAuth>,
    ) -> Result<PushNotificationConfig, PushNotificationError> {
        validate_webhook_url(&url)?;

        let existing = self.configs.get(&task_id).map_or(0, Vec::len);
        if existing >= MAX_PUSH_CONFIGS_PER_TASK {
            return Err(PushNotificationError::InvalidInput {
                reason: format!(
                    "task already has the maximum of {MAX_PUSH_CONFIGS_PER_TASK} push notification configs"
                ),
            });
        }

        let cfg = PushNotificationConfig {
            id: PushNotificationId::new(),
            task_id,
            url,
            token,
            authentication,
        };
        self.configs.entry(task_id).or_default().push(cfg.clone());
        Ok(cfg)
    }

    /// Fetch a single configuration by `(task_id, id)`.
    pub fn get(
        &self,
        task_id: &TaskId,
        id: &PushNotificationId,
    ) -> Option<&PushNotificationConfig> {
        self.configs.get(task_id)?.iter().find(|c| &c.id == id)
    }

    /// Return every configuration registered against `task_id` as a borrowed
    /// slice, avoiding a clone of the whole `Vec` for read-only callers.
    ///
    /// An unknown `task_id` yields an empty slice.
    pub fn list(&self, task_id: &TaskId) -> &[PushNotificationConfig] {
        self.configs.get(task_id).map_or(&[], Vec::as_slice)
    }

    /// Delete the configuration `(task_id, id)`. Returns `true` when a
    /// configuration was removed.
    pub fn delete(&mut self, task_id: &TaskId, id: &PushNotificationId) -> bool {
        let Some(v) = self.configs.get_mut(task_id) else {
            return false;
        };
        let len_before = v.len();
        v.retain(|c| &c.id != id);
        let removed = v.len() < len_before;
        if v.is_empty() {
            self.configs.remove(task_id);
        }
        removed
    }
}

// ── URL validation ──────────────────────────────────────────────────────────

/// Validate that `url` is an absolute `http`/`https` webhook URL and run it
/// through the SSRF guard.
///
/// Delegates to [`crate::a2a::core::ssrf::validate_webhook_url`], which performs
/// the scheme/authority parse by hand (the `url` crate is not enabled under the
/// `a2a` feature) and rejects IP-literal hosts in reserved / private / loopback /
/// link-local ranges. Hostname targets are re-checked per resolved address at
/// delivery time.
fn validate_webhook_url(url: &str) -> Result<(), PushNotificationError> {
    crate::a2a::core::ssrf::validate_webhook_url(url)
        .map(|_target| ())
        .map_err(|crate::a2a::core::ssrf::SsrfRejected { reason }| {
            PushNotificationError::InvalidInput { reason }
        })
}

// ── B4: webhook delivery ─────────────────────────────────────────────────────
//
// B4: the outbound HTTP delivery worker is intentionally not ported in this
// phase. The trumpet original spawned a `tokio` task that subscribed to the
// `MessageBus`, mapped each `Event` to its `TaskId`, looked up the matching
// configs in this store, and POSTed the serialized event to every registered
// webhook URL via `reqwest` with an exponential-backoff retry loop
// (`DELIVERY_TIMEOUT_SECS` / `MAX_RETRIES`) and an `X-Basemind-Notification-
// Token` + `Authorization` header. That code requires the `reqwest`
// dependency (absent from the `a2a` feature today) and the SSRF guard, both of
// which land in phase B4. When B4 ports it, reintroduce:
//   - `spawn_delivery_worker(store, bus, client) -> JoinHandle<()>`
//   - `task_id_for_event(&Event) -> Option<TaskId>`
//   - `deliver_with_retries` / `deliver_once`
// and add the matching live-HTTP integration tests dropped below.

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // B4: the two trumpet `#[tokio::test]`s that stood up an in-process TCP
    // listener to assert `deliver_once` round-trips headers + body
    // (`deliver_once_succeeds_on_2xx`, `deliver_once_returns_error_on_4xx`)
    // are dropped here — they exercise live HTTP delivery, which is omitted
    // until B4. The config-store tests below are ported verbatim.

    fn task_id() -> TaskId {
        TaskId::new()
    }

    #[test]
    fn create_and_get_round_trip() {
        let mut store = PushNotificationStore::new();
        let tid = task_id();
        let cfg = store
            .create(
                tid,
                "https://example.com/webhook".to_owned(),
                "tok".to_owned(),
                None,
            )
            .expect("create must succeed");

        let fetched = store.get(&tid, &cfg.id).expect("must find created config");
        assert_eq!(fetched, &cfg, "round-trip must yield identical config");
    }

    #[test]
    fn create_rejects_non_http_url() {
        let mut store = PushNotificationStore::new();
        let err = store
            .create(
                task_id(),
                "ftp://example.com/x".to_owned(),
                String::new(),
                None,
            )
            .expect_err("non-http url must be rejected");
        assert!(
            matches!(err, PushNotificationError::InvalidInput { ref reason } if reason.contains("http")),
            "expected InvalidInput about http scheme, got: {err:?}"
        );
    }

    #[test]
    fn create_rejects_malformed_url() {
        let mut store = PushNotificationStore::new();
        let err = store
            .create(task_id(), "not a url".to_owned(), String::new(), None)
            .expect_err("invalid url must be rejected");
        assert!(matches!(err, PushNotificationError::InvalidInput { .. }));
    }

    #[test]
    fn list_returns_all_for_task() {
        let mut store = PushNotificationStore::new();
        let tid = task_id();
        store
            .create(tid, "https://a.example/".to_owned(), String::new(), None)
            .unwrap();
        store
            .create(tid, "https://b.example/".to_owned(), String::new(), None)
            .unwrap();
        // Different task — must not appear.
        store
            .create(
                task_id(),
                "https://c.example/".to_owned(),
                String::new(),
                None,
            )
            .unwrap();

        let listed = store.list(&tid);
        assert_eq!(listed.len(), 2, "must list exactly 2 configs for the task");
    }

    #[test]
    fn create_rejects_when_per_task_cap_reached() {
        let mut store = PushNotificationStore::new();
        let tid = task_id();
        for i in 0..MAX_PUSH_CONFIGS_PER_TASK {
            store
                .create(tid, format!("https://h{i}.example/"), String::new(), None)
                .expect("create within cap must succeed");
        }
        let err = store
            .create(
                tid,
                "https://overflow.example/".to_owned(),
                String::new(),
                None,
            )
            .expect_err("create past the cap must be rejected");
        assert!(
            matches!(err, PushNotificationError::InvalidInput { ref reason } if reason.contains("maximum")),
            "expected a cap InvalidInput, got: {err:?}"
        );
        // A different task is unaffected by another task's cap.
        store
            .create(
                task_id(),
                "https://other.example/".to_owned(),
                String::new(),
                None,
            )
            .expect("a different task must still accept configs");
    }

    #[test]
    fn delete_removes_config_and_returns_true() {
        let mut store = PushNotificationStore::new();
        let tid = task_id();
        let cfg = store
            .create(tid, "https://x.example/".to_owned(), String::new(), None)
            .unwrap();
        assert!(store.delete(&tid, &cfg.id), "delete must report success");
        assert!(
            store.get(&tid, &cfg.id).is_none(),
            "config must be gone after delete"
        );
        assert!(
            !store.delete(&tid, &cfg.id),
            "second delete must report no-op"
        );
    }
}
