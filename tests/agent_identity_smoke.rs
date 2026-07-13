//! Agent-identity resolution: the regression suite for the IDENTITY-COLLISION bug where every
//! CLI invocation on a machine resolved to the same hardcoded agent id, so two agents in two
//! different repos shared one identity and one inbox.
//!
//! Every test here is hermetic and parallel-safe: [`IdentityPaths`] is constructed from per-test
//! temp dirs and the env/config tiers are passed as PARAMETERS, so no test reads or mutates
//! process env (`BASEMIND_AGENT_ID` / `BASEMIND_DATA_HOME`) and no test can touch the
//! machine-global cache.

use std::path::{Path, PathBuf};

use basemind::comms::identity::{self, IdentityPaths, IdentityRequest, IdentitySource};
use tempfile::TempDir;

/// A hermetic stand-in for the machine-global cache: one `BASEMIND_DATA_HOME` shared by every
/// workspace in a test, with per-workspace dirs keyed exactly like the real cache.
struct FakeMachine {
    _dir: TempDir,
    data_home: PathBuf,
}

impl FakeMachine {
    fn new() -> Self {
        let dir = TempDir::new().expect("temp data home");
        let data_home = dir.path().to_path_buf();
        Self { _dir: dir, data_home }
    }

    /// Mirror [`basemind::store::workspace_cache_dir`] / the claims dir under the fake data home,
    /// keyed by the same pure `workspace_key` hash the real cache uses.
    fn paths_for(&self, root: &Path) -> IdentityPaths {
        IdentityPaths {
            workspace_cache_dir: self
                .data_home
                .join(basemind::store::CACHE_DIR)
                .join(basemind::store::WORKSPACES_DIR)
                .join(basemind::store::workspace_key(root)),
            claims_dir: self
                .data_home
                .join(basemind::store::CACHE_DIR)
                .join(identity::CLAIMS_DIR),
        }
    }
}

/// A workspace root on disk (identity keys off the canonicalized path, so it must exist).
fn workspace() -> TempDir {
    TempDir::new().expect("temp workspace root")
}

fn request<'a>(machine: &FakeMachine, root: &'a Path) -> IdentityRequest<'a> {
    IdentityRequest {
        root,
        paths: machine.paths_for(root),
        env_agent_id: None,
        config_agent_id: None,
    }
}

/// THE headline regression: `basemind comms ...` run in ~/basemind and in ~/armis must not be the
/// same agent. Before the fix both returned the hardcoded `"basemind-cli"`, so the two agents
/// shared one inbox and each saw the other's messages as its own (0 unread, silently).
#[test]
fn two_cli_invocations_in_different_workspaces_get_distinct_agent_ids() {
    let machine = FakeMachine::new();
    let (a, b) = (workspace(), workspace());

    let id_a = identity::resolve(&request(&machine, a.path())).into_id();
    let id_b = identity::resolve(&request(&machine, b.path())).into_id();

    assert_ne!(
        id_a.as_str(),
        id_b.as_str(),
        "two workspaces must never resolve to one agent id (got {id_a} twice)"
    );
}

/// The behavior the old doc comment CLAIMED but never delivered: a CLI call in a workspace adopts
/// the identity the `serve` session in THAT workspace persisted.
#[test]
fn a_cli_invocation_shares_the_persisted_id_of_the_serve_session_in_the_same_workspace() {
    let machine = FakeMachine::new();
    let root = workspace();

    // `serve` boots first and persists its generated id in the workspace cache dir.
    let served = identity::resolve(&request(&machine, root.path()));
    assert_eq!(served.source(), IdentitySource::Workspace);

    // A later CLI invocation in the same workspace must adopt it, not mint a new one.
    let from_cli = identity::resolve(&request(&machine, root.path()));

    assert_eq!(served.id().as_str(), from_cli.id().as_str());
    let persisted = std::fs::read_to_string(
        machine
            .paths_for(root.path())
            .workspace_cache_dir
            .join(identity::AGENT_ID_FILE),
    )
    .expect("serve persists the agent id in the workspace cache dir");
    assert_eq!(persisted.trim(), served.id().as_str());
}

/// No root, anywhere, may yield a machine-wide shared constant.
#[test]
fn the_resolver_never_returns_a_shared_constant() {
    let machine = FakeMachine::new();
    let roots: Vec<TempDir> = (0..4).map(|_| workspace()).collect();

    let ids: Vec<String> = roots
        .iter()
        .map(|r| identity::resolve(&request(&machine, r.path())).into_id().into_string())
        .collect();

    for id in &ids {
        assert_ne!(id, "basemind-cli", "the shared CLI constant must be gone");
        assert_ne!(id, "anon", "the shared fallback constant must be gone");
    }
    let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "every workspace gets its own id: {ids:?}");
}

/// Tier 1 still wins: an explicit `BASEMIND_AGENT_ID` overrides the persisted workspace id.
#[test]
fn an_explicit_env_agent_id_wins_over_every_other_tier() {
    let machine = FakeMachine::new();
    let root = workspace();

    // Persist a workspace id first, so the env tier has something to beat.
    let persisted = identity::resolve(&request(&machine, root.path())).into_id();

    let resolved = identity::resolve(&IdentityRequest {
        env_agent_id: Some("orchestrator-1".to_string()),
        config_agent_id: Some("from-config".to_string()),
        ..request(&machine, root.path())
    });

    assert_eq!(resolved.id().as_str(), "orchestrator-1");
    assert_eq!(resolved.source(), IdentitySource::Env);
    assert_ne!(resolved.id().as_str(), persisted.as_str());
}

/// Tier 2: `config.comms.agent_id` beats the persisted workspace id. The CLI never honored this
/// tier at all before the fix.
#[test]
fn a_config_agent_id_wins_over_the_persisted_workspace_id() {
    let machine = FakeMachine::new();
    let root = workspace();

    let resolved = identity::resolve(&IdentityRequest {
        config_agent_id: Some("claude-code".to_string()),
        ..request(&machine, root.path())
    });

    assert_eq!(resolved.id().as_str(), "claude-code");
    assert_eq!(resolved.source(), IdentitySource::Config);
}

/// An invalid explicit id falls THROUGH to the next tier rather than failing the process.
#[test]
fn an_invalid_explicit_agent_id_falls_through_to_the_workspace_tier() {
    let machine = FakeMachine::new();
    let root = workspace();

    let resolved = identity::resolve(&IdentityRequest {
        env_agent_id: Some("has spaces and /slashes".to_string()),
        ..request(&machine, root.path())
    });

    assert_eq!(resolved.source(), IdentitySource::Workspace);
}

/// GUARDRAIL: the foot-gun that produced the original bug is pinning ONE explicit id across TWO
/// workspaces. That is still allowed (a legitimate reconnect must never be blocked), but it may no
/// longer be SILENT — the second claimant reports the collision and names both roots.
#[test]
fn reusing_one_explicit_agent_id_across_two_workspaces_surfaces_a_collision() {
    let machine = FakeMachine::new();
    let (a, b) = (workspace(), workspace());

    let first = identity::resolve(&IdentityRequest {
        env_agent_id: Some("basemind-cli".to_string()),
        ..request(&machine, a.path())
    });
    assert!(first.collision().is_none(), "the first claimant owns the id cleanly");

    let second = identity::resolve(&IdentityRequest {
        env_agent_id: Some("basemind-cli".to_string()),
        ..request(&machine, b.path())
    });

    let collision = second.collision().expect("a cross-workspace id reuse must be reported");
    assert_eq!(collision.agent_id, "basemind-cli");
    assert_eq!(
        collision.claimed_by_root.canonicalize().ok(),
        a.path().canonicalize().ok(),
        "the collision names the OTHER claimant's root"
    );
    let warning = second.collision_warning().expect("a collision renders a warning");
    assert!(warning.contains("basemind-cli"), "warning names the id: {warning}");
    assert!(
        warning.contains(&b.path().canonicalize().unwrap_or_default().display().to_string()),
        "warning names this claimant's root: {warning}"
    );

    // Not a hard failure: the id is still handed out, so a legitimate reconnect keeps working.
    assert_eq!(second.id().as_str(), "basemind-cli");
}

/// The guardrail must not cry wolf: repeated resolution in ONE workspace is a reconnect, not a
/// collision, whichever tier the id came from.
#[test]
fn repeated_resolution_in_one_workspace_never_reports_a_collision() {
    let machine = FakeMachine::new();
    let root = workspace();

    for _ in 0..3 {
        let generated = identity::resolve(&request(&machine, root.path()));
        assert!(
            generated.collision().is_none(),
            "same-workspace reconnect is not a collision"
        );

        let explicit = identity::resolve(&IdentityRequest {
            env_agent_id: Some("claude-code".to_string()),
            ..request(&machine, root.path())
        });
        assert!(
            explicit.collision().is_none(),
            "same-workspace reconnect is not a collision"
        );
    }
}

/// Binds the production wiring: identity state lives in the machine-global per-workspace cache dir
/// (`store::workspace_cache_dir`), NOT the legacy in-repo `<root>/.basemind/` that the old CLI
/// resolver read — the dead path that made every CLI call fall through to the shared constant.
#[test]
fn identity_paths_for_root_track_the_machine_global_workspace_cache_dir() {
    let root = workspace();
    let paths = IdentityPaths::for_root(root.path());

    assert_eq!(
        paths.workspace_cache_dir,
        basemind::store::workspace_cache_dir(root.path())
    );
    assert!(
        !paths.workspace_cache_dir.starts_with(root.path()),
        "identity must not live in the repo: {}",
        paths.workspace_cache_dir.display()
    );
}
