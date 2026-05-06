//! End-to-end test of the reverse-bootstrap pipeline.
//!
//! Builds a synthetic `DataModel` snapshot in memory, feeds it to the
//! syncback materializer, and compares the resulting directory tree
//! byte-for-byte against `tests/fixtures/tiny_place/expected/`. The test
//! also asserts the returned `SyncbackStats` summary.
//!
//! When the expected output needs to change deliberately, update the files
//! under `expected/` — the test will re-pass once they match. Regressions in
//! layout, sanitization, or property encoding show up here first.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tempfile::tempdir;

use yeet_daemon::{
    protocol::{
        BinaryPayload, SerializedInstance, SerializedProperty, SyncbackMode, SyncbackTemplate,
    },
    syncback::{self, SyncbackOptions, SyncbackSession},
    tree,
};

fn make_inst(
    id: u64,
    parent: Option<u64>,
    class: &str,
    name: &str,
) -> SerializedInstance {
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

/// Mirrors the scenario described at the top of the file. Returns every
/// instance in a single vec (chunk order is irrelevant; the materializer
/// rebuilds the tree from `id`/`parent_id`).
fn build_tiny_place() -> Vec<SerializedInstance> {
    let mut out: Vec<SerializedInstance> = vec![
        // Root.
        make_inst(1, None, "DataModel", "game"),
        // Services.
        make_inst(2, Some(1), "ReplicatedStorage", "ReplicatedStorage"),
        make_inst(3, Some(1), "ServerScriptService", "ServerScriptService"),
        make_inst(5, Some(1), "Workspace", "Workspace"),
        make_inst(20, Some(1), "Lighting", "Lighting"),
    ];

    // ServerScriptService/Init.server.luau
    let mut init_script = make_inst(4, Some(3), "Script", "Init");
    init_script.properties.insert(
        "Source".to_owned(),
        SerializedProperty::String("print('hello')\n".to_owned()),
    );
    out.push(init_script);

    // Workspace/Greeter as a Client-context Script → emitted as LocalScript.
    let mut greeter = make_inst(6, Some(5), "Script", "Greeter");
    greeter.properties.insert(
        "Source".to_owned(),
        SerializedProperty::String("-- client greeter\n".to_owned()),
    );
    greeter.properties.insert(
        "RunContext".to_owned(),
        SerializedProperty::Enum("RunContext.Client".to_owned()),
    );
    out.push(greeter);

    // Workspace/SharedUtils (ModuleScript with a child).
    let mut shared = make_inst(7, Some(5), "ModuleScript", "SharedUtils");
    shared.properties.insert(
        "Source".to_owned(),
        SerializedProperty::String("return {}\n".to_owned()),
    );
    out.push(shared);

    let mut helpers = make_inst(8, Some(7), "ModuleScript", "Helpers");
    helpers.properties.insert(
        "Source".to_owned(),
        SerializedProperty::String("return function() end\n".to_owned()),
    );
    out.push(helpers);

    // Workspace/Part with custom props + attribute + tag.
    let mut part = make_inst(9, Some(5), "Part", "Part");
    part.properties
        .insert("Size".to_owned(), SerializedProperty::Vector3([4.0, 1.0, 2.0]));
    part.properties.insert(
        "Color".to_owned(),
        SerializedProperty::Color3([0.5, 0.5, 0.5]),
    );
    part.properties
        .insert("Anchored".to_owned(), SerializedProperty::Bool(true));
    part.attributes
        .insert("Points".to_owned(), SerializedProperty::Number(10.0));
    part.tags.push("enemy".to_owned());
    out.push(part);

    // Name with illegal character — sanitized to Weird_Slash and round-trips
    // via `"name"` in meta.
    out.push(make_inst(10, Some(5), "Folder", "Weird/Slash"));

    // Two siblings with the same literal name → second one gets _1 suffix.
    out.push(make_inst(11, Some(5), "Folder", "Dup"));
    out.push(make_inst(12, Some(5), "Folder", "Dup"));

    out
}

fn run_materialize(
    target_path: PathBuf,
    instances: Vec<SerializedInstance>,
) -> yeet_daemon::protocol::SyncbackStats {
    let opts = SyncbackOptions {
        target_path,
        mode: SyncbackMode::NewProject,
        include_non_script: true,
        include_binary: false,
        template: SyncbackTemplate::Minimal,
        project_name: Some("tiny_place".to_owned()),
    };
    let mut session = SyncbackSession::new("req-tiny-1".to_owned(), opts);
    // One chunk with everything; the materializer doesn't care about chunking.
    session
        .ingest_chunk(0, instances)
        .expect("ingest chunk");
    syncback::materialize(session, 1).expect("materialize")
}

fn fixture_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.join("tests/fixtures/tiny_place/expected")
}

fn read_relative(root: &Path, rel: &str) -> String {
    let mut abs = root.to_path_buf();
    for part in rel.split('/') {
        abs.push(part);
    }
    std::fs::read_to_string(&abs)
        .unwrap_or_else(|e| panic!("read {}: {e}", abs.display()))
}

fn assert_file_eq(actual_root: &Path, expected_root: &Path, rel: &str) {
    let actual = read_relative(actual_root, rel);
    let expected = read_relative(expected_root, rel);
    if actual != expected {
        let diff = format!(
            "\n--- expected: {rel}\n{expected}\n--- actual: {rel}\n{actual}\n"
        );
        panic!("fixture mismatch at {rel}{diff}");
    }
}

fn assert_json_eq(actual_root: &Path, expected_root: &Path, rel: &str) {
    let actual: serde_json::Value = serde_json::from_str(&read_relative(actual_root, rel))
        .unwrap_or_else(|e| panic!("parse actual {rel}: {e}"));
    let expected: serde_json::Value = serde_json::from_str(&read_relative(expected_root, rel))
        .unwrap_or_else(|e| panic!("parse expected {rel}: {e}"));
    assert_eq!(actual, expected, "json mismatch at {rel}");
}

/// Regenerates the on-disk fixtures by running the materializer straight
/// into `tests/fixtures/tiny_place/expected/`. Gated with `#[ignore]` so it
/// never runs in normal `cargo test`; use it only when the expected output
/// should change deliberately:
///
/// ```text
/// cargo test -p yeet-daemon --test syncback_fixture \
///   capture_tiny_place_fixture -- --ignored
/// ```
///
/// Review the resulting diff before committing — the whole point of the
/// fixture is to catch accidental changes.
#[test]
#[ignore = "regenerates fixtures on disk; run explicitly with --ignored"]
fn capture_tiny_place_fixture() {
    let target = fixture_root();
    if target.exists() {
        std::fs::remove_dir_all(&target).expect("clear fixture dir");
    }
    std::fs::create_dir_all(&target).expect("mkdir fixture dir");
    run_materialize(target, build_tiny_place());
}

#[test]
fn tiny_place_matches_fixture() {
    let out = tempdir().expect("tempdir");
    let out_root = out.path().to_path_buf();
    let stats = run_materialize(out_root.clone(), build_tiny_place());
    let expected_root = fixture_root();

    // Scripts (byte-exact).
    assert_file_eq(
        &out_root,
        &expected_root,
        "src/ServerScriptService/Init.server.luau",
    );
    assert_file_eq(
        &out_root,
        &expected_root,
        "src/Workspace/Greeter.client.luau",
    );
    assert_file_eq(
        &out_root,
        &expected_root,
        "src/Workspace/SharedUtils/init.luau",
    );
    assert_file_eq(
        &out_root,
        &expected_root,
        "src/Workspace/SharedUtils/Helpers.luau",
    );

    // Non-script instances (JSON — compare structurally so key-order is a
    // non-issue, though our BTreeMap already produces stable output).
    assert_json_eq(
        &out_root,
        &expected_root,
        "src/Workspace/Part/init.meta.json",
    );
    assert_json_eq(
        &out_root,
        &expected_root,
        "src/Workspace/Dup/init.meta.json",
    );
    assert_json_eq(
        &out_root,
        &expected_root,
        "src/Workspace/Dup_1/init.meta.json",
    );
    assert_json_eq(
        &out_root,
        &expected_root,
        "src/Workspace/Weird_Slash/init.meta.json",
    );

    // Greeter picks up a sibling meta.json because its RunContext override
    // survives alongside the `.client.luau` file.
    assert_json_eq(
        &out_root,
        &expected_root,
        "src/Workspace/Greeter.meta.json",
    );

    // Project file.
    assert_json_eq(&out_root, &expected_root, "default.project.json");

    // Base tree was persisted and round-trips.
    let base = tree::load_base_tree(&out_root)
        .expect("load base tree")
        .expect("base tree present");
    // Four scripts materialized.
    assert_eq!(base.len(), 4, "base tree should have 4 entries");
    assert!(base.contains_key("src/ServerScriptService/Init.server.luau"));
    assert!(base.contains_key("src/Workspace/Greeter.client.luau"));
    assert!(base.contains_key("src/Workspace/SharedUtils/init.luau"));
    assert!(base.contains_key("src/Workspace/SharedUtils/Helpers.luau"));

    // Stats.
    assert_eq!(stats.scripts_written, 4);
    assert_eq!(stats.non_script_instances_written, 4);
    // 4 non-script inits (Part, Weird_Slash, Dup, Dup_1) + 1 sibling for Greeter.
    assert_eq!(stats.meta_files_written, 5);
    assert_eq!(
        stats.services_included,
        vec!["ServerScriptService".to_owned(), "Workspace".to_owned()]
    );
    // Two renames (illegal char + dup collision).
    assert!(
        stats.warnings.iter().any(|w| w.contains("Weird/Slash")),
        "expected warning for Weird/Slash, got: {:?}",
        stats.warnings
    );
    assert!(
        stats.warnings.iter().any(|w| w.contains("Dup_1")),
        "expected warning for Dup_1, got: {:?}",
        stats.warnings
    );
}

#[test]
fn new_project_refuses_existing_project_file() {
    let out = tempdir().expect("tempdir");
    // Seed the target with an existing default.project.json so NewProject
    // must refuse it.
    std::fs::write(
        out.path().join("default.project.json"),
        r#"{"name":"x","tree":{}}"#,
    )
    .unwrap();

    let opts = SyncbackOptions {
        target_path: out.path().to_path_buf(),
        mode: SyncbackMode::NewProject,
        include_non_script: true,
        include_binary: false,
        template: SyncbackTemplate::Minimal,
        project_name: None,
    };
    let mut session = SyncbackSession::new("x".to_owned(), opts);
    session
        .ingest_chunk(
            0,
            vec![make_inst(1, None, "DataModel", "game")],
        )
        .unwrap();
    let err = syncback::materialize(session, 1).unwrap_err();
    assert!(
        err_chain_contains(&err, "already contains"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn merge_without_overwrite_is_not_implemented() {
    let out = tempdir().expect("tempdir");
    let opts = SyncbackOptions {
        target_path: out.path().to_path_buf(),
        mode: SyncbackMode::MergeExisting { overwrite: false },
        include_non_script: true,
        include_binary: false,
        template: SyncbackTemplate::Minimal,
        project_name: None,
    };
    let mut session = SyncbackSession::new("y".to_owned(), opts);
    session
        .ingest_chunk(
            0,
            vec![make_inst(1, None, "DataModel", "game")],
        )
        .unwrap();
    let err = syncback::materialize(session, 1).unwrap_err();
    assert!(
        err_chain_contains(&err, "not yet implemented"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn finalize_rejects_missing_chunks() {
    let out = tempdir().expect("tempdir");
    let opts = SyncbackOptions {
        target_path: out.path().to_path_buf(),
        mode: SyncbackMode::NewProject,
        include_non_script: true,
        include_binary: false,
        template: SyncbackTemplate::Minimal,
        project_name: None,
    };
    let mut session = SyncbackSession::new("z".to_owned(), opts);
    session.ingest_chunk(0, vec![]).unwrap();
    // Claim total_seq=3 but only chunk 0 was received.
    let err = syncback::materialize(session, 3).unwrap_err();
    assert!(
        err_chain_contains(&err, "missing"),
        "unexpected error: {err:#}"
    );
}

/// `anyhow::Error::to_string()` only returns the top-level message. The
/// substring tests here want to match on the root cause produced by `bail!`
/// inside `materialize`, which `with_context(...)` buries one layer down.
fn err_chain_contains(err: &anyhow::Error, needle: &str) -> bool {
    err.chain().any(|cause| cause.to_string().contains(needle))
}

#[test]
fn binary_payload_writes_rbxm_byte_exact() {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64_STD;

    let out = tempdir().expect("tempdir");
    let target = out.path().to_path_buf();

    // Synthetic non-text payload to stand in for a real .rbxm. Includes nulls
    // and high bytes so a careless encoding round-trip would visibly corrupt
    // it.
    let raw_bytes: Vec<u8> = (0u8..=255).chain(0..=255).collect();
    let encoded = BASE64_STD.encode(&raw_bytes);

    let mut workspace = make_inst(2, Some(1), "Workspace", "Workspace");
    workspace.attributes.clear();
    let mut mesh = make_inst(3, Some(2), "MeshPart", "Anvil");
    mesh.binary = Some(BinaryPayload {
        class_name: "MeshPart".to_owned(),
        rbxm_bytes_base64: encoded,
    });
    let instances = vec![
        make_inst(1, None, "DataModel", "game"),
        workspace,
        mesh,
    ];

    let opts = SyncbackOptions {
        target_path: target.clone(),
        mode: SyncbackMode::NewProject,
        include_non_script: true,
        include_binary: true,
        template: SyncbackTemplate::Minimal,
        project_name: Some("binary_place".to_owned()),
    };
    let mut session = SyncbackSession::new("req-bin-1".to_owned(), opts);
    session.ingest_chunk(0, instances).unwrap();
    let stats = syncback::materialize(session, 1).expect("materialize");

    let written = std::fs::read(target.join("src/Workspace/Anvil.rbxm")).expect("read rbxm");
    assert_eq!(written, raw_bytes, ".rbxm bytes should round-trip verbatim");
    assert_eq!(stats.scripts_written, 0);
    assert_eq!(stats.non_script_instances_written, 1);
    // Workspace itself has no children visible to the directory walker (the
    // binary payload short-circuits recursion), so no init.meta.json sibling
    // gets written for the part — matches our intent for binary leaves.
    assert!(stats.warnings.iter().all(|w| !w.contains("class mismatch")));
}

#[test]
fn binary_payload_skipped_when_include_binary_off() {
    let out = tempdir().expect("tempdir");
    let target = out.path().to_path_buf();

    let mut workspace = make_inst(2, Some(1), "Workspace", "Workspace");
    workspace.attributes.clear();
    let mut mesh = make_inst(3, Some(2), "MeshPart", "Anvil");
    mesh.binary = Some(BinaryPayload {
        class_name: "MeshPart".to_owned(),
        rbxm_bytes_base64: "AAAA".to_owned(),
    });

    let opts = SyncbackOptions {
        target_path: target.clone(),
        mode: SyncbackMode::NewProject,
        include_non_script: true,
        include_binary: false, // <- the flag under test
        template: SyncbackTemplate::Minimal,
        project_name: Some("binary_place".to_owned()),
    };
    let mut session = SyncbackSession::new("req-bin-2".to_owned(), opts);
    session
        .ingest_chunk(
            0,
            vec![make_inst(1, None, "DataModel", "game"), workspace, mesh],
        )
        .unwrap();
    syncback::materialize(session, 1).expect("materialize");

    // Falls through to the regular non-script directory pipeline; the binary
    // bytes are dropped (intentional: the user explicitly opted out).
    assert!(!target.join("src/Workspace/Anvil.rbxm").exists());
    assert!(target.join("src/Workspace/Anvil/init.meta.json").exists());
}
