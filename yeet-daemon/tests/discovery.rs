//! Integration tests for the `role = "discover"` probe.
//!
//! This is the wire contract the Studio plugin's port scan depends on, and the
//! plugin has no test runner of its own — so these tests are the only automated
//! guard against a change here silently breaking the project picker.
//!
//! The probe is deliberately unauthenticated and answered before the version
//! gate, so the tests drive a raw socket rather than `common::connect_as`
//! (which always presents a token).

mod common;

use std::time::Duration;

use anyhow::{Context, Result};
use common::{TestDaemon, connect_as, next_matching, spawn_test_daemon};
use futures_util::SinkExt as _;
use serde_json::{Value, json};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

/// Opens a socket, sends a discovery Hello with no auth token, and returns the
/// first frame. `version` is a parameter so a test can probe as a client the
/// daemon would otherwise reject.
async fn probe_with_version(daemon: &TestDaemon, version: &str) -> Result<Value> {
    let request = daemon.ws_url().into_client_request().context("uri")?;
    let (mut ws, _resp) = connect_async(request).await.context("ws connect")?;
    let hello = json!({
        "type": "hello",
        "version": version,
        "role": "discover",
    });
    ws.send(Message::Text(hello.to_string()))
        .await
        .context("send discovery hello")?;
    next_matching(&mut ws, Duration::from_secs(3), |_| true).await
}

async fn probe(daemon: &TestDaemon) -> Result<Value> {
    probe_with_version(daemon, "0.5.0").await
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// The shape the plugin's picker parses. Every field it renders must be
/// present, and the frame must be exactly `daemon_info`.
#[tokio::test(flavor = "multi_thread")]
async fn discovery_probe_returns_daemon_identity() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/A.luau", "v1")])?;
    let info = probe(&daemon).await?;

    assert_eq!(str_field(&info, "type"), Some("daemon_info"));

    let daemon_id = str_field(&info, "daemon_id").context("daemon_id missing")?;
    assert_eq!(daemon_id.len(), 16, "daemon_id should be 16 hex chars");
    assert!(
        daemon_id.chars().all(|c| c.is_ascii_hexdigit()),
        "daemon_id should be hex, got {daemon_id:?}"
    );

    assert!(
        str_field(&info, "project_name").is_some(),
        "the picker labels each row with the project name"
    );
    let root = str_field(&info, "project_root").context("project_root missing")?;
    assert!(
        !root.is_empty(),
        "the picker shows the root to disambiguate same-named projects"
    );
    assert!(
        !root.starts_with(r"\\?\"),
        "the Windows verbatim prefix must be stripped before it reaches a client, \
         got {root:?}"
    );
    assert!(
        str_field(&info, "daemon_version").is_some(),
        "the picker greys out daemons too old for this plugin"
    );
    assert!(
        info.get("port").and_then(Value::as_u64).is_some_and(|p| p > 0),
        "port should be the real bound port"
    );
    assert_eq!(
        info.get("plugin_connected").and_then(Value::as_bool),
        Some(false),
        "no plugin has connected to this daemon yet"
    );
    Ok(())
}

/// A probe must never hand out anything an unauthenticated local process should
/// not have. This is the guard against the `DaemonInfo` payload quietly growing
/// a sensitive field later.
#[tokio::test(flavor = "multi_thread")]
async fn discovery_probe_never_leaks_secrets() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/A.luau", "v1")])?;
    let info = probe(&daemon).await?;

    for forbidden in ["auth_token", "initial_files", "session_id", "project"] {
        assert!(
            info.get(forbidden).is_none(),
            "daemon_info must not carry {forbidden} — the probe is unauthenticated by design"
        );
    }
    // Belt and braces: the daemon's real token must not appear anywhere in the
    // serialized frame, under any key.
    let serialized = info.to_string();
    assert!(
        !serialized.contains(&daemon.auth_token()),
        "the auth token leaked into a discovery reply"
    );
    Ok(())
}

/// The probe is answered BEFORE the version gate, so a plugin the daemon would
/// refuse to serve can still discover it and explain itself to the user. If
/// this regresses, such a daemon becomes invisible in the picker instead.
#[tokio::test(flavor = "multi_thread")]
async fn discovery_answers_even_a_client_too_old_to_connect() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/A.luau", "v1")])?;
    // Below MIN_COMPATIBLE_PLUGIN_VERSION (0.2.0) — a real session with this
    // version is rejected outright.
    let info = probe_with_version(&daemon, "0.1.0").await?;
    assert_eq!(
        str_field(&info, "type"),
        Some("daemon_info"),
        "discovery must not be gated on the client version"
    );
    Ok(())
}

/// The daemon hangs up after answering, so the plugin's scan can settle a port
/// as soon as it has its reply rather than waiting out a timeout per port.
#[tokio::test(flavor = "multi_thread")]
async fn discovery_probe_closes_the_socket_after_replying() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/A.luau", "v1")])?;
    let request = daemon.ws_url().into_client_request().context("uri")?;
    let (mut ws, _resp) = connect_async(request).await.context("ws connect")?;
    ws.send(Message::Text(
        json!({"type": "hello", "version": "0.5.0", "role": "discover"}).to_string(),
    ))
    .await?;

    let info = next_matching(&mut ws, Duration::from_secs(3), |_| true).await?;
    assert_eq!(str_field(&info, "type"), Some("daemon_info"));

    // Drain until the stream ends. A Close frame or a plain end-of-stream both
    // count; what must NOT happen is the socket staying open indefinitely.
    use futures_util::StreamExt as _;
    let closed = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(msg) = ws.next().await {
            match msg {
                Ok(Message::Close(_)) | Err(_) => return true,
                Ok(_) => continue,
            }
        }
        true
    })
    .await
    .unwrap_or(false);
    assert!(closed, "the daemon should close the socket after DaemonInfo");
    Ok(())
}

/// The property the whole scan rests on: probing must be free of side effects.
///
/// A plugin Hello without a `session_id` rotates the session, which clears
/// `pending_deltas` and `pending_applies`. If a probe went down that path, then
/// scanning ten ports would blow away the resume buffer of every other Studio
/// attached to those daemons. Here: connect a plugin, probe the same daemon,
/// and confirm the plugin can still resume on its original session id.
#[tokio::test(flavor = "multi_thread")]
async fn probing_does_not_disturb_a_connected_plugin_session() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/A.luau", "v1")])?;
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let opened = next_matching(&mut plugin, Duration::from_secs(5), |v| {
        str_field(v, "type") == Some("project_opened")
    })
    .await?;
    let session_before =
        str_field(&opened, "session_id").context("session_id missing")?.to_owned();

    // Two probes, as a scan of a small window would produce.
    for _ in 0..2 {
        let info = probe(&daemon).await?;
        assert_eq!(str_field(&info, "type"), Some("daemon_info"));
    }

    // Reconnect claiming the original session. If a probe had rotated it, the
    // daemon would fall back to a full handshake and answer `project_opened`.
    drop(plugin);
    let request = daemon.ws_url().into_client_request().context("uri")?;
    let (mut resumed, _resp) = connect_async(request).await.context("ws connect")?;
    resumed
        .send(Message::Text(
            json!({
                "type": "hello",
                "version": "0.5.0",
                "role": "plugin",
                "studio_snapshot": [],
                "auth_token": daemon.auth_token(),
                "session_id": session_before,
            })
            .to_string(),
        ))
        .await?;
    let frame = next_matching(&mut resumed, Duration::from_secs(5), |v| {
        matches!(str_field(v, "type"), Some("resumed" | "project_opened"))
    })
    .await?;
    assert_eq!(
        str_field(&frame, "type"),
        Some("resumed"),
        "the session id must survive being probed — a probe that rotates it would \
         destroy the resume buffer of every plugin attached to a scanned daemon"
    );
    Ok(())
}

/// `plugin_connected` is what the picker renders as "in use", so it must be
/// true while a plugin holds a session and false again once it leaves. The
/// false-after case is the one that rots easily: the flag is cleared by a Drop
/// guard precisely because `run_plugin_session` has many exit paths.
#[tokio::test(flavor = "multi_thread")]
async fn plugin_connected_tracks_the_live_session() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/A.luau", "v1")])?;

    let before = probe(&daemon).await?;
    assert_eq!(
        before.get("plugin_connected").and_then(Value::as_bool),
        Some(false)
    );

    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plugin, Duration::from_secs(5), |v| {
        str_field(v, "type") == Some("project_opened")
    })
    .await?;

    let during = probe(&daemon).await?;
    assert_eq!(
        during.get("plugin_connected").and_then(Value::as_bool),
        Some(true),
        "a live plugin session must show as in use"
    );

    drop(plugin);
    // Give the daemon a moment to notice the closed socket and run the guard.
    let mut cleared = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let after = probe(&daemon).await?;
        if after.get("plugin_connected").and_then(Value::as_bool) == Some(false) {
            cleared = true;
            break;
        }
    }
    assert!(
        cleared,
        "plugin_connected must clear when the session ends, or every daemon a user \
         ever connected to would read as permanently 'in use'"
    );
    Ok(())
}

/// `daemon_id` is the key the plugin will namespace its stored auth token by,
/// so the value discovery reports and the value a real session reports must be
/// the same. If they diverged, a plugin would save its token under one key and
/// look for it under another — and silently re-pair on every connect.
#[tokio::test(flavor = "multi_thread")]
async fn daemon_id_matches_between_discovery_and_project_opened() -> Result<()> {
    let daemon = spawn_test_daemon(&[("src/A.luau", "v1")])?;
    let info = probe(&daemon).await?;
    let discovered = str_field(&info, "daemon_id").context("daemon_id missing")?.to_owned();

    let mut plugin = connect_as(&daemon, "plugin").await?;
    let opened = next_matching(&mut plugin, Duration::from_secs(5), |v| {
        str_field(v, "type") == Some("project_opened")
    })
    .await?;
    let from_session = str_field(&opened, "daemon_id")
        .context("project_opened must carry daemon_id for plugins that skipped discovery")?;

    assert_eq!(
        discovered, from_session,
        "the same daemon must report one identity on both paths"
    );
    Ok(())
}
