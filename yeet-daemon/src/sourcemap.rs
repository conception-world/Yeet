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
//! The generator is pure over `(project, tree)`; `filePaths` therefore carry the
//! tree key verbatim. For an ordinary project the tree key equals the on-disk
//! path, so the LSP opens the right file. For the internals of an A10-collapsed
//! Wally package the key is the collapsed instance path (the real file keeps its
//! `src/` segment) — the user-facing `require(Packages.<name>)` link module is a
//! real top-level file and is unaffected.

use std::collections::BTreeMap;
use std::hash::{Hash as _, Hasher as _};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::project::Project;
use crate::protocol::{ScriptKind, classify, is_init_filename};
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
fn under_dir(key: &str, dir: &str) -> Option<String> {
    if dir.is_empty() {
        return Some(key.to_owned());
    }
    if key == dir {
        return Some(String::new());
    }
    let prefix = format!("{dir}/");
    key.strip_prefix(&prefix).map(str::to_owned)
}

/// Builds a Rojo-format sourcemap `Value` from the project's `$path` mounts and
/// the daemon's file tree. Root node is the `DataModel` (name = `project.name`,
/// or `"game"` when empty). Each `$path` mapping seeds a service/instance chain;
/// the files under it become the descendant Script/Folder tree.
#[must_use]
pub fn build_sourcemap(project: &Project, tree: &Tree) -> Value {
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

    // Pre-create every mount's instance chain so a service with no files yet
    // still appears — the LSP needs the node present to resolve `game.<Service>`.
    for (segments, _dir) in &mounts {
        ensure_segment_chain(&mut root, project, segments);
    }

    // Route each tracked file to its best (longest matching dir) mount, then
    // materialize it into that mount's subtree.
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
        let mount = ensure_segment_chain(&mut root, project, segments);
        place_file(mount, &rel, key, &script_name, kind);
    }

    root.to_json(root_name)
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

/// Materializes one file (relative path `rel` under its mount, full tree key
/// `full_key`) into `mount`'s subtree, mirroring `TreeBuilder.materializeFile`.
fn place_file(
    mount: &mut SourceNode,
    rel: &str,
    full_key: &str,
    script_name: &str,
    kind: ScriptKind,
) {
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        // `$path` points directly at this single file: the mount instance itself
        // carries the source. Keep the mount's own className (a service/Folder
        // can't be re-classed into a script from here — matches TreeBuilder's
        // `materializeAtMount` for a non-script mount).
        mount.file_paths.push(full_key.to_owned());
        return;
    }

    let file_name = parts[parts.len() - 1];
    if is_init_filename(file_name) {
        if parts.len() == 1 {
            // `init.[ext]` directly under the mount promotes the mount itself.
            mount.file_paths.push(full_key.to_owned());
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
        promoted.file_paths.push(full_key.to_owned());
    } else {
        // Plain leaf script: every path component before it is a Folder.
        let container = ensure_chain(mount, &parts[..parts.len() - 1]);
        let leaf = container
            .children
            .entry(script_name.to_owned())
            .or_insert_with(SourceNode::folder);
        class_for(kind).clone_into(&mut leaf.class_name);
        leaf.file_paths.push(full_key.to_owned());
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
pub fn structure_signature(tree: &Tree) -> u64 {
    let mut keys: Vec<&str> = tree.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    keys.hash(&mut hasher);
    hasher.finish()
}

/// Atomically writes a pre-built sourcemap `Value` to `<root>/sourcemap.json`.
/// Split from `write_sourcemap` so the background writer can build the JSON
/// under a read lock and then do the (blocking) disk write after releasing it.
pub fn write_value(root: &Path, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).context("serialize sourcemap")?;
    bytes.push(b'\n');
    let path = root.join("sourcemap.json");
    atomic_write(&path, &bytes)
}

/// Builds and atomically writes `<root>/sourcemap.json`. Used by the synchronous
/// callers (daemon bootstrap, syncback materialize); the live writer uses
/// `build_sourcemap` + `write_value` directly to keep the lock hold short.
pub fn write_sourcemap(root: &Path, project: &Project, tree: &Tree) -> Result<()> {
    let value = build_sourcemap(project, tree);
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

        let map = build_sourcemap(&project, &tree);

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

        let map = build_sourcemap(&project, &tree);
        let sss = child(&map, "ServerScriptService").expect("service node present");
        // No `$className` on the mount → the service keeps its own name as class.
        assert_eq!(class_name(sss), Some("ServerScriptService"));
        assert_eq!(class_name(child(sss, "Server").unwrap()), Some("Script"));
        assert_eq!(class_name(child(sss, "Client").unwrap()), Some("LocalScript"));
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

        let map = build_sourcemap(&project, &tree);
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

        let map = build_sourcemap(&project, &Tree::new());
        let rs = child(&map, "ReplicatedStorage").expect("service node present even with no files");
        assert_eq!(class_name(rs), Some("ReplicatedStorage"));
        assert_eq!(
            rs.get("children").and_then(Value::as_array).map(Vec::len),
            Some(0)
        );
    }

    #[test]
    fn structure_signature_ignores_content_changes() {
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
            structure_signature(&a),
            structure_signature(&b),
            "same key set ⇒ same signature regardless of content"
        );

        let mut c = Tree::new();
        c.insert("src/Bar.luau".to_owned(), entry(ScriptKind::ModuleScript));
        assert_ne!(
            structure_signature(&a),
            structure_signature(&c),
            "a different key set must change the signature"
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
        write_sourcemap(dir.path(), &project, &tree).expect("write sourcemap");
        let written = std::fs::read_to_string(dir.path().join("sourcemap.json")).expect("read back");
        let parsed: Value = serde_json::from_str(&written).expect("valid json");
        assert_eq!(class_name(&parsed), Some("DataModel"));
        assert!(
            !dir.path().join("sourcemap.json.yeet.tmp").exists(),
            "temp file must not linger"
        );
    }
}
