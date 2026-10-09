//! `basemind doctor` — a one-shot health check of this installation and workspace.
//!
//! Filesystem and process probes only: it never opens the index, never spawns the daemon and never
//! touches the network, so it is safe to run on a wedged machine. Exit code is 1 when any check
//! fails; warnings do not fail it.

use std::path::Path;

use anyhow::Result;
use serde_json::json;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "FAIL",
        }
    }
}

struct Check {
    name: &'static str,
    level: Level,
    detail: String,
}

impl Check {
    fn new(name: &'static str, level: Level, detail: impl Into<String>) -> Self {
        Self {
            name,
            level,
            detail: detail.into(),
        }
    }
}

pub(crate) fn cmd_doctor(root: &Path, json_out: bool) -> Result<()> {
    let mut checks = vec![Check::new("version", Level::Ok, env!("CARGO_PKG_VERSION"))];
    checks.push(check_root(root));
    checks.push(check_config(root));
    checks.push(check_index(root));
    checks.push(check_grammars());
    checks.push(check_hook(root));
    checks.push(check_daemon());

    let failed = checks.iter().any(|c| c.level == Level::Fail);
    if json_out {
        let rows: Vec<_> = checks
            .iter()
            .map(|c| json!({ "check": c.name, "status": c.level.label(), "detail": c.detail }))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": !failed, "checks": rows }))?
        );
    } else {
        for c in &checks {
            println!("{:<5} {:<9} {}", c.level.label(), c.name, c.detail);
        }
    }
    if failed {
        anyhow::bail!("doctor found problems; see the FAIL rows above");
    }
    Ok(())
}

fn check_root(root: &Path) -> Check {
    match basemind::config::root_guard::workspace_root_verdict(root) {
        Ok(resolved) => Check::new("root", Level::Ok, resolved.display().to_string()),
        Err(refusal) => Check::new(
            "root",
            Level::Fail,
            basemind::config::root_guard::refusal_message(root, refusal),
        ),
    }
}

fn check_config(root: &Path) -> Check {
    match basemind::config::load_with_overrides(root, None, None) {
        Ok(_) => Check::new("config", Level::Ok, "basemind.toml loaded"),
        Err(basemind::config::ConfigError::NotFound(_)) => Check::new(
            "config",
            Level::Ok,
            "no basemind.toml; defaults apply (`basemind init` writes one)",
        ),
        Err(e) => Check::new("config", Level::Fail, e.to_string()),
    }
}

fn check_index(root: &Path) -> Check {
    let cache = basemind::store::workspace_cache_dir(root);
    let index = cache
        .join(basemind::store::VIEWS_DIR)
        .join(basemind::store::VIEW_WORKING)
        .join(basemind::store::INDEX_FILE);
    if !index.exists() {
        return Check::new("index", Level::Warn, "no index yet; run `basemind scan`");
    }
    let writer = match basemind::store::probe_writer_lock(&cache) {
        basemind::store::WriterProbe::Free => "no writer running".to_string(),
        basemind::store::WriterProbe::Held { holder: Some(meta) } => {
            format!("writer: `{}` (pid {})", meta.command, meta.pid)
        }
        basemind::store::WriterProbe::Held { holder: None } => "a writer holds the lock".to_string(),
    };
    Check::new("index", Level::Ok, format!("{}; {writer}", index.display()))
}

fn check_grammars() -> Check {
    let langs = basemind::lang::downloaded_languages();
    if langs.is_empty() {
        Check::new(
            "grammars",
            Level::Warn,
            "none downloaded yet; `basemind lang install` fetches them",
        )
    } else {
        Check::new("grammars", Level::Ok, format!("{} downloaded", langs.len()))
    }
}

fn check_hook(root: &Path) -> Check {
    match basemind::cli::hook::resolve_hooks_dir(root) {
        Ok(dir) if basemind::cli::hook::is_installed(&dir) => {
            Check::new("hook", Level::Ok, format!("pre-commit installed in {}", dir.display()))
        }
        Ok(_) => Check::new(
            "hook",
            Level::Ok,
            "pre-commit hook not installed (optional: `basemind hook install`)",
        ),
        Err(_) => Check::new("hook", Level::Ok, "not a git repository"),
    }
}

#[cfg(all(feature = "comms", any(unix, windows)))]
fn check_daemon() -> Check {
    use basemind::comms::singleton;
    match singleton::resolve_paths() {
        Ok(paths) if singleton::probe_alive(&paths.socket_path) => Check::new(
            "daemon",
            Level::Ok,
            format!("running ({})", paths.socket_path.display()),
        ),
        Ok(_) => Check::new(
            "daemon",
            Level::Ok,
            "not running (starts on demand via `basemind serve`)",
        ),
        Err(e) => Check::new("daemon", Level::Warn, format!("cannot resolve the comms dir: {e}")),
    }
}

#[cfg(not(all(feature = "comms", any(unix, windows))))]
fn check_daemon() -> Check {
    Check::new(
        "daemon",
        Level::Warn,
        "built without the `comms` feature; `serve` is unavailable",
    )
}
