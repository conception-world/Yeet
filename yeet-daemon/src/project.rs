use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Rojo-compatible project file (subset sufficient for Phase 1).
///
/// Only the top-level shape and the `$path` keys of direct children are honored.
/// `$properties`, `$className` overrides, `$ignoreUnknownInstances`, nested `$path`,
/// and `init.luau` / `.meta.json` conventions are deliberately out of scope here —
/// the plugin's `TreeBuilder` shares this constraint.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Project {
    pub name: String,
    pub tree: TreeNode,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TreeNode {
    #[serde(rename = "$className", skip_serializing_if = "Option::is_none")]
    pub class_name: Option<String>,

    #[serde(rename = "$path", skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    #[serde(rename = "$properties", default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub properties: serde_json::Map<String, serde_json::Value>,

    #[serde(
        rename = "$ignoreUnknownInstances",
        skip_serializing_if = "Option::is_none"
    )]
    pub ignore_unknown_instances: Option<bool>,

    /// Any non-`$`-prefixed key becomes a child instance by name.
    #[serde(flatten)]
    pub children: BTreeMap<String, TreeNode>,
}

/// The `default.project.json` filename Rojo, Argon, and Wally all use. Kept
/// here so the nested-package walk in `state.rs` doesn't have to reach into
/// `main.rs` for the same literal.
pub const PROJECT_FILE_NAME: &str = "default.project.json";

/// A nested package project — the `{ "name": …, "tree": { "$path": "src" } }`
/// shape Wally writes inside every package folder (e.g.
/// `Packages/_Index/roblox_roact@1.4.4/roact/default.project.json`). Honoring
/// it is what lets `<pkg>/src/init.lua` mount AS the package `ModuleScript`
/// (named `name`) instead of leaving `src` as the module and the package a
/// bare `Folder` — the runtime breakage in AUDITORIA-YEET.md A10 (`wally-1`).
#[derive(Debug, Clone)]
pub struct NestedPackage {
    /// Instance name the package folder takes (the project's `name`).
    pub name: String,
    /// The single `$path`, relative to the package directory (typically `src`).
    pub src: String,
}

impl Project {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?;
        let project: Self = serde_json::from_str(&raw)
            .with_context(|| format!("parse {}", path.display()))?;
        Ok(project)
    }

    /// Parses a `default.project.json` found *inside* a package directory and
    /// returns the nested-package mount iff it matches the simple
    /// `{ name, tree: { $path } }` shape with no extra child instances and no
    /// `$className`. Anything richer (a full multi-node Rojo project) is left
    /// unhandled — we return `None` and the caller keeps the plain
    /// directory-structure mapping, preserving existing behavior. Returns
    /// `None` on read/parse error or a degenerate/unsafe `$path`.
    pub fn load_nested_package(path: &Path) -> Option<NestedPackage> {
        let raw = std::fs::read_to_string(path).ok()?;
        let project: Self = serde_json::from_str(&raw).ok()?;
        let src = project.tree.path.clone()?;
        // Only the minimal Wally shape: a lone `$path`, no sub-instances and no
        // class override to reconcile.
        if !project.tree.children.is_empty() || project.tree.class_name.is_some() {
            return None;
        }
        let src = src.replace('\\', "/");
        if src.is_empty() || src == "." || src.starts_with('/') || src.split('/').any(|s| s == "..")
        {
            return None;
        }
        Some(NestedPackage {
            name: project.name,
            src,
        })
    }

    /// Returns every `(instance_path_segments, filesystem_path)` pair produced by
    /// `$path` entries in the tree. The segments describe the target instance
    /// chain starting below the `DataModel` (e.g. `["ServerScriptService"]` for
    /// a top-level mapping).
    pub fn path_mappings(&self) -> Vec<(Vec<String>, PathBuf)> {
        let mut out = Vec::new();
        collect(&self.tree, &mut Vec::new(), &mut out);
        out
    }
}

fn collect(
    node: &TreeNode,
    parents: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, PathBuf)>,
) {
    if let Some(p) = &node.path {
        out.push((parents.clone(), PathBuf::from(p)));
    }
    for (name, child) in &node.children {
        parents.push(name.clone());
        collect(child, parents, out);
        parents.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_rojo_project() {
        let raw = r#"{
            "name": "Test",
            "tree": {
                "$className": "DataModel",
                "ServerScriptService": { "$path": "src/server" },
                "ReplicatedStorage": { "$path": "src/shared" }
            }
        }"#;
        let project: Project = serde_json::from_str(raw).expect("parse");
        assert_eq!(project.name, "Test");
        assert_eq!(project.tree.class_name.as_deref(), Some("DataModel"));
        assert_eq!(project.tree.children.len(), 2);

        let mappings = project.path_mappings();
        assert!(mappings.contains(&(
            vec!["ServerScriptService".to_owned()],
            PathBuf::from("src/server")
        )));
    }
}
