//! Integration tests for `ConflictResolvedManual` (Phase C).
//!
//! The line-by-line merge picker UI lives in Luau and we can't drive it
//! from `cargo test`, but the wire contract — "plugin sends merged
//! content, daemon validates + writes to disk + broadcasts to Studio" —
//! is testable end-to-end here. Pinning that contract guards against a
//! future refactor that wires the new ClientMsg variant somewhere
//! wrong (e.g. forgets the sha check, or skips the write).

mod common;

use std::time::Duration;

use anyhow::Result;
use common::{connect_as, next_matching, send_json, spawn_test_daemon, spawn_test_daemon_with_args};
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

/// Happy path: plugin sends a manual resolution with matching sha. The
/// daemon writes the merged content to disk AND broadcasts the change
/// (since the path was tracked in tree_fs from the initial scan).
#[tokio::test(flavor = "multi_thread")]
async fn manual_resolve_with_correct_sha_writes_to_disk() -> Result<()> {
    let initial = "-- v1\nlocal x = 1\nreturn x\n";
    let daemon = spawn_test_daemon(&[("src/Foo.luau", initial)])?;
    let abs = daemon.project_root.path().join("src/Foo.luau");
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    // Plugin sends a manual resolution carrying the user-built merged
    // content. There's no pre-existing pending_conflict for this path
    // in the test (the handshake didn't surface one), but the daemon's
    // fallback path should still classify the .luau filename as a
    // ModuleScript and write the content.
    let merged = "-- merged\nlocal x = 1\nlocal y = 2\nreturn x + y\n";
    let sha = sha256_hex(merged.as_bytes());
    send_json(
        &mut plugin,
        json!({
            "type": "conflict_resolved_manual",
            "path": "src/Foo.luau",
            "content": merged,
            "sha256": sha,
        }),
    )
    .await?;

    // Poll the disk for the new content. Real-time write — should land
    // within a few hundred ms; allow generous deadline for slow CI.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let actual = std::fs::read_to_string(&abs)?;
        if actual == merged {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "manual resolve did not propagate to disk within timeout; got {:?}",
                actual
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Mismatched sha: daemon must reject the frame, broadcast a SyncError
/// of kind `hash_mismatch`, AND leave the disk untouched. Mirrors the
/// guarantee `handle_studio_changed` provides for FileChanged frames.
#[tokio::test(flavor = "multi_thread")]
async fn manual_resolve_with_wrong_sha_emits_sync_error_and_leaves_disk_intact() -> Result<()> {
    let initial = "-- original\n";
    let daemon = spawn_test_daemon(&[("src/Foo.luau", initial)])?;
    let abs = daemon.project_root.path().join("src/Foo.luau");
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    // Wrong sha (all-zeros). The actual content's hash differs, so the
    // daemon's HashMismatch guard kicks in.
    let merged = "-- attacker payload\n";
    let wrong_sha = "0".repeat(64);
    send_json(
        &mut plugin,
        json!({
            "type": "conflict_resolved_manual",
            "path": "src/Foo.luau",
            "content": merged,
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

    // Disk must be untouched. Allow a short grace window for the
    // rejection broadcast to settle before reading.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let actual = std::fs::read_to_string(&abs)?;
    assert_eq!(
        actual, initial,
        "rejected manual resolution must not propagate to disk"
    );
    Ok(())
}

/// Unsafe path (sandbox escape attempt) → SyncError::UnsafePath + disk
/// untouched. Mirrors the FileChanged guard for completeness.
#[tokio::test(flavor = "multi_thread")]
async fn manual_resolve_with_unsafe_path_emits_sync_error() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/Foo.luau", "stay")])?;
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    let body = "anything";
    send_json(
        &mut plugin,
        json!({
            "type": "conflict_resolved_manual",
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
    Ok(())
}

/// Production cap: a manual resolution carrying >MAX_CONTENT_BYTES
/// (10 MiB) must be rejected with `oversized_content`. Mirrors the
/// guard `handle_studio_changed` provides for FileChanged frames so
/// the manual entry point isn't a silent escape hatch around the cap.
#[tokio::test(flavor = "multi_thread")]
async fn manual_resolve_oversized_content_emits_sync_error() -> Result<()> {
    let initial = "small\n";
    let daemon = spawn_test_daemon(&[("src/Foo.luau", initial)])?;
    let abs = daemon.project_root.path().join("src/Foo.luau");
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    // 10 MiB + 1 byte — exactly one byte over the cap. Don't compute
    // the sha (the daemon's size check fires before the hash check, so
    // any value works for `sha256` here).
    let huge = "x".repeat(10 * 1024 * 1024 + 1);
    send_json(
        &mut plugin,
        json!({
            "type": "conflict_resolved_manual",
            "path": "src/Foo.luau",
            "content": huge,
            "sha256": "0".repeat(64),
        }),
    )
    .await?;

    let err = next_matching(&mut plugin, Duration::from_secs(5), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("sync_error")
    })
    .await?;
    assert_eq!(
        err.get("kind").and_then(|s| s.as_str()),
        Some("oversized_content"),
        "kind must be oversized_content; got {err}"
    );
    // Disk untouched.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let actual = std::fs::read_to_string(&abs)?;
    assert_eq!(
        actual, initial,
        "rejected oversized manual resolution must not propagate to disk"
    );
    Ok(())
}

/// Audit trail contract: a successful manual resolve writes a
/// `conflict_resolved` entry to `<root>/.yeet/audit.log` with
/// `note: "manual"`. Without this any post-mortem can't tell a per-
/// hunk resolution from a line-level merge picker resolution — both
/// matter for "why does this file look like X".
#[tokio::test(flavor = "multi_thread")]
async fn manual_resolve_writes_audit_log_entry_with_manual_note() -> Result<()> {
    let initial = "before\n";
    let daemon = spawn_test_daemon(&[("src/Audit.luau", initial)])?;
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    let merged = "after\n";
    let sha = sha256_hex(merged.as_bytes());
    send_json(
        &mut plugin,
        json!({
            "type": "conflict_resolved_manual",
            "path": "src/Audit.luau",
            "content": merged,
            "sha256": sha,
        }),
    )
    .await?;

    // Wait for the write to land — the audit entry is written under
    // the same write path.
    let abs = daemon.project_root.path().join("src/Audit.luau");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::fs::read_to_string(&abs)? != merged {
        if std::time::Instant::now() > deadline {
            panic!("manual resolve did not land on disk within 3s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Read the audit log and assert at least one line carries both
    // `kind: conflict_resolved` AND `note: manual`. There can be other
    // lines from the underlying `write_to_fs` (`fs_write` kind) — we
    // just need to find OUR line.
    let log_path = daemon.project_root.path().join(".yeet/audit.log");
    let log = std::fs::read_to_string(&log_path)?;
    let found = log.lines().any(|line| {
        line.contains("\"kind\":\"conflict_resolved\"") && line.contains("\"note\":\"manual\"")
    });
    assert!(
        found,
        "audit log should contain conflict_resolved + note: manual; got:\n{log}"
    );
    Ok(())
}

/// Dry-run interaction: the manual entry point must respect the
/// `--dry-run` flag the same way every other write path does. Without
/// this a user reviewing changes via `--dry-run` would still get the
/// merged content written if they accidentally hit Apply in the
/// MergePicker.
#[tokio::test(flavor = "multi_thread")]
async fn manual_resolve_in_dry_run_mode_suppresses_disk_write() -> Result<()> {
    let initial = "stay\n";
    let daemon = spawn_test_daemon_with_args(
        &[("src/Foo.luau", initial)],
        &["--dry-run"],
    )?;
    let abs = daemon.project_root.path().join("src/Foo.luau");
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    let merged = "would-be-written\n";
    let sha = sha256_hex(merged.as_bytes());
    send_json(
        &mut plugin,
        json!({
            "type": "conflict_resolved_manual",
            "path": "src/Foo.luau",
            "content": merged,
            "sha256": sha,
        }),
    )
    .await?;

    // Generous grace window so a real (non-dry-run) write would
    // certainly have landed by now.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let actual = std::fs::read_to_string(&abs)?;
    assert_eq!(
        actual, initial,
        "--dry-run must suppress disk writes from manual resolve"
    );
    Ok(())
}

/// Boundary: the user's MergePicker can produce an empty merged
/// buffer (every line removed). The daemon should accept and write an
/// empty file rather than silently dropping. Empty files are valid
/// Roblox scripts (modules that return nil implicitly).
#[tokio::test(flavor = "multi_thread")]
async fn manual_resolve_with_empty_content_writes_empty_file() -> Result<()> {
    let initial = "non-empty\n";
    let daemon = spawn_test_daemon(&[("src/Empty.luau", initial)])?;
    let abs = daemon.project_root.path().join("src/Empty.luau");
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    let empty = "";
    let sha = sha256_hex(empty.as_bytes());
    send_json(
        &mut plugin,
        json!({
            "type": "conflict_resolved_manual",
            "path": "src/Empty.luau",
            "content": empty,
            "sha256": sha,
        }),
    )
    .await?;

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let actual = std::fs::read_to_string(&abs)?;
        if actual.is_empty() {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            panic!("empty manual resolve did not produce empty file; got {actual:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Multi-client broadcast: when plugin A applies a manual resolve,
/// plugin B's WebSocket must receive the resulting `file_changed` so
/// its DataModel stays in sync. Documents the contract that
/// `write_to_fs` broadcasts to ALL connected plugins — not just the
/// originator.
#[tokio::test(flavor = "multi_thread")]
async fn manual_resolve_broadcasts_filechanged_to_other_plugin_clients() -> Result<()> {
    let initial = "v0\n";
    let daemon = spawn_test_daemon(&[("src/Shared.luau", initial)])?;
    let mut plug_a = connect_as(&daemon, "plugin").await?;
    let mut plug_b = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plug_a, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;
    let _ = next_matching(&mut plug_b, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    let merged = "merged-by-A\n";
    let sha = sha256_hex(merged.as_bytes());
    send_json(
        &mut plug_a,
        json!({
            "type": "conflict_resolved_manual",
            "path": "src/Shared.luau",
            "content": merged,
            "sha256": sha,
        }),
    )
    .await?;

    // Plug B should observe the file_changed broadcast triggered by
    // the daemon's write_to_fs call (broadcast_to_studio = true).
    let frame = next_matching(&mut plug_b, Duration::from_secs(5), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("file_changed")
            && v.get("path").and_then(|p| p.as_str()) == Some("src/Shared.luau")
    })
    .await?;
    assert_eq!(
        frame.get("content").and_then(|c| c.as_str()),
        Some(merged),
        "plug B's file_changed must carry the merged content; got {frame}"
    );
    Ok(())
}
