//! Test harness for spinning up the real `yeet-daemon` binary as a
//! subprocess. Used by integration tests that need to exercise the
//! WebSocket plumbing — handshake, broadcast, resume, concurrent
//! clients — without rebuilding that pipeline in-process.
//!
//! The harness:
//!   * picks an ephemeral port via `--bind 127.0.0.1:0`,
//!   * scrapes the daemon's stderr for the `yeet-daemon listening
//!     addr=…` line so the test learns the actual port,
//!   * exposes a `connect_*` helper to open a `tokio_tungstenite`
//!     client and send the role-`Hello`,
//!   * shuts the daemon down on `Drop`.
//!
//! Tests should reach the binary via `env!("CARGO_BIN_EXE_yeet-daemon")`
//! which Cargo defines for the integration-test target.

#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
    MaybeTlsStream, WebSocketStream,
};

/// Handle to a running daemon. `Drop` terminates the process so each
/// test owns an isolated instance.
pub struct TestDaemon {
    pub addr: String,
    pub project_root: tempfile::TempDir,
    child: Option<Child>,
}

impl TestDaemon {
    pub fn ws_url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// Reads the daemon's auth token from `<project_root>/.yeet/auth-token`.
    /// The daemon writes this file synchronously during bootstrap, before
    /// it logs the listening address — by the time the harness has the
    /// addr (and the test calls `connect_as`), the file is guaranteed to
    /// exist. Tests use this to populate `Hello.auth_token` so the
    /// handshake passes the auth gate.
    pub fn auth_token(&self) -> String {
        let path = self.project_root.path().join(".yeet").join("auth-token");
        // Brief retry so a slow filesystem doesn't fail the read race
        // with daemon startup. The daemon emits the listening line
        // AFTER writing the token, so this is paranoia.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match std::fs::read_to_string(&path) {
                Ok(s) => return s,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => panic!("could not read {}: {e}", path.display()),
            }
        }
    }

    /// Convenience: writes a `.yeet/pairing` breadcrumb at the
    /// project root with the current unix timestamp. Used by tests
    /// that exercise the plugin pairing flow without going through
    /// the full extension command. The daemon's auth dance accepts
    /// this exactly the same way it would accept one written by the
    /// real extension — there's no extension-vs-test distinction at
    /// the wire level.
    pub fn write_pairing_breadcrumb(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let dir = self.project_root.path().join(".yeet");
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
        let path = dir.join("pairing");
        std::fs::write(&path, now.to_string())
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Bring up a daemon against a project containing the supplied files.
/// `disk_files` entries are `(rel_path, content)` and are written under
/// a single `src` mount mapped to `ServerScriptService`.
pub fn spawn_test_daemon(disk_files: &[(&str, &str)]) -> Result<TestDaemon> {
    spawn_test_daemon_with_args(disk_files, &[])
}

/// Same as `spawn_test_daemon` but lets the caller pass extra CLI flags
/// to the daemon (e.g. `--dry-run`, `--reset-base-tree`). Used by tests
/// that exercise mode-specific behaviour without cloning the harness.
pub fn spawn_test_daemon_with_args(
    disk_files: &[(&str, &str)],
    extra_args: &[&str],
) -> Result<TestDaemon> {
    let dir = tempfile::tempdir().context("tempdir")?;
    let root = dir.path();
    let project_json = r#"{
        "name": "TestDaemon",
        "tree": {
            "$className": "DataModel",
            "ServerScriptService": {
                "$className": "ServerScriptService",
                "$path": "src"
            }
        }
    }"#;
    std::fs::write(root.join("default.project.json"), project_json)
        .context("write default.project.json")?;
    std::fs::create_dir(root.join("src")).context("mkdir src")?;
    for (rel, content) in disk_files {
        let abs = root.join(rel);
        if let Some(p) = abs.parent() {
            std::fs::create_dir_all(p).with_context(|| format!("mkdir {}", p.display()))?;
        }
        std::fs::write(&abs, content).with_context(|| format!("write {}", abs.display()))?;
    }

    let exe = env!("CARGO_BIN_EXE_yeet-daemon");
    let mut cmd = Command::new(exe);
    cmd.arg(root.as_os_str())
        .arg("--bind")
        .arg("127.0.0.1:0");
    for extra in extra_args {
        cmd.arg(extra);
    }
    cmd.env("RUST_LOG", "yeet_daemon=info")
        // tracing-subscriber's fmt layer auto-detects a TTY and emits
        // ANSI color escapes when present. Under cargo's test runner
        // stderr happens to look TTY-ish on some shells, and the
        // escapes break our naive `addr=...` substring match. NO_COLOR
        // is the canonical opt-out and forces plain text.
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        // tracing-subscriber's `fmt().init()` writes to STDOUT by default,
        // so we pipe both streams and merge them in the scraper thread —
        // anything the daemon emits to either stream becomes visible
        // through the same loop.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn yeet-daemon")?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    // Pipe both streams through threads that scrape the listening line
    // and forward everything else to the test runner's stderr (visible
    // with `cargo test -- --nocapture`). Either stream might contain
    // the address line depending on the tracing-subscriber writer the
    // daemon was built with, so we listen on both.
    let (addr_tx, addr_rx) = mpsc::channel::<String>();
    spawn_log_scraper(stdout, addr_tx.clone(), "[test-daemon stdout]");
    spawn_log_scraper(stderr, addr_tx, "[test-daemon stderr]");

    let addr = addr_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| anyhow!("daemon did not log listening addr within 5s"))?;

    Ok(TestDaemon {
        addr,
        project_root: dir,
        child: Some(child),
    })
}

fn spawn_log_scraper<R>(reader: R, addr_tx: mpsc::Sender<String>, prefix: &'static str)
where
    R: std::io::Read + Send + 'static,
{
    thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(addr) = parse_addr_from_log(&line) {
                let _ = addr_tx.send(addr);
            }
            eprintln!("{prefix} {line}");
        }
    });
}

fn parse_addr_from_log(line: &str) -> Option<String> {
    // Matches both `addr=127.0.0.1:1234` (current format) and
    // `addr="127.0.0.1:1234"` (if `Display` ever changes).
    let needle = "yeet-daemon listening";
    if !line.contains(needle) {
        return None;
    }
    let key = "addr=";
    let start = line.find(key)? + key.len();
    let rest = &line[start..];
    let token: String = rest
        .chars()
        .skip_while(|c| *c == '"')
        .take_while(|c| !c.is_whitespace() && *c != '"')
        .collect();
    if token.is_empty() { None } else { Some(token) }
}

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Connects to a running daemon and performs the role-Hello handshake.
/// Returns the open WebSocket stream ready for further frames.
pub async fn connect_as(daemon: &TestDaemon, role: &str) -> Result<Ws> {
    let request = daemon.ws_url().into_client_request().context("uri")?;
    // `connect_async` also accepts the URL directly, but going through
    // `IntoClientRequest` keeps the door open for adding headers later.
    let (mut ws, _resp) = connect_async(request).await.context("ws connect")?;
    let hello = serde_json::json!({
        "type": "hello",
        // Bumped to 0.3.0 in lockstep with MIN_COMPATIBLE_PLUGIN_VERSION
        // — the tests must mirror what real plugins/extensions send so
        // the handshake succeeds.
        "version": "0.3.0",
        "role": role,
        "studio_snapshot": [],
        // Auth token read straight from the daemon's
        // <root>/.yeet/auth-token, mirroring what a real extension
        // does. Tests that want to exercise the pair flow should call
        // `connect_as` with role "plugin" and a fresh breadcrumb
        // BEFORE this — but the simpler "always pass the token" path
        // is what the bulk of the tests need.
        "auth_token": daemon.auth_token(),
    });
    ws.send(Message::Text(hello.to_string()))
        .await
        .context("send hello")?;
    Ok(ws)
}

/// Drains every text frame the daemon broadcasts within `timeout`,
/// returning them as parsed JSON values. Stops on the first idle gap
/// longer than 50ms past the first message.
pub async fn drain_frames(ws: &mut Ws, timeout: Duration) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    loop {
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(d) => d,
            None => break,
        };
        let step = if out.is_empty() {
            remaining
        } else {
            Duration::from_millis(80).min(remaining)
        };
        match tokio::time::timeout(step, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                    out.push(v);
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => break,
        }
    }
    out
}

/// Send an arbitrary JSON frame to the daemon.
pub async fn send_json(ws: &mut Ws, value: serde_json::Value) -> Result<()> {
    ws.send(Message::Text(value.to_string()))
        .await
        .context("ws send")
}

/// Convenience: read a single text frame within `timeout`.
pub async fn next_frame(ws: &mut Ws, timeout: Duration) -> Result<serde_json::Value> {
    let frame = tokio::time::timeout(timeout, ws.next())
        .await
        .map_err(|_| anyhow!("ws read timed out after {:?}", timeout))?
        .ok_or_else(|| anyhow!("ws closed unexpectedly"))?
        .context("ws frame")?;
    let text = match frame {
        Message::Text(t) => t,
        other => return Err(anyhow!("expected Text frame, got {other:?}")),
    };
    serde_json::from_str(&text).context("parse frame")
}

/// Read frames until one matches `predicate`, dropping anything else.
/// Useful when several broadcasts may arrive between the one the test
/// cares about (e.g. ProjectOpened comes after BulkSyncPreview etc.).
pub async fn next_matching(
    ws: &mut Ws,
    timeout: Duration,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> Result<serde_json::Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow!("next_matching deadline exceeded"))?;
        let frame = next_frame(ws, remaining).await?;
        if predicate(&frame) {
            return Ok(frame);
        }
    }
}
