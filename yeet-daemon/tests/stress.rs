//! Stress / large-project tests.
//!
//! These exist to keep us honest about the "does this scale?" claims in
//! the audit doc. The numbers are intentionally below the absolute caps
//! (e.g. 1000 not 6000 files) so the test runs in a few seconds rather
//! than tens — but they're large enough that any O(N²) regression in
//! bootstrap or any per-file synchronous IO would visibly time out.

mod common;

use std::time::{Duration, Instant};

use anyhow::Result;
use common::{connect_as, next_matching, spawn_test_daemon};

/// Generates a project with N files, connects as plugin, and asserts
/// that the bootstrap `ProjectOpened` arrives within `deadline_secs`
/// AND lists exactly N entries in `initial_files`. Catches:
///   * the "need 2 bootstraps" bug (T0.x): incomplete `initial_files`
///   * the WebSocket frame-size cap (Phase 1 fix): payload not chunked
///   * any per-file O(N²) regression: timing out at ~1-2 minutes
fn stress_bootstrap_for(file_count: usize, deadline_secs: u64) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        // Build the on-disk fixture inline so the harness's `&[(&str,&str)]`
        // input shape doesn't force the test to box hundreds of strings
        // up-front. We collect into Vec<(String,String)> then borrow.
        let owned: Vec<(String, String)> = (0..file_count)
            .map(|i| {
                let path = format!("src/scripts/Mod{i:04}.luau");
                let body = format!("-- module {i}\nreturn {{ id = {i} }}\n");
                (path, body)
            })
            .collect();
        let borrowed: Vec<(&str, &str)> = owned
            .iter()
            .map(|(p, b)| (p.as_str(), b.as_str()))
            .collect();

        let started = Instant::now();
        let daemon = spawn_test_daemon(&borrowed)?;
        let mut plugin = connect_as(&daemon, "plugin").await?;
        let opened = next_matching(
            &mut plugin,
            Duration::from_secs(deadline_secs),
            |v| v.get("type").and_then(|t| t.as_str()) == Some("project_opened"),
        )
        .await?;
        let elapsed = started.elapsed();

        let entries = opened
            .get("initial_files")
            .and_then(|f| f.as_array())
            .expect("project_opened.initial_files must be an array");
        assert_eq!(
            entries.len(),
            file_count,
            "expected {file_count} initial_files entries, got {}",
            entries.len()
        );
        eprintln!(
            "[stress] bootstrapped {file_count} files in {:.2}s",
            elapsed.as_secs_f64()
        );
        Ok(())
    })
}

/// 1000 files. Sized to fit comfortably under the 16 MiB pre-fix
/// frame cap as a baseline. Should be fast (<5s on developer hardware).
#[test]
fn bootstrap_one_thousand_files_completes_in_one_cycle() -> Result<()> {
    stress_bootstrap_for(1000, 30)
}

/// 3000 files. Sized to be larger than typical Roblox projects but
/// still well under the 256 MiB cap. Catches scaling regressions that
/// the 1k test misses (anything quadratic in bootstrap, IO contention,
/// rescan_fs races).
#[test]
fn bootstrap_three_thousand_files_completes_in_one_cycle() -> Result<()> {
    stress_bootstrap_for(3000, 60)
}

/// Reproduces the user's reported scenario: bootstrap-preview path
/// with ~848 files all `IdeOnly` (Studio empty, disk full). Pins that
/// the daemon emits a single `BulkSyncPreview` frame containing every
/// entry within the deadline, even at the size that previously
/// correlated with the "interface não aparece" symptom on the plugin
/// side.
///
/// Doesn't validate the dock rendering itself (Luau-only — not
/// reachable from cargo). What this DOES rule out: the daemon failing
/// to emit, the wire frame being malformed, or the broadcast lagging
/// past the plugin's heartbeat timeout.
#[test]
fn bootstrap_preview_with_848_files_emits_single_bulk_sync_preview() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let file_count = 848;
        let owned: Vec<(String, String)> = (0..file_count)
            .map(|i| {
                let path = format!("src/Mod{i:04}.luau");
                let body = format!("-- module {i}\nlocal x = {i}\nreturn x\n");
                (path, body)
            })
            .collect();
        let borrowed: Vec<(&str, &str)> = owned
            .iter()
            .map(|(p, b)| (p.as_str(), b.as_str()))
            .collect();

        let daemon = common::spawn_test_daemon(&borrowed)?;

        // Hand-rolled hello with `request_bootstrap_preview = true` —
        // the harness's `connect_as` doesn't carry that flag.
        use anyhow::Context;
        use futures_util::SinkExt;
        use serde_json::json;
        use tokio_tungstenite::{
            connect_async,
            tungstenite::{client::IntoClientRequest, Message},
        };
        let request = daemon.ws_url().into_client_request().context("uri")?;
        let (mut plugin, _resp) = connect_async(request).await.context("ws connect")?;
        let hello = json!({
            "type": "hello",
            "version": "0.3.0",
            "role": "plugin",
            "studio_snapshot": [],
            "request_bootstrap_preview": true,
            "auth_token": daemon.auth_token(),
        });
        plugin.send(Message::Text(hello.to_string())).await.context("hello")?;

        // Drain ProjectOpened.
        let _ = common::next_matching(&mut plugin, Duration::from_secs(60), |v| {
            v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
        })
        .await?;

        // Plugin would call TreeBuilder.discover here; we short-circuit
        // by sending an empty StudioSnapshotReport (Studio has nothing).
        // Daemon will diff: every disk file → IdeOnly entry.
        common::send_json(
            &mut plugin,
            json!({"type": "studio_snapshot_report", "snapshot": []}),
        )
        .await?;

        let started = Instant::now();
        let preview = common::next_matching(&mut plugin, Duration::from_secs(60), |v| {
            v.get("type").and_then(|t| t.as_str()) == Some("bulk_sync_preview")
        })
        .await?;
        let elapsed = started.elapsed();

        let entries = preview
            .get("entries")
            .and_then(|e| e.as_array())
            .expect("bulk_sync_preview.entries must be an array");
        assert_eq!(
            entries.len(),
            file_count,
            "expected {file_count} bulk preview entries, got {}",
            entries.len()
        );
        eprintln!(
            "[stress] bulk_sync_preview with {file_count} entries in {:.2}s ({} bytes serialized)",
            elapsed.as_secs_f64(),
            preview.to_string().len()
        );
        Ok(())
    })
}
