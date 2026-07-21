use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

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

#[derive(Debug, Clone, Serialize, Default)]
pub struct TreeNode {
    #[serde(rename = "$className", skip_serializing_if = "Option::is_none")]
    pub class_name: Option<String>,

    #[serde(rename = "$path", skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    #[serde(rename = "$properties", default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub properties: serde_json::Map<String, Value>,

    #[serde(
        rename = "$ignoreUnknownInstances",
        skip_serializing_if = "Option::is_none"
    )]
    pub ignore_unknown_instances: Option<bool>,

    /// Any non-`$`-prefixed key becomes a child instance by name.
    #[serde(flatten)]
    pub children: BTreeMap<String, TreeNode>,
}

/// Hand-written so unmodelled `$`-prefixed keys are *skipped* rather than
/// captured. The derived impl used `#[serde(flatten)]` for `children`, and serde
/// applies no name filter to a flatten target: a project carrying any Rojo key
/// Yeet does not model (`$attributes`, `$tags`, `$id`, `$keepUnknowns`, …) was
/// deserialized with that key as a *child instance*. Two failure modes followed
/// from the same line — a scalar or array value failed the whole load
/// (`invalid type: sequence, expected struct TreeNode`, and `Project::load`'s
/// error propagates out of `main` before the daemon ever binds), while an
/// all-object `$attributes` parsed *successfully* and injected a phantom
/// instance literally named `$attributes` into the tree and the sourcemap.
///
/// Malformed values for the four keys Yeet *does* model still error — silently
/// ignoring a typo'd `$className` would be worse than refusing to start.
impl<'de> Deserialize<'de> for TreeNode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::from_value(value).map_err(serde::de::Error::custom)
    }
}

impl TreeNode {
    fn from_value(value: Value) -> std::result::Result<Self, String> {
        let Value::Object(map) = value else {
            return Err(format!("project tree node must be an object, got `{value}`"));
        };
        let mut node = Self::default();
        for (key, value) in map {
            match key.as_str() {
                "$className" => {
                    node.class_name = Some(
                        value
                            .as_str()
                            .ok_or_else(|| format!("$className must be a string, got `{value}`"))?
                            .to_owned(),
                    );
                }
                "$path" => node.path = Some(parse_path(&value)?),
                "$properties" => {
                    let Value::Object(properties) = value else {
                        return Err("$properties must be an object".to_owned());
                    };
                    node.properties = properties;
                }
                "$ignoreUnknownInstances" => {
                    node.ignore_unknown_instances = Some(value.as_bool().ok_or_else(|| {
                        format!("$ignoreUnknownInstances must be a boolean, got `{value}`")
                    })?);
                }
                _ if key.starts_with('$') => {
                    tracing::debug!(key = %key, "ignoring unsupported Rojo project key");
                }
                _ => {
                    let child = Self::from_value(value)
                        .map_err(|e| format!("in child instance `{key}`: {e}"))?;
                    node.children.insert(key, child);
                }
            }
        }
        Ok(node)
    }
}

/// Rojo accepts `$path` either as a plain string or as an object
/// (`{"optional": "src"}`) that suppresses its "path does not exist" error.
/// Both name the same directory, so both resolve to the same `String` here —
/// a missing directory is already handled downstream (`rescan_fs` warns and
/// skips, `state.rs`).
fn parse_path(value: &Value) -> std::result::Result<String, String> {
    let path = match value {
        Value::String(path) => path.clone(),
        Value::Object(map) => map
            .get("optional")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "$path object form must carry a string `optional` key".to_owned())?,
        other => {
            return Err(format!(
                "$path must be a string or an object, got `{other}`"
            ));
        }
    };
    validate_path(&path)?;
    Ok(path)
}

/// Rejects a `$path` Yeet cannot honor. Both shapes below are accepted by the
/// old parser and then misbehave silently, so refusing them up front is the
/// only way the user finds out:
///
///   * escaping the project root (`../shared`) genuinely reads outside it —
///     `ProjectState::rel_raw` strips the root lexically and `Path::components`
///     preserves `ParentDir`, so those files land in `tree_fs` and are
///     broadcast to Studio. Yeet cannot sync them properly either: the watcher
///     observes only the root, so nothing out there is seen changing.
///     `load_nested_package` has rejected this shape since A10.
///   * an absolute path makes `root.join(...)` discard the root, and the
///     following `strip_prefix` then fails for every file — an empty mount that
///     looks exactly like an empty project.
fn validate_path(path: &str) -> std::result::Result<(), String> {
    let normalized = path.replace('\\', "/");
    if normalized.split('/').any(|segment| segment == "..") {
        return Err(format!(
            "$path `{path}` escapes the project root; Yeet only syncs files beneath it"
        ));
    }
    // `C:/…` and `C:…` are absolute-ish on Windows; `/…` and `//server/share`
    // everywhere. Checked textually so the rule does not vary by host OS —
    // a project file is shared across machines.
    let windows_drive = {
        let bytes = normalized.as_bytes();
        bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
    };
    if normalized.starts_with('/') || windows_drive {
        return Err(format!(
            "$path `{path}` is absolute; it must be relative to the project root"
        ));
    }
    Ok(())
}

/// Strips a leading UTF-8 BOM. `PowerShell`'s `Out-File` and
/// `Set-Content -Encoding utf8` prepend one, and `serde_json` then rejects the
/// file with `expected value at line 1 column 1` — which, for the root project
/// file, kills the daemon at startup. `state::read_meta_file` already applies
/// the same guard to `.meta.json` files.
fn strip_bom(raw: &str) -> &str {
    raw.strip_prefix('\u{feff}').unwrap_or(raw)
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
        let project: Self = serde_json::from_str(strip_bom(&raw))
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
        let project: Self = serde_json::from_str(strip_bom(&raw)).ok()?;
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

    /// Every `$`-prefixed key Rojo defines but Yeet does not model must be
    /// skipped, not parsed as a child instance. Before this, `#[serde(flatten)]`
    /// swallowed them and the whole load failed with
    /// `invalid type: sequence, expected struct TreeNode` — the daemon then
    /// exited before binding, so nothing synced and no sourcemap was written.
    #[test]
    fn ignores_unsupported_dollar_keys() {
        for raw in [
            r#"{"name":"T","tree":{"$className":"DataModel","R":{"$path":"src","$tags":["a","b"]}}}"#,
            r#"{"name":"T","tree":{"$className":"DataModel","R":{"$path":"src","$attributes":{"Speed":16}}}}"#,
            r#"{"name":"T","tree":{"$className":"DataModel","R":{"$path":"src","$attributes":{"Env":"prod"}}}}"#,
            r#"{"name":"T","tree":{"$className":"DataModel","R":{"$path":"src","$id":"abc"}}}"#,
            r#"{"name":"T","tree":{"$className":"DataModel","R":{"$path":"src","$keepUnknowns":true}}}"#,
        ] {
            let project: Project =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("parse {raw}: {e}"));
            let child = project.tree.children.get("R").expect("R child present");
            assert_eq!(child.path.as_deref(), Some("src"));
            assert!(
                child.children.is_empty(),
                "a `$`-prefixed key must not become a child instance: {raw}"
            );
        }
    }

    /// The nastier half of the same bug: when every `$attributes` value is
    /// itself an object, the old flatten *succeeded* and injected a phantom
    /// instance literally named `$attributes`, which then flowed through
    /// `path_mappings()` into the sourcemap.
    #[test]
    fn object_valued_attributes_do_not_inject_phantom_child() {
        let raw = r#"{
            "name": "T",
            "tree": {
                "$className": "DataModel",
                "R": { "$path": "src", "$attributes": { "Nested": { "a": 1 } } }
            }
        }"#;
        let project: Project = serde_json::from_str(raw).expect("parse");
        let child = project.tree.children.get("R").expect("R child present");
        assert!(
            !child.children.contains_key("$attributes"),
            "`$attributes` must never appear as an instance name"
        );
        assert!(child.children.is_empty());
    }

    /// Rojo's optional-path form suppresses its "path does not exist" error but
    /// still names the same directory.
    #[test]
    fn accepts_optional_path_object_form() {
        let raw = r#"{"name":"T","tree":{"$className":"DataModel","R":{"$path":{"optional":"src"}}}}"#;
        let project: Project = serde_json::from_str(raw).expect("parse");
        assert_eq!(
            project.tree.children["R"].path.as_deref(),
            Some("src"),
            "the optional form must resolve to the same filesystem path"
        );
    }

    /// A malformed value for a key Yeet *does* model still has to fail — the
    /// skip above must not turn into a blanket "ignore everything".
    #[test]
    fn rejects_malformed_supported_keys() {
        for raw in [
            r#"{"name":"T","tree":{"$className":42}}"#,
            r#"{"name":"T","tree":{"$ignoreUnknownInstances":"yes"}}"#,
            r#"{"name":"T","tree":{"R":{"$path":["src"]}}}"#,
            r#"{"name":"T","tree":{"R":{"$properties":"nope"}}}"#,
        ] {
            assert!(
                serde_json::from_str::<Project>(raw).is_err(),
                "malformed supported key must still be rejected: {raw}"
            );
        }
    }

    /// A top-level `$path` is joined onto the project root and walked. `..`
    /// escapes that root for real: `rel_raw`'s `strip_prefix` is lexical and
    /// `Path::components` preserves `ParentDir`, so files outside the project
    /// land in `tree_fs` and get broadcast to Studio. Yeet cannot sync them
    /// correctly either — the watcher only observes the root, so nothing outside
    /// it is ever seen changing. `load_nested_package` has rejected exactly this
    /// shape since A10; the root loader never did.
    #[test]
    fn rejects_path_escaping_the_project_root() {
        for raw in [
            r#"{"name":"T","tree":{"R":{"$path":"../outside"}}}"#,
            r#"{"name":"T","tree":{"R":{"$path":"src/../../outside"}}}"#,
            r#"{"name":"T","tree":{"R":{"$path":"..\\outside"}}}"#,
            r#"{"name":"T","tree":{"R":{"$path":".."}}}"#,
            r#"{"name":"T","tree":{"R":{"$path":{"optional":"../outside"}}}}"#,
        ] {
            let err = serde_json::from_str::<Project>(raw)
                .expect_err("must reject escaping $path")
                .to_string();
            assert!(
                err.contains("$path"),
                "the error must name the offending key, got: {err}"
            );
        }
    }

    /// An absolute `$path` silently ingests nothing (`root.join` discards the
    /// base, then `strip_prefix` fails), which looks exactly like an empty
    /// project. Refusing it up front turns a mystery into a message.
    #[test]
    fn rejects_absolute_path() {
        for raw in [
            r#"{"name":"T","tree":{"R":{"$path":"/etc/passwd"}}}"#,
            r#"{"name":"T","tree":{"R":{"$path":"C:/Windows/Temp"}}}"#,
        ] {
            assert!(
                serde_json::from_str::<Project>(raw).is_err(),
                "must reject absolute $path: {raw}"
            );
        }
    }

    /// The guard must not overreach: a `..` inside a *name*, or a relative path
    /// that stays inside the root, is legitimate.
    #[test]
    fn accepts_ordinary_relative_paths() {
        for raw in [
            r#"{"name":"T","tree":{"R":{"$path":"src/shared"}}}"#,
            r#"{"name":"T","tree":{"R":{"$path":"./src"}}}"#,
            r#"{"name":"T","tree":{"R":{"$path":"src/..weird/x"}}}"#,
            r#"{"name":"T","tree":{"$path":"."}}"#,
        ] {
            serde_json::from_str::<Project>(raw)
                .unwrap_or_else(|e| panic!("must accept {raw}: {e}"));
        }
    }

    /// `PowerShell`'s `Out-File` / `Set-Content -Encoding utf8` prepend a UTF-8
    /// BOM; `serde_json` then fails with `expected value at line 1 column 1`
    /// and the daemon exits at startup. `state::read_meta_file` already guards
    /// against this — the project loader did not.
    #[test]
    fn load_tolerates_utf8_bom() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(PROJECT_FILE_NAME);
        std::fs::write(
            &path,
            "\u{feff}{\"name\":\"T\",\"tree\":{\"$className\":\"DataModel\"}}",
        )
        .expect("write");
        let project = Project::load(&path).expect("BOM-prefixed project must load");
        assert_eq!(project.name, "T");
    }

    #[test]
    fn load_nested_package_tolerates_utf8_bom() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(PROJECT_FILE_NAME);
        std::fs::write(&path, "\u{feff}{\"name\":\"roact\",\"tree\":{\"$path\":\"src\"}}")
            .expect("write");
        let nested = Project::load_nested_package(&path).expect("BOM-prefixed package must load");
        assert_eq!(nested.name, "roact");
        assert_eq!(nested.src, "src");
    }

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
