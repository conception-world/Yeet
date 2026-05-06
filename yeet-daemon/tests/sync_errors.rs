//! Integration tests for the SyncError surface (T1.5, T1.6).
//!
//! These pin the contract that "the daemon never silently drops a
//! frame". Every rejection path emits a `ServerMsg::SyncError` so the
//! plugin can surface it in the activity log instead of leaving the
//! user wondering why a file mysteriously failed to sync.

mod common;

use std::time::Duration;

use anyhow::Result;
use common::{connect_as, next_matching, send_json, spawn_test_daemon};
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

/// Plugin sends a `file_changed` whose path tries to escape the project
/// root. Daemon must reject the frame AND broadcast a SyncError so the
/// user sees "this didn't sync, here's why".
#[tokio::test(flavor = "multi_thread")]
async fn unsafe_path_in_file_changed_emits_sync_error() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/Real.luau", "v1")])?;
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    let body = "compromised";
    send_json(
        &mut plugin,
        json!({
            "type": "file_changed",
            "path": "../../etc/passwd",
            "content": body,
            "sha256": sha256_hex(body.as_bytes()),
        }),
    )
    .await?;

    let err = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("sync_error")
    })
    .await?;
    assert_eq!(
        err.get("kind").and_then(|s| s.as_str()),
        Some("unsafe_path"),
        "kind must be unsafe_path; got {err}"
    );
    assert_eq!(
        err.get("path").and_then(|s| s.as_str()),
        Some("../../etc/passwd"),
        "path must be the offending value; got {err}"
    );
    Ok(())
}

/// Plugin sends a `file_changed` whose `sha256` doesn't match the
/// recomputed hash of the content. Daemon used to accept the frame
/// (logging a warning) — T1.6 changed it to reject AND broadcast a
/// SyncError.
#[tokio::test(flavor = "multi_thread")]
async fn hash_mismatch_in_file_changed_emits_sync_error_and_rejects_frame() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/Real.luau", "v1")])?;
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    let body = "v2";
    let wrong_sha = "0000000000000000000000000000000000000000000000000000000000000000";
    send_json(
        &mut plugin,
        json!({
            "type": "file_changed",
            "path": "src/Real.luau",
            "content": body,
            "sha256": wrong_sha,
        }),
    )
    .await?;

    let err = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("sync_error")
    })
    .await?;
    assert_eq!(
        err.get("kind").and_then(|s| s.as_str()),
        Some("hash_mismatch"),
        "kind must be hash_mismatch; got {err}"
    );

    // The frame was rejected, so the disk must NOT have been updated.
    // Allow a 500ms grace window in case the rejection is processed
    // asynchronously after the SyncError broadcast.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let actual = std::fs::read_to_string(daemon.project_root.path().join("src/Real.luau"))?;
    assert_eq!(
        actual, "v1",
        "rejected frame must not propagate to disk; got {actual}"
    );
    Ok(())
}

/// A `studio_snapshot_report` whose `content.len()` exceeds
/// MAX_CONTENT_BYTES drops the offending entry AND broadcasts a
/// summary SyncError naming the path. Smaller siblings still go
/// through.
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_oversized_entry_emits_sync_error_summary() -> Result<()> {
    let daemon = spawn_test_daemon(&[
        ("src/Tiny.luau", "small"),
        ("src/Sibling.luau", "ok"),
    ])?;
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    let huge = "x".repeat(10 * 1024 * 1024 + 1);
    send_json(
        &mut plugin,
        json!({
            "type": "studio_snapshot_report",
            "snapshot": [
                {
                    "path": "src/Tiny.luau",
                    "kind": "module_script",
                    "content": "small",
                    "sha256": sha256_hex(b"small"),
                },
                {
                    "path": "src/Huge.luau",
                    "kind": "module_script",
                    "content": huge,
                    "sha256": sha256_hex(b"huge"),
                },
            ],
        }),
    )
    .await?;

    let err = next_matching(&mut plugin, Duration::from_secs(5), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("sync_error")
            && v.get("kind").and_then(|s| s.as_str()) == Some("snapshot_entry_dropped")
    })
    .await?;
    let reason = err
        .get("reason")
        .and_then(|s| s.as_str())
        .expect("reason field");
    assert!(
        reason.contains("src/Huge.luau"),
        "reason should name the dropped path; got {reason}"
    );
    Ok(())
}
