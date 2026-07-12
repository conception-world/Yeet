//! Reverse-bootstrap: takes a `DataModel` snapshot streamed by the plugin and
//! writes out a fresh Rojo-compatible project on disk.
//!
//! Fase 4 delivers the pipeline but not full Rojo interop on `*.meta.json` —
//! the daemon emits its own tagged property format that round-trips with the
//! daemon's own reader. Rojo can still consume the `.luau` files; property
//! metadata is preserved inside the project and not yet useful outside.
//!
//! ## Directory layout decisions
//!
//! - Every non-script instance becomes a directory. Even leaves get an
//!   `init.meta.json` so the class name round-trips on re-ingest.
//! - Scripts with no children become a single `.luau` file.
//! - Scripts with children become `Name/init.luau` + children as siblings.
//! - Services (top-level children of the `DataModel`) are mapped through
//!   `default.project.json`; their own directory sits at `src/<ServiceName>/`.
//! - Name collisions between siblings — including collisions that differ
//!   only by case, since NTFS/OneDrive are case-insensitive — are
//!   deterministically disambiguated by appending `_1`, `_2`, ... and
//!   recorded in `stats.warnings`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STD;
use serde::Serialize;
use serde_json::json;

use crate::project::{Project, TreeNode};
use crate::protocol::{
    ScriptKind, SerializedInstance, SerializedProperty, SyncbackMode, SyncbackStats,
    SyncbackTemplate,
};
use crate::tree::{self, Tree, TreeEntry};

/// Options captured on `SyncbackBegin`; kept alive for the full lifetime of
/// one syncback.
#[derive(Debug, Clone)]
pub struct SyncbackOptions {
    pub target_path: PathBuf,
    pub mode: SyncbackMode,
    pub include_non_script: bool,
    /// When true, instances arriving with a `binary` payload short-circuit the
    /// directory pipeline and get written as a single `.rbxm` file. When
    /// false, the field is ignored — useful so the user can re-yeet the same
    /// place at script-only fidelity without the plugin re-walking it.
    pub include_binary: bool,
    // `template` is echoed for logging/diagnostics; `SyncbackTemplate::Minimal`
    // is currently the only variant and the plugin already filters the walk.
    #[allow(dead_code)]
    pub template: SyncbackTemplate,
    pub project_name: Option<String>,
}

/// Accumulates chunks until `finalize` is called. Owned by
/// `SyncbackSessions`; created on `SyncbackBegin`, consumed on
/// `SyncbackFinalize`.
#[derive(Debug)]
pub struct SyncbackSession {
    pub request_id: String,
    pub opts: SyncbackOptions,
    /// Ingested instances keyed by `id`. Later chunks may reference parents
    /// already present from earlier chunks — that is expected.
    pub instances: HashMap<u64, SerializedInstance>,
    /// Every `seq` value we have received. Finalize requires this to be
    /// exactly `0..total_seq` with no gaps.
    pub received_seqs: BTreeSet<u32>,
    /// Accumulated non-fatal warnings surfaced by ingestion; merged into
    /// `SyncbackStats::warnings` at finalize time.
    pub warnings: Vec<String>,
}

impl SyncbackSession {
    #[must_use]
    pub fn new(request_id: String, opts: SyncbackOptions) -> Self {
        Self {
            request_id,
            opts,
            instances: HashMap::new(),
            received_seqs: BTreeSet::new(),
            warnings: Vec::new(),
        }
    }

    /// Adds one chunk. Duplicate `seq` is an error because it signals a buggy
    /// plugin; duplicate `id` inside the instances list is an error too.
    pub fn ingest_chunk(
        &mut self,
        seq: u32,
        instances: Vec<SerializedInstance>,
    ) -> Result<()> {
        if !self.received_seqs.insert(seq) {
            bail!("duplicate syncback chunk seq={seq}");
        }
        for inst in instances {
            if let Some(prev) = self.instances.insert(inst.id, inst) {
                bail!("duplicate instance id={} in syncback", prev.id);
            }
        }
        Ok(())
    }
}

/// Holds all live sessions keyed by `request_id`. Wrapped in a tokio mutex in
/// `main.rs`; the type itself is a thin newtype over `HashMap` for clarity.
#[derive(Debug, Default)]
pub struct SyncbackSessions {
    inner: HashMap<String, SyncbackSession>,
}

impl SyncbackSessions {
    pub fn insert(&mut self, session: SyncbackSession) -> Result<()> {
        if self.inner.contains_key(&session.request_id) {
            bail!("syncback request_id already in progress: {}", session.request_id);
        }
        self.inner.insert(session.request_id.clone(), session);
        Ok(())
    }

    pub fn get_mut(&mut self, request_id: &str) -> Option<&mut SyncbackSession> {
        self.inner.get_mut(request_id)
    }

    pub fn remove(&mut self, request_id: &str) -> Option<SyncbackSession> {
        self.inner.remove(request_id)
    }
}

// ─── Materialization ──────────────────────────────────────────────────────

/// Validates `total_seq` matches what was received, walks the accumulated
/// tree, writes files, generates `default.project.json`, seeds
/// `.yeet/base-tree.msgpack`, and returns the summary stats.
pub fn materialize(session: SyncbackSession, total_seq: u32) -> Result<SyncbackStats> {
    let expected: BTreeSet<u32> = (0..total_seq).collect();
    if session.received_seqs != expected {
        let missing: Vec<u32> = expected.difference(&session.received_seqs).copied().collect();
        let extra: Vec<u32> = session.received_seqs.difference(&expected).copied().collect();
        bail!(
            "syncback chunk mismatch (expected 0..{total_seq}); missing={missing:?} extra={extra:?}"
        );
    }

    prepare_target(&session.opts).with_context(|| {
        format!("prepare target {}", session.opts.target_path.display())
    })?;

    // Snapshot what the previous materialization tracked so obsolete files can
    // be reconciled after the fresh walk (M2 / setup-2). Only overwrite-merge
    // reconciles: a `NewProject` starts clean and a first-ever overwrite has no
    // stored base tree yet, so both collapse to an empty (no-op) baseline.
    let prev_base = match session.opts.mode {
        SyncbackMode::MergeExisting { overwrite: true } => {
            tree::load_base_tree(&session.opts.target_path)
                .context("load previous base tree for overwrite reconcile")?
                .unwrap_or_default()
        }
        _ => Tree::new(),
    };

    let forest = build_forest(&session.instances)?;
    let root = forest.root;
    let mut ctx = WriteContext {
        opts: &session.opts,
        instances: &session.instances,
        children: &forest.children,
        stats: SyncbackStats::default(),
        tree_base: Tree::new(),
        written_paths: BTreeSet::new(),
    };
    ctx.stats.warnings = session.warnings;

    let root_inst = ctx
        .instances
        .get(&root)
        .ok_or_else(|| anyhow!("forest root {root} not in instance map"))?;
    if root_inst.class_name != "DataModel" {
        bail!(
            "syncback root must be DataModel, got {}",
            root_inst.class_name
        );
    }

    let mut service_entries: BTreeMap<String, (String, PathBuf)> = BTreeMap::new();
    let root_children = forest.children.get(&root).cloned().unwrap_or_default();
    let root_ordered = order_children(&root_children, ctx.instances);

    let src_dir = session.opts.target_path.join("src");
    std::fs::create_dir_all(extended_path(&src_dir))
        .with_context(|| format!("mkdir {}", src_dir.display()))?;

    for service_id in root_ordered {
        let service = &ctx.instances[&service_id];
        if !service_has_content(service_id, &ctx) {
            continue;
        }
        let sanitized = sanitize_name(&service.name);
        let service_dir = src_dir.join(&sanitized);
        std::fs::create_dir_all(extended_path(&service_dir))
            .with_context(|| format!("mkdir {}", service_dir.display()))?;
        if has_custom_props(service) {
            ctx.stats.warnings.push(format!(
                "service {} has custom properties/attributes/tags which are not exported (v1 limitation)",
                service.name
            ));
        }
        let service_rel = format!("src/{sanitized}");
        write_children(service_id, &service_dir, &service_rel, &mut ctx)?;
        ctx.stats.services_included.push(service.name.clone());
        service_entries.insert(
            service.name.clone(),
            (sanitized.clone(), PathBuf::from(format!("src/{sanitized}"))),
        );
    }

    let project = build_project_json(
        session.opts.project_name.as_deref(),
        &session.opts.target_path,
        &service_entries,
    );
    let project_path = session.opts.target_path.join("default.project.json");
    let project_bytes = serde_json::to_vec_pretty(&project).context("serialize project json")?;
    atomic_write(&project_path, &project_bytes)
        .with_context(|| format!("write {}", project_path.display()))?;
    ctx.stats.total_bytes += project_bytes.len() as u64;

    // Reconcile before persisting the new base tree: drop files the previous
    // materialization tracked that this run no longer produces, so a re-yeet
    // can't resurrect a Studio-side deletion or leave both shapes of a
    // reshaped script on disk (M2 / setup-2).
    reconcile_overwrite(
        &session.opts.target_path,
        &prev_base,
        &ctx.tree_base,
        &ctx.written_paths,
        &mut ctx.stats,
    )
    .context("reconcile obsolete files after overwrite merge")?;

    tree::save_base_tree(&session.opts.target_path, &ctx.tree_base)
        .context("persist initial base tree")?;

    Ok(ctx.stats)
}

/// Prepares the target directory according to the mode. Errors out if the
/// mode forbids the target's current state.
fn prepare_target(opts: &SyncbackOptions) -> Result<()> {
    let project_file = opts.target_path.join("default.project.json");
    match opts.mode {
        SyncbackMode::NewProject => {
            if project_file.exists() {
                bail!(
                    "{} already contains a default.project.json — use merge mode to overwrite",
                    opts.target_path.display()
                );
            }
            std::fs::create_dir_all(&opts.target_path)
                .with_context(|| format!("mkdir {}", opts.target_path.display()))?;
        }
        SyncbackMode::MergeExisting { overwrite } => {
            if !overwrite {
                // Non-overwriting merge requires the full 3-way conflict
                // pipeline, which is scoped to a later sub-phase. For now we
                // surface a clear error so the plugin can message the user.
                bail!(
                    "merge-existing without overwrite is not yet implemented (Fase 4 ships overwrite-only merge)"
                );
            }
            std::fs::create_dir_all(&opts.target_path)
                .with_context(|| format!("mkdir {}", opts.target_path.display()))?;
        }
    }
    Ok(())
}

// ─── Overwrite reconciliation (M2 / setup-2) ──────────────────────────────

/// After an overwrite-merge, removes files the *previous* materialization
/// tracked (`prev_base`) that this run did not reproduce (`new_base`). Without
/// this a second yeet resurrects scripts the user deleted in Studio — they
/// linger on disk as orphans with no base-tree entry — and can leave both
/// shapes of a reshaped script on disk (`Foo/init.luau` next to a fresh
/// `Foo.luau`).
///
/// Scope is deliberately narrow: only the `src/` mount this materializer owns,
/// and never a package-manager landing (M7). Removed files are relocated under
/// `.yeet/backup/<rel>` rather than unlinked so a mistaken overwrite stays
/// recoverable; directories emptied by the moves are pruned.
fn reconcile_overwrite(
    target_path: &Path,
    prev_base: &Tree,
    new_base: &Tree,
    written: &BTreeSet<String>,
    stats: &mut SyncbackStats,
) -> Result<()> {
    for rel in prev_base.keys() {
        if new_base.contains_key(rel) || !is_reconcilable(rel) {
            continue;
        }
        let abs = target_path.join(rel);
        if move_to_backup(target_path, rel)? {
            stats
                .warnings
                .push(format!("removed obsolete {rel} (backed up under .yeet/backup)"));
        }
        // The stale script's paired `.meta.json` is orphaned too — unless this
        // run wrote that exact path for a different instance (e.g. a folder now
        // occupying the removed script's directory), which we must not clobber.
        if let Some(meta_rel) = meta_sibling_for(rel)
            && !written.contains(&meta_rel)
        {
            move_to_backup(target_path, &meta_rel)?;
        }
        if let Some(parent) = abs.parent() {
            prune_empty_dirs(target_path, parent);
        }
    }
    Ok(())
}

/// True when `rel` names a path this materializer may reconcile: it must live
/// under the `src/` mount materialize writes, and no path segment may be a
/// package-manager landing (owned by Wally/pesde — never touch them, M7).
fn is_reconcilable(rel: &str) -> bool {
    rel.starts_with("src/") && !rel.split('/').any(is_package_manager_dir)
}

/// Moves `<target>/<rel>` under `<target>/.yeet/backup/<rel>`, preserving the
/// relative layout. Returns `Ok(false)` when there was nothing to move. A stale
/// backup already at the destination is replaced. Uses `extended_path` so deep
/// OneDrive trees don't trip MAX_PATH (M23).
fn move_to_backup(target_path: &Path, rel: &str) -> Result<bool> {
    let src = target_path.join(rel);
    if !extended_path(&src).exists() {
        return Ok(false);
    }
    let dest = target_path.join(".yeet").join("backup").join(rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(extended_path(parent))
            .with_context(|| format!("mkdir {}", parent.display()))?;
    }
    if extended_path(&dest).exists() {
        std::fs::remove_file(extended_path(&dest))
            .with_context(|| format!("clear stale backup {}", dest.display()))?;
    }
    std::fs::rename(extended_path(&src), extended_path(&dest))
        .with_context(|| format!("back up {} -> {}", src.display(), dest.display()))?;
    Ok(true)
}

/// Given a project-relative script path, returns the sibling `.meta.json` path
/// that `write_instance` would have emitted beside it. Init-form scripts
/// (`.../init.luau`) pair with `.../init.meta.json`; leaf scripts pair with
/// `.../<Name>.meta.json`, with any `.server`/`.client` sub-extension stripped.
/// Returns `None` for a path that isn't a `.luau` file.
fn meta_sibling_for(script_rel: &str) -> Option<String> {
    let (dir, file) = match script_rel.rsplit_once('/') {
        Some((d, f)) => (Some(d), f),
        None => (None, script_rel),
    };
    let meta_file = if crate::protocol::is_init_filename(file) {
        "init.meta.json".to_owned()
    } else {
        let stem = file.strip_suffix(".luau")?;
        let base = stem
            .strip_suffix(".server")
            .or_else(|| stem.strip_suffix(".client"))
            .unwrap_or(stem);
        format!("{base}.meta.json")
    };
    Some(match dir {
        Some(d) => format!("{d}/{meta_file}"),
        None => meta_file,
    })
}

/// Removes empty directories from `start` upward toward `target_path`
/// (exclusive), stopping at the first non-empty directory. Best-effort: any I/O
/// hiccup ends the walk silently, since a leftover empty directory is harmless.
fn prune_empty_dirs(target_path: &Path, start: &Path) {
    let mut cur = start.to_path_buf();
    while cur.starts_with(target_path) && cur.as_path() != target_path {
        let empty = match std::fs::read_dir(extended_path(&cur)) {
            Ok(mut rd) => rd.next().is_none(),
            Err(_) => break,
        };
        if !empty || std::fs::remove_dir(extended_path(&cur)).is_err() {
            break;
        }
        let Some(parent) = cur.parent().map(Path::to_path_buf) else {
            break;
        };
        cur = parent;
    }
}

#[derive(Debug)]
struct Forest {
    root: u64,
    children: HashMap<u64, Vec<u64>>,
}

fn build_forest(instances: &HashMap<u64, SerializedInstance>) -> Result<Forest> {
    let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
    let mut roots: Vec<u64> = Vec::new();
    for inst in instances.values() {
        match inst.parent_id {
            Some(pid) => {
                if !instances.contains_key(&pid) {
                    bail!(
                        "instance {} refers to unknown parent {}",
                        inst.id,
                        pid
                    );
                }
                children.entry(pid).or_default().push(inst.id);
            }
            None => roots.push(inst.id),
        }
    }
    if roots.len() != 1 {
        bail!("expected exactly one root, got {}", roots.len());
    }
    // Cheap cycle guard: BFS from root must visit every instance.
    let root = roots[0];
    let mut visited: BTreeSet<u64> = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if !visited.insert(node) {
            bail!("cycle in instance graph at id={node}");
        }
        if let Some(kids) = children.get(&node) {
            stack.extend(kids.iter().copied());
        }
    }
    if visited.len() != instances.len() {
        bail!(
            "disconnected instance graph: reachable {} vs total {}",
            visited.len(),
            instances.len()
        );
    }
    Ok(Forest { root, children })
}

struct WriteContext<'a> {
    opts: &'a SyncbackOptions,
    instances: &'a HashMap<u64, SerializedInstance>,
    children: &'a HashMap<u64, Vec<u64>>,
    stats: SyncbackStats,
    tree_base: Tree,
    /// Project-relative, forward-slashed paths this run has actually written.
    /// Consulted during overwrite reconciliation so a freshly-written meta file
    /// (e.g. a folder now occupying a removed script's directory) is never
    /// mistaken for the removed script's orphaned sidecar and clobbered.
    written_paths: BTreeSet<String>,
}

fn write_children(
    parent_id: u64,
    parent_dir: &Path,
    parent_rel: &str,
    ctx: &mut WriteContext,
) -> Result<()> {
    let kids = ctx.children.get(&parent_id).cloned().unwrap_or_default();
    let ordered = order_children(&kids, ctx.instances);
    let mut used: BTreeSet<String> = BTreeSet::new();
    for child_id in ordered {
        write_instance(child_id, parent_dir, parent_rel, &mut used, ctx)?;
    }
    Ok(())
}

fn write_instance(
    id: u64,
    parent_dir: &Path,
    parent_rel: &str,
    used_names: &mut BTreeSet<String>,
    ctx: &mut WriteContext,
) -> Result<()> {
    let inst = &ctx.instances[&id];

    // M7 (wally-6): never materialize a package-manager landing. These trees
    // are generated and owned by Wally/pesde and get corrupted if syncback
    // rewrites them (`.lua`→`.luau`, deformed structure); the package manager
    // regenerates them via its own install step. Skip by name, mirroring the
    // daemon's SWEEP_SKIP_DIR_NAMES.
    if is_package_manager_dir(&sanitize_name(&inst.name)) {
        ctx.stats.warnings.push(format!(
            "skipped package-manager tree {}/{} during syncback (owned by the package manager, not written by Yeet)",
            parent_rel, inst.name
        ));
        return Ok(());
    }

    // Binary payloads carry their own subtree and aren't gated by
    // `include_non_script` — the plugin only emits them when the user has
    // explicitly opted in via `include_binary`. We still honor the daemon-side
    // `include_binary` flag so an out-of-sync plugin can't slip them through.
    let binary_active = ctx.opts.include_binary && inst.binary.is_some();
    if binary_active {
        let sanitized_base = sanitize_name(&inst.name);
        let name = disambiguate(&sanitized_base, used_names);
        if name != inst.name {
            ctx.stats.warnings.push(format!(
                "renamed {}/{} -> {}/{} (illegal or case-insensitive duplicate name)",
                parent_rel, inst.name, parent_rel, name
            ));
        }
        // Folded, not literal: keeps later siblings colliding case-insensitively too.
        used_names.insert(name.to_lowercase());
        write_binary_instance(&name, parent_dir, parent_rel, inst, ctx)?;
        return Ok(());
    }

    // Scripts are always included regardless of `include_non_script`.
    if !is_script(&inst.class_name) && !ctx.opts.include_non_script {
        return Ok(());
    }

    let script_kind = script_kind_of(inst);
    let child_ids = ctx.children.get(&id).cloned().unwrap_or_default();

    let mut sanitized_base = sanitize_name(&inst.name);
    // M21 (path-3): reserve the promotion stems (`init` / `init.server` /
    // `init.client`) for a *leaf* script. Writing one as `init*.luau` lets the
    // re-ingest reader promote the PARENT directory from it, absorbing this
    // instance and shadowing a real folder-init.
    if let Some(kind) = script_kind
        && child_ids.is_empty()
        && is_reserved_leaf_stem(&sanitized_base, kind)
    {
        sanitized_base = format!("_{sanitized_base}");
    }
    let name = disambiguate(&sanitized_base, used_names);
    if name != inst.name {
        ctx.stats.warnings.push(format!(
            "renamed {}/{} -> {}/{} (illegal, reserved, or case-insensitive duplicate name)",
            parent_rel, inst.name, parent_rel, name
        ));
    }
    // Folded, not literal: keeps later siblings colliding case-insensitively too.
    used_names.insert(name.to_lowercase());

    match (script_kind, child_ids.is_empty()) {
        (Some(kind), true) => {
            let file_name = format!("{name}{}.luau", script_suffix(kind));
            let file_path = parent_dir.join(&file_name);
            let rel = format!("{parent_rel}/{file_name}");
            write_script_file(&file_path, &rel, inst, kind, ctx)?;
            if has_non_source_customizations(inst) {
                let meta_name = format!("{name}.meta.json");
                let meta_path = parent_dir.join(&meta_name);
                write_meta_file(&meta_path, inst, None, ctx)?;
            }
        }
        (Some(kind), false) => {
            let dir_path = parent_dir.join(&name);
            std::fs::create_dir_all(extended_path(&dir_path))
                .with_context(|| format!("mkdir {}", dir_path.display()))?;
            let init_name = format!("init{}.luau", script_suffix(kind));
            let init_path = dir_path.join(&init_name);
            let rel = format!("{parent_rel}/{name}/{init_name}");
            write_script_file(&init_path, &rel, inst, kind, ctx)?;
            if has_non_source_customizations(inst) {
                let meta_path = dir_path.join("init.meta.json");
                write_meta_file(&meta_path, inst, None, ctx)?;
            }
            let sub_rel = format!("{parent_rel}/{name}");
            write_children(id, &dir_path, &sub_rel, ctx)?;
        }
        (None, has_no_children) => {
            let dir_path = parent_dir.join(&name);
            std::fs::create_dir_all(extended_path(&dir_path))
                .with_context(|| format!("mkdir {}", dir_path.display()))?;
            let display_name = (inst.name != name).then(|| inst.name.clone());
            if has_custom_props(inst) || has_no_children || display_name.is_some() {
                let meta_path = dir_path.join("init.meta.json");
                write_meta_file(&meta_path, inst, display_name, ctx)?;
            }
            ctx.stats.non_script_instances_written += 1;
            let sub_rel = format!("{parent_rel}/{name}");
            write_children(id, &dir_path, &sub_rel, ctx)?;
        }
    }
    Ok(())
}

fn write_script_file(
    path: &Path,
    rel_for_tree: &str,
    inst: &SerializedInstance,
    kind: ScriptKind,
    ctx: &mut WriteContext,
) -> Result<()> {
    // A5 (swallow-2, daemon guard): the plugin is contractually required to
    // inject `Source` for LuaSourceContainers. If it's absent (or not a
    // string), the body never crossed the wire — writing an empty `.luau` and
    // counting it as a success would silently lose the script. Warn and skip
    // instead. An explicitly empty Source *string* is a real empty script and
    // is still written. The plugin now injects Source, so this is
    // defense-in-depth.
    let source = if let Some(SerializedProperty::String(s)) = inst.properties.get("Source") {
        s.clone()
    } else {
        ctx.stats.warnings.push(format!(
            "script {rel_for_tree} arrived without a Source property; skipped (no body to write)"
        ));
        return Ok(());
    };
    atomic_write(path, source.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    ctx.stats.scripts_written += 1;
    ctx.stats.total_bytes += source.len() as u64;
    let sha256 = crate::state::sha256_hex(source.as_bytes());
    ctx.tree_base.insert(
        rel_for_tree.to_owned(),
        TreeEntry {
            kind,
            content: source,
            sha256,
        },
    );
    Ok(())
}

fn write_meta_file(
    path: &Path,
    inst: &SerializedInstance,
    display_name: Option<String>,
    ctx: &mut WriteContext,
) -> Result<()> {
    let meta = MetaJson::from_instance(inst, display_name);
    let body = serde_json::to_vec_pretty(&meta).context("serialize meta.json")?;
    atomic_write(path, &body).with_context(|| format!("write {}", path.display()))?;
    ctx.stats.meta_files_written += 1;
    ctx.stats.total_bytes += body.len() as u64;
    // Record the project-relative path so overwrite reconciliation can tell an
    // orphaned sidecar apart from one this run just wrote (see M2 reconcile).
    if let Ok(rel) = path.strip_prefix(&ctx.opts.target_path) {
        ctx.written_paths
            .insert(rel.to_string_lossy().replace('\\', "/"));
    }
    Ok(())
}

/// Materializes a single instance whose subtree the plugin chose to ship as a
/// pre-serialized `.rbxm` blob. We don't decode or re-encode the bytes — the
/// payload travels base64-wrapped only because Fase 4-5 still talks JSON, and
/// the contents are written verbatim. Children are NOT recursed into because
/// the blob already contains them; surfacing them in the tree would create a
/// duplicate sibling on re-ingest.
fn write_binary_instance(
    name: &str,
    parent_dir: &Path,
    parent_rel: &str,
    inst: &SerializedInstance,
    ctx: &mut WriteContext,
) -> Result<()> {
    let payload = inst
        .binary
        .as_ref()
        .ok_or_else(|| anyhow!("write_binary_instance called without binary payload"))?;
    let bytes = BASE64_STD
        .decode(payload.rbxm_bytes_base64.as_bytes())
        .with_context(|| format!("decode base64 rbxm for {parent_rel}/{name}"))?;
    let file_name = format!("{name}.rbxm");
    let file_path = parent_dir.join(&file_name);
    atomic_write(&file_path, &bytes)
        .with_context(|| format!("write {}", file_path.display()))?;
    ctx.stats.non_script_instances_written += 1;
    ctx.stats.total_bytes += bytes.len() as u64;
    if payload.class_name != inst.class_name {
        ctx.stats.warnings.push(format!(
            "binary payload class mismatch at {parent_rel}/{name}: instance={} payload={}",
            inst.class_name, payload.class_name
        ));
    }
    Ok(())
}

// ─── Name sanitization ────────────────────────────────────────────────────

const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Package-manager landing directory names that syncback must never
/// materialize (M7 / wally-6). These trees are generated and owned by the
/// package manager (Wally / pesde) and get corrupted if the reverse-bootstrap
/// rewrites them (`.lua`→`.luau`, deformed structure, lost nested project
/// files). Mirrors `SWEEP_SKIP_DIR_NAMES` in `main.rs`, kept in sync by hand
/// because that const lives in the binary crate root and isn't reachable from
/// this library module.
const PACKAGE_MANAGER_DIR_NAMES: &[&str] = &[
    "Packages",
    "_Index",
    "roblox_packages",
    ".pesde",
    "node_modules",
    ".yeet",
];

/// True when `name` (already sanitized) is a package-manager landing that
/// syncback must skip. See `PACKAGE_MANAGER_DIR_NAMES`.
fn is_package_manager_dir(name: &str) -> bool {
    PACKAGE_MANAGER_DIR_NAMES.contains(&name)
}

fn sanitize_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0')
            || ch.is_control()
        {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    let trimmed = out.trim_matches(|c: char| c == '.' || c.is_whitespace());
    let mut result = if trimmed.is_empty() {
        "_unnamed".to_owned()
    } else {
        trimmed.to_owned()
    };
    // M22 (path-4): compare the segment BEFORE the first '.'. Windows treats
    // `CON.luau`, `aux.config`, `NUL.data`, … as the reserved *device* too, so
    // matching the whole uppercased string (pre-M22) let them through and the
    // OS write aborted the entire syncback. `trimmed` never starts with '.'
    // (leading dots are trimmed above), so the first split segment is the stem.
    let stem = result.split('.').next().unwrap_or(result.as_str());
    if WINDOWS_RESERVED.contains(&stem.to_ascii_uppercase().as_str()) {
        result = format!("_{result}");
    }
    result
}

/// Picks a name that doesn't collide with anything in `used`. Collisions are
/// detected case-INsensitively (via `to_lowercase`) because the destination
/// filesystem (NTFS/OneDrive) is case-insensitive: two siblings differing
/// only by case would otherwise round-trip to the same physical file and the
/// second `atomic_write` would silently clobber the first (C1 in the audit).
///
/// Contract: `used` must already hold the FOLDED (lowercased) form of every
/// name chosen so far — callers are responsible for feeding it that way (see
/// `write_instance`). The returned candidate always preserves `base`'s
/// original casing; only a numeric suffix is ever appended.
fn disambiguate(base: &str, used: &BTreeSet<String>) -> String {
    if !used.contains(&base.to_lowercase()) {
        return base.to_owned();
    }
    for i in 1..u32::MAX {
        let candidate = format!("{base}_{i}");
        if !used.contains(&candidate.to_lowercase()) {
            return candidate;
        }
    }
    unreachable!("exhausted 2^32 name suffixes");
}

/// True when a LEAF script's generated on-disk stem (`name` + kind suffix, no
/// `.luau` extension) would equal a directory-promotion stem — `init`,
/// `init.server`, or `init.client`. Such a file is read back on the next
/// Studio→disk round-trip as the *parent* directory's own source, silently
/// absorbing this instance and shadowing a real folder-init (M21 / path-3).
/// The match is case-sensitive because the promotion reader matches these
/// literally. Callers must gate on the instance being a childless script; a
/// folder or a script-with-children legitimately owns an `init.luau` inside
/// its own directory and must not be renamed.
fn is_reserved_leaf_stem(name: &str, kind: ScriptKind) -> bool {
    let stem = format!("{name}{}", script_suffix(kind));
    matches!(stem.as_str(), "init" | "init.server" | "init.client")
}

// ─── Instance classification helpers ──────────────────────────────────────

fn is_script(class_name: &str) -> bool {
    matches!(class_name, "ModuleScript" | "Script" | "LocalScript")
}

fn script_kind_of(inst: &SerializedInstance) -> Option<ScriptKind> {
    match inst.class_name.as_str() {
        "ModuleScript" => Some(ScriptKind::ModuleScript),
        "LocalScript" => Some(ScriptKind::LocalScript),
        "Script" => {
            // Honor modern Script.RunContext when present; default to server.
            if let Some(SerializedProperty::Enum(item)) = inst.properties.get("RunContext")
                && (item.eq_ignore_ascii_case("RunContext.Client") || item.ends_with(".Client"))
            {
                return Some(ScriptKind::LocalScript);
            }
            Some(ScriptKind::Script)
        }
        _ => None,
    }
}

const fn script_suffix(kind: ScriptKind) -> &'static str {
    match kind {
        ScriptKind::ModuleScript => "",
        ScriptKind::Script => ".server",
        ScriptKind::LocalScript => ".client",
    }
}

fn has_custom_props(inst: &SerializedInstance) -> bool {
    let script_only_source = is_script(&inst.class_name) && inst.properties.len() <= 1
        && inst.properties.keys().all(|k| k == "Source");
    if script_only_source && inst.attributes.is_empty() && inst.tags.is_empty() {
        return false;
    }
    !inst.properties.is_empty() || !inst.attributes.is_empty() || !inst.tags.is_empty()
}

fn has_non_source_customizations(inst: &SerializedInstance) -> bool {
    let non_source_props = inst.properties.iter().filter(|(k, _)| k.as_str() != "Source").count();
    non_source_props > 0 || !inst.attributes.is_empty() || !inst.tags.is_empty()
}

fn service_has_content(service_id: u64, ctx: &WriteContext) -> bool {
    let Some(kids) = ctx.children.get(&service_id) else {
        return has_custom_props(&ctx.instances[&service_id]);
    };
    !kids.is_empty() || has_custom_props(&ctx.instances[&service_id])
}

fn order_children(ids: &[u64], instances: &HashMap<u64, SerializedInstance>) -> Vec<u64> {
    let mut sorted: Vec<u64> = ids.to_vec();
    sorted.sort_by(|a, b| {
        let ia = &instances[a];
        let ib = &instances[b];
        ia.name.cmp(&ib.name).then_with(|| ia.id.cmp(&ib.id))
    });
    sorted
}

// ─── `default.project.json` construction ─────────────────────────────────

fn build_project_json(
    project_name_override: Option<&str>,
    target_path: &Path,
    services: &BTreeMap<String, (String, PathBuf)>,
) -> Project {
    let name = project_name_override.map_or_else(
        || {
            target_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("YeetedPlace")
                .to_owned()
        },
        str::to_owned,
    );
    let mut children = BTreeMap::new();
    for (service_name, (_sanitized, rel_path)) in services {
        let node = TreeNode {
            class_name: None,
            path: Some(rel_path.to_string_lossy().replace('\\', "/")),
            properties: serde_json::Map::new(),
            ignore_unknown_instances: None,
            children: BTreeMap::new(),
        };
        children.insert(service_name.clone(), node);
    }
    Project {
        name,
        tree: TreeNode {
            class_name: Some("DataModel".to_owned()),
            path: None,
            properties: serde_json::Map::new(),
            ignore_unknown_instances: None,
            children,
        },
    }
}

// ─── `.meta.json` serialization (Yeet-specific schema) ───────────────────

/// Our meta-json shape. Not Rojo-compatible (Rojo uses rbx-dom's encoding);
/// re-ingesting these files is the daemon's responsibility. The `name` field
/// is present only when the on-disk filename had to be disambiguated away
/// from the original instance name.
#[derive(Debug, Serialize)]
struct MetaJson {
    #[serde(rename = "className")]
    class_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    properties: serde_json::Value,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    attributes: serde_json::Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
}

impl MetaJson {
    fn from_instance(inst: &SerializedInstance, display_name: Option<String>) -> Self {
        let properties = props_to_json(&inst.properties, /*drop_source=*/ is_script(&inst.class_name));
        let attributes = props_to_json(&inst.attributes, false);
        Self {
            class_name: inst.class_name.clone(),
            name: display_name,
            properties,
            attributes,
            tags: inst.tags.clone(),
        }
    }
}

fn props_to_json(
    map: &HashMap<String, SerializedProperty>,
    drop_source: bool,
) -> serde_json::Value {
    let mut ordered: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
    for (k, v) in map {
        if drop_source && k == "Source" {
            continue;
        }
        ordered.insert(k.as_str(), property_to_json(v));
    }
    if ordered.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::to_value(&ordered).unwrap_or(serde_json::Value::Null)
}

fn property_to_json(prop: &SerializedProperty) -> serde_json::Value {
    match prop {
        SerializedProperty::String(s) => json!({ "type": "string", "value": s }),
        SerializedProperty::Bool(b) => json!({ "type": "bool", "value": b }),
        SerializedProperty::Number(n) => json!({ "type": "number", "value": n }),
        SerializedProperty::Vector2(v) => json!({ "type": "vector2", "value": v }),
        SerializedProperty::Vector3(v) => json!({ "type": "vector3", "value": v }),
        SerializedProperty::CFrame(m) => json!({ "type": "cframe", "value": m }),
        SerializedProperty::Color3(c) => json!({ "type": "color3", "value": c }),
        SerializedProperty::UDim(u) => json!({ "type": "udim", "value": u }),
        SerializedProperty::UDim2(u) => json!({ "type": "udim2", "value": u }),
        SerializedProperty::Enum(item) => json!({ "type": "enum", "value": item }),
        SerializedProperty::BrickColor(n) => json!({ "type": "brickcolor", "value": n }),
    }
}

// ─── Atomic write ─────────────────────────────────────────────────────────

/// On Windows, rewrites an absolute disk/UNC path to its extended-length
/// `\\?\` verbatim form so `std::fs` (not long-path-aware by default) can
/// create files and directories whose full path exceeds the legacy ~260-char
/// MAX_PATH — routine under a deep OneDrive root (M23 / path-5). Verbatim
/// paths disable normalization, so `/` is flipped to `\`. No-op for paths that
/// are already verbatim, non-disk, or relative, and on non-Windows targets.
#[cfg(windows)]
fn extended_path(path: &Path) -> PathBuf {
    use std::path::{Component, Prefix};
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(_) | Prefix::Verbatim(_) | Prefix::VerbatimUNC(_, _) => {
                path.to_path_buf()
            }
            Prefix::Disk(_) => {
                let s = path.to_string_lossy().replace('/', "\\");
                PathBuf::from(format!(r"\\?\{s}"))
            }
            Prefix::UNC(_, _) => {
                // \\server\share\… -> \\?\UNC\server\share\…
                let s = path.to_string_lossy().replace('/', "\\");
                PathBuf::from(format!(r"\\?\UNC\{}", s.trim_start_matches('\\')))
            }
            Prefix::DeviceNS(_) => path.to_path_buf(),
        },
        // Relative / rootless paths can't overflow on their own; leave them.
        _ => path.to_path_buf(),
    }
}

#[cfg(not(windows))]
fn extended_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(extended_path(parent))
            .with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let tmp = {
        let mut os = path.as_os_str().to_owned();
        os.push(".tmp");
        PathBuf::from(os)
    };
    std::fs::write(extended_path(&tmp), bytes)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(extended_path(&tmp), extended_path(path))
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(id: u64, parent: Option<u64>, class: &str, name: &str) -> SerializedInstance {
        SerializedInstance {
            id,
            parent_id: parent,
            class_name: class.to_owned(),
            name: name.to_owned(),
            properties: HashMap::new(),
            attributes: HashMap::new(),
            tags: Vec::new(),
            binary: None,
        }
    }

    fn script_inst(id: u64, parent: Option<u64>, name: &str, source: &str) -> SerializedInstance {
        let mut i = inst(id, parent, "ModuleScript", name);
        i.properties.insert(
            "Source".to_owned(),
            SerializedProperty::String(source.to_owned()),
        );
        i
    }

    fn test_opts(target_path: PathBuf) -> SyncbackOptions {
        SyncbackOptions {
            target_path,
            mode: SyncbackMode::NewProject,
            include_non_script: true,
            include_binary: false,
            template: SyncbackTemplate::Minimal,
            project_name: None,
        }
    }

    #[test]
    fn sanitize_handles_forbidden_chars() {
        assert_eq!(sanitize_name("foo/bar"), "foo_bar");
        assert_eq!(sanitize_name("a:b*c?"), "a_b_c_");
        assert_eq!(sanitize_name(""), "_unnamed");
        assert_eq!(sanitize_name("   "), "_unnamed");
        assert_eq!(sanitize_name("CON"), "_CON");
        assert_eq!(sanitize_name("con"), "_con");
        assert_eq!(sanitize_name("trailing."), "trailing");
    }

    #[test]
    fn disambiguate_suffixes() {
        // `used` holds the folded form per the function's contract (callers
        // fold before inserting — see `write_instance`).
        let mut used: BTreeSet<String> = ["foo".to_owned(), "foo_1".to_owned()].into();
        assert_eq!(disambiguate("Foo", &used), "Foo_2");
        used.insert("bar".to_owned());
        assert_eq!(disambiguate("Bar", &used), "Bar_1");
    }

    #[test]
    fn disambiguate_is_case_insensitive() {
        let mut used: BTreeSet<String> = BTreeSet::new();
        let first = disambiguate("Foo", &used);
        assert_eq!(first, "Foo", "first occurrence keeps its original casing");
        used.insert(first.to_lowercase());

        // A sibling whose name differs only by case must still be pushed to
        // a suffixed form — this is the C1 invariant: two distinct instances
        // under the same parent must never fold to the same physical path.
        let second = disambiguate("foo", &used);
        assert_eq!(second, "foo_1");
        assert_ne!(
            first.to_lowercase(),
            second.to_lowercase(),
            "folded forms must differ or NTFS/OneDrive would collapse them into one file"
        );
        used.insert(second.to_lowercase());

        let third = disambiguate("FOO", &used);
        assert_eq!(third, "FOO_2");
    }

    #[test]
    fn build_forest_rejects_cycle() {
        let mut map = HashMap::new();
        map.insert(1, inst(1, Some(2), "Folder", "A"));
        map.insert(2, inst(2, Some(1), "Folder", "B"));
        let err = build_forest(&map).unwrap_err();
        assert!(err.to_string().contains("root"));
    }

    #[test]
    fn build_forest_requires_single_root() {
        let mut map = HashMap::new();
        map.insert(1, inst(1, None, "DataModel", "game"));
        map.insert(2, inst(2, None, "DataModel", "game"));
        let err = build_forest(&map).unwrap_err();
        assert!(err.to_string().contains("root"));
    }

    #[test]
    fn script_kind_of_respects_run_context() {
        let mut script = inst(1, Some(0), "Script", "A");
        script.properties.insert(
            "RunContext".to_owned(),
            SerializedProperty::Enum("RunContext.Client".to_owned()),
        );
        assert_eq!(script_kind_of(&script), Some(ScriptKind::LocalScript));
        script.properties.insert(
            "RunContext".to_owned(),
            SerializedProperty::Enum("RunContext.Server".to_owned()),
        );
        assert_eq!(script_kind_of(&script), Some(ScriptKind::Script));
    }

    // ─── C1: case-insensitive sibling collisions (path-1) ─────────────────

    #[test]
    fn write_children_disambiguates_case_insensitive_leaf_siblings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opts = test_opts(dir.path().to_path_buf());

        let mut instances = HashMap::new();
        instances.insert(1, script_inst(1, Some(0), "Foo", "return 1\n"));
        instances.insert(2, script_inst(2, Some(0), "foo", "return 2\n"));
        let mut children = HashMap::new();
        children.insert(0u64, vec![1u64, 2u64]);

        let mut ctx = WriteContext {
            opts: &opts,
            instances: &instances,
            children: &children,
            stats: SyncbackStats::default(),
            tree_base: Tree::new(),
            written_paths: BTreeSet::new(),
        };
        write_children(0, dir.path(), "src", &mut ctx).expect("write_children");

        // The two siblings must land on physically distinct files: their
        // folded (case-insensitive) forms must differ, or NTFS/OneDrive
        // would silently collapse them into one — the C1 data-loss bug.
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .map(|e| e.expect("dir entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["Foo.luau".to_owned(), "foo_1.luau".to_owned()]);

        let mut folded: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
        folded.sort();
        folded.dedup();
        assert_eq!(
            folded.len(),
            names.len(),
            "folded filenames must be pairwise distinct"
        );

        assert_eq!(
            std::fs::read_to_string(dir.path().join("Foo.luau")).unwrap(),
            "return 1\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("foo_1.luau")).unwrap(),
            "return 2\n"
        );

        assert!(
            ctx.stats
                .warnings
                .iter()
                .any(|w| w.contains("src/foo -> src/foo_1")),
            "expected a warning recording the case-collision rename, got: {:?}",
            ctx.stats.warnings
        );
    }

    #[test]
    fn write_children_disambiguates_case_insensitive_folder_siblings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opts = test_opts(dir.path().to_path_buf());

        let mut instances = HashMap::new();
        instances.insert(1, inst(1, Some(0), "Folder", "Config"));
        instances.insert(3, inst(3, Some(0), "Folder", "config"));
        instances.insert(2, script_inst(2, Some(1), "SettingsA", "return \"a\"\n"));
        instances.insert(4, script_inst(4, Some(3), "SettingsB", "return \"b\"\n"));

        let mut children = HashMap::new();
        children.insert(0u64, vec![1u64, 3u64]);
        children.insert(1u64, vec![2u64]);
        children.insert(3u64, vec![4u64]);

        let mut ctx = WriteContext {
            opts: &opts,
            instances: &instances,
            children: &children,
            stats: SyncbackStats::default(),
            tree_base: Tree::new(),
            written_paths: BTreeSet::new(),
        };
        write_children(0, dir.path(), "src", &mut ctx).expect("write_children");

        // Distinct directories — not the same folder receiving both children.
        // Assert via read_dir (case-preserved entry names) rather than
        // `join("config").exists()`: on a case-insensitive filesystem
        // (NTFS/OneDrive — the target platform) "config" resolves to the
        // existing "Config", which would make that check spuriously succeed
        // and hide the very collision this test guards against.
        let mut dirs: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .map(|e| e.expect("dir entry").file_name().to_string_lossy().into_owned())
            .filter(|n| dir.path().join(n).is_dir())
            .collect();
        dirs.sort();
        assert_eq!(dirs, vec!["Config".to_owned(), "config_1".to_owned()]);

        // Both subtrees survive untouched.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Config/SettingsA.luau")).unwrap(),
            "return \"a\"\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config_1/SettingsB.luau")).unwrap(),
            "return \"b\"\n"
        );

        // The renamed folder keeps its original Studio name recoverable via
        // init.meta.json, same as any other disambiguated instance.
        let meta: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("config_1/init.meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["name"], "config");

        assert!(
            ctx.stats
                .warnings
                .iter()
                .any(|w| w.contains("src/config -> src/config_1")),
            "expected a warning recording the case-collision rename, got: {:?}",
            ctx.stats.warnings
        );
    }

    #[test]
    fn write_children_leaves_non_colliding_siblings_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opts = test_opts(dir.path().to_path_buf());

        let mut instances = HashMap::new();
        instances.insert(1, script_inst(1, Some(0), "Foo", "return 1\n"));
        instances.insert(2, script_inst(2, Some(0), "Bar", "return 2\n"));
        let mut children = HashMap::new();
        children.insert(0u64, vec![1u64, 2u64]);

        let mut ctx = WriteContext {
            opts: &opts,
            instances: &instances,
            children: &children,
            stats: SyncbackStats::default(),
            tree_base: Tree::new(),
            written_paths: BTreeSet::new(),
        };
        write_children(0, dir.path(), "src", &mut ctx).expect("write_children");

        // Regression: distinct (non-colliding, even when folded) names must
        // come out exactly as before — untouched and silent.
        assert!(dir.path().join("Foo.luau").exists());
        assert!(dir.path().join("Bar.luau").exists());
        assert!(
            ctx.stats.warnings.is_empty(),
            "non-colliding names must not trigger any warning, got: {:?}",
            ctx.stats.warnings
        );
    }

    // ─── M22: reserved device names WITH an extension (path-4) ────────────

    #[test]
    fn sanitize_reserves_device_names_with_extension() {
        // The reserved-name guard must compare the segment BEFORE the first
        // '.', because Windows treats `CON.luau`, `aux.config`, etc. as the
        // reserved device too. Guarding only the whole string (pre-M22) let
        // these through and the OS write aborted the entire syncback.
        assert_eq!(sanitize_name("CON.luau"), "_CON.luau");
        assert_eq!(sanitize_name("aux.config"), "_aux.config");
        assert_eq!(sanitize_name("NUL.data"), "_NUL.data");
        assert_eq!(sanitize_name("com1.server.luau"), "_com1.server.luau");
        // Bare reserved names keep the pre-M22 behavior.
        assert_eq!(sanitize_name("CON"), "_CON");
        assert_eq!(sanitize_name("nul"), "_nul");
        // No false positives: names that merely start with reserved letters.
        assert_eq!(sanitize_name("console.luau"), "console.luau");
        assert_eq!(sanitize_name("auxiliary"), "auxiliary");
        assert_eq!(sanitize_name("Content.luau"), "Content.luau");
    }

    #[test]
    fn materialize_completes_with_reserved_device_name_instance() {
        // A single instance whose filename stem is a reserved device name
        // (`aux.config` -> `aux.config.luau`, stem `aux`) must not abort the
        // whole syncback. Before the fix the OS write fails on Windows and
        // `materialize` returns Err without writing default.project.json.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session =
            SyncbackSession::new("req-m22".to_owned(), test_opts(dir.path().to_path_buf()));
        session
            .ingest_chunk(
                0,
                vec![
                    inst(1, None, "DataModel", "game"),
                    inst(2, Some(1), "Folder", "ServerScriptService"),
                    script_inst(3, Some(2), "aux.config", "return 1\n"),
                ],
            )
            .expect("ingest");
        let stats = materialize(session, 1).expect("materialize must complete, not abort");
        assert!(
            dir.path().join("default.project.json").exists(),
            "project file must be written"
        );
        assert!(
            dir.path()
                .join("src/ServerScriptService/_aux.config.luau")
                .exists(),
            "script must land under a de-reserved name"
        );
        // NB: we deliberately don't assert `!aux.config.luau.exists()` — on
        // Windows a trailing `aux.config.luau` resolves to the AUX *device*,
        // so `exists()` can report true regardless of what we wrote.
        assert_eq!(stats.scripts_written, 1);
    }

    // ─── M21: leaf instance named `init` (path-3) ─────────────────────────

    #[test]
    fn write_instance_reserves_leaf_named_init_against_parent_promotion() {
        // A leaf script literally named `init` collides with the PARENT's own
        // promotion file (`init.luau`) and would clobber it; on re-ingest the
        // parent absorbs the leaf. It must be renamed (`_init`) with a warning.
        let dir = tempfile::tempdir().expect("tempdir");
        let opts = test_opts(dir.path().to_path_buf());
        // Foo is a ModuleScript WITH a child -> promoted to Foo/init.luau (its
        // own source). Its child is a leaf ModuleScript literally named `init`.
        let mut instances = HashMap::new();
        instances.insert(1, script_inst(1, Some(0), "Foo", "foo-source\n"));
        instances.insert(2, script_inst(2, Some(1), "init", "leaf-source\n"));
        let mut children = HashMap::new();
        children.insert(0u64, vec![1u64]);
        children.insert(1u64, vec![2u64]);

        let mut ctx = WriteContext {
            opts: &opts,
            instances: &instances,
            children: &children,
            stats: SyncbackStats::default(),
            tree_base: Tree::new(),
            written_paths: BTreeSet::new(),
        };
        write_children(0, dir.path(), "src", &mut ctx).expect("write_children");

        // Foo's own promotion source survives intact...
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Foo/init.luau")).unwrap(),
            "foo-source\n"
        );
        // ...and the leaf `init` is written to a distinct, non-promotion file.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Foo/_init.luau")).unwrap(),
            "leaf-source\n"
        );
        assert!(
            ctx.stats.warnings.iter().any(|w| w.contains("init")),
            "expected a rename warning, got: {:?}",
            ctx.stats.warnings
        );
    }

    #[test]
    fn write_instance_reserves_leaf_init_for_all_script_kinds() {
        // Each leaf named `init`, one per kind, must be pushed off the
        // promotion filename for that kind.
        let dir = tempfile::tempdir().expect("tempdir");
        let opts = test_opts(dir.path().to_path_buf());
        let mut instances = HashMap::new();
        instances.insert(10, inst(10, Some(0), "Folder", "M"));
        instances.insert(11, script_inst(11, Some(10), "init", "m\n")); // ModuleScript
        instances.insert(20, inst(20, Some(0), "Folder", "S"));
        let mut s = inst(21, Some(20), "Script", "init");
        s.properties
            .insert("Source".to_owned(), SerializedProperty::String("s\n".to_owned()));
        instances.insert(21, s);
        instances.insert(30, inst(30, Some(0), "Folder", "L"));
        let mut l = inst(31, Some(30), "LocalScript", "init");
        l.properties
            .insert("Source".to_owned(), SerializedProperty::String("l\n".to_owned()));
        instances.insert(31, l);
        let mut children = HashMap::new();
        children.insert(0u64, vec![10u64, 20u64, 30u64]);
        children.insert(10u64, vec![11u64]);
        children.insert(20u64, vec![21u64]);
        children.insert(30u64, vec![31u64]);

        let mut ctx = WriteContext {
            opts: &opts,
            instances: &instances,
            children: &children,
            stats: SyncbackStats::default(),
            tree_base: Tree::new(),
            written_paths: BTreeSet::new(),
        };
        write_children(0, dir.path(), "src", &mut ctx).expect("write_children");

        assert!(dir.path().join("M/_init.luau").exists());
        assert!(dir.path().join("S/_init.server.luau").exists());
        assert!(dir.path().join("L/_init.client.luau").exists());
        // No promotion-shaped leaf leaked through.
        assert!(!dir.path().join("M/init.luau").exists());
        assert!(!dir.path().join("S/init.server.luau").exists());
        assert!(!dir.path().join("L/init.client.luau").exists());
    }

    // ─── A5: script instance missing `Source` (swallow-2 daemon guard) ────

    #[test]
    fn write_script_file_warns_on_missing_source_instead_of_empty() {
        // A script that arrives WITHOUT a Source property must not be written
        // as an empty `.luau` and counted as success — the body never crossed
        // the wire. Warn and skip instead.
        let dir = tempfile::tempdir().expect("tempdir");
        let opts = test_opts(dir.path().to_path_buf());
        let mut instances = HashMap::new();
        instances.insert(1, inst(1, Some(0), "ModuleScript", "Broken")); // no Source
        let mut children = HashMap::new();
        children.insert(0u64, vec![1u64]);

        let mut ctx = WriteContext {
            opts: &opts,
            instances: &instances,
            children: &children,
            stats: SyncbackStats::default(),
            tree_base: Tree::new(),
            written_paths: BTreeSet::new(),
        };
        write_children(0, dir.path(), "src", &mut ctx).expect("write_children");

        assert!(
            !dir.path().join("Broken.luau").exists(),
            "must not write an empty script file"
        );
        assert_eq!(
            ctx.stats.scripts_written, 0,
            "a body-less script must not count as written"
        );
        assert!(
            ctx.stats
                .warnings
                .iter()
                .any(|w| w.contains("Source") && w.contains("Broken")),
            "expected a missing-Source warning, got: {:?}",
            ctx.stats.warnings
        );
        assert!(
            ctx.tree_base.is_empty(),
            "no base-tree entry for an unwritten script"
        );
    }

    #[test]
    fn write_script_file_writes_empty_source_when_present() {
        // Regression guard for A5: an explicitly empty Source (present, "") is
        // a real empty script and must still be written and counted — only an
        // ABSENT Source is the error case.
        let dir = tempfile::tempdir().expect("tempdir");
        let opts = test_opts(dir.path().to_path_buf());
        let mut instances = HashMap::new();
        instances.insert(1, script_inst(1, Some(0), "Empty", ""));
        let mut children = HashMap::new();
        children.insert(0u64, vec![1u64]);

        let mut ctx = WriteContext {
            opts: &opts,
            instances: &instances,
            children: &children,
            stats: SyncbackStats::default(),
            tree_base: Tree::new(),
            written_paths: BTreeSet::new(),
        };
        write_children(0, dir.path(), "src", &mut ctx).expect("write_children");

        assert!(dir.path().join("Empty.luau").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Empty.luau")).unwrap(),
            ""
        );
        assert_eq!(ctx.stats.scripts_written, 1);
        assert!(ctx.stats.warnings.is_empty());
    }

    // ─── M7: skip package-manager landings during syncback (wally-6) ──────

    #[test]
    fn write_children_skips_package_manager_dirs() {
        // A `Packages` landing (and its `_Index`) is owned by the package
        // manager; syncback must not rewrite it. Skipped by name, mirroring
        // the daemon's SWEEP_SKIP_DIR_NAMES.
        let dir = tempfile::tempdir().expect("tempdir");
        let opts = test_opts(dir.path().to_path_buf());
        let mut instances = HashMap::new();
        instances.insert(1, inst(1, Some(0), "Folder", "Packages"));
        instances.insert(2, inst(2, Some(1), "Folder", "_Index"));
        instances.insert(3, script_inst(3, Some(2), "SomePkg", "return 'pkg'\n"));
        instances.insert(4, script_inst(4, Some(0), "MyModule", "return 'mine'\n"));
        let mut children = HashMap::new();
        children.insert(0u64, vec![1u64, 4u64]);
        children.insert(1u64, vec![2u64]);
        children.insert(2u64, vec![3u64]);

        let mut ctx = WriteContext {
            opts: &opts,
            instances: &instances,
            children: &children,
            stats: SyncbackStats::default(),
            tree_base: Tree::new(),
            written_paths: BTreeSet::new(),
        };
        write_children(0, dir.path(), "src", &mut ctx).expect("write_children");

        assert!(
            !dir.path().join("Packages").exists(),
            "the Packages landing must be skipped entirely"
        );
        assert!(
            dir.path().join("MyModule.luau").exists(),
            "a normal sibling must still be materialized"
        );
        assert!(
            ctx.stats
                .warnings
                .iter()
                .any(|w| w.to_lowercase().contains("package")),
            "expected a skip warning, got: {:?}",
            ctx.stats.warnings
        );
    }

    // ─── M23: long-path (MAX_PATH) handling under a deep root (path-5) ────

    #[cfg(windows)]
    #[test]
    fn extended_path_verbatimizes_windows_disk_paths() {
        assert_eq!(
            extended_path(Path::new(r"C:\a\b")),
            PathBuf::from(r"\\?\C:\a\b")
        );
        // Forward slashes are flipped (verbatim paths don't normalize).
        assert_eq!(
            extended_path(Path::new("C:/a/b")),
            PathBuf::from(r"\\?\C:\a\b")
        );
        // Already-verbatim paths are left untouched.
        assert_eq!(
            extended_path(Path::new(r"\\?\C:\a\b")),
            PathBuf::from(r"\\?\C:\a\b")
        );
        // Relative paths can't overflow on their own; unchanged.
        assert_eq!(extended_path(Path::new(r"a\b")), PathBuf::from(r"a\b"));
    }

    #[cfg(not(windows))]
    #[test]
    fn extended_path_is_identity_off_windows() {
        assert_eq!(extended_path(Path::new("/a/b/c")), PathBuf::from("/a/b/c"));
    }

    #[cfg(windows)]
    #[test]
    fn write_children_handles_paths_over_max_path() {
        // A tree deep enough that the full path exceeds ~260 chars must still
        // materialize. Without the `\\?\` rewrite, std::fs aborts mid-write.
        let dir = tempfile::tempdir().expect("tempdir");
        let opts = test_opts(dir.path().to_path_buf());
        let mut instances = HashMap::new();
        let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
        let seg = "verylongsegmentnamethatpadsthepath_"; // 35 chars
        let depth: u64 = 8;
        let mut parent = 0u64;
        for i in 1..=depth {
            instances.insert(i, inst(i, Some(parent), "Folder", &format!("{seg}{i:02}")));
            children.entry(parent).or_default().push(i);
            parent = i;
        }
        let leaf_id = depth + 1;
        instances.insert(
            leaf_id,
            script_inst(leaf_id, Some(parent), "Deep", "return 'deep'\n"),
        );
        children.entry(parent).or_default().push(leaf_id);

        let mut ctx = WriteContext {
            opts: &opts,
            instances: &instances,
            children: &children,
            stats: SyncbackStats::default(),
            tree_base: Tree::new(),
            written_paths: BTreeSet::new(),
        };
        write_children(0, dir.path(), "src", &mut ctx)
            .expect("deep write must not fail on MAX_PATH");
        assert_eq!(ctx.stats.scripts_written, 1);
    }

    // ─── M2: overwrite-merge cleans obsolete / reshaped files (setup-2) ────

    fn materialize_overwrite(
        target: &Path,
        request_id: &str,
        instances: Vec<SerializedInstance>,
    ) -> SyncbackStats {
        let opts = SyncbackOptions {
            target_path: target.to_path_buf(),
            mode: SyncbackMode::MergeExisting { overwrite: true },
            include_non_script: true,
            include_binary: false,
            template: SyncbackTemplate::Minimal,
            project_name: Some("m2".to_owned()),
        };
        let mut session = SyncbackSession::new(request_id.to_owned(), opts);
        session.ingest_chunk(0, instances).expect("ingest chunk");
        materialize(session, 1).expect("materialize")
    }

    /// Project-relative, forward-slashed paths of every `.luau` file under
    /// `dir`. Used to assert base tree and disk agree in both directions.
    fn collect_luau_files(dir: &Path, root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(collect_luau_files(&path, root));
            } else if path.extension().and_then(|s| s.to_str()) == Some("luau")
                && let Ok(rel) = path.strip_prefix(root)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
        out
    }

    #[test]
    fn meta_sibling_matches_written_form() {
        assert_eq!(
            meta_sibling_for("src/RS/Foo/init.luau").as_deref(),
            Some("src/RS/Foo/init.meta.json")
        );
        assert_eq!(
            meta_sibling_for("src/RS/Foo/init.server.luau").as_deref(),
            Some("src/RS/Foo/init.meta.json")
        );
        assert_eq!(
            meta_sibling_for("src/RS/Bar.luau").as_deref(),
            Some("src/RS/Bar.meta.json")
        );
        assert_eq!(
            meta_sibling_for("src/RS/Bar.client.luau").as_deref(),
            Some("src/RS/Bar.meta.json")
        );
        assert_eq!(meta_sibling_for("src/RS/notes.txt"), None);
    }

    #[test]
    fn is_reconcilable_scopes_to_src_and_skips_package_dirs() {
        assert!(is_reconcilable("src/ServerScriptService/Foo.luau"));
        // Other mounts materialize doesn't own must be left alone.
        assert!(!is_reconcilable("shared/Foo.luau"));
        // Package-manager landings are owned elsewhere (M7).
        assert!(!is_reconcilable("src/Packages/Foo.luau"));
        assert!(!is_reconcilable("src/ReplicatedStorage/_Index/Pkg/init.luau"));
    }

    /// A second overwrite-merge with a reduced tree must clean up after the
    /// first: a script deleted in Studio must not resurrect, and a script that
    /// changed shape (`Foo/init.luau` -> `Foo.luau`) must not leave both forms
    /// on disk. The persisted base tree must agree with disk in both
    /// directions.
    #[test]
    fn merge_overwrite_removes_obsolete_and_reshaped_scripts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().to_path_buf();

        // Run 1: `Shape` has a child, so it materializes as `Shape/init.luau`
        // plus `Shape/Child.luau`.
        materialize_overwrite(
            &target,
            "run-1",
            vec![
                inst(1, None, "DataModel", "game"),
                inst(2, Some(1), "ServerScriptService", "ServerScriptService"),
                script_inst(3, Some(2), "Keep", "return 'keep v1'\n"),
                script_inst(4, Some(2), "Gone", "return 'gone'\n"),
                script_inst(5, Some(2), "Shape", "return 'shape v1'\n"),
                script_inst(6, Some(5), "Child", "return 'child'\n"),
            ],
        );

        let keep = target.join("src/ServerScriptService/Keep.luau");
        let gone = target.join("src/ServerScriptService/Gone.luau");
        let shape_init = target.join("src/ServerScriptService/Shape/init.luau");
        let shape_child = target.join("src/ServerScriptService/Shape/Child.luau");
        let shape_leaf = target.join("src/ServerScriptService/Shape.luau");
        assert!(keep.exists(), "run 1 should write Keep.luau");
        assert!(gone.exists(), "run 1 should write Gone.luau");
        assert!(shape_init.exists(), "run 1 should write Shape/init.luau");
        assert!(shape_child.exists(), "run 1 should write Shape/Child.luau");

        // Run 2: `Gone` was deleted in Studio; `Shape` lost its child, so it
        // flips to a single-file leaf `Shape.luau`.
        materialize_overwrite(
            &target,
            "run-2",
            vec![
                inst(1, None, "DataModel", "game"),
                inst(2, Some(1), "ServerScriptService", "ServerScriptService"),
                script_inst(3, Some(2), "Keep", "return 'keep v2'\n"),
                script_inst(5, Some(2), "Shape", "return 'shape v2'\n"),
            ],
        );

        // The deleted script must not survive the second overwrite.
        assert!(!gone.exists(), "deleted script resurrected on 2nd overwrite");
        // Shape flipped to a leaf; the two forms must not coexist.
        assert!(shape_leaf.exists(), "reshaped script missing as leaf file");
        assert!(!shape_init.exists(), "stale Foo/init.luau form left on disk");
        assert!(!shape_child.exists(), "obsolete child script left on disk");
        assert!(keep.exists(), "kept script must remain");

        let base = tree::load_base_tree(&target)
            .expect("load base tree")
            .expect("base tree present");
        // base subset of disk: nothing tracked that isn't materialized.
        for rel in base.keys() {
            assert!(
                target.join(rel).exists(),
                "base tree references a path absent from disk: {rel}"
            );
        }
        // disk subset of base: no orphan script the base tree forgot.
        for rel in collect_luau_files(&target.join("src"), &target) {
            assert!(
                base.contains_key(&rel),
                "orphan script left on disk after overwrite: {rel}"
            );
        }
        assert_eq!(base.len(), 2, "only Keep + Shape should remain tracked");

        // Obsolete files are recoverable under .yeet/backup, not hard-deleted.
        assert!(
            target
                .join(".yeet/backup/src/ServerScriptService/Gone.luau")
                .exists(),
            "removed file should be backed up under .yeet/backup"
        );
    }
}
