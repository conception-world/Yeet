//! Integration tests for the heartbeat / liveness pipeline (T2.1).
//!
//! These exercise the daemon side only — the plugin's `Reconnect.luau`
//! sends Pings on a 30s timer and tracks Pong roundtrips, but we don't
//! have a Luau VM in `cargo test`. Instead we drive the daemon with a
//! raw `tokio_tungstenite` client that mimics the same wire shape.

mod common;

use std::time::Duration;

use anyhow::Result;
use common::{connect_as, next_matching, send_json, spawn_test_daemon};
use serde_json::json;

/// Daemon must reply to a `ping` with a `pong` echoing the same `seq`.
/// This is the round-trip the plugin's heartbeat task relies on; if it
/// breaks the plugin will spuriously force reconnects every 90s.
#[tokio::test(flavor = "multi_thread")]
async fn ping_is_answered_with_matching_pong_seq() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/A.luau", "v1")])?;
    let mut plugin = connect_as(&daemon, "plugin").await?;
    // Drain the handshake (ProjectOpened) before sending pings — without
    // this, the next_matching below could trip over leftover handshake
    // frames.
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    send_json(&mut plugin, json!({"type": "ping", "seq": 7})).await?;
    let pong = next_matching(&mut plugin, Duration::from_secs(2), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("pong")
    })
    .await?;
    assert_eq!(
        pong.get("seq").and_then(|s| s.as_u64()),
        Some(7),
        "pong should echo the request seq verbatim"
    );
    Ok(())
}

/// Bursts of pings under the rate-limit cap (default 100/s) all get
/// answered. Documents the seq-pairing contract so a future change that
/// e.g. coalesces consecutive pings won't silently regress.
#[tokio::test(flavor = "multi_thread")]
async fn burst_of_pings_each_get_individual_pongs() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/A.luau", "v1")])?;
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    // 5 pings is well under the rate-limit cap (100 tokens) so all
    // should pass and produce 5 pongs in seq order.
    let want = [11_u64, 12, 13, 14, 15];
    for &seq in &want {
        send_json(&mut plugin, json!({"type": "ping", "seq": seq})).await?;
    }
    let mut got: Vec<u64> = Vec::new();
    while got.len() < want.len() {
        let pong = next_matching(&mut plugin, Duration::from_secs(2), |v| {
            v.get("type").and_then(|t| t.as_str()) == Some("pong")
        })
        .await?;
        if let Some(s) = pong.get("seq").and_then(|s| s.as_u64()) {
            got.push(s);
        }
    }
    assert_eq!(got, want, "every ping must get a unique pong with matching seq");
    Ok(())
}
