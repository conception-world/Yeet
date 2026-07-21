//! Rojo-format `sourcemap.json` generation.
//!
//! luau-lsp resolves `game.ServerScriptService.Foo`, `require(Packages.X)`, and
//! Wally package types from a `sourcemap.json` at the project root. Yeet never
//! produced one, so a freshly-set-up project had no type resolution until the
//! user hand-ran Rojo. This module builds a Rojo-compatible sourcemap from the
//! daemon's own file tree, mirroring the exact path→instance mapping the plugin
//! applies in `TreeBuilder.luau` and the daemon classifies in `protocol::classify`:
//!
//!   * `Foo.luau` / `Foo.server.luau` / `Foo.client.luau` → a
//!     ModuleScript / Script / LocalScript named `Foo`.
//!   * a directory containing `init.luau` (`.server`/`.client` variants too)
//!     becomes that Script, with its siblings as children.
//!   * every other directory becomes a `Folder`.
//!
//! `tree_fs` keys are already the A10-collapsed canonical paths, so a Wally
//! package (`.../foo/src/init.lua`, collapsed to `.../foo/init.lua`) maps as the
//! `foo` ModuleScript rather than a `Folder` with a stray `src` child.
//!
//! The generator is pure over `(project, tree, remaps)`. Instance *shape* comes
//! from the tree key, but `filePaths` is put back through `state::fs_rel_with`
//! so it names the real file on disk: for an ordinary project the two are equal,
//! while inside an A10-collapsed Wally package the key has dropped the package's
//! own `$path`/`src` segment and nothing exists at that path. luau-lsp opens the
//! string verbatim, so emitting the key there resolved to nothing.

use std::collections::BTreeMap;
use std::hash::{Hash as _, Hasher as _};
use std::io::Write as _;
use std::ops::Not as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::project::Project;
use crate::protocol::{ScriptKind, classify, is_init_filename};
use crate::state::{PackageRemap, fs_rel_with};
use crate::tree::Tree;

/// Mutable node while assembling the sourcemap. Children are keyed by instance
/// name in a `BTreeMap` so the emitted JSON is deterministic (sorted) regardless
/// of the order files are visited — no churn between otherwise-identical runs.
struct SourceNode {
    class_name: String,
    file_paths: Vec<String>,
    children: BTreeMap<String, SourceNode>,
}

impl SourceNode {
    fn new(class_name: String) -> Self {
        Self {
            class_name,
            file_paths: Vec::new(),
            children: BTreeMap::new(),
        }
    }

    fn folder() -> Self {
        Self::new("Folder".to_owned())
    }

    fn to_json(&self, name: &str) -> Value {
        let children: Vec<Value> = self
            .children
            .iter()
            .map(|(child_name, node)| node.to_json(child_name))
            .collect();
        json!({
            "name": name,
            "className": self.class_name,
            "filePaths": self.file_paths,
            "children": children,
        })
    }
}

/// Marker Yeet stamps on the sourcemap root, and looks for before overwriting
/// an existing one. Rojo does not define this key and luau-lsp ignores keys it
/// does not know, so it is inert to every consumer — its only job is to let a
/// later run tell "a map I wrote" from "a map the user maintains".
pub const GENERATED_BY_KEY: &str = "generatedBy";
pub const GENERATED_BY_VALUE: &str = "yeet";

/// True when `<root>/sourcemap.json` exists but was not written by Yeet — a
/// hand-maintained map, or one produced by `rojo sourcemap --watch`, which is
/// the standard luau-lsp setup. Yeet used to overwrite it unconditionally on
/// every daemon start, silently narrowing it to Yeet's own view.
///
/// A file we cannot parse counts as foreign: refusing to touch what we cannot
/// read is the only safe reading of an ambiguous state.
#[must_use]
pub fn is_foreign(root: &Path) -> bool {
    let path = root.join("sourcemap.json");
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<Value>(&raw).is_ok_and(|value| {
            value.get(GENERATED_BY_KEY).and_then(Value::as_str) == Some(GENERATED_BY_VALUE)
        })
        .not(),
        // Genuinely absent — nothing to protect, so writing is fine.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        // Present but unreadable (permissions, a lock, non-UTF-8 such as a
        // UTF-16 map from another tool). We cannot confirm it is ours, and the
        // whole point of this guard is to not clobber what we cannot read.
        Err(_) => true,
    }
}

fn class_for(kind: ScriptKind) -> &'static str {
    match kind {
        ScriptKind::ModuleScript => "ModuleScript",
        ScriptKind::Script => "Script",
        ScriptKind::LocalScript => "LocalScript",
    }
}

/// Normalizes a `$path` filesystem dir to a forward-slashed, trailing-slash-free
/// string. `.` (a mount at the project root) collapses to the empty string so
/// `under_dir` treats every key as directly beneath it.
fn normalize_dir(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let trimmed = s.trim_end_matches('/');
    if trimmed == "." {
        String::new()
    } else {
        trimmed.to_owned()
    }
}

/// Returns `key`'s path relative to the mount dir `dir`, or `None` when `key`
/// is not inside it. An empty `dir` (root mount) returns `key` unchanged; a
/// `key` equal to `dir` returns `""` (a `$path` pointing directly at one file).
/// The trailing-slash guard keeps `src` from matching a sibling `srcextra`.
///
/// The comparison folds ASCII case, matching `ProjectState::is_under_mapping`.
/// Tree keys carry whichever casing produced them — the project file's for the
/// boot scan (`rescan_fs` walks `root.join($path)`), the real directory's for
/// every watcher event — so on Windows / macOS a project declaring `"src"` over
/// a disk folder named `Src/` yields both spellings in one tree. Matching
/// literally dropped every watcher-sourced file on the floor here while sync
/// itself kept working, leaving a sourcemap that silently stopped growing.
/// Only the mount prefix is folded; the returned remainder (and therefore every
/// `filePaths` entry) keeps its on-disk casing so the LSP can open the file.
fn under_dir(key: &str, dir: &str) -> Option<String> {
    if dir.is_empty() {
        return Some(key.to_owned());
    }
    if key.eq_ignore_ascii_case(dir) {
        return Some(String::new());
    }
    // Checking the separator byte first also guarantees `dir.len()` is a char
    // boundary: `/` is ASCII, so it can never be a UTF-8 continuation byte.
    if key.len() > dir.len()
        && key.as_bytes()[dir.len()] == b'/'
        && key[..dir.len()].eq_ignore_ascii_case(dir)
    {
        return Some(key[dir.len() + 1..].to_owned());
    }
    None
}

/// Builds a Rojo-format sourcemap `Value` from the project's `$path` mounts and
/// the daemon's file tree. Root node is the `DataModel` (name = `project.name`,
/// or `"game"` when empty). Each `$path` mapping seeds a service/instance chain;
/// the files under it become the descendant Script/Folder tree.
///
/// `remaps` are the honored nested-package mounts (A10). Tree keys are
/// *instance* paths, which for a Wally package elide the package's own `$path`
/// segment — so the instance shape is built from the key, but each `filePaths`
/// entry is put back through `state::fs_rel_with` first. Pass `&[]` when no
/// package remaps apply (e.g. the syncback materializer, whose tree is keyed
/// straight off the `src/<Service>` mounts it just wrote).
#[must_use]
pub fn build_sourcemap(project: &Project, tree: &Tree, remaps: &[PackageRemap]) -> Value {
    let root_name = if project.name.trim().is_empty() {
        "game"
    } else {
        project.name.as_str()
    };
    let mut root = SourceNode::new(
        project
            .tree
            .class_name
            .clone()
            .unwrap_or_else(|| "DataModel".to_owned()),
    );

    // Normalize every mount dir once; keep the segments alongside so we can walk
    // the instance chain and route files by longest-prefix (Rojo's `bestMapping`).
    let mounts: Vec<(Vec<String>, String)> = project
        .path_mappings()
        .into_iter()
        .map(|(segments, dir)| (segments, normalize_dir(&dir)))
        .collect();

    // Pre-create every instance the project *declares*, not just the ones
    // carrying a `$path`. Rojo builds the whole declared tree — `$path` only
    // says where a node's contents come from — so a pure container
    // (`"Shared": {"$className": "Folder"}`, `"Lighting": {"$properties": …}`,
    // the shapes `rojo init` emits) exists at runtime and has to resolve in the
    // editor too. Seeding only from `path_mappings()` dropped all of them, and
    // also left a mapped-but-empty service missing until its first file landed.
    let mut declared: Vec<Vec<String>> = Vec::new();
    collect_declared(&project.tree, &mut Vec::new(), &mut declared);
    for segments in &declared {
        ensure_segment_chain(&mut root, project, segments);
    }

    // Route each tracked file to its best (longest matching dir) mount, then
    // materialize it into that mount's subtree.
    //
    // On a tie only ONE mount wins, matching the plugin. Rojo would materialize
    // the directory under every mount that names it, and an earlier revision of
    // this function did the same — but `TreeBuilder.luau` keys its mapping table
    // by the `$path` *string* (`mappings[path] = parent`), so two mounts sharing
    // a path collapse to one entry and Yeet only ever creates one of the two
    // instances. Emitting both made the sourcemap advertise an instance that
    // does not exist at runtime, which is worse than the empty service it
    // replaced: luau-lsp would resolve a path that fails in Studio. Making both
    // real is a plugin change; until then the map tells the truth about what
    // Yeet syncs, and `detect_shared_mounts` warns that the other mount is
    // unreachable.
    let mut keys: Vec<&str> = tree.keys().map(String::as_str).collect();
    keys.sort_unstable();
    for key in keys {
        let best = mounts
            .iter()
            .filter_map(|(segments, dir)| under_dir(key, dir).map(|rel| (segments, dir.len(), rel)))
            .max_by_key(|(_, dir_len, _)| *dir_len);
        let Some((segments, _dir_len, rel)) = best else {
            continue;
        };
        let file_name = key.rsplit('/').next().unwrap_or(key);
        let Some((script_name, kind)) = classify(file_name) else {
            continue;
        };
        // luau-lsp opens the `filePaths` string verbatim, so it has to be the
        // real path on disk — undo the package collapse the tree key carries.
        let disk_path = fs_rel_with(remaps, key);
        let mount = ensure_segment_chain(&mut root, project, segments);
        place_file(mount, &rel, &disk_path, &script_name, kind);
    }

    let mut value = root.to_json(root_name);
    if let Some(object) = value.as_object_mut() {
        object.insert(
            GENERATED_BY_KEY.to_owned(),
            Value::String(GENERATED_BY_VALUE.to_owned()),
        );
    }
    value
}

/// Collects the instance-path segments of every child the project tree
/// declares, at any depth — the `$path`-less counterpart to
/// `Project::path_mappings`.
fn collect_declared(
    node: &crate::project::TreeNode,
    parents: &mut Vec<String>,
    out: &mut Vec<Vec<String>>,
) {
    for (name, child) in &node.children {
        parents.push(name.clone());
        out.push(parents.clone());
        collect_declared(child, parents, out);
        parents.pop();
    }
}

/// Every `$path` claimed by more than one mount, as
/// `(path, the instance chains that declare it)`, sorted.
///
/// Rojo materializes such a directory under each mount. Yeet cannot: the
/// plugin's `TreeBuilder` keys its mapping table by the `$path` string, so the
/// mounts collapse to one entry and only one instance is ever created — which
/// one depends on traversal order. Making both real is a plugin change; naming
/// the mounts is what turns "one of my services is mysteriously empty" into a
/// message.
#[must_use]
pub fn detect_shared_mounts(project: &Project) -> Vec<(String, Vec<String>)> {
    let mut by_path: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (segments, dir) in project.path_mappings() {
        let instance = if segments.is_empty() {
            "game".to_owned()
        } else {
            format!("game.{}", segments.join("."))
        };
        by_path
            .entry(normalize_dir(&dir))
            .or_default()
            .push(instance);
    }
    by_path
        .into_iter()
        .filter(|(_, mounts)| mounts.len() > 1)
        .map(|(path, mut mounts)| {
            mounts.sort();
            (path, mounts)
        })
        .collect()
}

/// Every set of tracked files that would collapse onto a single Roblox
/// instance, as `(instance path without extension, the colliding file keys)`.
///
/// `Foo.lua` and `Foo.luau` in one directory both classify as the module `Foo`;
/// so do `Bar.luau` and `Bar.server.luau`. Rojo rejects such a project outright.
/// Yeet instead syncs every file, and since the plugin reuses the instance it
/// already created and overwrites `Source`, whichever apply lands last wins
/// while `tree_base` keeps one entry per *file* — two entries describing one
/// instance, which is a standing desync. Resolving that is a cross-component
/// decision; reporting it is not, and a named pair is the difference between a
/// diagnosable problem and a silent one.
///
/// Results are sorted by instance path, and each file list is sorted, so the
/// warning is stable across runs.
#[must_use]
pub fn detect_name_collisions(tree: &Tree) -> Vec<(String, Vec<String>)> {
    let mut by_instance: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for key in tree.keys() {
        let (dir, file_name) = match key.rsplit_once('/') {
            Some((dir, file)) => (dir, file),
            None => ("", key.as_str()),
        };
        let Some((script_name, _kind)) = classify(file_name) else {
            continue;
        };
        // `init.luau` names its *parent*, so two init files in one directory
        // collide with each other but not with a sibling literally named `init`.
        let instance = if dir.is_empty() {
            script_name
        } else {
            format!("{dir}/{script_name}")
        };
        by_instance.entry(instance).or_default().push(key.clone());
    }
    by_instance
        .into_iter()
        .filter(|(_, files)| files.len() > 1)
        .map(|(instance, mut files)| {
            files.sort();
            (instance, files)
        })
        .collect()
}

/// Walks (creating as needed) the instance chain named by `segments`, returning
/// the deepest node. Each segment's className is resolved the way
/// `TreeBuilder.ensureNode` does: an explicit `$className` from the project tree
/// wins; otherwise a top-level `DataModel` child keeps its own name (services'
/// ClassName equals their name), and a deeper node defaults to `Folder`.
fn ensure_segment_chain<'a>(
    root: &'a mut SourceNode,
    project: &Project,
    segments: &[String],
) -> &'a mut SourceNode {
    let mut node = root;
    let mut proj_node = &project.tree;
    for (depth, seg) in segments.iter().enumerate() {
        let child_proj = proj_node.children.get(seg);
        let class_name = child_proj
            .and_then(|n| n.class_name.clone())
            .unwrap_or_else(|| {
                if depth == 0 {
                    seg.clone()
                } else {
                    "Folder".to_owned()
                }
            });
        node = node
            .children
            .entry(seg.clone())
            .or_insert_with(|| SourceNode::new(class_name));
        if let Some(child) = child_proj {
            proj_node = child;
        }
    }
    node
}

/// Materializes one file into `mount`'s subtree, mirroring
/// `TreeBuilder.materializeFile`. The *instance shape* comes from `rel` (the
/// tree key's path under its mount); `disk_path` is what lands in `filePaths`
/// and is the real on-disk path, which for a Wally package differs from the key.
fn place_file(
    mount: &mut SourceNode,
    rel: &str,
    disk_path: &str,
    script_name: &str,
    kind: ScriptKind,
) {
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        // `$path` points directly at this single file: the mount instance itself
        // carries the source. Keep the mount's own className (a service/Folder
        // can't be re-classed into a script from here — matches TreeBuilder's
        // `materializeAtMount` for a non-script mount).
        mount.file_paths.push(disk_path.to_owned());
        return;
    }

    let file_name = parts[parts.len() - 1];
    if is_init_filename(file_name) {
        if parts.len() == 1 {
            // `init.[ext]` directly under the mount promotes the mount itself.
            mount.file_paths.push(disk_path.to_owned());
            return;
        }
        // Intermediate dirs are Folders; the immediate parent directory is
        // promoted to a script of the init file's kind (classify already
        // resolved that kind for us).
        let container = ensure_chain(mount, &parts[..parts.len() - 2]);
        let promoted = container
            .children
            .entry(parts[parts.len() - 2].to_owned())
            .or_insert_with(SourceNode::folder);
        class_for(kind).clone_into(&mut promoted.class_name);
        promoted.file_paths.push(disk_path.to_owned());
    } else {
        // Plain leaf script: every path component before it is a Folder.
        let container = ensure_chain(mount, &parts[..parts.len() - 1]);
        let leaf = container
            .children
            .entry(script_name.to_owned())
            .or_insert_with(SourceNode::folder);
        class_for(kind).clone_into(&mut leaf.class_name);
        leaf.file_paths.push(disk_path.to_owned());
    }
}

/// Walks (creating Folder nodes as needed) the directory chain `dirs` under
/// `start`, returning the deepest node. Never downgrades a node that a sibling
/// already promoted to a script — `or_insert_with` leaves an existing entry
/// untouched — so file visit order doesn't matter.
fn ensure_chain<'a>(start: &'a mut SourceNode, dirs: &[&str]) -> &'a mut SourceNode {
    let mut node = start;
    for dir in dirs {
        node = node
            .children
            .entry((*dir).to_owned())
            .or_insert_with(SourceNode::folder);
    }
    node
}

/// A cheap, deterministic fingerprint of the tree's *structure* — its set of
/// keys, which is exactly what the sourcemap shape depends on (each key encodes
/// its own instance kind via the filename suffix). The background writer skips a
/// rewrite when this is unchanged, so a content-only edit never regenerates the
/// file.
#[must_use]
pub fn structure_signature(project: &Project, tree: &Tree) -> u64 {
    let mut keys: Vec<&str> = tree.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    keys.hash(&mut hasher);
    // The map is a function of the project too, not just the file set: its
    // name, every declared node, and each `$className` all shape the output.
    // Hashing only the keys meant a project-file reload that added a mount or
    // renamed the DataModel woke the writer and was then skipped as a no-op.
    serde_json::to_string(project)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// Atomically writes a pre-built sourcemap `Value` to `<root>/sourcemap.json`.
/// Split from `write_sourcemap` so the background writer can build the JSON
/// under a read lock and then do the (blocking) disk write after releasing it.
pub fn write_value(root: &Path, value: &Value) -> Result<()> {
    // Re-check ownership on every write, not just at startup: the common way to
    // end up with a foreign map is `rojo sourcemap --watch` started in a second
    // terminal *after* `Yeet: Start`, which the bootstrap check cannot see. This
    // is the single choke point for every writer (bootstrap, the background
    // writer, syncback), so one guard here covers them all.
    if is_foreign(root) {
        tracing::warn!(
            "sourcemap.json is not Yeet's (no `{GENERATED_BY_KEY}: {GENERATED_BY_VALUE}` \
             marker) — not overwriting it"
        );
        return Ok(());
    }
    let mut bytes = serde_json::to_vec_pretty(value).context("serialize sourcemap")?;
    bytes.push(b'\n');
    let path = root.join("sourcemap.json");
    atomic_write(&path, &bytes)
}

/// Builds and atomically writes `<root>/sourcemap.json`. Used by the synchronous
/// callers (daemon bootstrap, syncback materialize); the live writer uses
/// `build_sourcemap` + `write_value` directly to keep the lock hold short.
pub fn write_sourcemap(
    root: &Path,
    project: &Project,
    tree: &Tree,
    remaps: &[PackageRemap],
) -> Result<()> {
    let value = build_sourcemap(project, tree, remaps);
    write_value(root, &value)
}

/// Write to a sibling temp file, fsync, then rename — same durability contract
/// as `main::atomic_write`, duplicated here (like `tree::save_base_tree`) so the
/// generator stays self-contained and callable from the library crate.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = {
        let mut os = path.as_os_str().to_owned();
        os.push(".yeet.tmp");
        PathBuf::from(os)
    };
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::TreeEntry;

    fn entry(kind: ScriptKind) -> TreeEntry {
        TreeEntry {
            kind,
            content: String::new(),
            sha256: String::new(),
        }
    }

    /// Locates a child node by instance name inside a node's `children` array.
    fn child<'a>(node: &'a Value, name: &str) -> Option<&'a Value> {
        node.get("children")?
            .as_array()?
            .iter()
            .find(|c| c.get("name").and_then(Value::as_str) == Some(name))
    }

    fn file_paths(node: &Value) -> Vec<String> {
        node.get("filePaths")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn class_name(node: &Value) -> Option<&str> {
        node.get("className").and_then(Value::as_str)
    }

    #[test]
    fn build_sourcemap_maps_service_files() {
        // A project mounting ServerScriptService → `src`, with a leaf module, a
        // directory promoted by `init.luau`, and that directory's own child.
        let project: Project = serde_json::from_str(
            r#"{
                "name": "Test",
                "tree": {
                    "$className": "DataModel",
                    "ServerScriptService": {
                        "$className": "ServerScriptService",
                        "$path": "src"
                    }
                }
            }"#,
        )
        .expect("parse project");

        let mut tree = Tree::new();
        tree.insert("src/Foo.luau".to_owned(), entry(ScriptKind::ModuleScript));
        tree.insert("src/Bar/init.luau".to_owned(), entry(ScriptKind::ModuleScript));
        tree.insert("src/Bar/Baz.luau".to_owned(), entry(ScriptKind::ModuleScript));

        let map = build_sourcemap(&project, &tree, &[]);

        assert_eq!(map.get("name").and_then(Value::as_str), Some("Test"));
        assert_eq!(class_name(&map), Some("DataModel"));

        let sss = child(&map, "ServerScriptService").expect("service node present");
        assert_eq!(class_name(sss), Some("ServerScriptService"));

        let foo = child(sss, "Foo").expect("Foo module present");
        assert_eq!(class_name(foo), Some("ModuleScript"));
        assert_eq!(file_paths(foo), vec!["src/Foo.luau".to_owned()]);

        let bar = child(sss, "Bar").expect("Bar module present");
        assert_eq!(
            class_name(bar),
            Some("ModuleScript"),
            "a directory with init.luau becomes the ModuleScript, not a Folder"
        );
        assert_eq!(file_paths(bar), vec!["src/Bar/init.luau".to_owned()]);

        let baz = child(bar, "Baz").expect("Baz child present");
        assert_eq!(class_name(baz), Some("ModuleScript"));
        assert_eq!(file_paths(baz), vec!["src/Bar/Baz.luau".to_owned()]);
    }

    #[test]
    fn build_sourcemap_resolves_script_kinds() {
        let project: Project = serde_json::from_str(
            r#"{
                "name": "Kinds",
                "tree": {
                    "$className": "DataModel",
                    "ServerScriptService": { "$path": "src" }
                }
            }"#,
        )
        .expect("parse project");

        let mut tree = Tree::new();
        tree.insert("src/Server.server.luau".to_owned(), entry(ScriptKind::Script));
        tree.insert("src/Client.client.luau".to_owned(), entry(ScriptKind::LocalScript));

        let map = build_sourcemap(&project, &tree, &[]);
        let sss = child(&map, "ServerScriptService").expect("service node present");
        // No `$className` on the mount → the service keeps its own name as class.
        assert_eq!(class_name(sss), Some("ServerScriptService"));
        assert_eq!(class_name(child(sss, "Server").unwrap()), Some("Script"));
        assert_eq!(class_name(child(sss, "Client").unwrap()), Some("LocalScript"));
    }

    /// The instance path a Wally package's sources are keyed by is NOT their
    /// path on disk: `relative` elides the package's own `$path` segment so
    /// `<pkg>/src/init.lua` keys as `<pkg>/init.lua` (A10). `filePaths` is the
    /// one field that must undo that — luau-lsp opens exactly the string we
    /// emit, and no file exists at the collapsed path. Pushing the tree key
    /// verbatim left every package source unresolvable, so `require(Packages.X)`
    /// had no types and go-to-definition dead-ended.
    #[test]
    fn build_sourcemap_file_paths_point_at_real_disk_paths() {
        let project: Project = serde_json::from_str(
            r#"{"name":"T","tree":{"$className":"DataModel","Packages":{"$path":"Packages"}}}"#,
        )
        .expect("parse project");

        // What `ProjectState::rebuild_package_remaps` derives for a package whose
        // nested project is `{"name":"Roact","tree":{"$path":"src"}}` — note the
        // instance prefix also carries the project's `name`, not the dir name.
        let remaps = vec![PackageRemap {
            fs_prefix: "Packages/_Index/roblox_roact@1.4.4/roact/src".to_owned(),
            instance_prefix: "Packages/_Index/roblox_roact@1.4.4/Roact".to_owned(),
        }];

        let mut tree = Tree::new();
        tree.insert(
            "Packages/_Index/roblox_roact@1.4.4/Roact/init.lua".to_owned(),
            entry(ScriptKind::ModuleScript),
        );
        tree.insert(
            "Packages/_Index/roblox_roact@1.4.4/Roact/Binding.lua".to_owned(),
            entry(ScriptKind::ModuleScript),
        );
        // A plain top-level file must round-trip untouched.
        tree.insert("Packages/Roact.lua".to_owned(), entry(ScriptKind::ModuleScript));

        let map = build_sourcemap(&project, &tree, &remaps);
        let packages = child(&map, "Packages").expect("Packages mount");
        let index = child(packages, "_Index").expect("_Index folder");
        let versioned = child(index, "roblox_roact@1.4.4").expect("versioned folder");
        let roact = child(versioned, "Roact").expect("package module");

        assert_eq!(
            class_name(roact),
            Some("ModuleScript"),
            "instance shape must stay collapsed — only filePaths are disk-shaped"
        );
        assert_eq!(
            file_paths(roact),
            vec!["Packages/_Index/roblox_roact@1.4.4/roact/src/init.lua".to_owned()],
            "the package module must point at the real file, `src/` segment intact"
        );
        assert_eq!(
            file_paths(child(roact, "Binding").expect("Binding child")),
            vec!["Packages/_Index/roblox_roact@1.4.4/roact/src/Binding.lua".to_owned()],
        );
        assert_eq!(
            file_paths(child(packages, "Roact").expect("link module")),
            vec!["Packages/Roact.lua".to_owned()],
            "a non-package key must be emitted verbatim"
        );
    }

    #[test]
    fn build_sourcemap_collapses_wally_package() {
        // A Wally package mount: the A10-collapsed key drops the package's own
        // `src/` segment, so `.../foo/init.lua` must land as a ModuleScript named
        // `foo`, with the intermediate `_Index` / `foo@1.0.0` dirs as Folders —
        // NOT a Folder `foo` with a stray `src` child.
        let project: Project = serde_json::from_str(
            r#"{
                "name": "T",
                "tree": {
                    "$className": "DataModel",
                    "Packages": { "$path": "Packages" }
                }
            }"#,
        )
        .expect("parse project");

        let mut tree = Tree::new();
        tree.insert(
            "Packages/_Index/foo@1.0.0/foo/init.lua".to_owned(),
            entry(ScriptKind::ModuleScript),
        );

        let map = build_sourcemap(&project, &tree, &[]);
        let packages = child(&map, "Packages").expect("Packages mount present");
        let index = child(packages, "_Index").expect("_Index folder present");
        assert_eq!(class_name(index), Some("Folder"));
        let versioned = child(index, "foo@1.0.0").expect("versioned folder present");
        assert_eq!(class_name(versioned), Some("Folder"));
        let foo = child(versioned, "foo").expect("foo package present");
        assert_eq!(
            class_name(foo),
            Some("ModuleScript"),
            "collapsed package init.lua must be a ModuleScript, not a Folder with src"
        );
        assert!(
            child(foo, "src").is_none(),
            "the src segment must not appear as a child instance"
        );
    }

    #[test]
    fn build_sourcemap_seeds_empty_mount() {
        // A mapped service with no files still needs a node so the LSP resolves
        // `game.ReplicatedStorage` immediately after `Yeet: Create`.
        let project: Project = serde_json::from_str(
            r#"{
                "name": "Empty",
                "tree": {
                    "$className": "DataModel",
                    "ReplicatedStorage": {
                        "$className": "ReplicatedStorage",
                        "$path": "src/ReplicatedStorage"
                    }
                }
            }"#,
        )
        .expect("parse project");

        let map = build_sourcemap(&project, &Tree::new(), &[]);
        let rs = child(&map, "ReplicatedStorage").expect("service node present even with no files");
        assert_eq!(class_name(rs), Some("ReplicatedStorage"));
        assert_eq!(
            rs.get("children").and_then(Value::as_array).map(Vec::len),
            Some(0)
        );
    }

    /// Tree keys carry whichever casing produced them: the project file's for
    /// the boot scan (`rescan_fs` walks `root.join($path)`), the real disk's for
    /// every watcher event. On Windows / macOS those differ whenever the folder
    /// on disk is `Src/` and the project declares `"src"` — a rename in Explorer
    /// or a branch checkout is enough. The rest of the daemon already folds case
    /// (`protocol::classify`, `ProjectState::is_under_mapping`); routing here did
    /// not, so every watcher-created file was silently dropped from the map while
    /// sync itself kept working.
    #[test]
    fn build_sourcemap_routes_case_insensitively() {
        let project: Project = serde_json::from_str(
            r#"{"name":"C","tree":{"$className":"DataModel","ReplicatedStorage":{"$path":"src/shared"}}}"#,
        )
        .expect("parse project");

        let mut tree = Tree::new();
        // Boot-scan casing (from the project file) …
        tree.insert(
            "src/shared/Boot.luau".to_owned(),
            entry(ScriptKind::ModuleScript),
        );
        // … and watcher casing (from the real `Src\` directory on disk).
        tree.insert(
            "Src/shared/Watched.luau".to_owned(),
            entry(ScriptKind::ModuleScript),
        );

        let map = build_sourcemap(&project, &tree, &[]);
        let rs = child(&map, "ReplicatedStorage").expect("service node present");
        assert!(child(rs, "Boot").is_some(), "boot-scan file must be mapped");
        let watched = child(rs, "Watched").expect("watcher-cased file must be mapped too");
        assert_eq!(
            file_paths(watched),
            vec!["Src/shared/Watched.luau".to_owned()],
            "filePaths must keep the on-disk casing so the LSP can open the file"
        );
    }

    /// Two mounts sharing one `$path` is legal Rojo, and Rojo puts the files
    /// under both. Yeet cannot: `TreeBuilder.luau` keys its mapping table by the
    /// `$path` string, so the mounts collapse and only one instance is created.
    /// The map therefore describes ONE of them — advertising both would resolve
    /// in the editor and fail at runtime — and the conflict is reported instead.
    #[test]
    fn shared_path_mounts_are_reported_not_duplicated() {
        let project: Project = serde_json::from_str(
            r#"{
                "name": "Shared",
                "tree": {
                    "$className": "DataModel",
                    "ReplicatedStorage": { "$path": "src/shared" },
                    "StarterPlayer": {
                        "StarterPlayerScripts": { "$className": "StarterPlayerScripts", "$path": "src/shared" }
                    }
                }
            }"#,
        )
        .expect("parse project");

        let mut tree = Tree::new();
        tree.insert(
            "src/shared/Config.luau".to_owned(),
            entry(ScriptKind::ModuleScript),
        );

        let map = build_sourcemap(&project, &tree, &[]);
        let rs = child(&map, "ReplicatedStorage").expect("ReplicatedStorage present");
        let sps = child(
            child(&map, "StarterPlayer").expect("StarterPlayer present"),
            "StarterPlayerScripts",
        )
        .expect("StarterPlayerScripts present");
        let placed = usize::from(child(rs, "Config").is_some())
            + usize::from(child(sps, "Config").is_some());
        assert_eq!(
            placed, 1,
            "exactly one mount may claim the file — the map must not promise an \
             instance the plugin never creates"
        );

        assert_eq!(
            detect_shared_mounts(&project),
            vec![(
                "src/shared".to_owned(),
                vec![
                    "game.ReplicatedStorage".to_owned(),
                    "game.StarterPlayer.StarterPlayerScripts".to_owned()
                ]
            )],
            "the unreachable mount must be named so the empty service is not a mystery"
        );
    }

    #[test]
    fn detect_shared_mounts_ignores_distinct_paths() {
        let project: Project = serde_json::from_str(
            r#"{"name":"D","tree":{"$className":"DataModel","A":{"$path":"src/a"},"B":{"$path":"src/b"}}}"#,
        )
        .expect("parse project");
        assert!(detect_shared_mounts(&project).is_empty());
    }

    /// A longer mount must still win outright — routing to *all* tied mounts
    /// must not degrade into routing to every mount that merely contains the
    /// file. `src/shared/ui` is more specific than `src/shared`.
    #[test]
    fn build_sourcemap_still_prefers_the_longest_mount() {
        let project: Project = serde_json::from_str(
            r#"{
                "name": "Nested",
                "tree": {
                    "$className": "DataModel",
                    "ReplicatedStorage": { "$path": "src/shared" },
                    "StarterGui": { "$className": "StarterGui", "$path": "src/shared/ui" }
                }
            }"#,
        )
        .expect("parse project");

        let mut tree = Tree::new();
        tree.insert(
            "src/shared/ui/Button.luau".to_owned(),
            entry(ScriptKind::ModuleScript),
        );

        let map = build_sourcemap(&project, &tree, &[]);
        let gui = child(&map, "StarterGui").expect("StarterGui present");
        assert!(
            child(gui, "Button").is_some(),
            "the more specific mount must own the file"
        );
        let rs = child(&map, "ReplicatedStorage").expect("ReplicatedStorage present");
        assert!(
            child(rs, "ui").is_none() && child(rs, "Button").is_none(),
            "the shorter mount must not also claim it"
        );
    }

    /// Rojo creates every instance the project tree declares; `$path` only says
    /// where its *contents* come from. Seeding the sourcemap exclusively from
    /// `path_mappings()` therefore dropped every pure-container node — the
    /// `"Shared": {"$className": "Folder"}` and `"Lighting": {"$properties": …}`
    /// shapes `rojo init` emits — so `game.ReplicatedStorage.Shared` existed at
    /// runtime but would not resolve in the editor.
    #[test]
    fn build_sourcemap_seeds_declared_nodes_without_path() {
        let project: Project = serde_json::from_str(
            r#"{
                "name": "Declared",
                "tree": {
                    "$className": "DataModel",
                    "Lighting": { "$properties": { "Brightness": 2 } },
                    "ReplicatedStorage": {
                        "$className": "ReplicatedStorage",
                        "Shared": { "$className": "Folder" },
                        "Mounted": { "$path": "src/shared" }
                    }
                }
            }"#,
        )
        .expect("parse project");

        let map = build_sourcemap(&project, &Tree::new(), &[]);
        assert_eq!(
            class_name(child(&map, "Lighting").expect("Lighting declared")),
            Some("Lighting"),
            "a top-level declared child keeps its name as className, like a service"
        );
        let rs = child(&map, "ReplicatedStorage").expect("ReplicatedStorage present");
        assert_eq!(
            class_name(child(rs, "Shared").expect("Shared declared")),
            Some("Folder"),
            "a nested declared child honors its explicit $className"
        );
        assert!(
            child(rs, "Mounted").is_some(),
            "declared-node seeding must not lose the $path mounts"
        );
    }

    /// `under_dir`'s trailing-slash guard has to survive the case fold: `src`
    /// must not swallow a sibling `srcextra`.
    #[test]
    fn under_dir_case_folds_without_matching_siblings() {
        assert_eq!(under_dir("Src/Foo.luau", "src"), Some("Foo.luau".to_owned()));
        assert_eq!(under_dir("SRC", "src"), Some(String::new()));
        assert_eq!(under_dir("srcextra/Foo.luau", "src"), None);
        assert_eq!(under_dir("other/Foo.luau", "src"), None);
    }

    /// `Foo.lua` and `Foo.luau` side by side both classify as the module `Foo`,
    /// so they collapse onto one instance — Rojo rejects the project outright,
    /// while Yeet syncs both and lets whichever ACK lands last win. The full fix
    /// is a cross-component decision (the plugin overwrites `Source` on the
    /// second apply and `tree_base` ends up holding two entries for one
    /// instance); until then the daemon must at least name the pair so the
    /// resulting desync is diagnosable instead of silent.
    #[test]
    fn detects_instance_name_collisions() {
        let mut tree = Tree::new();
        tree.insert("src/shared/Foo.lua".to_owned(), entry(ScriptKind::ModuleScript));
        tree.insert("src/shared/Foo.luau".to_owned(), entry(ScriptKind::ModuleScript));
        // Same name, different directory — not a collision.
        tree.insert("src/other/Foo.luau".to_owned(), entry(ScriptKind::ModuleScript));
        // Same stem, different kind: `Bar` vs `Bar` as Script — still one
        // instance name in one folder, so still a collision.
        tree.insert("src/shared/Bar.luau".to_owned(), entry(ScriptKind::ModuleScript));
        tree.insert(
            "src/shared/Bar.server.luau".to_owned(),
            entry(ScriptKind::Script),
        );

        let collisions = detect_name_collisions(&tree);
        assert_eq!(
            collisions,
            vec![
                (
                    "src/shared/Bar".to_owned(),
                    vec![
                        "src/shared/Bar.luau".to_owned(),
                        "src/shared/Bar.server.luau".to_owned()
                    ]
                ),
                (
                    "src/shared/Foo".to_owned(),
                    vec![
                        "src/shared/Foo.lua".to_owned(),
                        "src/shared/Foo.luau".to_owned()
                    ]
                ),
            ],
            "every set of files resolving to one instance name must be reported, sorted"
        );
    }

    #[test]
    fn detects_no_collisions_in_an_ordinary_tree() {
        let mut tree = Tree::new();
        tree.insert("src/shared/Foo.luau".to_owned(), entry(ScriptKind::ModuleScript));
        tree.insert("src/shared/Bar.luau".to_owned(), entry(ScriptKind::ModuleScript));
        tree.insert("src/shared/Sub/Foo.luau".to_owned(), entry(ScriptKind::ModuleScript));
        assert!(detect_name_collisions(&tree).is_empty());
    }

    #[test]
    fn structure_signature_ignores_content_changes() {
        let proj: Project =
            serde_json::from_str(r#"{"name":"S","tree":{"$className":"DataModel"}}"#)
                .expect("parse project");
        let mut a = Tree::new();
        a.insert("src/Foo.luau".to_owned(), entry(ScriptKind::ModuleScript));
        let mut b = Tree::new();
        b.insert(
            "src/Foo.luau".to_owned(),
            TreeEntry {
                kind: ScriptKind::ModuleScript,
                content: "return 1\n".to_owned(),
                sha256: "different".to_owned(),
            },
        );
        assert_eq!(
            structure_signature(&proj, &a),
            structure_signature(&proj, &b),
            "same key set ⇒ same signature regardless of content"
        );

        let mut c = Tree::new();
        c.insert("src/Bar.luau".to_owned(), entry(ScriptKind::ModuleScript));
        assert_ne!(
            structure_signature(&proj, &a),
            structure_signature(&proj, &c),
            "a different key set must change the signature"
        );

        // The map is a function of the project too: a reload that renames the
        // DataModel or declares a node must not be skipped as a no-op.
        let renamed: Project =
            serde_json::from_str(r#"{"name":"RENAMED","tree":{"$className":"DataModel"}}"#)
                .expect("parse project");
        assert_ne!(
            structure_signature(&proj, &a),
            structure_signature(&renamed, &a),
            "a project change must change the signature even with an identical tree"
        );
        let declared: Project = serde_json::from_str(
            r#"{"name":"S","tree":{"$className":"DataModel","Shared":{"$className":"Folder"}}}"#,
        )
        .expect("parse project");
        assert_ne!(
            structure_signature(&proj, &a),
            structure_signature(&declared, &a),
            "a newly declared node must change the signature"
        );
    }

    /// The root node carries an ownership marker so a later run can tell a map
    /// Yeet wrote from one the user maintains (`rojo sourcemap --watch` is the
    /// common case). It goes on the root only — every child would be noise, and
    /// luau-lsp ignores keys it does not know.
    #[test]
    fn build_sourcemap_marks_the_root_as_yeet_generated() {
        let project: Project = serde_json::from_str(
            r#"{"name":"M","tree":{"$className":"DataModel","ReplicatedStorage":{"$path":"src"}}}"#,
        )
        .expect("parse project");
        let mut tree = Tree::new();
        tree.insert("src/Foo.luau".to_owned(), entry(ScriptKind::ModuleScript));

        let map = build_sourcemap(&project, &tree, &[]);
        assert_eq!(
            map.get(GENERATED_BY_KEY).and_then(Value::as_str),
            Some(GENERATED_BY_VALUE)
        );
        let rs = child(&map, "ReplicatedStorage").expect("service present");
        assert!(
            rs.get(GENERATED_BY_KEY).is_none(),
            "only the root carries the marker"
        );
        // The Rojo-defined fields must be untouched by the addition.
        assert_eq!(map.get("name").and_then(Value::as_str), Some("M"));
        assert_eq!(class_name(&map), Some("DataModel"));
    }

    #[test]
    fn is_foreign_only_flags_a_map_yeet_did_not_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let path = root.join("sourcemap.json");

        assert!(
            !is_foreign(root),
            "no file at all is not foreign — there is nothing to protect"
        );

        let foreign = r#"{"name":"X","className":"DataModel","filePaths":[],"children":[]}"#;
        std::fs::write(&path, foreign).expect("write foreign");
        assert!(is_foreign(root), "a map without the marker belongs to the user");

        // The write path re-checks ownership, so a foreign map survives even a
        // direct `write_sourcemap` — the guard is not just a startup decision.
        let project: Project =
            serde_json::from_str(r#"{"name":"X","tree":{"$className":"DataModel"}}"#)
                .expect("parse project");
        write_sourcemap(root, &project, &Tree::new(), &[]).expect("write is a no-op, not an error");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            foreign,
            "a foreign map must survive a write attempt byte-for-byte"
        );

        std::fs::remove_file(&path).expect("clear foreign");
        write_sourcemap(root, &project, &Tree::new(), &[]).expect("write ours");
        assert!(
            !is_foreign(root),
            "a map we just wrote must be recognized as ours"
        );

        std::fs::write(&path, "{ not json").expect("write garbage");
        assert!(
            is_foreign(root),
            "an unparseable file is treated as the user's — never clobber what we cannot read"
        );
    }

    #[test]
    fn write_sourcemap_emits_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project: Project = serde_json::from_str(
            r#"{ "name": "W", "tree": { "$className": "DataModel", "ServerScriptService": { "$path": "src" } } }"#,
        )
        .expect("parse project");
        let mut tree = Tree::new();
        tree.insert("src/Foo.luau".to_owned(), entry(ScriptKind::ModuleScript));
        write_sourcemap(dir.path(), &project, &tree, &[]).expect("write sourcemap");
        let written = std::fs::read_to_string(dir.path().join("sourcemap.json")).expect("read back");
        let parsed: Value = serde_json::from_str(&written).expect("valid json");
        assert_eq!(class_name(&parsed), Some("DataModel"));
        assert!(
            !dir.path().join("sourcemap.json.yeet.tmp").exists(),
            "temp file must not linger"
        );
    }
}
