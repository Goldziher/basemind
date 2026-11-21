//! `basemind upgrade --plan`: what the first open of each view after a release-minor bump rebuilds,
//! before anything is rebuilt.
//!
//! Every verdict comes from the code that acts on it, so the plan and the upgrade cannot disagree:
//! a view's from `store::view_on_open`, the decision the writer open of [`crate::store::Store`]
//! takes; the blob namespaces' from `store_gc_namespaces::superseded`, the predicate of the sweep
//! that reaps them; the disk a group's rebuild needs from
//! [`SpaceEstimate::required_bytes`], the estimate the scan's free-space check refuses on, here over
//! the existing index alone: the plan reads no source. The
//! group index has no verdict of its own that can be read without opening it, and opening a Fjall
//! database writes; its schema and the file maps' both follow [`crate::version::RELEASE_MINOR`],
//! so it is replaced exactly when one of the group's file maps is of an older schema.
//!
//! The plan reads and never writes: no lock is taken, no database opened, no marker refreshed.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::data_root::DataRoot;
use crate::disk_space::{SpaceEstimate, available_bytes, dir_bytes};
use crate::store::{INDEX_FILE, VIEWS_DIR, ViewOnOpen, read_workspace_marker, sets_aside, view_on_open};
use crate::store_gc::{GcError, read_dir};
use crate::store_layout::{GROUPS_DIR, VIEW_WORKING, group_of_workspace};
use crate::sysres::human_bytes;

/// Code-map scan throughput the estimate assumes, in files per second: the slowest scan the README
/// reports (the TypeScript compiler, 81 324 files in about 18 s), rounded down. Document extraction
/// and embedding are not in it.
const CODE_MAP_FILES_PER_SEC: u64 = 4_000;

const SECS_PER_MIN: u64 = 60;

/// What the first writer open of a view does with its file map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewAction {
    /// The map is current and kept.
    Keep,
    /// The map is current and kept, but an earlier rebuild stopped after emptying it: a full scan
    /// still owes the view's files.
    Pending,
    /// The view has no file map yet.
    Absent,
    /// The map is removed and the view rebuilt by a full scan.
    Rebuild,
    /// The map, whose version cannot be read, is renamed aside and the view rebuilt.
    SetAside,
    /// The open fails and leaves the view as it is: the map is a newer basemind's, or unreadable.
    Refuse,
}

impl ViewAction {
    /// Whether the view is rebuilt from a full scan.
    pub fn rebuilds(self) -> bool {
        matches!(self, ViewAction::Pending | ViewAction::Rebuild | ViewAction::SetAside)
    }

    fn label(self) -> &'static str {
        match self {
            ViewAction::Keep => "kept",
            ViewAction::Pending => "kept, rebuild pending",
            ViewAction::Absent => "no file map",
            ViewAction::Rebuild => "rebuilt",
            ViewAction::SetAside => "set aside and rebuilt",
            ViewAction::Refuse => "refused, left in place",
        }
    }
}

/// The plan for one view of a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ViewPlan {
    /// The view name (`working`, `staged`, `rev-<sha7>`).
    pub view: String,
    /// What its first writer open does.
    pub action: ViewAction,
    /// Why, when the map is not kept.
    pub reason: Option<String>,
    /// The schema the map was written under, when that is older than this binary's.
    pub older_schema: Option<u16>,
    /// Bytes the open removes: the file map, and the view's per-view index database of a release up
    /// to 0.33, which the writer open deletes.
    pub bytes_freed: u64,
}

/// The plan for one workspace cache directory (`cache/workspaces/<key>/`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspacePlan {
    /// The cache directory's name.
    pub key: String,
    /// The worktree root its marker names; `None` without a readable marker.
    pub root: Option<PathBuf>,
    /// Whether that root still exists. A cache whose root is gone is the orphan sweep's to remove.
    pub root_exists: bool,
    /// Every view, by name.
    pub views: Vec<ViewPlan>,
    /// Bytes its views' rebuilds free.
    pub bytes_freed: u64,
    /// Code files the last scan of the working view indexed, when that view is rebuilt.
    pub files: Option<usize>,
    /// Seconds the working view's code map takes to rebuild, at 4 000 files a second (the slowest
    /// scan the README reports, rounded down); document extraction and embedding come on top.
    pub scan_estimate_secs: Option<u64>,
}

impl WorkspacePlan {
    /// Whether any view of the workspace is rebuilt.
    pub fn rebuilds(&self) -> bool {
        self.views.iter().any(|view| view.action.rebuilds())
    }
}

/// The plan for one repository group: its index and its workspaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupPlan {
    /// The group key; `None` for workspaces whose group is unknown.
    pub group: Option<String>,
    /// Bytes of the group index (`cache/groups/<group>/index.fjall`).
    pub index_bytes: u64,
    /// Whether the group index is replaced. `Some(true)` when every file map of the group is of an
    /// older schema, so no worktree has opened it since the bump; `Some(false)` when none is; `None`
    /// when some worktrees are upgraded and others are not, which read-only evidence cannot settle:
    /// the group open keeps a current index.
    pub index_replaced: Option<bool>,
    /// Bytes the rebuild frees: the replaced index and the removed file maps and per-view indexes.
    pub bytes_freed: u64,
    /// Free bytes the rebuild's free-space check asks for, when any view of the group is rebuilt.
    pub bytes_needed: u64,
    /// The group's workspaces, by root.
    pub workspaces: Vec<WorkspacePlan>,
}

/// One blob namespace the sweep reaps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlobNamespace {
    /// The directory name (`v<schema>`).
    pub name: String,
    /// Its bytes.
    pub bytes: u64,
}

/// The machine-global blob store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlobPlan {
    /// Namespaces of older schemas, by name.
    pub superseded: Vec<BlobNamespace>,
    /// Bytes of the blob files of builds from before the namespaces.
    pub unnamespaced_bytes: u64,
    /// Bytes the sweep frees once they are past the blob grace period.
    pub bytes_freed: u64,
    /// Bytes the rebuild writes into the current namespace: the largest superseded blob set, when a
    /// view of an older schema is rebuilt.
    pub bytes_needed: u64,
}

/// The whole plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpgradePlan {
    /// The data home planned over.
    pub data_home: PathBuf,
    /// The schema this binary reads and writes.
    pub schema: u16,
    /// Every repository group, the unknown group last.
    pub groups: Vec<GroupPlan>,
    /// The blob store.
    pub blobs: BlobPlan,
    /// Bytes freed in all.
    pub bytes_freed: u64,
    /// Bytes needed in all: the free-space estimate over every rebuilt group's index, plus the blobs.
    pub bytes_needed: u64,
    /// Free bytes on the data home's volume; `None` when the platform does not say.
    pub free_bytes: Option<u64>,
}

/// Plan the upgrade of every workspace under `data`. `headroom_bytes` is the fixed headroom the
/// free-space check adds ([`crate::config::engine::EngineConfig::min_free_bytes`]).
pub fn plan(data: &DataRoot, headroom_bytes: u64) -> Result<UpgradePlan, GcError> {
    let schema = crate::extract::SCHEMA_VER;
    let mut by_group: BTreeMap<(bool, Option<String>), Vec<WorkspacePlan>> = BTreeMap::new();
    for dir in subdirectories(&data.workspaces_dir())? {
        let group = group_of_workspace(&dir);
        by_group
            .entry((group.is_none(), group))
            .or_default()
            .push(workspace_plan(&dir)?);
    }
    let groups: Vec<GroupPlan> = by_group
        .into_iter()
        .map(|((_, group), workspaces)| group_plan(data, group, workspaces, headroom_bytes))
        .collect();
    let any_older = groups
        .iter()
        .flat_map(|group| &group.workspaces)
        .flat_map(|workspace| &workspace.views)
        .any(|view| view.older_schema.is_some());
    let blobs = blob_plan(&data.global_blobs_root(), schema, any_older)?;
    let rebuilt_index_bytes: u64 = groups
        .iter()
        .filter(|group| group.bytes_needed > 0)
        .map(|group| group.index_bytes)
        .sum();
    let any_rebuild = groups.iter().any(|group| group.bytes_needed > 0);
    let index_needed = if any_rebuild {
        rebuild_required_bytes(rebuilt_index_bytes, headroom_bytes)
    } else {
        0
    };
    Ok(UpgradePlan {
        data_home: data.path().to_path_buf(),
        schema,
        bytes_freed: groups.iter().map(|group| group.bytes_freed).sum::<u64>() + blobs.bytes_freed,
        bytes_needed: index_needed + blobs.bytes_needed,
        groups,
        blobs,
        free_bytes: available_bytes(data.path()),
    })
}

/// The free-space check's estimate for a rebuilt group index of `index_bytes`, with `headroom` on top.
fn rebuild_required_bytes(index_bytes: u64, headroom: u64) -> u64 {
    SpaceEstimate {
        index_bytes,
        source_bytes: 0,
        vectors: false,
        headroom_bytes: headroom,
    }
    .required_bytes()
}

/// The plan for the workspace cache directory `basemind_dir`.
pub(crate) fn workspace_plan(basemind_dir: &Path) -> Result<WorkspacePlan, GcError> {
    let views: Vec<ViewPlan> = subdirectories(&basemind_dir.join(VIEWS_DIR))?
        .iter()
        .map(PathBuf::as_path)
        .map(view_plan)
        .collect();
    let root = read_workspace_marker(basemind_dir).map(|marker| marker.root);
    let working_rebuilds = views
        .iter()
        .any(|view| view.view == VIEW_WORKING && view.action.rebuilds());
    let files = if working_rebuilds {
        crate::store::read_status_sidecar(basemind_dir).map(|sidecar| sidecar.file_count)
    } else {
        None
    };
    Ok(WorkspacePlan {
        key: file_name(basemind_dir),
        root_exists: root.as_ref().is_some_and(|root| root.exists()),
        root,
        bytes_freed: views.iter().map(|view| view.bytes_freed).sum(),
        scan_estimate_secs: files.map(|files| (files as u64).div_ceil(CODE_MAP_FILES_PER_SEC)),
        files,
        views,
    })
}

fn view_plan(view_dir: &Path) -> ViewPlan {
    let map_bytes = || std::fs::metadata(view_dir.join(INDEX_FILE)).map_or(0, |meta| meta.len());
    let (action, reason, older_schema, bytes_freed) = match view_on_open(view_dir) {
        ViewOnOpen::Keep(index) if index.rebuild_pending => (
            ViewAction::Pending,
            Some("an earlier rebuild did not finish".to_string()),
            None,
            0,
        ),
        ViewOnOpen::Keep(_) => (ViewAction::Keep, None, None, 0),
        ViewOnOpen::Absent => (ViewAction::Absent, None, None, 0),
        ViewOnOpen::OlderSchema { found } => (
            ViewAction::Rebuild,
            Some(format!("written under schema {found}")),
            Some(found),
            map_bytes(),
        ),
        ViewOnOpen::Discard(why) if sets_aside(&why) => (ViewAction::SetAside, Some(why.to_string()), None, 0),
        ViewOnOpen::Discard(why) => (ViewAction::Rebuild, Some(why.to_string()), None, map_bytes()),
        ViewOnOpen::Refuse(why) => (ViewAction::Refuse, Some(why.to_string()), None, 0),
    };
    // The writer open deletes a per-view index database of a release up to 0.33 in any view it
    // opens; a refused open touches nothing.
    let per_view_index = if action == ViewAction::Refuse {
        0
    } else {
        dir_bytes(&view_dir.join(crate::index::INDEX_DIR))
    };
    ViewPlan {
        view: file_name(view_dir),
        action,
        reason,
        older_schema,
        bytes_freed: bytes_freed + per_view_index,
    }
}

fn group_plan(data: &DataRoot, group: Option<String>, mut workspaces: Vec<WorkspacePlan>, headroom: u64) -> GroupPlan {
    workspaces.sort_by(|a, b| (&a.root, &a.key).cmp(&(&b.root, &b.key)));
    let index_bytes = group.as_deref().map_or(0, |group| {
        dir_bytes(
            &data
                .cache_dir()
                .join(GROUPS_DIR)
                .join(group)
                .join(crate::index::INDEX_DIR),
        )
    });
    let index_replaced = group.as_ref().map_or(Some(false), |_| index_replacement(&workspaces));
    let bytes_needed = if workspaces.iter().any(WorkspacePlan::rebuilds) {
        rebuild_required_bytes(index_bytes, headroom)
    } else {
        0
    };
    let maps_freed: u64 = workspaces.iter().map(|workspace| workspace.bytes_freed).sum();
    GroupPlan {
        bytes_freed: maps_freed + if index_replaced == Some(true) { index_bytes } else { 0 },
        group,
        index_bytes,
        index_replaced,
        bytes_needed,
        workspaces,
    }
}

/// Whether the group index of `workspaces` is replaced by the upgrade, from their file maps alone.
/// Every map older: no worktree has opened the group since the bump, so its index is the old one.
/// None older: nothing replaces it. Both kinds: a worktree already opened the group, and the group
/// open keeps a current index, so the old maps beside it do not say.
fn index_replacement(workspaces: &[WorkspacePlan]) -> Option<bool> {
    let mut older = false;
    let mut current = false;
    for view in workspaces.iter().flat_map(|workspace| &workspace.views) {
        match view.action {
            ViewAction::Keep | ViewAction::Pending => current = true,
            _ if view.older_schema.is_some() => older = true,
            _ => {}
        }
    }
    match (older, current) {
        (true, false) => Some(true),
        (false, _) => Some(false),
        (true, true) => None,
    }
}

fn blob_plan(blobs_root: &Path, current: u16, rebuilds_older: bool) -> Result<BlobPlan, GcError> {
    let mut superseded = Vec::new();
    let mut unnamespaced_bytes = 0;
    if blobs_root.exists() {
        for entry in read_dir(blobs_root)? {
            let path = entry
                .map_err(|source| GcError::Io {
                    path: blobs_root.to_path_buf(),
                    source,
                })?
                .path();
            let is_dir = path.is_dir();
            // A name that is not UTF-8 is not the sweep's to take, so not the plan's to count. ~keep
            let Some(name) = path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .filter(|name| crate::store_gc_namespaces::superseded(name, is_dir, current))
                .map(str::to_string)
            else {
                continue;
            };
            if is_dir {
                superseded.push(BlobNamespace {
                    bytes: dir_bytes(&path),
                    name,
                });
            } else {
                unnamespaced_bytes += std::fs::metadata(&path).map_or(0, |meta| meta.len());
            }
        }
    }
    superseded.sort_by(|a, b| a.name.cmp(&b.name));
    let largest = superseded
        .iter()
        .map(|namespace| namespace.bytes)
        .chain([unnamespaced_bytes])
        .max()
        .unwrap_or(0);
    Ok(BlobPlan {
        bytes_freed: superseded.iter().map(|namespace| namespace.bytes).sum::<u64>() + unnamespaced_bytes,
        bytes_needed: if rebuilds_older { largest } else { 0 },
        superseded,
        unnamespaced_bytes,
    })
}

/// The subdirectories of `dir`, sorted; none when it is absent.
fn subdirectories(dir: &Path) -> Result<Vec<PathBuf>, GcError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut dirs = Vec::new();
    for entry in read_dir(dir)? {
        let path = entry
            .map_err(|source| GcError::Io {
                path: dir.to_path_buf(),
                source,
            })?
            .path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn format_secs(secs: u64) -> String {
    if secs < SECS_PER_MIN {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / SECS_PER_MIN, secs % SECS_PER_MIN)
    }
}

/// Render `plan` for a terminal: one block per group, one line per workspace and view, the command
/// that rebuilds one workspace on its own, then the blob store and the totals.
pub fn render_human(plan: &UpgradePlan, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(
        out,
        "upgrade plan for schema {} (data home {})",
        plan.schema,
        plan.data_home.display()
    )?;
    for group in &plan.groups {
        render_group(group, out)?;
    }
    render_blobs(&plan.blobs, plan.schema, out)?;
    let free = plan.free_bytes.map_or_else(|| "unknown".to_string(), human_bytes);
    writeln!(
        out,
        "total: frees {}, needs {}, {free} free",
        human_bytes(plan.bytes_freed),
        human_bytes(plan.bytes_needed)
    )
}

fn render_group(group: &GroupPlan, out: &mut impl Write) -> std::io::Result<()> {
    let state = match group.index_replaced {
        Some(true) => "replaced",
        Some(false) => "kept",
        None => "replaced or kept (some of its worktrees are already upgraded)",
    };
    match &group.group {
        Some(key) => writeln!(
            out,
            "group {key}: index {} {state}, frees {}, needs {}",
            human_bytes(group.index_bytes),
            human_bytes(group.bytes_freed),
            human_bytes(group.bytes_needed)
        )?,
        None => writeln!(
            out,
            "no known group: frees {}, needs {}",
            human_bytes(group.bytes_freed),
            human_bytes(group.bytes_needed)
        )?,
    }
    for workspace in &group.workspaces {
        render_workspace(workspace, out)?;
    }
    Ok(())
}

fn render_workspace(workspace: &WorkspacePlan, out: &mut impl Write) -> std::io::Result<()> {
    let name = workspace.root.as_ref().map_or_else(
        || format!("{} (no marker)", workspace.key),
        |root| root.display().to_string(),
    );
    let scan = match (workspace.files, workspace.scan_estimate_secs) {
        (Some(files), Some(secs)) => format!(", {files} files, code map scan about {}", format_secs(secs)),
        _ => String::new(),
    };
    writeln!(
        out,
        "  workspace {name}: frees {}{scan}",
        human_bytes(workspace.bytes_freed)
    )?;
    for view in &workspace.views {
        let reason = view
            .reason
            .as_ref()
            .map_or_else(String::new, |reason| format!(" ({reason})"));
        let freed = match view.bytes_freed {
            0 => String::new(),
            bytes => format!(", frees {}", human_bytes(bytes)),
        };
        writeln!(out, "    view {}: {}{reason}{freed}", view.view, view.action.label())?;
    }
    if !workspace.rebuilds() {
        return Ok(());
    }
    match (&workspace.root, workspace.root_exists) {
        (Some(root), true) => writeln!(
            out,
            "    rebuild it alone: basemind scan --root {}",
            shell_quote(&root.display().to_string())
        ),
        (Some(_), false) => writeln!(out, "    its root is gone: the orphan sweep removes this cache"),
        (None, _) => writeln!(
            out,
            "    its root is not recorded: scan its worktree to rebuild it; the orphan sweep keeps a cache it cannot place"
        ),
    }
}

/// `text` as one POSIX shell word: unchanged when it holds only characters the shell leaves alone,
/// else single-quoted with each embedded quote closed, escaped and reopened.
fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '=' | ':' | '@' | ','));
    if plain {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

fn render_blobs(blobs: &BlobPlan, schema: u16, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(
        out,
        "blobs: frees {} once past the blob grace period, needs {} in v{schema}",
        human_bytes(blobs.bytes_freed),
        human_bytes(blobs.bytes_needed)
    )?;
    for namespace in &blobs.superseded {
        writeln!(out, "  {}: {}", namespace.name, human_bytes(namespace.bytes))?;
    }
    if blobs.unnamespaced_bytes > 0 {
        writeln!(out, "  un-namespaced: {}", human_bytes(blobs.unnamespaced_bytes))?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "upgrade_plan_tests.rs"]
mod tests;
