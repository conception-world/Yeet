use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::protocol::ScriptKind;

/// A single tracked file as seen by one of the three trees. `content` is the
/// canonical form (LF, no BOM); `sha256` is the hex digest of that content.
/// We store content (not just hashes) because the conflict UI needs the full
/// base text when the daemon has been restarted and the on-disk file no
/// longer represents what `Tree_Base` remembers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TreeEntry {
    pub kind: ScriptKind,
    pub content: String,
    pub sha256: String,
}

/// Path → entry map. Paths are project-relative, forward-slashed, matching
/// the keys used by `ProjectState::files` historically.
pub type Tree = HashMap<String, TreeEntry>;

const BASE_TREE_FILE: &str = ".yeet/base-tree.msgpack";

/// Returns the absolute path to `<project_root>/.yeet/base-tree.msgpack`.
fn base_tree_path(project_root: &Path) -> PathBuf {
    project_root.join(BASE_TREE_FILE)
}

/// Persists `tree` to disk as `MessagePack`. Writes to a sibling `.tmp` first
/// and renames — same atomicity guarantee as the source-file writer in
/// `main::atomic_write`, duplicated here so this module stays self-contained.
pub fn save_base_tree(project_root: &Path, tree: &Tree) -> Result<()> {
    let path = base_tree_path(project_root);
    let dir = path
        .parent()
        .expect(".yeet directory path must have a parent");
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let bytes = rmp_serde::to_vec_named(tree).context("encode base tree")?;
    let tmp = {
        let mut os = path.as_os_str().to_owned();
        os.push(".tmp");
        PathBuf::from(os)
    };
    std::fs::write(&tmp, &bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Removes the persisted base tree from disk if it exists. Used by the
/// `--reset-base-tree` CLI flag when the user wants the daemon to forget
/// every prior merge decision and re-derive the base from a fresh
/// handshake. Best-effort: missing file is not an error.
pub fn delete_base_tree(project_root: &Path) -> Result<()> {
    let path = base_tree_path(project_root);
    if !path.exists() {
        return Ok(());
    }
    std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))
}

/// Returns `Ok(None)` if there is no stored base tree yet (first boot).
pub fn load_base_tree(project_root: &Path) -> Result<Option<Tree>> {
    let path = base_tree_path(project_root);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let tree: Tree = rmp_serde::from_slice(&bytes).context("decode base tree")?;
    Ok(Some(tree))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn base_tree_roundtrip() {
        let dir = tempdir().unwrap();
        let mut tree: Tree = HashMap::new();
        tree.insert(
            "src/Foo.luau".to_owned(),
            TreeEntry {
                kind: ScriptKind::ModuleScript,
                content: "return {}\n".to_owned(),
                sha256: "deadbeef".to_owned(),
            },
        );
        save_base_tree(dir.path(), &tree).unwrap();
        let loaded = load_base_tree(dir.path()).unwrap().expect("tree present");
        assert_eq!(loaded, tree);
    }

    #[test]
    fn base_tree_missing_returns_none() {
        let dir = tempdir().unwrap();
        assert!(load_base_tree(dir.path()).unwrap().is_none());
    }
}
