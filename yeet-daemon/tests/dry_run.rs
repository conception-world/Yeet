//! Integration tests for `--dry-run` (T2.2).
//!
//! Dry-run is "log the intent, but suppress the side effect" for every
//! mutating path: write_to_fs, delete_from_fs, push_to_studio,
//! delete_on_studio. Tree state stays untouched, so divergences resurface
//! on every reconcile cycle. The test exercises the most user-visible
//! contract: a Studio edit that would normally be written to disk does
//! NOT touch disk.

mod common;

use std::time::Duration;

use anyhow::Result;
use common::{connect_as, next_matching, send_json, spawn_test_daemon_with_args};
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

/// In dry-run mode, a `file_changed` arriving from the plugin (i.e. a
/// Studio edit that the daemon would normally reconcile to disk) does
/// NOT update the on-disk file. The disk content stays as-is.
#[tokio::test(flavor = "multi_thread")]
async fn dry_run_suppresses_disk_writes_for_studio_edits() -> Result<()> {
    let initial = "before";
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

    // Plugin reports a Studio edit. In a non-dry-run daemon this would
    // flow through reconcile_path → write_to_fs and the disk file would
    // become "after edit". With --dry-run, write_to_fs short-circuits
    // after recording the intent.
    let new_content = "after edit";
    let sha = sha256_hex(new_content.as_bytes());
    send_json(
        &mut plugin,
        json!({
            "type": "file_changed",
            "path": "src/Foo.luau",
            "content": new_content,
            "sha256": sha,
        }),
    )
    .await?;

    // Give the daemon plenty of time to process the frame; in non-dry
    // mode the disk write would be visible within milliseconds.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let actual = std::fs::read_to_string(&abs)?;
    assert_eq!(
        actual, initial,
        "--dry-run must NOT write to disk; disk should still contain the initial content"
    );
    Ok(())
}

/// Sanity check: without `--dry-run` the same scenario DOES write to
/// disk. Pinning this contract so a future refactor that accidentally
/// rewires the dry-run check (e.g. inverts the boolean) breaks both
/// tests, not just one — making the regression obvious.
#[tokio::test(flavor = "multi_thread")]
async fn without_dry_run_studio_edits_reach_disk() -> Result<()> {
    let initial = "before";
    let daemon = spawn_test_daemon_with_args(&[("src/Foo.luau", initial)], &[])?;
    let abs = daemon.project_root.path().join("src/Foo.luau");
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    let new_content = "after edit";
    let sha = sha256_hex(new_content.as_bytes());
    send_json(
        &mut plugin,
        json!({
            "type": "file_changed",
            "path": "src/Foo.luau",
            "content": new_content,
            "sha256": sha,
        }),
    )
    .await?;

    // Poll until the write lands or the deadline fires.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let actual = std::fs::read_to_string(&abs)?;
        if actual == new_content {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "non-dry-run daemon did not write to disk within timeout; got {:?}",
                actual
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
