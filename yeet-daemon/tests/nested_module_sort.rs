//! Integration test for the nested-module sort fix (Phase A).
//!
//! Pins the contract that `BulkSyncPreview.entries` arrive ordered as
//! "shallower depth first, init.luau before non-init siblings within
//! the same depth, alphabetical tiebreaker". Without this ordering the
//! plugin's TreeBuilder materializes a Folder for `Foo` (when
//! `Foo/Bar.luau` arrives first), then has to swap it to a ModuleScript
//! when `Foo/init.luau` lands later — a chain that breaks
//! intermittently when ChangeHistoryService is busy mid-batch.

mod common;

use std::time::Duration;

use anyhow::Result;
use common::{next_matching, send_json, spawn_test_daemon};
use serde_json::json;
use sha2::{Digest, Sha256};

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Spin up a daemon with a project that has nested init.luau files mixed
/// with siblings, trigger the bootstrap-preview flow, and assert the
/// `BulkSyncPreview.entries` come back in the expected order.
#[tokio::test(flavor = "multi_thread")]
async fn bulk_preview_emits_init_files_before_siblings() -> Result<()> {
    // Disk layout — paths chosen so that alphabetical sort would produce
    // the WRONG order (Bar before init.luau, Inner before Sub/init).
    let daemon = spawn_test_daemon(&[
        ("src/Foo/Bar.luau", "-- Bar"),
        ("src/Foo/Sub/Inner.luau", "-- Inner"),
        ("src/Foo/Sub/init.luau", "return {}"),
        ("src/Foo/init.luau", "return {}"),
    ])?;

    // Connect as plugin and immediately advance through the bootstrap
    // handshake. We need request_bootstrap_preview = true to get the
    // BulkSyncPreview path activated.
    let mut plugin = {
        use anyhow::Context;
        use futures_util::SinkExt;
        use tokio_tungstenite::{
            connect_async,
            tungstenite::{client::IntoClientRequest, Message},
        };
        let request = daemon.ws_url().into_client_request().context("uri")?;
        let (mut ws, _resp) = connect_async(request).await.context("ws connect")?;
        let hello = json!({
            "type": "hello",
            "version": "0.3.0",
            "role": "plugin",
            "studio_snapshot": [],
            "request_bootstrap_preview": true,
            "auth_token": daemon.auth_token(),
        });
        ws.send(Message::Text(hello.to_string()))
            .await
            .context("send hello")?;
        ws
    };

    // Drain ProjectOpened (carries initial_files; sorting there is
    // covered by the plugin-side TreeBuilder.build sort, which we can't
    // reach from cargo test — the bootstrap-preview path the daemon
    // uses for the entry sort is the BulkSyncPreview that arrives after
    // we send our snapshot report).
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    // Send an empty StudioSnapshotReport — Studio has nothing — so every
    // disk file appears in the preview as `ide_only`.
    send_json(
        &mut plugin,
        json!({
            "type": "studio_snapshot_report",
            "snapshot": [],
        }),
    )
    .await?;

    let preview = next_matching(&mut plugin, Duration::from_secs(5), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("bulk_sync_preview")
    })
    .await?;
    let entries = preview
        .get("entries")
        .and_then(|e| e.as_array())
        .expect("entries array");
    let paths: Vec<&str> = entries
        .iter()
        .filter_map(|e| e.get("path").and_then(|p| p.as_str()))
        .collect();

    // Expected order, derived by hand from the rule:
    //   1) shallower first  2) init before non-init at same depth
    //   3) alphabetical tiebreaker.
    //
    //   src/Foo/init.luau          (depth 2, init)
    //   src/Foo/Bar.luau           (depth 2, non-init)
    //   src/Foo/Sub/init.luau      (depth 3, init)
    //   src/Foo/Sub/Inner.luau     (depth 3, non-init)
    let expected = vec![
        "src/Foo/init.luau",
        "src/Foo/Bar.luau",
        "src/Foo/Sub/init.luau",
        "src/Foo/Sub/Inner.luau",
    ];
    assert_eq!(
        paths, expected,
        "bulk preview entries must be init-first depth-ordered; got {paths:?}"
    );
    Ok(())
}

/// Sanity: a flat tree with no init files keeps the plain alphabetical
/// order. Pinning this so a future regression that over-applies the
/// "init first" rule (e.g. case bug treating every file as init)
/// doesn't reshape the boring case.
#[tokio::test(flavor = "multi_thread")]
async fn bulk_preview_keeps_alphabetical_order_for_flat_tree() -> Result<()> {
    let daemon = spawn_test_daemon(&[
        ("src/Charlie.luau", "-- C"),
        ("src/Alpha.luau", "-- A"),
        ("src/Bravo.luau", "-- B"),
    ])?;

    let mut plugin = {
        use anyhow::Context;
        use futures_util::SinkExt;
        use tokio_tungstenite::{
            connect_async,
            tungstenite::{client::IntoClientRequest, Message},
        };
        let request = daemon.ws_url().into_client_request().context("uri")?;
        let (mut ws, _resp) = connect_async(request).await.context("ws connect")?;
        let hello = json!({
            "type": "hello",
            "version": "0.3.0",
            "role": "plugin",
            "studio_snapshot": [],
            "request_bootstrap_preview": true,
            "auth_token": daemon.auth_token(),
        });
        ws.send(Message::Text(hello.to_string()))
            .await
            .context("send hello")?;
        ws
    };

    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;
    let _ = sha256_hex(b"unused"); // keep import in scope; future tests may need it
    send_json(
        &mut plugin,
        json!({"type": "studio_snapshot_report", "snapshot": []}),
    )
    .await?;
    let preview = next_matching(&mut plugin, Duration::from_secs(5), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("bulk_sync_preview")
    })
    .await?;
    let paths: Vec<&str> = preview
        .get("entries")
        .and_then(|e| e.as_array())
        .expect("entries")
        .iter()
        .filter_map(|e| e.get("path").and_then(|p| p.as_str()))
        .collect();
    assert_eq!(
        paths,
        vec!["src/Alpha.luau", "src/Bravo.luau", "src/Charlie.luau"]
    );
    Ok(())
}

/// Helper: open a fresh plugin connection, drain ProjectOpened, and
/// return the WebSocket. Inlined here because the existing harness's
/// `connect_as` doesn't carry `request_bootstrap_preview = true` which
/// these tests need to trigger the BulkSyncPreview path.
async fn connect_with_bootstrap_preview(daemon: &common::TestDaemon) -> Result<common::Ws> {
    use anyhow::Context;
    use futures_util::SinkExt;
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{client::IntoClientRequest, Message},
    };
    let request = daemon.ws_url().into_client_request().context("uri")?;
    let (mut ws, _resp) = connect_async(request).await.context("ws connect")?;
    let hello = json!({
        "type": "hello",
        "version": "0.3.0",
        "role": "plugin",
        "studio_snapshot": [],
        "request_bootstrap_preview": true,
        "auth_token": daemon.auth_token(),
    });
    ws.send(Message::Text(hello.to_string()))
        .await
        .context("send hello")?;
    let _ = next_matching(&mut ws, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;
    Ok(ws)
}

/// 6-file deeply nested chain mixing init.luau and non-init siblings at
/// 3 different depths. The expected order:
///
///   1. src/Foo/init.luau          (depth 2, init)
///   2. src/Foo/A.luau             (depth 2, non-init)
///   3. src/Foo/Sub/init.luau      (depth 3, init)
///   4. src/Foo/Sub/B.luau         (depth 3, non-init)
///   5. src/Foo/Sub/Deep/init.luau (depth 4, init)
///   6. src/Foo/Sub/Deep/C.luau    (depth 4, non-init)
///
/// Pins that the depth metric scales — without depth-first ordering the
/// daemon would interleave the chain and the plugin would have to swap
/// each Folder→ModuleScript at every level, which is exactly the
/// "intermittent failure" mode the user originally reported.
#[tokio::test(flavor = "multi_thread")]
async fn bulk_preview_handles_deeply_nested_init_chain() -> Result<()> {
    let daemon = spawn_test_daemon(&[
        ("src/Foo/Sub/Deep/C.luau", "-- C"),
        ("src/Foo/Sub/Deep/init.luau", "return {}"),
        ("src/Foo/Sub/B.luau", "-- B"),
        ("src/Foo/Sub/init.luau", "return {}"),
        ("src/Foo/A.luau", "-- A"),
        ("src/Foo/init.luau", "return {}"),
    ])?;
    let mut plugin = connect_with_bootstrap_preview(&daemon).await?;
    send_json(
        &mut plugin,
        json!({"type": "studio_snapshot_report", "snapshot": []}),
    )
    .await?;
    let preview = next_matching(&mut plugin, Duration::from_secs(5), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("bulk_sync_preview")
    })
    .await?;
    let paths: Vec<&str> = preview
        .get("entries")
        .and_then(|e| e.as_array())
        .expect("entries")
        .iter()
        .filter_map(|e| e.get("path").and_then(|p| p.as_str()))
        .collect();
    assert_eq!(
        paths,
        vec![
            "src/Foo/init.luau",
            "src/Foo/A.luau",
            "src/Foo/Sub/init.luau",
            "src/Foo/Sub/B.luau",
            "src/Foo/Sub/Deep/init.luau",
            "src/Foo/Sub/Deep/C.luau",
        ],
        "deeply nested init chain must order shallowest-init-first at every depth"
    );
    Ok(())
}

/// Mixed init variants — `init.luau`, `init.client.lua`,
/// `init.server.luau` — all of them must be treated equivalently by
/// the sort (all "init" → all sort-first within their parent dir).
/// Pins the case-insensitive comparison and the variant set.
#[tokio::test(flavor = "multi_thread")]
async fn bulk_preview_orders_mixed_init_variants() -> Result<()> {
    let daemon = spawn_test_daemon(&[
        ("src/Bar/Other.luau", "-- Bar/Other"),
        ("src/Bar/init.client.lua", "-- Bar local script"),
        ("src/Foo/Helper.luau", "-- Foo helper"),
        ("src/Foo/init.luau", "-- Foo module"),
        ("src/Baz/Other.luau", "-- Baz/Other"),
        ("src/Baz/init.server.luau", "-- Baz server script"),
    ])?;
    let mut plugin = connect_with_bootstrap_preview(&daemon).await?;
    send_json(
        &mut plugin,
        json!({"type": "studio_snapshot_report", "snapshot": []}),
    )
    .await?;
    let preview = next_matching(&mut plugin, Duration::from_secs(5), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("bulk_sync_preview")
    })
    .await?;
    let paths: Vec<&str> = preview
        .get("entries")
        .and_then(|e| e.as_array())
        .expect("entries")
        .iter()
        .filter_map(|e| e.get("path").and_then(|p| p.as_str()))
        .collect();

    // All three init variants are at depth 2. Sort key:
    //   1) depth 2 (all 6)
    //   2) init first → init.* takes positions [0..3]
    //   3) alphabetical tiebreaker among init: "src/Bar/init.client.lua",
    //      "src/Baz/init.server.luau", "src/Foo/init.luau"
    //   4) alphabetical among non-init: "src/Bar/Other", "src/Baz/Other",
    //      "src/Foo/Helper"
    assert_eq!(
        paths,
        vec![
            "src/Bar/init.client.lua",
            "src/Baz/init.server.luau",
            "src/Foo/init.luau",
            "src/Bar/Other.luau",
            "src/Baz/Other.luau",
            "src/Foo/Helper.luau",
        ],
        "all init.* variants must sort first; non-init siblings follow alphabetically"
    );
    Ok(())
}
