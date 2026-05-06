//! Integration tests for protocol versioning (T1.7) and rate limiting
//! (T2.8).

mod common;

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
};

use common::{connect_as, next_matching, send_json, spawn_test_daemon};

/// Plugin reporting a `version` below `MIN_COMPATIBLE_PLUGIN_VERSION`
/// must be rejected at the handshake — the daemon closes the socket
/// instead of silently accepting frames it might misinterpret.
#[tokio::test(flavor = "multi_thread")]
async fn pre_compat_version_rejected_at_handshake() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/A.luau", "v1")])?;
    let request = daemon.ws_url().into_client_request().context("uri")?;
    let (mut ws, _resp) = connect_async(request).await.context("ws connect")?;

    // "0.1.99" is below the current floor of "0.2.0" by semver
    // precedence (minor 1 < minor 2). The daemon should bail and
    // close the socket. The floor was lowered to 0.2.0 to accept
    // the plugin layout cached by Studio across reloads; anything
    // truly below that range is still rejected.
    let hello = json!({
        "type": "hello",
        "version": "0.1.99",
        "role": "plugin",
        "studio_snapshot": [],
    });
    ws.send(Message::Text(hello.to_string()))
        .await
        .context("send hello")?;

    // The daemon's handshake bails via `?`, which propagates up and the
    // tokio task closes the WebSocket. From the client perspective the
    // stream returns either Close or None within a short window.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        let step = Duration::from_millis(200);
        match tokio::time::timeout(step, ws.next()).await {
            Ok(None) | Ok(Some(Ok(Message::Close(_)))) => return Ok(()),
            Ok(Some(Err(_))) => return Ok(()),
            // Daemon emits "client connected" log and bails — no text
            // frames should reach us. Anything else is a regression.
            Ok(Some(Ok(Message::Text(t)))) => {
                panic!("expected disconnect, got text frame: {t}");
            }
            Ok(Some(Ok(_))) | Err(_) => continue,
        }
    }
    panic!("daemon did not close the socket after pre-compat hello");
}

/// Regression test for the lex-vs-semver bug. Lexically `"0.10.0" <
/// "0.3.0"` (because '1' < '3'), but semver-wise 0.10.0 is GREATER —
/// the daemon's first minor bump past 0.9.x would have locked out
/// every newer plugin under the old comparison. This pins that the
/// daemon now compares with the `semver` crate.
#[tokio::test(flavor = "multi_thread")]
async fn future_minor_version_is_accepted_by_semver() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/A.luau", "v1")])?;
    let request = daemon.ws_url().into_client_request().context("uri")?;
    let (mut ws, _resp) = connect_async(request).await.context("ws connect")?;
    // 0.10.0 is lex-less than 0.3.0 but semver-greater. Must be accepted.
    let hello = json!({
        "type": "hello",
        "version": "0.10.0",
        "role": "plugin",
        "studio_snapshot": [],
        "auth_token": daemon.auth_token(),
    });
    ws.send(Message::Text(hello.to_string()))
        .await
        .context("send hello")?;
    let opened = next_matching(&mut ws, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;
    assert!(opened.get("daemon_version").and_then(|v| v.as_str()).is_some());
    assert!(opened.get("project_root").and_then(|v| v.as_str()).is_some());
    Ok(())
}

/// Malformed version strings (missing dots, empty, garbage) are
/// rejected at the parse step — the daemon refuses to guess what the
/// client meant. Without this, the old lex comparison silently
/// accepted "" because empty < anything.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_version_rejected() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/A.luau", "v1")])?;
    for bad in ["", "garbage", "0", "0.3", "1.0.0.1.beta"] {
        let request = daemon.ws_url().into_client_request().context("uri")?;
        let (mut ws, _resp) = connect_async(request).await.context("ws connect")?;
        let hello = json!({
            "type": "hello",
            "version": bad,
            "role": "plugin",
            "studio_snapshot": [],
        });
        ws.send(Message::Text(hello.to_string()))
            .await
            .context("send hello")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut closed = false;
        while std::time::Instant::now() < deadline {
            let step = Duration::from_millis(150);
            match tokio::time::timeout(step, ws.next()).await {
                Ok(None) | Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) => {
                    closed = true;
                    break;
                }
                Ok(Some(Ok(Message::Text(t)))) => {
                    panic!("expected disconnect for version {bad:?}, got {t}");
                }
                _ => continue,
            }
        }
        assert!(closed, "daemon did not close socket for malformed version {bad:?}");
    }
    Ok(())
}

/// Sanity: the current advertised version "0.3.0" is accepted, and
/// `ProjectOpened` carries the new `daemon_version` + `project_root`
/// fields the plugin needs.
#[tokio::test(flavor = "multi_thread")]
async fn floor_version_is_accepted() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/A.luau", "v1")])?;
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let opened = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;
    assert!(opened.get("session_id").is_some());
    // Pin the new wire fields so a future protocol change can't drop
    // them silently — the plugin relies on both for project-identity
    // and version-mismatch detection.
    let dv = opened
        .get("daemon_version")
        .and_then(|v| v.as_str())
        .expect("project_opened must carry daemon_version");
    assert!(!dv.is_empty(), "daemon_version must not be empty");
    let pr = opened
        .get("project_root")
        .and_then(|v| v.as_str())
        .expect("project_opened must carry project_root");
    assert!(!pr.is_empty(), "project_root must not be empty");
    Ok(())
}

/// Burst >RATE_LIMIT_CAPACITY frames within <1s. The bucket starts full
/// (100 tokens) and refills at 100/s, so a burst of 200 pings sent
/// back-to-back exhausts the bucket — about half should be dropped
/// without crashing the daemon.
///
/// Note: we count Pongs rather than asserting "exactly N dropped"
/// because the bucket math depends on wall-clock timing of the send +
/// processing loop, which is flaky to assert tightly. The contract
/// being pinned is "rate limiter doesn't crash and DOES drop some".
#[tokio::test(flavor = "multi_thread")]
async fn rate_limited_burst_drops_some_pongs_without_crashing() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/A.luau", "v1")])?;
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    // Send 250 pings as fast as possible. The rate limit (100 tokens
    // capacity, 100/s refill) should cause some to be dropped.
    let burst_size = 250_u64;
    for seq in 0..burst_size {
        send_json(&mut plugin, json!({"type": "ping", "seq": seq})).await?;
    }

    // Drain pongs for up to 3s, count how many we received. Anything
    // less than `burst_size` proves the limiter dropped frames; we
    // don't assert a tight upper bound because token refill during the
    // drain itself adds capacity unpredictably.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut got = 0_u64;
    while std::time::Instant::now() < deadline {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or(Duration::ZERO);
        let step = Duration::from_millis(150).min(remaining);
        match tokio::time::timeout(step, plugin.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                    if v.get("type").and_then(|s| s.as_str()) == Some("pong") {
                        got += 1;
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => {
                if got > 0 {
                    break;
                }
            }
        }
    }
    assert!(
        got > 0,
        "rate limiter swallowed 100% of frames — bucket appears completely closed"
    );
    assert!(
        got < burst_size,
        "rate limiter let everything through ({got} of {burst_size}) — \
         the cap is not engaged"
    );
    eprintln!("[rate_limit] {got} pongs out of {burst_size} pings");
    Ok(())
}
