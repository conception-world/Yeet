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
//! - Name collisions between siblings are deterministically disambiguated by
//!   appending `_1`, `_2`, ... and recorded in `stats.warnings`.

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

    let forest = build_forest(&session.instances)?;
    let root = forest.root;
    let mut ctx = WriteContext {
        opts: &session.opts,
        instances: &session.instances,
        children: &forest.children,
        stats: SyncbackStats::default(),
        tree_base: Tree::new(),
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
    std::fs::create_dir_all(&src_dir)
        .with_context(|| format!("mkdir {}", src_dir.display()))?;

    for service_id in root_ordered {
        let service = &ctx.instances[&service_id];
        if !service_has_content(service_id, &ctx) {
            continue;
        }
        let sanitized = sanitize_name(&service.name);
        let service_dir = src_dir.join(&sanitized);
        std::fs::create_dir_all(&service_dir)
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
                "renamed {}/{} -> {}/{} (illegal or duplicate name)",
                parent_rel, inst.name, parent_rel, name
            ));
        }
        used_names.insert(name.clone());
        write_binary_instance(&name, parent_dir, parent_rel, inst, ctx)?;
        return Ok(());
    }

    // Scripts are always included regardless of `include_non_script`.
    if !is_script(&inst.class_name) && !ctx.opts.include_non_script {
        return Ok(());
    }

    let script_kind = script_kind_of(inst);
    let child_ids = ctx.children.get(&id).cloned().unwrap_or_default();

    let sanitized_base = sanitize_name(&inst.name);
    let name = disambiguate(&sanitized_base, used_names);
    if name != inst.name {
        ctx.stats.warnings.push(format!(
            "renamed {}/{} -> {}/{} (illegal or duplicate name)",
            parent_rel, inst.name, parent_rel, name
        ));
    }
    used_names.insert(name.clone());

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
            std::fs::create_dir_all(&dir_path)
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
            std::fs::create_dir_all(&dir_path)
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
    let source = inst
        .properties
        .get("Source")
        .and_then(|p| match p {
            SerializedProperty::String(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();
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
    let upper = result.to_ascii_uppercase();
    if WINDOWS_RESERVED.contains(&upper.as_str()) {
        result = format!("_{result}");
    }
    result
}

fn disambiguate(base: &str, used: &BTreeSet<String>) -> String {
    if !used.contains(base) {
        return base.to_owned();
    }
    for i in 1..u32::MAX {
        let candidate = format!("{base}_{i}");
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("exhausted 2^32 name suffixes");
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

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let tmp = {
        let mut os = path.as_os_str().to_owned();
        os.push(".tmp");
        PathBuf::from(os)
    };
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
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
        let mut used: BTreeSet<String> = ["Foo".to_owned(), "Foo_1".to_owned()].into();
        assert_eq!(disambiguate("Foo", &used), "Foo_2");
        used.insert("Bar".to_owned());
        assert_eq!(disambiguate("Bar", &used), "Bar_1");
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
}
