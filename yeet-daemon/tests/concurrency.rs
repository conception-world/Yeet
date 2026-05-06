//! End-to-end integration tests against a real `yeet-daemon`
//! subprocess. These exercise everything the in-process bulk_tests
//! can't: the actual WebSocket plumbing, the per-session resume
//! buffer, multi-client broadcasts, fs-watcher → reconcile → broadcast
//! pipeline.
//!
//! Each test gets its own daemon (random ephemeral port via
//! `--bind 127.0.0.1:0`), so they parallelize cleanly.

mod common;

use std::time::Duration;

use anyhow::Result;
use common::{
    connect_as, drain_frames, next_frame, next_matching, send_json, spawn_test_daemon,
};
use serde_json::json;

/// Helper: drain `ProjectOpened` for a freshly-connected plugin and
/// return its `session_id`. Tests use this to capture the id before
/// triggering an action that should rotate or preserve it.
async fn drain_project_opened(
    ws: &mut common::Ws,
) -> Result<String> {
    let frame = next_matching(ws, Duration::from_secs(3), |v| {
        v.get("type")
            .and_then(|t| t.as_str())
            .map(|s| s == "project_opened")
            .unwrap_or(false)
    })
    .await?;
    Ok(frame
        .get("session_id")
        .and_then(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("project_opened missing session_id"))?
        .to_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_resume_with_matching_session_id_replays_buffered_frames() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/Initial.luau", "v1")])?;
    let mut plugin1 = connect_as(&daemon, "plugin").await?;
    let session_id = drain_project_opened(&mut plugin1).await?;

    // Disconnect the plugin (drop the socket) but keep the daemon
    // around. Any broadcasts that fire next should land in the
    // daemon's pending_deltas buffer.
    drop(plugin1);
    // Yield so the daemon's task notices the close.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Trigger a broadcast — write a new file on disk so the watcher
    // emits FileChanged through reconcile_path.
    let new_file = daemon.project_root.path().join("src/Late.luau");
    std::fs::write(&new_file, "after disconnect")?;

    // Reconnect with the same session id and assert the buffered
    // frame is replayed (Resumed envelope first, then the FileCreated).
    let mut plugin2 = connect_as_with_session(&daemon, "plugin", &session_id).await?;
    let resumed = next_matching(&mut plugin2, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("resumed")
    })
    .await?;
    assert_eq!(
        resumed.get("session_id").and_then(|s| s.as_str()),
        Some(session_id.as_str())
    );

    let replayed = next_matching(&mut plugin2, Duration::from_secs(3), |v| {
        let ty = v.get("type").and_then(|t| t.as_str());
        let path = v.get("path").and_then(|t| t.as_str());
        matches!(ty, Some("file_created" | "file_changed")) && path == Some("src/Late.luau")
    })
    .await?;
    assert_eq!(
        replayed.get("path").and_then(|s| s.as_str()),
        Some("src/Late.luau")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_reconnect_with_stale_session_id_falls_back_to_full_bootstrap() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/A.luau", "v1")])?;
    let mut plugin = connect_as_with_session(&daemon, "plugin", "stale-session-that-doesnt-exist")
        .await?;
    // No `resumed` envelope; we get straight `project_opened` with a
    // fresh session_id (rotated from the bootstrap path).
    let frame = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        let ty = v.get("type").and_then(|t| t.as_str());
        matches!(ty, Some("project_opened" | "resumed"))
    })
    .await?;
    assert_eq!(
        frame.get("type").and_then(|t| t.as_str()),
        Some("project_opened"),
        "expected fresh project_opened, got {frame}"
    );
    let new_session = frame
        .get("session_id")
        .and_then(|s| s.as_str())
        .expect("project_opened.session_id");
    assert_ne!(new_session, "stale-session-that-doesnt-exist");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn studio_snapshot_oversized_entry_dropped_others_kept() -> Result<()> {
    let daemon = spawn_test_daemon(&[
        ("src/Small.luau", "tiny"),
        ("src/Other.luau", "also tiny"),
    ])?;
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = drain_project_opened(&mut plugin).await?;

    // Build a snapshot with one valid entry and one entry whose
    // `content` is over MAX_CONTENT_BYTES (10 MiB). The daemon must
    // drop the oversized entry but keep the small one in tree_studio.
    let huge = "x".repeat(10 * 1024 * 1024 + 1);
    let small_content = "module";
    let small_sha = sha256_hex(small_content.as_bytes());
    let huge_sha = sha256_hex(huge.as_bytes());
    send_json(
        &mut plugin,
        json!({
            "type": "studio_snapshot_report",
            "snapshot": [
                {"path": "src/Small.luau", "kind": "module_script",
                 "content": small_content, "sha256": small_sha},
                {"path": "src/Huge.luau", "kind": "module_script",
                 "content": huge, "sha256": huge_sha},
            ],
        }),
    )
    .await?;

    // The daemon replies with BulkSyncPreview after ingesting the
    // snapshot. For Small.luau the studio sha matches the disk sha
    // (no, actually they differ since "module" != "tiny"), so it
    // should appear as Modified. Huge.luau was dropped, so the disk
    // file doesn't show as IdeOnly only if we compare to what is in
    // tree_studio — it's not in tree_studio so it shows as IdeOnly.
    // Either way, Huge.luau MUST NOT appear in entries.
    let preview = next_matching(&mut plugin, Duration::from_secs(5), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("bulk_sync_preview")
    })
    .await?;
    let entries = preview
        .get("entries")
        .and_then(|e| e.as_array())
        .expect("entries array");
    let huge_present = entries
        .iter()
        .any(|e| e.get("path").and_then(|s| s.as_str()) == Some("src/Huge.luau"));
    assert!(
        !huge_present,
        "oversized entry leaked into preview: {entries:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_change_propagates_to_connected_plugin_via_filechanged() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/Watched.luau", "before")])?;
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = drain_project_opened(&mut plugin).await?;
    // Drain anything else the daemon emitted post-handshake before
    // the assertion-driving event.
    let _ = drain_frames(&mut plugin, Duration::from_millis(150)).await;

    std::fs::write(
        daemon.project_root.path().join("src/Watched.luau"),
        b"after edit",
    )?;
    let frame = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("file_changed")
            && v.get("path").and_then(|s| s.as_str()) == Some("src/Watched.luau")
    })
    .await?;
    assert_eq!(
        frame.get("content").and_then(|s| s.as_str()),
        Some("after edit")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn second_plugin_connection_replaces_first_role_holder() -> Result<()> {
    // Smoke test for "two devs accidentally connect to the same daemon"
    // — both plugin sessions stay open (the daemon doesn't reject), and
    // events broadcast after the second connect reach both. The current
    // implementation broadcasts to every subscriber; this test pins
    // that contract so a future change doesn't silently start
    // dropping the older client.
    let daemon = spawn_test_daemon(&[("src/Shared.luau", "v0")])?;
    let mut plug_a = connect_as(&daemon, "plugin").await?;
    let _ = drain_project_opened(&mut plug_a).await?;
    let mut plug_b = connect_as(&daemon, "plugin").await?;
    let _ = drain_project_opened(&mut plug_b).await?;
    let _ = drain_frames(&mut plug_a, Duration::from_millis(150)).await;
    let _ = drain_frames(&mut plug_b, Duration::from_millis(150)).await;

    std::fs::write(daemon.project_root.path().join("src/Shared.luau"), b"v1")?;

    let received_a = next_matching(&mut plug_a, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("file_changed")
            && v.get("path").and_then(|s| s.as_str()) == Some("src/Shared.luau")
    })
    .await;
    let received_b = next_matching(&mut plug_b, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("file_changed")
            && v.get("path").and_then(|s| s.as_str()) == Some("src/Shared.luau")
    })
    .await;
    assert!(received_a.is_ok(), "plugin A didn't receive: {received_a:?}");
    assert!(received_b.is_ok(), "plugin B didn't receive: {received_b:?}");
    Ok(())
}

// ─── Helpers used only here ─────────────────────────────────────────────

async fn connect_as_with_session(
    daemon: &common::TestDaemon,
    role: &str,
    session_id: &str,
) -> Result<common::Ws> {
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
        "role": role,
        "studio_snapshot": [],
        "session_id": session_id,
        "auth_token": daemon.auth_token(),
    });
    ws.send(Message::Text(hello.to_string()))
        .await
        .context("send hello")?;
    Ok(ws)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
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

// `next_frame` is exercised inside the suite via `next_matching`; the
// import is kept for tests that want a single-frame read.
#[allow(dead_code)]
async fn _next_frame_used(ws: &mut common::Ws) -> Result<serde_json::Value> {
    next_frame(ws, Duration::from_secs(1)).await
}
