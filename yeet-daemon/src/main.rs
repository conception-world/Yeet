use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio_tungstenite::{
    accept_hdr_async_with_config,
    tungstenite::handshake::server::{ErrorResponse, Request, Response},
    tungstenite::http::StatusCode,
    tungstenite::protocol::{Message, WebSocketConfig},
};
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::EnvFilter;

use yeet_daemon::audit;
use yeet_daemon::auth;
use yeet_daemon::merge::{
    ConflictHunk, ConflictKind, FileConflict, MergeOutcome, ResolvedAction, Side, merge_file,
    resolve_to_action,
};
use yeet_daemon::project::Project;
use yeet_daemon::protocol::{
    BulkSyncAction, BulkSyncDirection, BulkSyncEntry, BulkSyncEntryStatus, BulkSyncFailure,
    BulkSyncResolution, ClientMsg, FileConflictView, FileResolution, FileSnapshot, ScriptKind,
    SerializedInstance, ServerMsg, StudioFileSnapshot, SyncErrorKind, SyncbackMode,
    SyncbackTemplate, classify, is_init_filename,
};
use yeet_daemon::state::{
    FileMeta, FsRemovedPending, ProjectState, SharedState, encode_for_disk, is_meta_file,
    normalize_from_disk, resolve_inside, sha256_hex,
};
use yeet_daemon::syncback::{self, SyncbackOptions, SyncbackSession, SyncbackSessions};
use yeet_daemon::tree::{self, TreeEntry};
use yeet_daemon::watcher::{self, FileEvent};

const BIND_ADDR: &str = "127.0.0.1:34872";
const PROJECT_FILE: &str = "default.project.json";
const EVENT_CHANNEL_CAPACITY: usize = 256;
// Sized for bursty IDE operations: format-on-save across a folder, git
// checkout/pull rewriting many files, search-and-replace across the project.
// 256 (the previous value) was easy to overrun, and the receive-side `Lagged`
// branch silently dropped events with no recovery — files would simply not
// appear in Studio. The ceiling here is paired with the Lagged handler below
// (which now disconnects so the plugin's Reconnect drains `pending_deltas`),
// but a generous buffer keeps the disconnect path off the hot path.
const BROADCAST_CAPACITY: usize = 4096;
/// Maximum size of a single WebSocket frame/message the daemon will accept
/// from or send to a client. Sized to comfortably hold the largest legal
/// `content` payload (10 MiB, see `MAX_CONTENT_BYTES`) plus JSON envelope,
/// metadata, and bulk-sync batches. **Was 256 MiB before the v1.0 audit
/// hardening pass**; lowered to 16 MiB because tungstenite buffers the
/// entire frame before yielding it, so a multi-connection attacker could
/// stack N × 256 MiB allocations before any rate-limit or content-size
/// check kicks in. With the lower cap and the connection cap below,
/// peak attacker-controlled buffering is bounded at
/// `WS_MAX_FRAME_BYTES × MAX_CONCURRENT_CONNECTIONS = 64 MiB`.
/// If a future protocol change ships content > 10 MiB, raise this in
/// lockstep — but think hard about chunking on the wire instead of
/// bumping the cap.
const WS_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const WS_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// Maximum number of concurrent WebSocket connections the daemon serves
/// at once. Real usage is `1 plugin + 1 extension = 2`; the cap is set
/// to `4` to leave headroom for a brief reconnect overlap (old socket
/// still draining while new one finishes handshake) without rejecting
/// legitimate traffic. Excess connections are accepted at the TCP layer
/// then immediately dropped before the WS upgrade — preserves the
/// "anyone can probe" property without letting a flood saturate memory
/// or CPU. Pairs with `WS_MAX_FRAME_BYTES` to bound peak attacker memory
/// (worst case: 4 × 16 MiB = 64 MiB).
const MAX_CONCURRENT_CONNECTIONS: usize = 4;
/// Threshold above which `write_frame` logs the outbound payload size at
/// `warn!`. Sized so steady-state `FileChanged` frames (~10s of KB) stay at
/// `trace!` and only handshake / bulk-sync payloads draw attention — the
/// signal is "did the bootstrap-sized message actually fit?", not raw frame
/// counts.
const WS_LARGE_FRAME_LOG_BYTES: usize = 1024 * 1024;
/// Per-connection inbound frame rate cap in messages per second. Healthy
/// usage tops out around the SourceWatcher's 5 Hz debounce per file × a
/// handful of concurrently-edited files; the cap leaves >10× headroom and
/// just protects against a buggy or compromised client flooding the
/// daemon's main loop with parse work. Sized in tokens so a brief burst
/// (e.g. opening a place spams 50 FileChanged echoes) doesn't trip it.
const RATE_LIMIT_CAPACITY: f64 = 100.0;
const RATE_LIMIT_PER_SEC: f64 = 100.0;
/// Wall-clock window after which a connection that has neither read nor
/// written anything is considered dead and dropped. The plugin's heartbeat
/// (T2.1, ping every 30s) keeps a healthy session well inside this window;
/// an actual zombie (TCP socket dead but neither side noticed because OS
/// keepalive is hours) gets cleaned up here. Set generous enough that the
/// pre-T2.1 plugin (no heartbeat) doesn't get evicted during normal idle.
const WS_IDLE_TIMEOUT_SECS: u64 = 600;
/// How often the idle watchdog wakes up to check `WS_IDLE_TIMEOUT_SECS`.
/// 30s gives a reasonable detection latency without burning a tokio task
/// timer on a per-second tick.
const WS_IDLE_CHECK_SECS: u64 = 30;
/// Minimum plugin/extension `Hello { version }` value the daemon will accept.
/// Compared via `semver::Version`, NOT lexicographically.
///
/// Set to `"0.2.0"` deliberately: that version is the latest one
/// actively in the wild (Studio plugin layout cache often holds it
/// across Studio relaunches even after the new .rbxm is dropped into
/// the Plugins folder). Rejecting it would force users to manually
/// flush the Studio plugin cache to upgrade — which they shouldn't
/// have to do for what's a backwards-compatible wire change.
///
/// The daemon is robust to old plugins missing newer fields
/// (`auth_token`, etc.) because every new field is `#[serde(default)]`
/// or `Option<T>`. The auth gate (`authenticate_or_pair`) is
/// best-effort and never bails, so a v0.2.0 plugin that doesn't know
/// about `auth_token` is treated as "no token offered" and proceeds.
/// Bump this floor only when a future protocol change actually
/// breaks old plugins in a way the daemon can't handle.
const MIN_COMPATIBLE_PLUGIN_VERSION: &str = "0.2.0";
/// Hard cap on the size of a single `content` payload the daemon will accept
/// from a client, in bytes. 10 MiB is well above any realistic script (the
/// largest script in Roblox core is under 300 KB) and well below what would
/// plausibly be a memory-pressure vector. Exists mostly as a guard against
/// a misbehaving or compromised client shipping a gigabyte blob.
const MAX_CONTENT_BYTES: usize = 10 * 1024 * 1024;

/// Human-readable build identifier appended to `daemon_version` in
/// `ProjectOpened`. Bump whenever a behaviour-visible daemon change ships
/// so the plugin dock log proves which binary is talking to it (the user
/// can grep the dock for this string to verify a respawn happened).
const BUILD_STAMP: &str = "rename-surface-errors-1";

/// How long the daemon waits after a watcher `Removed` event before
/// committing it as a real delete, looking for a matching `Touched`
/// with the same `sha256` in the meantime. Pairing them yields a
/// `FileRenamed` instead of Delete+Create — Studio-side state survives.
/// Sized above the watcher's 100 ms debounce so the From/To pair always
/// lands inside the window even on slower platforms.
const FS_RENAME_PAIR_TTL: Duration = Duration::from_millis(250);

struct CliArgs {
    project_root: PathBuf,
    debug_echo: bool,
    /// Override of `BIND_ADDR`. Set via `--bind <addr>` so integration
    /// tests can spawn the daemon on `127.0.0.1:0` and read the actual
    /// port back from the `yeet-daemon listening addr=...` log line.
    /// Production users never need this; the default is the hard-coded
    /// `BIND_ADDR` the plugin and extension expect.
    bind: Option<String>,
    /// `--reset-base-tree`: deletes the on-disk merge base before bootstrap
    /// runs. Use when `tree_base` has gotten into a confused state (rare;
    /// usually a daemon crash mid-write or hand-edited base tree). The
    /// next plugin connect goes through a full handshake and re-derives
    /// the base from the current Studio + IDE state — the user gets a
    /// `BulkSyncPreview` for every divergent file and picks per-row.
    reset_base_tree: bool,
    /// `--dry-run`: every mutation path (disk write, disk delete, push to
    /// Studio, delete on Studio) records its intent to the audit log and
    /// then no-ops. State (`tree_fs`, `tree_studio`, `tree_base`) is left
    /// untouched, so subsequent reconciles re-discover the same
    /// divergences — the daemon never converges, on purpose. Lets the
    /// user see exactly what a real sync would do (via `BulkSyncPreview`
    /// + audit log entries) before exposing a production place to actual
    /// writes. Toggle off by restarting without the flag.
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    // Build stamp so users can verify they're running the binary with the
    // latest sweep / init.luau / pruning fixes. Bumped when behaviour-
    // visible changes ship; lets a "still seeing X" report trivially
    // confirm whether the daemon actually picked up the new code.
    info!(
        version = env!("CARGO_PKG_VERSION"),
        build = BUILD_STAMP,
        "yeet-daemon starting"
    );
    let args = parse_args()?;
    info!(
        root = %args.project_root.display(),
        debug_echo = args.debug_echo,
        reset_base_tree = args.reset_base_tree,
        "loading project"
    );
    if args.reset_base_tree {
        // User asked for a hard reset of the merge base. Delete the on-
        // disk base tree before bootstrap runs so `ProjectState::bootstrap`
        // sees no prior history and re-derives everything from a fresh
        // handshake. The next plugin connect produces a `BulkSyncPreview`
        // that names every divergent file — the user picks per-row.
        if let Err(e) = yeet_daemon::tree::delete_base_tree(&args.project_root) {
            warn!(error = ?e, "failed to delete base tree on --reset-base-tree");
        } else {
            info!("--reset-base-tree: deleted .yeet/base-tree.msgpack");
        }
    }
    let project = Project::load(&args.project_root.join(PROJECT_FILE))
        .with_context(|| format!("load {PROJECT_FILE}"))?;

    // Spawn the file watcher BEFORE the initial scan / sweep / event_pump so
    // that any IDE writes during the startup window are queued in `fs_rx`
    // instead of silently lost. Without this, a save that happens between
    // `rescan_fs` (which captures `tree_fs` once) and `watcher::spawn`
    // (which starts observing) is invisible to the daemon: `tree_fs` keeps
    // the pre-save content, and the next bootstrap-preview reports no
    // divergence — so the file is missing from Studio until the user
    // disconnects+reconnects (forcing a fresh `rescan_fs`). Reported as
    // "need 2 bootstraps to get all files across".
    //
    // Ordering invariant: watcher → rescan_fs → event_pump. Events that
    // fire during rescan accumulate in the bounded mpsc; event_pump drains
    // them as soon as it spins up, overwriting any stale `tree_fs` entries
    // with the post-save content (the merge logic handles that fine — same
    // path, newer hash wins).
    let (fs_tx, fs_rx) = mpsc::channel::<FileEvent>(EVENT_CHANNEL_CAPACITY);
    let _watcher = watcher::spawn(&args.project_root, fs_tx)?;

    let state_inner =
        ProjectState::bootstrap(&args.project_root, project, args.debug_echo, args.dry_run)?;
    if args.dry_run {
        warn!(
            "DRY-RUN MODE: every disk write, disk delete, push to Studio and delete on \
             Studio will be logged to the audit log and SUPPRESSED. Restart without \
             --dry-run to enable real writes."
        );
    }
    // Persist the freshly-generated auth token to `<root>/.yeet/auth-token`
    // and emit it on stdout so the parent extension can pick it up
    // without needing to re-read the file. The stdout marker uses a
    // distinctive prefix (`yeet-auth-token: `) the extension's
    // log-line parser can recognize.
    if let Err(e) = auth::write_auth_token_file(&args.project_root, &state_inner.auth_token) {
        warn!(error = ?e, "failed to write .yeet/auth-token; pairing flow will still work via breadcrumb");
    } else {
        info!("auth-token written to .yeet/auth-token");
    }
    println!("yeet-auth-token: {}", state_inner.auth_token);
    let state: SharedState = Arc::new(RwLock::new(state_inner));
    info!(
        files = state.read().await.tree_fs.len(),
        "initial scan complete"
    );
    // Bootstrap-time sweep — clears any pre-existing empty dirs that the
    // daemon's `rescan_fs` doesn't track (it only sees files matching
    // `classify(...)`). Without this, a project the user already cleaned
    // out from a previous session keeps the orphan dirs around forever.
    // Aggressive mode descends into `Packages/_Index/`, `.pesde/`, etc.
    // because real-world Wally / pesde projects accumulate empty leaves
    // there across syncbacks; the junction guard inside
    // `is_safe_to_prune_with` keeps the actual package-manager symlinks
    // intact.
    {
        let guard = state.read().await;
        let canonical_root =
            std::fs::canonicalize(&guard.root).unwrap_or_else(|_| guard.root.clone());
        let mapping_roots = guard.mapping_roots_canonical.clone();
        drop(guard);
        let removed = prune_all_empty_subdirs_with(
            &canonical_root,
            &mapping_roots,
            /*aggressive=*/ true,
        );
        if removed > 0 {
            info!(removed, "bootstrap: pruned empty dirs");
        }
    }

    let (bcast_tx, _) = broadcast::channel::<Arc<ServerMsg>>(BROADCAST_CAPACITY);

    tokio::spawn(event_pump(state.clone(), fs_rx, bcast_tx.clone()));

    // Give the watcher's 100ms debounce + event_pump a moment to drain any
    // events that fired during the scan/sweep window above. Without this,
    // a plugin that auto-connects the instant the listener binds can race
    // ahead of the in-flight events and read a still-stale `tree_fs`. The
    // 400ms budget covers DEBOUNCE (100ms) + a couple of round-trips
    // through event_pump for typical project sizes; on a fresh daemon
    // start the user is rarely waiting on this.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let sessions: Arc<Mutex<SyncbackSessions>> = Arc::new(Mutex::new(SyncbackSessions::default()));

    let bind_addr = args.bind.as_deref().unwrap_or(BIND_ADDR);
    // When bound to loopback (the default), enforce a loopback `Host` header
    // on the WS upgrade as extra anti-rebinding defence. A non-loopback bind
    // (only reachable via `--allow-remote`) legitimately sees remote Hosts,
    // so the check is disabled there.
    let enforce_loopback_host = is_loopback_bind_addr(bind_addr);
    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("bind {bind_addr}"))?;
    // Re-read the actual bound address — `127.0.0.1:0` lets the OS pick
    // an ephemeral port and tests parse it back from this log line.
    let actual_addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| bind_addr.to_owned());
    info!(addr = %actual_addr, "yeet-daemon listening");

    tokio::select! {
        res = accept_loop(listener, state, sessions, bcast_tx, enforce_loopback_host) => res,
        res = tokio::signal::ctrl_c() => {
            res.context("install ctrl+c handler")?;
            info!("shutdown: ctrl+c received");
            Ok(())
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("yeet_daemon=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Refuses a non-loopback `--bind` unless `--allow-remote` was passed, and
/// emits a prominent warning when a remote bind is allowed (AUDITORIA-YEET.md
/// B13). Loopback binds (the default) always pass silently. Split out from
/// `parse_args` so the policy is unit-testable without touching argv.
fn validate_bind_addr(bind: Option<&str>, allow_remote: bool) -> Result<()> {
    let Some(addr) = bind else {
        return Ok(());
    };
    if is_loopback_bind_addr(addr) {
        return Ok(());
    }
    if !allow_remote {
        bail!(
            "--bind {addr} is not a loopback address. Binding to a non-loopback \
             interface exposes your project's source code to the network — anyone who \
             can reach this port could read and write it. Re-run with --allow-remote to \
             confirm you intend this."
        );
    }
    warn!(
        bind = %addr,
        "SECURITY: binding to a non-loopback address (--allow-remote). The daemon is \
         reachable from the network; only do this on a trusted network."
    );
    Ok(())
}

fn parse_args() -> Result<CliArgs> {
    let mut positional: Option<String> = None;
    let mut debug_echo = false;
    let mut bind: Option<String> = None;
    let mut reset_base_tree = false;
    let mut dry_run = false;
    let mut allow_remote = false;
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        if arg == "--debug-echo" {
            debug_echo = true;
        } else if arg == "--reset-base-tree" {
            reset_base_tree = true;
        } else if arg == "--dry-run" {
            dry_run = true;
        } else if arg == "--allow-remote" {
            allow_remote = true;
        } else if arg == "--bind" {
            bind = Some(
                iter.next()
                    .context("--bind requires an addr (e.g. 127.0.0.1:0)")?,
            );
        } else if let Some(rest) = arg.strip_prefix("--bind=") {
            bind = Some(rest.to_owned());
        } else if arg.starts_with("--") {
            bail!("unknown flag: {arg}");
        } else if positional.is_none() {
            positional = Some(arg);
        } else {
            bail!("unexpected extra argument: {arg}");
        }
    }
    // A non-loopback bind must be opted into explicitly — the default stays
    // loopback and unaffected.
    validate_bind_addr(bind.as_deref(), allow_remote)?;

    let dir = match positional {
        Some(s) => PathBuf::from(s),
        None => std::env::current_dir().context("get cwd")?,
    };
    let dir = dir
        .canonicalize()
        .with_context(|| format!("resolve {}", dir.display()))?;
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    if !dir.join(PROJECT_FILE).is_file() {
        bail!("{} has no {PROJECT_FILE}", dir.display());
    }
    Ok(CliArgs {
        project_root: dir,
        debug_echo,
        bind,
        reset_base_tree,
        dry_run,
    })
}

// ─── FS event pipeline ──────────────────────────────────────────────────────

async fn event_pump(
    state: SharedState,
    mut fs_rx: mpsc::Receiver<FileEvent>,
    bcast_tx: broadcast::Sender<Arc<ServerMsg>>,
) {
    while let Some(event) = fs_rx.recv().await {
        if let Err(e) = handle_fs_event(&state, event, &bcast_tx).await {
            warn!(error = ?e, "fs event handling failed");
        }
    }
}

async fn handle_fs_event(
    state: &SharedState,
    event: FileEvent,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    // `.meta.json` files are a distinct routing target: they feed the
    // per-path attribute map and emit `AttributesChanged`, never a
    // three-way source merge.
    let abs_for_meta = match &event {
        FileEvent::Touched(p) | FileEvent::Removed(p) => p.clone(),
    };
    if is_meta_file(&abs_for_meta) {
        return handle_meta_event(state, event, bcast_tx).await;
    }

    // Special-case the project file: a change to `default.project.json`
    // mid-session means the user added/removed a `$path` mapping. We can't
    // safely hot-reload (mapping_roots_canonical is captured at startup
    // and threaded into every prune call), but we MUST stop pruning empty
    // dirs — a freshly-mapped path with no files yet would otherwise be
    // wiped by the next aggressive sweep. Set the dirty flag and emit a
    // SyncError so the user knows to restart the daemon.
    {
        let project_file_rel = state.read().await.relative(&abs_for_meta);
        if project_file_rel.as_deref() == Some(PROJECT_FILE) {
            let already_dirty = {
                let mut guard = state.write().await;
                let was = guard.project_dirty;
                guard.project_dirty = true;
                was
            };
            if !already_dirty {
                warn!(
                    "default.project.json changed at runtime; further prune operations \
                     will be skipped until daemon restart"
                );
                broadcast_server_msg(
                    state,
                    bcast_tx,
                    ServerMsg::SyncError {
                        kind: SyncErrorKind::UnsafePath,
                        path: PROJECT_FILE.to_owned(),
                        reason: "default.project.json changed at runtime; restart the daemon \
                                 for new $path mappings to take effect"
                            .to_owned(),
                    },
                )
                .await;
            }
            return Ok(());
        }
    }

    let path = match event {
        FileEvent::Touched(abs) => {
            // Each early return here would otherwise silently swallow the
            // event with no signal to the user, making "files don't appear
            // in Studio" undebuggable. Log every drop with the path and the
            // reason so `RUST_LOG=yeet_daemon=debug` surfaces it.
            if !abs.is_file() {
                debug!(path = %abs.display(), "fs drop: path is not a file (race or non-regular)");
                return Ok(());
            }
            let Some(rel) = ({
                let guard = state.read().await;
                guard.relative(&abs)
            }) else {
                debug!(path = %abs.display(), "fs drop: path is outside project root");
                return Ok(());
            };
            if !state.read().await.is_under_mapping(&rel) {
                debug!(path = %rel, "fs drop: path is not under any $path mapping in default.project.json");
                return Ok(());
            }
            // Drop watcher echoes from a daemon-initiated `fs::rename`. The
            // matching Remove(old) + Touched(new) pair is one-shot so the
            // second `consume_rename_echo` call returns false naturally.
            {
                let mut guard = state.write().await;
                if guard.consume_rename_echo(&rel) {
                    debug!(path = %rel, "fs drop: suppressed echo of daemon rename");
                    return Ok(());
                }
            }
            let file_name = abs
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            let Some((_name, kind)) = classify(file_name) else {
                debug!(path = %rel, file = file_name, "fs drop: file name does not classify as a Roblox script (.lua/.luau, optionally .server/.client)");
                return Ok(());
            };

            let raw = std::fs::read_to_string(&abs)
                .with_context(|| format!("read {}", abs.display()))?;
            let (content, meta) = normalize_from_disk(&raw);
            let sha = sha256_hex(content.as_bytes());

            let mut guard = state.write().await;
            let prev_hash = guard.tree_fs.get(&rel).map(|e| e.sha256.clone());
            let debug_echo = guard.debug_echo;

            if prev_hash.as_deref() == Some(sha.as_str()) {
                log_echo(
                    debug_echo,
                    format_args!("fs watcher echo drop: path={rel} sha={sha}"),
                );
                return Ok(());
            }
            // Look for a recently-removed entry with the same content
            // hash — that's the source half of an IDE-side rename. The
            // pairing window is small enough that natural Delete+Create
            // sequences from a user (delete X.luau, then start fresh
            // typing into a new file with same content seconds later)
            // never trip it, but tight enough that the Modify(From) +
            // Modify(To) pair the OS emits for a rename always lands
            // inside it.
            //
            // `fs_removed_pending` is keyed by path now (AUDITORIA-YEET.md
            // A1), so pairing means scanning for an entry whose content
            // sha matches — guarded by two checks before it's promoted to
            // a rename:
            //   - watcher-4: only pair when `rel` (the new path) was NOT
            //     already a tracked file (`prev_hash.is_none()`). If it
            //     was, this Touched is an edit of a pre-existing file, not
            //     a rename target — pairing would clobber its identity
            //     (attributes/tags) with the deleted file's.
            //   - move-2: never pair a removed entry with empty content.
            //     Unrelated delete+create of empty stub files must stay
            //     an independent delete+create, not leak the deleted
            //     file's meta_attributes onto an unrelated new file.
            // When several pending entries share the sha, prefer the one
            // whose basename (file stem) matches the new path, else the
            // oldest by `since`.
            let rename_pick = if prev_hash.is_none() {
                let new_stem = Path::new(&rel).file_stem().and_then(|s| s.to_str());
                guard
                    .fs_removed_pending
                    .values()
                    .filter(|p| p.entry.sha256 == sha && !p.entry.content.is_empty())
                    .min_by_key(|p| {
                        let same_stem =
                            Path::new(&p.path).file_stem().and_then(|s| s.to_str()) == new_stem;
                        (!same_stem, p.since)
                    })
                    .map(|p| p.path.clone())
            } else {
                None
            };
            if let Some(old_path) = rename_pick {
                let pending = guard
                    .fs_removed_pending
                    .remove(&old_path)
                    .expect("rename_pick was just read from fs_removed_pending");
                let new_path = rel.clone();
                let kind_resolved = pending.entry.kind; // preserve the original kind, matches `kind` here too
                let _ = kind_resolved;
                let new_entry = TreeEntry {
                    kind,
                    content: content.clone(),
                    sha256: sha.clone(),
                };
                guard.tree_fs.insert(new_path.clone(), new_entry.clone());
                guard.meta.insert(new_path.clone(), meta);
                if let Some(attrs) = guard.meta_attributes.remove(&old_path) {
                    guard.meta_attributes.insert(new_path.clone(), attrs);
                }
                if let Some(base) = guard.tree_base.remove(&old_path) {
                    guard.tree_base.insert(new_path.clone(), base);
                }
                if let Some(studio) = guard.tree_studio.remove(&old_path) {
                    guard.tree_studio.insert(new_path.clone(), studio);
                }
                // Also rename any DirToDir-style descendants if old_path
                // was a folder-with-init script. The fs watcher emits one
                // event per child, so each child rename is handled on its
                // own; the top-level rename only needs to move the script
                // entry itself.
                guard.note_rename_echo(old_path.clone(), new_path.clone());
                persist_base(&guard)?;
                let root = guard.root.clone();
                let session_id = guard.session_id.clone();
                let collision_cleared = guard.pending_collisions.remove(&old_path)
                    | guard.pending_collisions.remove(&new_path);
                drop(guard);
                audit::record(
                    &root,
                    &audit::Entry {
                        ts: audit::now_rfc3339(),
                        kind: audit::Kind::FsRename,
                        path: &new_path,
                        sha_before: Some(&old_path),
                        sha_after: Some(&sha),
                        session_id: &session_id,
                        note: Some("ide-side rename paired via fs watcher"),
                    },
                );
                broadcast_server_msg(
                    state,
                    bcast_tx,
                    ServerMsg::FileRenamed {
                        old_path: old_path.clone(),
                        new_path: new_path.clone(),
                        content,
                        sha256: sha,
                        kind,
                    },
                )
                .await;
                if collision_cleared {
                    broadcast_server_msg(
                        state,
                        bcast_tx,
                        ServerMsg::NameCollision {
                            path: String::new(),
                            message: format!(
                                "name collision at {old_path} resolved by rename to {new_path}"
                            ),
                        },
                    )
                    .await;
                }
                return Ok(());
            }
            log_echo(
                debug_echo,
                format_args!(
                    "fs update accepted: path={rel} prev={:?} new={sha}",
                    prev_hash.as_deref().unwrap_or("<new>")
                ),
            );
            guard.tree_fs.insert(
                rel.clone(),
                TreeEntry {
                    kind,
                    content,
                    sha256: sha,
                },
            );
            guard.meta.insert(rel.clone(), meta);
            rel
        }
        FileEvent::Removed(abs) => {
            let mut guard = state.write().await;
            let rel_for_echo = guard.relative(&abs);
            if let Some(rel) = rel_for_echo.as_deref() {
                if guard.consume_rename_echo(rel) {
                    debug!(path = %rel, "fs drop: suppressed echo of daemon rename (removal side)");
                    return Ok(());
                }
            }
            // Capture the entry BEFORE forgetting it so a later Touched
            // with a matching sha can promote the pair to a FileRenamed.
            let rel = match guard.relative(&abs) {
                Some(r) => r,
                None => {
                    debug!(path = %abs.display(), "fs drop: removal of untracked path (outside project)");
                    return Ok(());
                }
            };
            let Some(entry) = guard.tree_fs.get(&rel).cloned() else {
                debug!(path = %abs.display(), "fs drop: removal of untracked path (already gone or never tracked)");
                return Ok(());
            };
            let entry_meta = guard.meta_for(&rel);
            guard.tree_fs.remove(&rel);
            guard.meta.remove(&rel);
            // Keyed by path (AUDITORIA-YEET.md A1) — every removal gets its
            // own slot, so two same-content deletes in the same window can
            // no longer clobber each other. `entry.sha256` is what a later
            // Touched matches against for rename pairing.
            guard.fs_removed_pending.insert(
                rel.clone(),
                FsRemovedPending {
                    path: rel.clone(),
                    entry,
                    meta: entry_meta,
                    since: Instant::now(),
                },
            );
            drop(guard);
            // Defer the actual reconcile (which would emit FileDeleted to
            // Studio + drop the tree_base entry) until the pairing window
            // expires. If a matching Touched lands inside the window, it
            // consumes `fs_removed_pending[rel]` and the rename is emitted
            // as a single `FileRenamed` instead. Removing by its own path
            // means this reconcile is independent of any other pending
            // removal, even one with identical content.
            let state2 = state.clone();
            let bcast_tx2 = bcast_tx.clone();
            let rel2 = rel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(FS_RENAME_PAIR_TTL).await;
                let still_pending = {
                    let mut guard = state2.write().await;
                    guard.fs_removed_pending.remove(&rel2).is_some()
                };
                if still_pending {
                    if let Err(e) = reconcile_path(&state2, &rel2, &bcast_tx2).await {
                        warn!(path = %rel2, error = ?e, "deferred fs delete reconcile failed");
                    }
                }
            });
            return Ok(());
        }
    };

    // From here on we're holding nothing; re-acquire to run the merge step.
    reconcile_path(state, &path, bcast_tx).await
}

/// Handles a `.meta.json` create/change/delete. Emits `AttributesChanged`
/// when the parsed `attributes` block for the paired script differs from
/// what we previously stored. Delete is treated as "cleared all
/// attributes" — the plugin uses the path lookup to know which instance
/// to scrub.
async fn handle_meta_event(
    state: &SharedState,
    event: FileEvent,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    match event {
        FileEvent::Touched(abs) => {
            if !abs.is_file() {
                return Ok(());
            }
            let (tracked_path, attributes) = {
                let mut guard = state.write().await;
                let Some((tracked, attrs)) = guard.ingest_meta_file(&abs)? else {
                    return Ok(());
                };
                // Skip the broadcast if nothing actually moved. `ingest_meta_file`
                // has already overwritten the stored map, so comparing after
                // insertion requires saving the prior snapshot — instead we
                // compare before inserting, but the simpler rewrite is: let
                // `ingest_meta_file` tell us what changed. For now, always emit;
                // the plugin's `SetAttribute` is idempotent and cheap.
                (tracked, attrs)
            };
            broadcast_server_msg(
                state,
                bcast_tx,
                ServerMsg::AttributesChanged {
                    path: tracked_path,
                    attributes,
                },
            )
            .await;
        }
        FileEvent::Removed(abs) => {
            let tracked_path = {
                let mut guard = state.write().await;
                let Some(tracked) = guard.forget_meta_file(&abs) else {
                    return Ok(());
                };
                tracked
            };
            broadcast_server_msg(
                state,
                bcast_tx,
                ServerMsg::AttributesChanged {
                    path: tracked_path,
                    attributes: HashMap::new(),
                },
            )
            .await;
        }
    }
    Ok(())
}

// ─── Path-ordering helpers ──────────────────────────────────────────────────

/// Orders project-relative paths for apply: shallowest paths first
/// (services/folders before their contents), `init.{server,client.}{lua,luau}`
/// files before non-init siblings within the same directory, alphabetical
/// as a stable tiebreaker. Plain alphabetical sort lets non-init siblings
/// land before their `init.luau` parent, which forces the plugin to
/// create a Folder for the parent and then swap it to a ModuleScript on
/// the next event — a chain that breaks intermittently when ChangeHistory
/// returns "busy" mid-batch and leaves the Folder in place.
///
/// Implemented as a free fn (rather than inlined into the one call site)
/// so `apply_bulk_resolutions` can call it defensively too — both the
/// preview-time enumeration and the apply loop must agree on the order.
fn sort_paths_for_apply<S: AsRef<str>>(paths: &mut [S]) {
    paths.sort_by(|a, b| {
        let a = a.as_ref();
        let b = b.as_ref();
        let depth_a = a.matches('/').count();
        let depth_b = b.matches('/').count();
        let init_a = is_init_filename(a);
        let init_b = is_init_filename(b);
        // 1) shallowest first — parent directories materialize before
        //    their children so the parent is the right kind by the time
        //    a child arrives.
        depth_a
            .cmp(&depth_b)
            // 2) within the same depth, init files first — the init
            //    promotes its parent to the right script kind, and any
            //    sibling that arrives next is added under that script.
            .then(init_b.cmp(&init_a))
            // 3) alphabetical tiebreaker — keeps the order stable
            //    regardless of HashSet iteration order.
            .then(a.cmp(b))
    });
}

// ─── Shared reconciliation helper ───────────────────────────────────────────

/// Runs `merge_file` for `path` and applies the outcome, including I/O and
/// broadcasts. Caller is expected to have already updated whichever tree
/// triggered the change.
async fn reconcile_path(
    state: &SharedState,
    path: &str,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    let outcome = {
        let guard = state.read().await;
        merge_file(
            path,
            guard.tree_base.get(path),
            guard.tree_studio.get(path),
            guard.tree_fs.get(path),
        )
    };
    apply_outcome(state, path, outcome, bcast_tx).await
}

async fn apply_outcome(
    state: &SharedState,
    path: &str,
    outcome: MergeOutcome,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    // Log the merge decision so the user can trace "conflict didn't open"
    // back to whichever branch fired. INFO-level because this runs at human
    // interaction frequency, not per keystroke.
    let outcome_label = match &outcome {
        MergeOutcome::Noop => "noop",
        MergeOutcome::AdoptBase => "adopt_base",
        MergeOutcome::Apply { side: Side::Fs, content: Some(_), .. } => "apply_fs_write",
        MergeOutcome::Apply { side: Side::Fs, content: None, .. } => "apply_fs_delete",
        MergeOutcome::Apply { side: Side::Studio, content: Some(_), .. } => "apply_studio_write",
        MergeOutcome::Apply { side: Side::Studio, content: None, .. } => "apply_studio_delete",
        MergeOutcome::AutoMerge { .. } => "auto_merge",
        MergeOutcome::Conflict(_) => "conflict",
    };
    info!(path = %path, outcome = outcome_label, "merge outcome");
    match outcome {
        MergeOutcome::Noop => Ok(()),
        MergeOutcome::AdoptBase => {
            let mut guard = state.write().await;
            // Both sides agree; their entries are identical by sha. Copy
            // whichever is present.
            let new_base = guard
                .tree_fs
                .get(path)
                .cloned()
                .or_else(|| guard.tree_studio.get(path).cloned());
            match new_base {
                Some(e) => {
                    guard.tree_base.insert(path.to_owned(), e);
                }
                None => {
                    guard.tree_base.remove(path);
                }
            }
            persist_base(&guard)
        }
        MergeOutcome::Apply {
            side: Side::Fs,
            content: Some(c),
            kind,
        } => write_to_fs(state, path, c, kind, bcast_tx, /*broadcast_to_studio=*/ false).await,
        MergeOutcome::Apply {
            side: Side::Fs,
            content: None,
            kind: _,
        } => delete_from_fs(state, path, bcast_tx, /*broadcast_to_studio=*/ false).await,
        MergeOutcome::Apply {
            side: Side::Studio,
            content: Some(c),
            kind,
        } => push_to_studio(state, path, c, kind, bcast_tx).await,
        MergeOutcome::Apply {
            side: Side::Studio,
            content: None,
            kind: _,
        } => delete_on_studio(state, path, bcast_tx).await,
        MergeOutcome::AutoMerge { content, kind } => {
            write_to_fs(state, path, content.clone(), kind, bcast_tx, true).await
        }
        MergeOutcome::Conflict(conflict) => record_conflict(state, conflict, bcast_tx).await,
    }
}

/// Writes `content` to disk (with the stored line-ending flavor), updates
/// `tree_fs` + `tree_base`, and optionally broadcasts a `FileChanged` to the
/// plugin (used by `AutoMerge`: both sides need to see the merged bytes).
async fn write_to_fs(
    state: &SharedState,
    path: &str,
    content: String,
    kind: ScriptKind,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
    broadcast_to_studio: bool,
) -> Result<()> {
    {
        let guard = state.read().await;
        if guard.dry_run {
            audit::record(
                &guard.root,
                &audit::Entry {
                    ts: audit::now_rfc3339(),
                    kind: audit::Kind::FsWrite,
                    path,
                    sha_before: guard.tree_fs.get(path).map(|e| e.sha256.as_str()),
                    sha_after: Some(&sha256_hex(content.as_bytes())),
                    session_id: &guard.session_id,
                    note: Some("dry-run: suppressed"),
                },
            );
            info!(path = %path, "DRY-RUN write_to_fs suppressed");
            // Don't broadcast either — see push_to_studio for the same
            // reasoning. Caller treats this as success; the divergence
            // will resurface on the next reconcile, exactly the point
            // of dry-run.
            let _ = (kind, bcast_tx, broadcast_to_studio);
            return Ok(());
        }
    }
    let mut guard = state.write().await;
    let abs = resolve_inside(&guard.root, path)
        .with_context(|| format!("refusing to write unsafe path {path}"))?;
    let meta = guard.meta_for(path);
    let encoded = encode_for_disk(&content, meta);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    atomic_write(&abs, encoded.as_bytes())
        .with_context(|| format!("write {}", abs.display()))?;
    let sha = sha256_hex(content.as_bytes());
    let sha_before = guard.tree_fs.get(path).map(|e| e.sha256.clone());
    let entry = TreeEntry {
        kind,
        content: content.clone(),
        sha256: sha.clone(),
    };
    guard.tree_fs.insert(path.to_owned(), entry.clone());
    guard.tree_base.insert(path.to_owned(), entry.clone());
    if broadcast_to_studio {
        guard.tree_studio.insert(path.to_owned(), entry);
    }
    guard.meta.entry(path.to_owned()).or_insert(meta);
    persist_base(&guard)?;
    let root = guard.root.clone();
    let session_id = guard.session_id.clone();
    drop(guard);
    audit::record(
        &root,
        &audit::Entry {
            ts: audit::now_rfc3339(),
            kind: audit::Kind::FsWrite,
            path,
            sha_before: sha_before.as_deref(),
            sha_after: Some(&sha),
            session_id: &session_id,
            note: None,
        },
    );
    if broadcast_to_studio {
        broadcast_server_msg(
            state,
            bcast_tx,
            ServerMsg::FileChanged {
                path: path.to_owned(),
                content,
                sha256: sha,
            },
        )
        .await;
    }
    Ok(())
}

async fn delete_from_fs(
    state: &SharedState,
    path: &str,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
    broadcast_to_studio: bool,
) -> Result<()> {
    {
        let guard = state.read().await;
        if guard.dry_run {
            audit::record(
                &guard.root,
                &audit::Entry {
                    ts: audit::now_rfc3339(),
                    kind: audit::Kind::FsDelete,
                    path,
                    sha_before: guard.tree_fs.get(path).map(|e| e.sha256.as_str()),
                    sha_after: None,
                    session_id: &guard.session_id,
                    note: Some("dry-run: suppressed"),
                },
            );
            info!(path = %path, "DRY-RUN delete_from_fs suppressed");
            let _ = (bcast_tx, broadcast_to_studio);
            return Ok(());
        }
    }
    let mut guard = state.write().await;
    let abs = resolve_inside(&guard.root, path)
        .with_context(|| format!("refusing to delete unsafe path {path}"))?;
    if abs.is_file() {
        std::fs::remove_file(&abs)
            .with_context(|| format!("remove {}", abs.display()))?;
    }
    // Walk up and remove every parent directory that became empty as a
    // result of the file delete. Stops at the project root so we never
    // blast the place the daemon is serving. This matters a lot after a
    // `Migrate all: Studio → IDE` that drops every ide_only file under a
    // whole subtree — without this pass, the user is left with a pile of
    // empty folders on disk even though nothing tracked lives inside.
    //
    // We canonicalize the root here so `is_safe_to_prune`'s `starts_with`
    // check sees the same shape on both sides — Windows tempdirs come in
    // non-UNC form while `canonicalize` on the candidate returns
    // `\\?\C:\...`. Fall back to the raw root if canonicalize fails (which
    // would be weird, since the dir is currently being mutated).
    let root = std::fs::canonicalize(&guard.root).unwrap_or_else(|_| guard.root.clone());
    let mapping_roots = guard.mapping_roots_canonical.clone();
    if guard.project_dirty {
        // See ProjectState::project_dirty — skipping the prune is the
        // safe default after a project file change since we may have a
        // freshly-mapped empty dir that the prune would delete.
        warn!("skipping post-delete prune: project file dirty (restart daemon)");
    } else {
        prune_empty_dirs(&root, &mapping_roots, abs.parent());
    }
    let sha_before = guard.tree_fs.get(path).map(|e| e.sha256.clone());
    guard.tree_fs.remove(path);
    guard.tree_base.remove(path);
    guard.meta.remove(path);
    guard.meta_attributes.remove(path);
    if broadcast_to_studio {
        guard.tree_studio.remove(path);
    }
    persist_base(&guard)?;
    let project_root = guard.root.clone();
    let session_id = guard.session_id.clone();
    drop(guard);
    audit::record(
        &project_root,
        &audit::Entry {
            ts: audit::now_rfc3339(),
            kind: audit::Kind::FsDelete,
            path,
            sha_before: sha_before.as_deref(),
            sha_after: None,
            session_id: &session_id,
            note: None,
        },
    );
    if broadcast_to_studio {
        broadcast_server_msg(
            state,
            bcast_tx,
            ServerMsg::FileDeleted {
                path: path.to_owned(),
            },
        )
        .await;
    }
    Ok(())
}

/// Returns true when `candidate` may safely be removed by the prune
/// helpers — i.e. it sits strictly under the project root, isn't a Rojo
/// `$path` mount point, and isn't a symlink. The symlink check matters
/// because `read_dir` on a symlink-to-dir returns the *target's*
/// children; if those happened to be filtered away (or the symlink
/// pointed to an empty dir we don't own) we'd be `remove_dir`-ing a
/// link the user explicitly placed.
///
/// `canonical_root` and the entries of `mapping_roots` MUST already be
/// canonicalized (they come from `ProjectState::bootstrap`); `candidate`
/// is canonicalized inside the function so a UNC vs. non-UNC mismatch
/// (Windows) doesn't break containment.
fn is_safe_to_prune(
    canonical_root: &Path,
    mapping_roots: &[PathBuf],
    candidate: &Path,
) -> bool {
    is_safe_to_prune_with(canonical_root, mapping_roots, candidate, /*respect_package_skip=*/ true)
}

fn is_safe_to_prune_with(
    canonical_root: &Path,
    mapping_roots: &[PathBuf],
    candidate: &Path,
    respect_package_skip: bool,
) -> bool {
    let canonical_candidate = match std::fs::canonicalize(candidate) {
        Ok(c) => c,
        Err(_) => return false,
    };
    if !canonical_candidate.starts_with(canonical_root) {
        return false;
    }
    if canonical_candidate == canonical_root {
        return false;
    }
    if mapping_roots.iter().any(|m| m == &canonical_candidate) {
        return false;
    }
    // Package-manager protection by directory name (`Packages`, `_Index`,
    // `.pesde`, …). The aggressive sweep skips this proxy because the
    // junction check below already protects what actually matters — it's
    // the empty leaves DEEP inside those landings (orphans from a previous
    // syncback / hand-edited project) that we want to clean up after a
    // bootstrap.
    if respect_package_skip {
        let mut cursor: &Path = canonical_candidate.as_path();
        while cursor != canonical_root {
            if let Some(name) = cursor.file_name() {
                if dir_name_is_skipped(name) {
                    return false;
                }
            }
            match cursor.parent() {
                Some(p) => cursor = p,
                None => break,
            }
        }
    }
    // Junctions / reparse points: refuse regardless of `respect_package_skip`.
    // `remove_dir` on a junction unlinks the junction (not its target), but
    // the user installed it on purpose — package managers treat that as
    // structural breakage. `is_symlink()` returns true for both unix
    // symlinks and Windows junctions.
    match std::fs::symlink_metadata(&canonical_candidate) {
        Ok(meta) => !meta.file_type().is_symlink(),
        Err(_) => false,
    }
}

/// Removes `start` and every ancestor up to (but not including) the
/// canonical project root or the nearest `$path` mapping root, provided
/// each is empty. A `ReadDir` failure is treated as "not empty, stop
/// walking" — we never want a transient I/O hiccup to cascade into
/// deleting a folder the user still had files in.
fn prune_empty_dirs(
    canonical_root: &Path,
    mapping_roots: &[PathBuf],
    start: Option<&Path>,
) {
    let Some(mut cursor_buf) = start.map(Path::to_path_buf) else {
        return;
    };
    loop {
        let cursor = cursor_buf.as_path();
        if !is_safe_to_prune(canonical_root, mapping_roots, cursor) {
            return;
        }
        // `read_dir().next().is_none()` is the cheap "is this directory
        // empty?" check — avoids collecting a Vec for a bool.
        let empty = match std::fs::read_dir(cursor) {
            Ok(mut it) => it.next().is_none(),
            Err(_) => return,
        };
        if !empty {
            return;
        }
        if let Err(e) = std::fs::remove_dir(cursor) {
            tracing::warn!(dir = %cursor.display(), error = ?e, "prune_empty_dirs: remove failed");
            return;
        }
        let Some(parent) = cursor.parent() else {
            return;
        };
        cursor_buf = parent.to_path_buf();
    }
}

/// Sweep variant of `prune_empty_dirs`: walks every `$path` mapping
/// bottom-up and prunes any directory that ends up empty. Catches two
/// cases the per-file walk misses:
///
/// * pre-existing empty dirs (the daemon's `rescan_fs` only ingests
///   files matching `classify(...)`, so a folder that was already empty
///   before the daemon started never enters `tree_fs` — no per-file
///   prune walk ever visits it);
/// * cross-iteration leftovers in a `BulkSyncConfirm` batch where the
///   first delete in a folder gave up at a non-empty parent because
///   sibling files only got deleted later in the loop.
///
/// Returns the number of directories actually removed (for logging).
/// Directory names whose contents the sweep MUST NOT recurse into. These
/// are package-manager landings (Wally, pesde, Rojo's `Packages` mount,
/// nodejs `node_modules`) where on Windows the entries are often
/// junctions/reparse points or read-only by design — touching them
/// flooded the log with `PermissionDenied` warnings on real projects
/// and risked unlinking the junction itself in the few cases where
/// `remove_dir` succeeded. Yeet has no business cleaning these even if
/// they end up empty: the package manager owns that filesystem region.
const SWEEP_SKIP_DIR_NAMES: &[&str] = &[
    "Packages",
    "_Index",
    "roblox_packages",
    ".pesde",
    "node_modules",
    ".yeet",
];

fn dir_name_is_skipped(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .map(|s| SWEEP_SKIP_DIR_NAMES.iter().any(|skip| *skip == s))
        .unwrap_or(false)
}

/// True when any path component below `mount` matches a name in
/// `SWEEP_SKIP_DIR_NAMES`. Used as the inner-loop filter for
/// `prune_all_empty_subdirs` because `WalkDir::filter_entry` doesn't
/// reliably suppress descent when paired with `contents_first(true)` —
/// children are visited before the predicate runs on the parent. The
/// path-component check is cheap (small, fixed list) and unambiguous.
fn path_under_skipped_dir(path: &Path, mount: &Path) -> bool {
    let rel = match path.strip_prefix(mount) {
        Ok(r) => r,
        Err(_) => return false,
    };
    rel.components().any(|c| match c {
        std::path::Component::Normal(name) => dir_name_is_skipped(name),
        _ => false,
    })
}

/// Conservative wrapper kept only for tests that pinned the historical
/// "skip package-manager dirs by name" behaviour. Production call sites use
/// `prune_all_empty_subdirs_with(.., true)` because real Wally / pesde
/// projects accumulate empty leaves the conservative sweep refused to
/// touch (see the user-reported regression where `Packages/_Index/foo/`
/// kept stale orphans across reconnects).
#[cfg(test)]
fn prune_all_empty_subdirs(canonical_root: &Path, mapping_roots: &[PathBuf]) -> usize {
    prune_all_empty_subdirs_with(canonical_root, mapping_roots, /*aggressive=*/ false)
}

/// Variant of `prune_all_empty_subdirs` that, when `aggressive` is true,
/// recurses into package-manager landings (`Packages`, `_Index`, `.pesde`,
/// `roblox_packages`, …). The junction / symlink check inside
/// `is_safe_to_prune_with` still refuses to delete a reparse point, so the
/// invariant "Yeet never breaks a junction installed by a package manager"
/// is preserved — what changes is that empty *plain* directories deep inside
/// those landings (orphans from earlier syncbacks / manual edits) become
/// eligible for cleanup. Used by the post-syncback and post-handshake
/// sweeps where users observed leftover empty dirs inside `Packages/_Index/`.
///
/// The implementation drives a hand-rolled bottom-up traversal rather than
/// `walkdir`: the previous walkdir version intermittently left deep
/// `Packages/_Index/<pkg>/<pkg>/<sub>/` chains untouched even with
/// `contents_first(true)`, because removing a leaf during iteration
/// invalidated the lazy walker's cached state for the parent. Manual
/// recursion + a stabilization loop guarantees that every parent gets
/// re-examined after its children change.
fn prune_all_empty_subdirs_with(
    canonical_root: &Path,
    mapping_roots: &[PathBuf],
    aggressive: bool,
) -> usize {
    let mut total = 0usize;
    // Bounded retry: each pass removes at least one directory or stops.
    // Five iterations is enough to fully collapse projects we've seen in
    // the wild (Wally + pesde mixed) without risking a runaway loop on
    // a pathological tree. Each pass is O(files), so the cap is cheap.
    for _ in 0..5 {
        let pass_removed = prune_all_empty_subdirs_pass(canonical_root, mapping_roots, aggressive);
        if pass_removed == 0 {
            break;
        }
        total += pass_removed;
    }
    total
}

fn prune_all_empty_subdirs_pass(
    canonical_root: &Path,
    mapping_roots: &[PathBuf],
    aggressive: bool,
) -> usize {
    let mut removed = 0usize;
    let mut visited = 0usize;
    let mut refused_unsafe = 0usize;
    let mut kept_with_code = 0usize;
    for mount in mapping_roots {
        // Collect every directory under `mount` in deepest-first order.
        // Then process serially without any walker state to invalidate.
        let mut dirs: Vec<PathBuf> = Vec::new();
        collect_dirs_bottom_up(mount, &mut dirs);
        for candidate in &dirs {
            visited += 1;
            if !aggressive && path_under_skipped_dir(candidate, mount) {
                refused_unsafe += 1;
                continue;
            }
            if !is_safe_to_prune_with(canonical_root, mapping_roots, candidate, !aggressive) {
                refused_unsafe += 1;
                continue;
            }
            // Empty: remove_dir wins. A directory drained by an earlier
            // iteration of this pass falls into this branch on revisit.
            let empty = match std::fs::read_dir(candidate) {
                Ok(mut it) => it.next().is_none(),
                Err(_) => continue,
            };
            if empty {
                match std::fs::remove_dir(candidate) {
                    Ok(()) => {
                        removed += 1;
                        tracing::info!(
                            dir = %candidate.display(),
                            "prune: removed empty dir"
                        );
                    }
                    Err(e) => {
                        tracing::debug!(
                            dir = %candidate.display(),
                            error = ?e,
                            "prune: remove skipped"
                        );
                    }
                }
                continue;
            }
            // Aggressive sweep extension: a subtree that contains no
            // `.luau` / `.lua` source file is logically dead — its
            // contents are scaffolding from an earlier syncback (init
            // metas, package-manager leftovers, sentinel files like
            // `.gitkeep`). Drop the whole subtree so users don't see
            // ghost folders in the IDE after every reconnect (this is
            // the Wally/pesde leftover scenario users keep hitting).
            if aggressive {
                if subtree_has_no_code(candidate) {
                    match std::fs::remove_dir_all(candidate) {
                        Ok(()) => {
                            removed += 1;
                            tracing::info!(
                                dir = %candidate.display(),
                                "prune: removed code-less subtree"
                            );
                        }
                        Err(e) => {
                            tracing::debug!(
                                dir = %candidate.display(),
                                error = ?e,
                                "prune: code-less remove_dir_all skipped"
                            );
                        }
                    }
                } else {
                    kept_with_code += 1;
                }
            } else {
                kept_with_code += 1;
            }
        }
    }
    if visited > 0 {
        tracing::info!(
            visited,
            removed,
            kept_with_code,
            refused_unsafe,
            aggressive,
            "prune pass summary"
        );
    }
    removed
}

/// Collects every directory rooted at `start` into `out` in deepest-first
/// order. Symlink directories (junctions on Windows) are NOT descended
/// into to avoid following package-manager links into their targets.
/// `start` itself is appended last so callers naturally see it after
/// every descendant when iterating left-to-right.
fn collect_dirs_bottom_up(start: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(start) {
        Ok(it) => it,
        Err(_) => {
            out.push(start.to_path_buf());
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            collect_dirs_bottom_up(&path, out);
        }
    }
    out.push(start.to_path_buf());
}

/// Returns true when no `.luau` / `.lua` file lives anywhere beneath `dir`.
/// Empty subtrees and subtrees made up entirely of `.meta.json` artifacts,
/// `.gitkeep`-style sentinels, READMEs or `.rbxm` blobs all count as
/// "no code present" — Yeet treats the directory as a structural orphan
/// of an old syncback and prunes it. The user explicitly asked for this:
/// "deletar todas as pastas que não contêm um código dentro".
///
/// We deliberately do NOT classify `.rbxm` as code. The intent is that
/// folders without source files are scaffolding produced by the syncback
/// (`init.meta.json` for an empty Folder instance, package-manager
/// leftovers, etc.) and re-emit deterministically on the next syncback
/// if Studio still wants them.
///
/// Symlinks aren't followed (`follow_links(false)`); a symlink entry has
/// `file_type().is_file()` false on most platforms, but even where it
/// reports true the surrounding `is_safe_to_prune_with` already refused
/// the candidate root by junction check.
fn subtree_has_no_code(dir: &Path) -> bool {
    let walker = walkdir::WalkDir::new(dir).follow_links(false);
    for entry in walker.into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(name) = entry.path().file_name().and_then(|s| s.to_str()) else {
            // Non-UTF-8 file names: be conservative, treat as "code present"
            // so we don't blow away unfamiliar user content.
            return false;
        };
        if classify(name).is_some() {
            return false;
        }
    }
    true
}

/// Runs the aggressive sweep across the project's `$path` mappings. Shared
/// between cold-boot and merge-pass branches of the handshake so neither
/// drift on whether/when stale dirs get cleaned. The return value is
/// dropped on purpose — sweep failures are non-fatal and already log.
async fn sweep_project_after_handshake(state: &SharedState) {
    let (canonical_root, mapping_roots, dirty) = {
        let guard = state.read().await;
        let root = std::fs::canonicalize(&guard.root).unwrap_or_else(|_| guard.root.clone());
        (
            root,
            guard.mapping_roots_canonical.clone(),
            guard.project_dirty,
        )
    };
    if dirty {
        warn!("skipping handshake sweep: project file dirty (restart daemon)");
        return;
    }
    let removed =
        prune_all_empty_subdirs_with(&canonical_root, &mapping_roots, /*aggressive=*/ true);
    if removed > 0 {
        info!(removed, "handshake: pruned empty dirs");
    }
}

/// Studio hasn't caught up yet; push the current fs content to it.
async fn push_to_studio(
    state: &SharedState,
    path: &str,
    content: String,
    kind: ScriptKind,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    {
        let guard = state.read().await;
        if guard.dry_run {
            audit::record(
                &guard.root,
                &audit::Entry {
                    ts: audit::now_rfc3339(),
                    kind: audit::Kind::StudioPush,
                    path,
                    sha_before: guard.tree_studio.get(path).map(|e| e.sha256.as_str()),
                    sha_after: Some(&sha256_hex(content.as_bytes())),
                    session_id: &guard.session_id,
                    note: Some("dry-run: suppressed"),
                },
            );
            info!(path = %path, "DRY-RUN push_to_studio suppressed");
            let _ = (kind, bcast_tx);
            return Ok(());
        }
    }
    let mut guard = state.write().await;
    let sha = sha256_hex(content.as_bytes());
    let sha_before = guard.tree_studio.get(path).map(|e| e.sha256.clone());
    let entry = TreeEntry {
        kind,
        content: content.clone(),
        sha256: sha.clone(),
    };
    let existed = guard.tree_studio.contains_key(path);
    guard.tree_studio.insert(path.to_owned(), entry.clone());
    guard.tree_base.insert(path.to_owned(), entry);
    persist_base(&guard)?;
    let receivers = bcast_tx.receiver_count();
    let project_root = guard.root.clone();
    let session_id = guard.session_id.clone();
    drop(guard);
    audit::record(
        &project_root,
        &audit::Entry {
            ts: audit::now_rfc3339(),
            kind: audit::Kind::StudioPush,
            path,
            sha_before: sha_before.as_deref(),
            sha_after: Some(&sha),
            session_id: &session_id,
            note: None,
        },
    );
    let (kind_label, msg) = if existed {
        (
            "file_changed",
            ServerMsg::FileChanged {
                path: path.to_owned(),
                content,
                sha256: sha,
            },
        )
    } else {
        (
            "file_created",
            ServerMsg::FileCreated {
                path: path.to_owned(),
                kind,
                content,
                sha256: sha,
            },
        )
    };
    info!(
        path = %path,
        kind = kind_label,
        receivers = receivers,
        "push_to_studio: broadcasting"
    );
    broadcast_server_msg(state, bcast_tx, msg).await;
    Ok(())
}

async fn delete_on_studio(
    state: &SharedState,
    path: &str,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    {
        let guard = state.read().await;
        if guard.dry_run {
            audit::record(
                &guard.root,
                &audit::Entry {
                    ts: audit::now_rfc3339(),
                    kind: audit::Kind::StudioDelete,
                    path,
                    sha_before: guard.tree_studio.get(path).map(|e| e.sha256.as_str()),
                    sha_after: None,
                    session_id: &guard.session_id,
                    note: Some("dry-run: suppressed"),
                },
            );
            info!(path = %path, "DRY-RUN delete_on_studio suppressed");
            let _ = bcast_tx;
            return Ok(());
        }
    }
    let mut guard = state.write().await;
    let sha_before = guard.tree_studio.get(path).map(|e| e.sha256.clone());
    guard.tree_studio.remove(path);
    guard.tree_base.remove(path);
    persist_base(&guard)?;
    let project_root = guard.root.clone();
    let session_id = guard.session_id.clone();
    drop(guard);
    audit::record(
        &project_root,
        &audit::Entry {
            ts: audit::now_rfc3339(),
            kind: audit::Kind::StudioDelete,
            path,
            sha_before: sha_before.as_deref(),
            sha_after: None,
            session_id: &session_id,
            note: None,
        },
    );
    broadcast_server_msg(
        state,
        bcast_tx,
        ServerMsg::FileDeleted {
            path: path.to_owned(),
        },
    )
    .await;
    Ok(())
}

async fn record_conflict(
    state: &SharedState,
    conflict: FileConflict,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    let mut guard = state.write().await;
    if guard.pending_conflicts.contains_key(&conflict.path) {
        // A conflict for this path is already waiting for the user to resolve
        // it in the UI. Overwriting it would corrupt the resolution: the user
        // would finish resolving the old conflict, submit their hunks, and the
        // daemon would apply them to a different snapshot. Drop the new event
        // and let the existing conflict stand.
        debug!(
            path = %conflict.path,
            "dropping new conflict: resolution already pending for this path"
        );
        return Ok(());
    }
    let view = conflict_to_view(&conflict);
    guard.pending_conflicts.insert(conflict.path.clone(), conflict);
    drop(guard);
    broadcast_server_msg(
        state,
        bcast_tx,
        ServerMsg::ConflictDetected {
            conflicts: vec![view],
        },
    )
    .await;
    Ok(())
}

fn persist_base(guard: &ProjectState) -> Result<()> {
    tree::save_base_tree(&guard.root, &guard.tree_base)?;
    Ok(())
}

/// Converts a `FileConflict` (with `auto_hunks` and internal fields) to the
/// subset the plugin actually sees on the wire.
fn conflict_to_view(c: &FileConflict) -> FileConflictView {
    FileConflictView {
        path: c.path.clone(),
        script_kind: c.script_kind,
        conflict_kind: c.conflict_kind,
        base_content: c.base_content.clone(),
        studio_content: c.studio_content.clone(),
        fs_content: c.fs_content.clone(),
        hunks: c.conflict_hunks.clone(),
    }
}

// ─── Client frame handlers ──────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn handle_studio_changed(
    state: &SharedState,
    path: String,
    content: String,
    claimed_sha256: String,
    kind_hint: Option<ScriptKind>,
    bootstrap_diverge: bool,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    if content.len() > MAX_CONTENT_BYTES {
        warn!(
            path = %path,
            bytes = content.len(),
            cap = MAX_CONTENT_BYTES,
            "rejecting client-supplied content: exceeds MAX_CONTENT_BYTES"
        );
        broadcast_server_msg(
            state,
            bcast_tx,
            ServerMsg::SyncError {
                kind: SyncErrorKind::OversizedContent,
                path: path.clone(),
                reason: format!(
                    "content length {} exceeds cap of {} bytes",
                    content.len(),
                    MAX_CONTENT_BYTES
                ),
            },
        )
        .await;
        return Ok(());
    }
    // Reject the path early if it tries to escape the project root. `path`
    // comes straight from the wire and every write path downstream hits
    // `resolve_inside` eventually, but failing here keeps merge/tree state
    // from being mutated against a path we'd refuse to write anyway.
    //
    // The `is_under_mapping` check is the second gate: `resolve_inside`
    // only stops sandbox escape (../, NUL, absolute). A path like
    // `default.project.json` or `README.md` passes that check but is
    // outside any declared `$path` mapping. Refuse those — the FS
    // watcher already drops them at line ~412, mirror that here so
    // `FileChanged`/`FileCreated` can't be used as a back door to
    // overwrite project-root scaffolding.
    {
        let guard = state.read().await;
        if let Err(e) = resolve_inside(&guard.root, &path) {
            let reason = format!("{e:#}");
            warn!(path = %path, error = %reason, "rejecting client file_changed/created: unsafe path");
            drop(guard);
            broadcast_server_msg(
                state,
                bcast_tx,
                ServerMsg::SyncError {
                    kind: SyncErrorKind::UnsafePath,
                    path: path.clone(),
                    reason,
                },
            )
            .await;
            return Ok(());
        }
        if !guard.is_under_mapping(&path) {
            let reason =
                "path is inside project root but outside every declared $path mapping".to_string();
            warn!(path = %path, "rejecting client file_changed/created: path not under any mapping");
            drop(guard);
            broadcast_server_msg(
                state,
                bcast_tx,
                ServerMsg::SyncError {
                    kind: SyncErrorKind::UnsafePath,
                    path: path.clone(),
                    reason,
                },
            )
            .await;
            return Ok(());
        }
    }
    {
        let guard = state.read().await;
        if guard.pending_collisions.contains(&path) {
            drop(guard);
            broadcast_server_msg(
                state,
                bcast_tx,
                ServerMsg::SyncError {
                    kind: SyncErrorKind::NameCollisionPending,
                    path: path.clone(),
                    reason: format!(
                        "sync paused for {path}: two Studio scripts collide at this path"
                    ),
                },
            )
            .await;
            return Ok(());
        }
    }
    let actual_sha = sha256_hex(content.as_bytes());
    if claimed_sha256 != actual_sha {
        // Hash disagreement is a protocol violation: either the plugin
        // computed sha differently from the daemon (bug) or the content
        // was tampered with mid-flight (impossible over loopback today
        // but matters if we ever ship over a network). Reject the frame
        // entirely so we don't silently absorb the inconsistency into
        // tree_studio. SyncError tells the user the file did NOT sync
        // and points at the hash divergence.
        warn!(
            path = %path,
            claimed = %claimed_sha256,
            actual = %actual_sha,
            "rejecting frame: client hash disagrees with its own content"
        );
        broadcast_server_msg(
            state,
            bcast_tx,
            ServerMsg::SyncError {
                kind: SyncErrorKind::HashMismatch,
                path: path.clone(),
                reason: format!(
                    "claimed sha256 {claimed_sha256} does not match content (recomputed {actual_sha})"
                ),
            },
        )
        .await;
        return Ok(());
    }
    let mut guard = state.write().await;
    let debug_echo = guard.debug_echo;
    let prev_hash = guard.tree_studio.get(&path).map(|e| e.sha256.clone());
    if prev_hash.as_deref() == Some(actual_sha.as_str()) {
        // Log but don't early-return: the plugin may have echoed Studio's
        // current Source during bootstrap-diverge and it happens to match
        // `tree_studio`'s cloned-from-`tree_base` placeholder. Dropping
        // here would skip the reconcile and miss a genuine disk-vs-Studio
        // mismatch. Plugin-side `recentlyApplied` is the real anti-echo.
        log_echo(
            debug_echo,
            format_args!("client echo (hash match, still reconciling): path={path} hash={actual_sha}"),
        );
    }
    // Kind: trust an existing base/studio entry, fall back to classifying
    // the file name, fall back to the plugin's hint.
    let kind = guard
        .tree_base
        .get(&path)
        .map(|e| e.kind)
        .or_else(|| guard.tree_studio.get(&path).map(|e| e.kind))
        .or_else(|| {
            let file_name = Path::new(&path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            classify(file_name).map(|(_, k)| k)
        })
        .or(kind_hint)
        .context("cannot classify path")?;
    let entry = TreeEntry {
        kind,
        content: content.clone(),
        sha256: actual_sha.clone(),
    };
    guard.tree_studio.insert(path.clone(), entry);
    // Bootstrap-diverge echoes use a 2-way compare (Studio vs IDE) instead
    // of the 3-way merge. The merge engine's "only one side changed
    // relative to base" optimization auto-applies Studio→disk (or the
    // reverse) without surfacing a resolver, which mismatches the user's
    // expectation for the initial connection: they want to see both sides
    // and pick. If content equals `tree_fs`, there's nothing to surface.
    if bootstrap_diverge {
        let fs_snapshot = guard.tree_fs.get(&path).cloned();
        drop(guard);
        let Some(fs_entry) = fs_snapshot else {
            // No disk side — the sync pipeline will create it later via
            // the normal reconcile. Treat as a non-bootstrap change.
            return reconcile_path(state, &path, bcast_tx).await;
        };
        if fs_entry.sha256 == actual_sha {
            return Ok(());
        }
        let conflict = FileConflict {
            path: path.clone(),
            conflict_kind: ConflictKind::Edit,
            script_kind: kind,
            base_content: None,
            studio_content: Some(content.clone()),
            fs_content: Some(fs_entry.content.clone()),
            conflict_hunks: two_way_whole_file_hunk(&content, &fs_entry.content),
            auto_hunks: vec![],
        };
        return record_conflict(state, conflict, bcast_tx).await;
    }
    drop(guard);
    reconcile_path(state, &path, bcast_tx).await
}

/// Mirrors `merge.rs::whole_file_conflict` but keeps the construction local
/// so we don't re-export an internal helper. Used for 2-way bootstrap
/// divergences where `tree_base` is deliberately out of the picture.
fn two_way_whole_file_hunk(studio: &str, fs: &str) -> Vec<ConflictHunk> {
    let studio_lines = studio.split_inclusive('\n').count();
    let fs_lines = fs.split_inclusive('\n').count();
    vec![ConflictHunk {
        id: "hunk_0".to_owned(),
        base_range: [0, 0],
        studio_range: [0, studio_lines],
        fs_range: [0, fs_lines],
        studio_text: studio.to_owned(),
        fs_text: fs.to_owned(),
        context_before: String::new(),
        context_after: String::new(),
    }]
}

async fn handle_studio_deleted(
    state: &SharedState,
    path: String,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    let mut guard = state.write().await;
    if let Err(e) = resolve_inside(&guard.root, &path) {
        warn!(path = %path, error = ?e, "rejecting client file_deleted: unsafe path");
        return Ok(());
    }
    if guard.tree_studio.remove(&path).is_none() {
        return Ok(());
    }
    drop(guard);
    reconcile_path(state, &path, bcast_tx).await
}

/// Classifies a `(old_path, new_path)` pair the plugin sent in a
/// `FileRenamed` frame into one of four filesystem operations.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum RenameCase {
    /// Both ends are leaf script files (e.g. `Foo.luau` → `Bar.luau`).
    /// Straight `fs::rename` after `mkdir -p` on the new parent.
    LeafToLeaf,
    /// Both ends are `init.luau` (etc.) inside a script-container folder.
    /// Rename the *directory* — moves every child in one shot.
    DirToDir,
    /// Leaf → container: `Foo.luau` becomes `Foo/init.luau` because a
    /// child was added to it in Studio. The path collision (`Foo.luau`
    /// occupies the slot where the new directory `Foo/` needs to land)
    /// is sidestepped via a tmp file.
    Promote,
    /// Container → leaf: the last child was removed; `Foo/init.luau`
    /// folds back into a flat `Foo.luau`. Refuses to demote if the
    /// directory still has other children — guards against the plugin
    /// emitting the rename before the watcher saw the child deletion.
    Demote,
}

impl RenameCase {
    fn classify(old_path: &str, new_path: &str) -> Self {
        let old_is_init = is_init_filename(old_path);
        let new_is_init = is_init_filename(new_path);
        match (old_is_init, new_is_init) {
            (false, false) => Self::LeafToLeaf,
            (true, true) => Self::DirToDir,
            (false, true) => Self::Promote,
            (true, false) => Self::Demote,
        }
    }
}

/// Wraps `std::fs::rename` with a small retry loop. The project commonly
/// lives inside `OneDrive\Documentos\...`; the OneDrive client momentarily
/// holds the rename target during cloud sync and the first attempt then
/// fails with `Access is denied`. Three tries with 100/200/300 ms backoff
/// covers every observed OneDrive hold without making real failures slow.
fn fs_rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut last_err: Option<std::io::Error> = None;
    for attempt in 0..3u64 {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                // PermissionDenied is the OneDrive/antivirus signal on
                // Windows; ErrorKind::Other covers Windows's
                // STATUS_SHARING_VIOLATION when the file is open elsewhere.
                let recoverable = matches!(
                    e.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Other
                );
                if !recoverable {
                    return Err(e);
                }
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(100 * (attempt + 1)));
            }
        }
    }
    Err(last_err.unwrap())
}

/// Performs every disk mutation for `handle_studio_renamed` and returns
/// a `String` error on the first failure. The async caller catches the
/// `Err` and emits a `SyncError` over the WebSocket so the plugin dock
/// shows what actually went wrong (instead of swallowing the failure in
/// the daemon's stderr log, which is invisible to the user).
fn perform_rename_io(
    case: RenameCase,
    old_abs: &Path,
    new_abs: &Path,
    new_meta: FileMeta,
    content: &str,
    old_path_disp: &str,
    new_path_disp: &str,
) -> Result<(), String> {
    fn io_step<E: std::fmt::Display>(op: &str, e: E) -> String {
        format!("{op}: {e}")
    }

    match case {
        RenameCase::LeafToLeaf => {
            if let Some(parent) = new_abs.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| io_step(&format!("mkdir -p {}", parent.display()), e))?;
            }
            if old_abs.is_file() {
                fs_rename_with_retry(old_abs, new_abs).map_err(|e| {
                    io_step(
                        &format!("rename {} -> {}", old_abs.display(), new_abs.display()),
                        e,
                    )
                })?;
            } else {
                let encoded = encode_for_disk(content, new_meta);
                atomic_write(new_abs, encoded.as_bytes())
                    .map_err(|e| io_step(&format!("write {}", new_abs.display()), e))?;
            }
        }
        RenameCase::DirToDir => {
            let old_dir = old_abs
                .parent()
                .ok_or_else(|| format!("init path {old_path_disp} has no parent dir"))?
                .to_path_buf();
            let new_dir = new_abs
                .parent()
                .ok_or_else(|| format!("init path {new_path_disp} has no parent dir"))?
                .to_path_buf();
            if let Some(parent) = new_dir.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| io_step(&format!("mkdir -p {}", parent.display()), e))?;
            }
            if old_dir.is_dir() {
                fs_rename_with_retry(&old_dir, &new_dir).map_err(|e| {
                    io_step(
                        &format!("rename dir {} -> {}", old_dir.display(), new_dir.display()),
                        e,
                    )
                })?;
            } else {
                std::fs::create_dir_all(&new_dir)
                    .map_err(|e| io_step(&format!("mkdir {}", new_dir.display()), e))?;
                let encoded = encode_for_disk(content, new_meta);
                atomic_write(new_abs, encoded.as_bytes())
                    .map_err(|e| io_step(&format!("write {}", new_abs.display()), e))?;
            }
        }
        RenameCase::Promote => {
            let tmp = {
                let mut os = old_abs.as_os_str().to_owned();
                os.push(".yeet-tmp");
                PathBuf::from(os)
            };
            if old_abs.is_file() {
                fs_rename_with_retry(old_abs, &tmp).map_err(|e| {
                    io_step(
                        &format!("stage {} -> {}", old_abs.display(), tmp.display()),
                        e,
                    )
                })?;
            }
            if let Some(parent) = new_abs.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| io_step(&format!("mkdir -p {}", parent.display()), e))?;
            }
            if tmp.is_file() {
                fs_rename_with_retry(&tmp, new_abs).map_err(|e| {
                    io_step(
                        &format!("finalize {} -> {}", tmp.display(), new_abs.display()),
                        e,
                    )
                })?;
            } else {
                let encoded = encode_for_disk(content, new_meta);
                atomic_write(new_abs, encoded.as_bytes())
                    .map_err(|e| io_step(&format!("write {}", new_abs.display()), e))?;
            }
        }
        RenameCase::Demote => {
            let old_dir = old_abs
                .parent()
                .ok_or_else(|| format!("init path {old_path_disp} has no parent dir"))?
                .to_path_buf();
            let extra_children = match std::fs::read_dir(&old_dir) {
                Ok(it) => it
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path() != *old_abs)
                    .count(),
                Err(_) => 0,
            };
            if extra_children > 0 {
                return Err(format!(
                    "demote rename refused: directory {} still has {extra_children} child(ren)",
                    old_dir.display()
                ));
            }
            if let Some(parent) = new_abs.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| io_step(&format!("mkdir -p {}", parent.display()), e))?;
            }
            if old_abs.is_file() {
                fs_rename_with_retry(old_abs, new_abs).map_err(|e| {
                    io_step(
                        &format!("rename {} -> {}", old_abs.display(), new_abs.display()),
                        e,
                    )
                })?;
            } else {
                let encoded = encode_for_disk(content, new_meta);
                atomic_write(new_abs, encoded.as_bytes())
                    .map_err(|e| io_step(&format!("write {}", new_abs.display()), e))?;
            }
            if old_dir.is_dir() {
                let _ = std::fs::remove_dir(&old_dir);
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_studio_renamed(
    state: &SharedState,
    old_path: String,
    new_path: String,
    kind: ScriptKind,
    content: String,
    claimed_sha256: String,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    info!(
        old = %old_path,
        new = %new_path,
        kind = ?kind,
        bytes = content.len(),
        "handle_studio_renamed: entered"
    );
    if content.len() > MAX_CONTENT_BYTES {
        warn!(
            old = %old_path,
            new = %new_path,
            bytes = content.len(),
            cap = MAX_CONTENT_BYTES,
            "rejecting client file_renamed: exceeds MAX_CONTENT_BYTES"
        );
        broadcast_server_msg(
            state,
            bcast_tx,
            ServerMsg::SyncError {
                kind: SyncErrorKind::OversizedContent,
                path: new_path.clone(),
                reason: format!(
                    "content length {} exceeds cap of {} bytes",
                    content.len(),
                    MAX_CONTENT_BYTES
                ),
            },
        )
        .await;
        return Ok(());
    }
    let actual_sha = sha256_hex(content.as_bytes());
    if claimed_sha256 != actual_sha {
        warn!(
            old = %old_path,
            new = %new_path,
            claimed = %claimed_sha256,
            actual = %actual_sha,
            "rejecting file_renamed: hash mismatch"
        );
        broadcast_server_msg(
            state,
            bcast_tx,
            ServerMsg::SyncError {
                kind: SyncErrorKind::HashMismatch,
                path: new_path.clone(),
                reason: format!(
                    "claimed sha256 {claimed_sha256} does not match content (recomputed {actual_sha})"
                ),
            },
        )
        .await;
        return Ok(());
    }
    {
        let guard = state.read().await;
        for p in [&old_path, &new_path] {
            if let Err(e) = resolve_inside(&guard.root, p) {
                let reason = format!("{e:#}");
                warn!(path = %p, error = %reason, "rejecting file_renamed: unsafe path");
                drop(guard);
                broadcast_server_msg(
                    state,
                    bcast_tx,
                    ServerMsg::SyncError {
                        kind: SyncErrorKind::UnsafePath,
                        path: p.clone(),
                        reason,
                    },
                )
                .await;
                return Ok(());
            }
            if !guard.is_under_mapping(p) {
                let reason = "path is inside project root but outside every declared $path mapping"
                    .to_string();
                warn!(path = %p, "rejecting file_renamed: path not under any mapping");
                drop(guard);
                broadcast_server_msg(
                    state,
                    bcast_tx,
                    ServerMsg::SyncError {
                        kind: SyncErrorKind::UnsafePath,
                        path: p.clone(),
                        reason,
                    },
                )
                .await;
                return Ok(());
            }
        }
        if guard.pending_collisions.contains(&old_path)
            || guard.pending_collisions.contains(&new_path)
        {
            let stuck = if guard.pending_collisions.contains(&old_path) {
                old_path.clone()
            } else {
                new_path.clone()
            };
            drop(guard);
            broadcast_server_msg(
                state,
                bcast_tx,
                ServerMsg::SyncError {
                    kind: SyncErrorKind::NameCollisionPending,
                    path: stuck.clone(),
                    reason: format!(
                        "sync paused for {stuck}: two Studio scripts collide at this path"
                    ),
                },
            )
            .await;
            // Don't return early — the rename itself is the resolution
            // signal the user needs to clear the collision, so we still
            // perform it below. Re-take the lock and proceed.
        }
    }
    {
        let guard = state.read().await;
        if guard.dry_run {
            audit::record(
                &guard.root,
                &audit::Entry {
                    ts: audit::now_rfc3339(),
                    kind: audit::Kind::FsRename,
                    path: &new_path,
                    sha_before: Some(&old_path),
                    sha_after: Some(&actual_sha),
                    session_id: &guard.session_id,
                    note: Some("dry-run: suppressed"),
                },
            );
            info!(old = %old_path, new = %new_path, "DRY-RUN file_renamed suppressed");
            return Ok(());
        }
    }

    let mut guard = state.write().await;
    let old_abs = match resolve_inside(&guard.root, &old_path) {
        Ok(p) => p,
        Err(e) => {
            let reason = format!("resolve old_path {old_path}: {e:#}");
            drop(guard);
            broadcast_server_msg(
                state,
                bcast_tx,
                ServerMsg::SyncError {
                    kind: SyncErrorKind::HandlerFailed,
                    path: old_path.clone(),
                    reason,
                },
            )
            .await;
            return Ok(());
        }
    };
    let new_abs = match resolve_inside(&guard.root, &new_path) {
        Ok(p) => p,
        Err(e) => {
            let reason = format!("resolve new_path {new_path}: {e:#}");
            drop(guard);
            broadcast_server_msg(
                state,
                bcast_tx,
                ServerMsg::SyncError {
                    kind: SyncErrorKind::HandlerFailed,
                    path: new_path.clone(),
                    reason,
                },
            )
            .await;
            return Ok(());
        }
    };
    let case = RenameCase::classify(&old_path, &new_path);
    let new_meta = guard.meta_for(&new_path);

    // Run all I/O outside the lock-holding path. Errors come back as
    // human-readable strings; we surface them to the plugin so the user
    // sees the real failure (OneDrive lock, antivirus, missing source,
    // demote refused — whatever it is) instead of a silent no-op.
    if let Err(reason) =
        perform_rename_io(case, &old_abs, &new_abs, new_meta, &content, &old_path, &new_path)
    {
        warn!(
            old = %old_path,
            new = %new_path,
            error = %reason,
            "handle_studio_renamed: I/O failed"
        );
        drop(guard);
        broadcast_server_msg(
            state,
            bcast_tx,
            ServerMsg::SyncError {
                kind: SyncErrorKind::HandlerFailed,
                path: new_path.clone(),
                reason,
            },
        )
        .await;
        return Ok(());
    }

    // Update trees. For DirToDir we also have to re-key every descendant
    // path that lived under the old directory.
    let old_meta = guard.meta.get(&old_path).copied();
    let new_entry = TreeEntry {
        kind,
        content: content.clone(),
        sha256: actual_sha.clone(),
    };

    if matches!(case, RenameCase::DirToDir) {
        let old_dir_prefix = format!(
            "{}/",
            old_path
                .rsplit_once('/')
                .map(|(d, _)| d)
                .unwrap_or_default()
        );
        let new_dir_prefix = format!(
            "{}/",
            new_path
                .rsplit_once('/')
                .map(|(d, _)| d)
                .unwrap_or_default()
        );
        rekey_tree(&mut guard.tree_fs, &old_dir_prefix, &new_dir_prefix);
        rekey_tree(&mut guard.tree_base, &old_dir_prefix, &new_dir_prefix);
        rekey_tree(&mut guard.tree_studio, &old_dir_prefix, &new_dir_prefix);
        rekey_meta(&mut guard.meta, &old_dir_prefix, &new_dir_prefix);
        rekey_meta_attributes(&mut guard.meta_attributes, &old_dir_prefix, &new_dir_prefix);
    } else {
        guard.tree_fs.remove(&old_path);
        guard.tree_base.remove(&old_path);
        guard.tree_studio.remove(&old_path);
        guard.meta.remove(&old_path);
        if let Some(attrs) = guard.meta_attributes.remove(&old_path) {
            guard.meta_attributes.insert(new_path.clone(), attrs);
        }
    }
    guard.tree_fs.insert(new_path.clone(), new_entry.clone());
    guard.tree_base.insert(new_path.clone(), new_entry.clone());
    guard.tree_studio.insert(new_path.clone(), new_entry);
    if let Some(meta) = old_meta {
        guard.meta.insert(new_path.clone(), meta);
    }

    guard.note_rename_echo(old_path.clone(), new_path.clone());
    let collision_cleared =
        guard.pending_collisions.remove(&old_path) | guard.pending_collisions.remove(&new_path);
    if let Err(e) = persist_base(&guard) {
        let reason = format!("persist_base after rename {old_path} -> {new_path}: {e:#}");
        warn!(error = %reason, "handle_studio_renamed: persist_base failed");
        drop(guard);
        broadcast_server_msg(
            state,
            bcast_tx,
            ServerMsg::SyncError {
                kind: SyncErrorKind::HandlerFailed,
                path: new_path.clone(),
                reason,
            },
        )
        .await;
        return Ok(());
    }
    let root = guard.root.clone();
    let session_id = guard.session_id.clone();
    drop(guard);

    audit::record(
        &root,
        &audit::Entry {
            ts: audit::now_rfc3339(),
            kind: audit::Kind::FsRename,
            path: &new_path,
            sha_before: Some(&old_path),
            sha_after: Some(&actual_sha),
            session_id: &session_id,
            note: None,
        },
    );

    broadcast_server_msg(
        state,
        bcast_tx,
        ServerMsg::FileRenamed {
            old_path: old_path.clone(),
            new_path: new_path.clone(),
            content,
            sha256: actual_sha,
            kind,
        },
    )
    .await;

    if collision_cleared {
        audit::record(
            &root,
            &audit::Entry {
                ts: audit::now_rfc3339(),
                kind: audit::Kind::NameCollisionResolved,
                path: &new_path,
                sha_before: None,
                sha_after: None,
                session_id: &session_id,
                note: Some("rename cleared collision"),
            },
        );
        broadcast_server_msg(
            state,
            bcast_tx,
            ServerMsg::NameCollision {
                path: String::new(),
                message: format!(
                    "name collision at {} resolved by rename to {}",
                    old_path, new_path
                ),
            },
        )
        .await;
    }
    Ok(())
}

/// Re-keys every entry in `tree` whose key starts with `old_prefix` to use
/// `new_prefix` instead. Used by DirToDir renames to move every descendant
/// of a renamed `Foo/` to `Bar/` in lockstep with the on-disk
/// `fs::rename(Foo, Bar)`.
fn rekey_tree(tree: &mut crate::tree::Tree, old_prefix: &str, new_prefix: &str) {
    let to_move: Vec<String> = tree
        .keys()
        .filter(|k| k.starts_with(old_prefix))
        .cloned()
        .collect();
    for old_key in to_move {
        let suffix = &old_key[old_prefix.len()..];
        let new_key = format!("{new_prefix}{suffix}");
        if let Some(entry) = tree.remove(&old_key) {
            tree.insert(new_key, entry);
        }
    }
}

fn rekey_meta(meta: &mut HashMap<String, FileMeta>, old_prefix: &str, new_prefix: &str) {
    let to_move: Vec<String> = meta
        .keys()
        .filter(|k: &&String| k.starts_with(old_prefix))
        .cloned()
        .collect();
    for old_key in to_move {
        let suffix = &old_key[old_prefix.len()..];
        let new_key = format!("{new_prefix}{suffix}");
        if let Some(entry) = meta.remove(&old_key) {
            meta.insert(new_key, entry);
        }
    }
}

fn rekey_meta_attributes<V>(
    map: &mut HashMap<String, V>,
    old_prefix: &str,
    new_prefix: &str,
) {
    let to_move: Vec<String> = map
        .keys()
        .filter(|k: &&String| k.starts_with(old_prefix))
        .cloned()
        .collect();
    for old_key in to_move {
        let suffix = &old_key[old_prefix.len()..];
        let new_key = format!("{new_prefix}{suffix}");
        if let Some(entry) = map.remove(&old_key) {
            map.insert(new_key, entry);
        }
    }
}

/// One match made by the handshake's offline-rename heuristic: a path
/// disappeared from `tree_base` and a new path showed up in the plugin's
/// `studio_snapshot` carrying the same `(kind, sha256)`. Promoted to a
/// real `fs::rename` so the git log keeps history instead of recording
/// delete + create.
#[derive(Debug, Clone)]
struct OfflineRenamePair {
    old_path: String,
    new_path: String,
    kind: ScriptKind,
    content: String,
    sha256: String,
}

/// Strips `path` down to the basename without script extension or Rojo
/// kind suffix. `src/Foo.server.luau` → `Foo`. Used for the offline-rename
/// tiebreaker — two candidates with the same content hash but different
/// basenames score lower than ones that kept the human-readable name.
fn rojo_basename_stem(path: &str) -> String {
    let basename = path.rsplit('/').next().unwrap_or(path);
    let lower = basename.to_ascii_lowercase();
    let after_ext = lower
        .strip_suffix(".luau")
        .or_else(|| lower.strip_suffix(".lua"))
        .unwrap_or(&lower);
    let after_kind = after_ext
        .strip_suffix(".server")
        .or_else(|| after_ext.strip_suffix(".client"))
        .unwrap_or(after_ext);
    after_kind.to_owned()
}

/// Score one candidate `(old, new)` pairing for the offline-rename
/// heuristic. Higher is better. Same basename dominates; longest common
/// path prefix breaks ties; lexicographic order is the deterministic
/// fallback so the same pair always wins for the same inputs.
fn score_offline_pair(old_path: &str, new_path: &str) -> i64 {
    let basename_match: i64 = if rojo_basename_stem(old_path) == rojo_basename_stem(new_path) {
        100_000
    } else {
        0
    };
    let prefix_len: i64 = old_path
        .as_bytes()
        .iter()
        .zip(new_path.as_bytes().iter())
        .take_while(|(a, b)| a == b)
        .count() as i64;
    basename_match + prefix_len
}

/// Compares `tree_base` (what the daemon last saw at the end of the
/// previous session) against the plugin's just-arrived `studio_snapshot`
/// to spot renames that happened while the plugin was offline. A "rename"
/// here means the same `(kind, sha256)` shows up under a different path.
/// Empty content is excluded — boilerplate `return nil`-style scripts
/// would collide constantly and the false-positive cost outweighs the
/// occasional missed history-preserving rename.
fn detect_offline_renames(
    tree_base: &crate::tree::Tree,
    snapshot: &[StudioFileSnapshot],
) -> Vec<OfflineRenamePair> {
    let empty_sha = sha256_hex(b"");
    let snapshot_paths: HashSet<&str> = snapshot.iter().map(|s| s.path.as_str()).collect();

    let removed: Vec<(&String, &TreeEntry)> = tree_base
        .iter()
        .filter(|(p, _)| !snapshot_paths.contains(p.as_str()))
        .collect();

    let mut buckets: HashMap<(ScriptKind, String), Vec<&StudioFileSnapshot>> = HashMap::new();
    for s in snapshot {
        if tree_base.contains_key(&s.path) {
            continue;
        }
        if s.sha256 == empty_sha {
            continue;
        }
        buckets
            .entry((s.kind, s.sha256.clone()))
            .or_default()
            .push(s);
    }

    // Sort removed paths so iteration order is stable across runs.
    let mut removed_sorted = removed;
    removed_sorted.sort_by(|a, b| a.0.cmp(b.0));

    let mut pairs: Vec<OfflineRenamePair> = Vec::new();
    let mut consumed: HashSet<String> = HashSet::new();

    for (old_path, entry) in removed_sorted {
        if entry.sha256 == empty_sha {
            continue;
        }
        let key = (entry.kind, entry.sha256.clone());
        let Some(cands) = buckets.get(&key) else {
            continue;
        };
        let mut best: Option<(i64, &StudioFileSnapshot)> = None;
        for cand in cands {
            if consumed.contains(&cand.path) {
                continue;
            }
            let score = score_offline_pair(old_path, &cand.path);
            match best {
                None => best = Some((score, cand)),
                Some((s, _)) if score > s => best = Some((score, cand)),
                Some((s, prev)) if score == s && cand.path < prev.path => {
                    best = Some((score, cand));
                }
                _ => {}
            }
        }
        if let Some((_, picked)) = best {
            consumed.insert(picked.path.clone());
            pairs.push(OfflineRenamePair {
                old_path: old_path.clone(),
                new_path: picked.path.clone(),
                kind: entry.kind,
                content: entry.content.clone(),
                sha256: entry.sha256.clone(),
            });
        }
    }
    pairs
}

/// Drops `Foo.luau` from `tree_fs` (and the disk file alongside it) when
/// a `Foo/init.luau` (or its `.server`/`.client` flavors) lives in the
/// same directory. Without this sweep, a project that survived a buggy
/// promotion in a previous session would re-materialize both copies in
/// Studio every reconnect — see Bug #2 in the plan.
async fn sweep_leaf_folder_duplicates(
    state: &SharedState,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) {
    let duplicates: Vec<String> = {
        let guard = state.read().await;
        let fs = &guard.tree_fs;
        fs.keys()
            .filter(|p| !is_init_filename(p))
            .filter(|p| {
                let basename = match p.rsplit('/').next() {
                    Some(b) => b,
                    None => return false,
                };
                let lower = basename.to_ascii_lowercase();
                let stem = match lower
                    .strip_suffix(".luau")
                    .or_else(|| lower.strip_suffix(".lua"))
                {
                    Some(s) => s,
                    None => return false,
                };
                let bare = stem
                    .strip_suffix(".server")
                    .or_else(|| stem.strip_suffix(".client"))
                    .unwrap_or(stem);
                let dir_prefix = p
                    .rsplit_once('/')
                    .map(|(d, _)| format!("{d}/"))
                    .unwrap_or_default();
                let init_candidates = [
                    format!("{dir_prefix}{bare}/init.luau"),
                    format!("{dir_prefix}{bare}/init.lua"),
                    format!("{dir_prefix}{bare}/init.server.luau"),
                    format!("{dir_prefix}{bare}/init.server.lua"),
                    format!("{dir_prefix}{bare}/init.client.luau"),
                    format!("{dir_prefix}{bare}/init.client.lua"),
                ];
                init_candidates.iter().any(|c| fs.contains_key(c.as_str()))
            })
            .cloned()
            .collect()
    };
    for path in duplicates {
        warn!(path = %path, "handshake sweep: dropping leaf script that duplicates X/init.luau");
        broadcast_server_msg(
            state,
            bcast_tx,
            ServerMsg::SyncError {
                kind: SyncErrorKind::SnapshotEntryDropped,
                path: path.clone(),
                reason: format!(
                    "{path} duplicates an existing init.luau in the same folder; dropping the leaf"
                ),
            },
        )
        .await;
        if let Err(e) = delete_from_fs(state, &path, bcast_tx, true).await {
            warn!(path = %path, error = ?e, "sweep delete failed");
        }
    }
}

async fn handle_name_collision(
    state: &SharedState,
    path: String,
    _sha256: String,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    let (root, session_id, message) = {
        let mut guard = state.write().await;
        // Validate but don't reject — the path is informational; even an
        // unsafe one is worth logging so the user sees the warning.
        let inserted = guard.pending_collisions.insert(path.clone());
        let root = guard.root.clone();
        let session_id = guard.session_id.clone();
        if !inserted {
            return Ok(());
        }
        let message = format!(
            "Two scripts collide at {path}. Rename one in Studio to resolve."
        );
        (root, session_id, message)
    };
    audit::record(
        &root,
        &audit::Entry {
            ts: audit::now_rfc3339(),
            kind: audit::Kind::NameCollisionDetected,
            path: &path,
            sha_before: None,
            sha_after: None,
            session_id: &session_id,
            note: None,
        },
    );
    broadcast_server_msg(
        state,
        bcast_tx,
        ServerMsg::NameCollision { path, message },
    )
    .await;
    Ok(())
}

async fn handle_conflict_resolved(
    state: &SharedState,
    resolutions: Vec<FileResolution>,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    for res in resolutions {
        let Some(conflict) = ({
            let mut guard = state.write().await;
            guard.pending_conflicts.remove(&res.path)
        }) else {
            warn!(path = %res.path, "ConflictResolved for unknown path");
            continue;
        };
        let action = resolve_to_action(&conflict, &res.hunks);
        let kind = conflict.script_kind;
        // Audit the resolution decision separately from the write/delete
        // it produces — the write/delete also emits its own audit entry,
        // but we want the explicit "user picked X" line so the post-mortem
        // can tie a content change to a deliberate choice.
        let action_label = match &action {
            ResolvedAction::Write(_) => "write",
            ResolvedAction::Delete => "delete",
        };
        {
            let guard = state.read().await;
            audit::record(
                &guard.root,
                &audit::Entry {
                    ts: audit::now_rfc3339(),
                    kind: audit::Kind::ConflictResolved,
                    path: &res.path,
                    sha_before: None,
                    sha_after: None,
                    session_id: &guard.session_id,
                    note: Some(action_label),
                },
            );
        }
        match action {
            ResolvedAction::Write(content) => {
                write_to_fs(state, &res.path, content, kind, bcast_tx, true).await?;
            }
            ResolvedAction::Delete => {
                delete_from_fs(state, &res.path, bcast_tx, true).await?;
            }
        }
    }
    Ok(())
}

/// Same as `handle_conflict_resolved` but bypasses the per-hunk
/// `resolve_to_action` rebuild — the user has already produced the final
/// merged content via the line-level merge picker, so we just write it
/// to disk + Studio. Validates path safety, content size, and sha
/// (mirrors `handle_studio_changed`'s defenses) so a bad frame can't
/// land arbitrary bytes anywhere.
async fn handle_conflict_resolved_manual(
    state: &SharedState,
    path: String,
    content: String,
    claimed_sha256: String,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    if content.len() > MAX_CONTENT_BYTES {
        warn!(
            path = %path,
            bytes = content.len(),
            cap = MAX_CONTENT_BYTES,
            "rejecting manual conflict resolution: content exceeds MAX_CONTENT_BYTES"
        );
        broadcast_server_msg(
            state,
            bcast_tx,
            ServerMsg::SyncError {
                kind: SyncErrorKind::OversizedContent,
                path: path.clone(),
                reason: format!(
                    "content length {} exceeds cap of {} bytes",
                    content.len(),
                    MAX_CONTENT_BYTES
                ),
            },
        )
        .await;
        return Ok(());
    }
    {
        let guard = state.read().await;
        if let Err(e) = resolve_inside(&guard.root, &path) {
            let reason = format!("{e:#}");
            warn!(path = %path, error = %reason, "rejecting manual resolution: unsafe path");
            drop(guard);
            broadcast_server_msg(
                state,
                bcast_tx,
                ServerMsg::SyncError {
                    kind: SyncErrorKind::UnsafePath,
                    path: path.clone(),
                    reason,
                },
            )
            .await;
            return Ok(());
        }
        // Defence in depth: `resolve_inside` only stops sandbox escape
        // (../, absolute paths, NUL bytes). It does NOT stop a client
        // from naming a file inside `project_root` but OUTSIDE every
        // `$path` mapping declared in `default.project.json` —
        // e.g. `default.project.json` itself, `README.md`, `.yeet/
        // base-tree.msgpack`. The watcher path drops these via the
        // `is_under_mapping` check at line ~412; mirror that gate here
        // so `ConflictResolvedManual` can't be used as a back door to
        // overwrite project-root files that the FS watcher would have
        // refused to acknowledge in the first place.
        if !guard.is_under_mapping(&path) {
            let reason =
                "path is inside project root but outside every declared $path mapping".to_string();
            warn!(path = %path, "rejecting manual resolution: path not under any mapping");
            drop(guard);
            broadcast_server_msg(
                state,
                bcast_tx,
                ServerMsg::SyncError {
                    kind: SyncErrorKind::UnsafePath,
                    path: path.clone(),
                    reason,
                },
            )
            .await;
            return Ok(());
        }
    }
    let actual_sha = sha256_hex(content.as_bytes());
    if claimed_sha256 != actual_sha {
        warn!(
            path = %path,
            claimed = %claimed_sha256,
            actual = %actual_sha,
            "rejecting manual resolution: client hash disagrees with content"
        );
        broadcast_server_msg(
            state,
            bcast_tx,
            ServerMsg::SyncError {
                kind: SyncErrorKind::HashMismatch,
                path: path.clone(),
                reason: format!(
                    "claimed sha256 {claimed_sha256} does not match content (recomputed {actual_sha})"
                ),
            },
        )
        .await;
        return Ok(());
    }

    // Find the conflict so we know the script kind to write back to.
    // Fall back to classifying the path's basename if for some reason
    // the conflict was already removed (e.g. a stale frame retried after
    // a successful resolution).
    let (conflict, fallback_kind) = {
        let mut guard = state.write().await;
        let removed = guard.pending_conflicts.remove(&path);
        let kind = removed.as_ref().map(|c| c.script_kind).or_else(|| {
            let basename = std::path::Path::new(&path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            classify(basename).map(|(_, k)| k)
        });
        (removed, kind)
    };
    let Some(kind) = fallback_kind else {
        warn!(
            path = %path,
            "manual resolution for path with no pending conflict and unclassifiable name; skipping"
        );
        return Ok(());
    };
    if conflict.is_none() {
        warn!(
            path = %path,
            "manual resolution for unknown pending conflict; applying anyway via classified kind"
        );
    }

    // Audit the manual decision before the write so a post-mortem sees
    // the choice even if the write itself fails. `note: "manual"`
    // distinguishes from the per-hunk "write"/"delete" decisions.
    {
        let guard = state.read().await;
        audit::record(
            &guard.root,
            &audit::Entry {
                ts: audit::now_rfc3339(),
                kind: audit::Kind::ConflictResolved,
                path: &path,
                sha_before: None,
                sha_after: Some(&actual_sha),
                session_id: &guard.session_id,
                note: Some("manual"),
            },
        );
    }
    write_to_fs(state, &path, content, kind, bcast_tx, true).await
}

// ─── Handshake ──────────────────────────────────────────────────────────────

/// Ingests the plugin's Hello snapshot, runs merges across every known path,
/// applies clean outcomes inline, and returns the set of conflicts plus the
/// `initial_files` payload that should go into `ProjectOpened`.
///
/// Cold-boot path (empty `snapshot`): the plugin doesn't yet know what Studio
/// has in memory, so `tree_studio` stays empty here. Running the 3-way merge
/// against an empty `tree_studio` would be a lie — it cannot distinguish
/// "Studio agrees with base" from "Studio has unsaved edits" — and the
/// subsequent `apply_outcome` would clobber `tree_base` with the disk
/// content, masking any real divergence. Instead we send `initial_files`
/// derived from `tree_fs` and let the plugin's `TreeBuilder` divergence
/// detection echo the actual Studio `Source` back as `FileChanged`. Those
/// frames flow through the normal `reconcile_path` and surface conflicts
/// properly.
#[allow(clippy::too_many_lines)]
async fn handshake(
    state: &SharedState,
    snapshot: Vec<StudioFileSnapshot>,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<(Vec<FileConflictView>, Vec<FileSnapshot>, Project)> {
    let cold_boot = snapshot.is_empty();

    // 0a. Drop leaf scripts that duplicate a sibling `init.luau` on disk.
    //     Otherwise we'd materialize both in Studio on reconnect — see Bug #2.
    sweep_leaf_folder_duplicates(state, bcast_tx).await;

    // 0b. Detect renames that happened while the plugin was offline by
    //     matching disappeared `tree_base` paths against new `snapshot`
    //     paths with identical `(kind, sha256)`. Promote each pair to a
    //     real `fs::rename` so git keeps history.
    if !cold_boot {
        let pairs = {
            let guard = state.read().await;
            detect_offline_renames(&guard.tree_base, &snapshot)
        };
        if !pairs.is_empty() {
            let (root, session_id) = {
                let guard = state.read().await;
                (guard.root.clone(), guard.session_id.clone())
            };
            let count_note = format!("{} pair(s)", pairs.len());
            audit::record(
                &root,
                &audit::Entry {
                    ts: audit::now_rfc3339(),
                    kind: audit::Kind::OfflineRenamesDetected,
                    path: "",
                    sha_before: None,
                    sha_after: None,
                    session_id: &session_id,
                    note: Some(&count_note),
                },
            );
            for pair in &pairs {
                if let Err(e) = handle_studio_renamed(
                    state,
                    pair.old_path.clone(),
                    pair.new_path.clone(),
                    pair.kind,
                    pair.content.clone(),
                    pair.sha256.clone(),
                    bcast_tx,
                )
                .await
                {
                    warn!(
                        old = %pair.old_path,
                        new = %pair.new_path,
                        error = ?e,
                        "offline rename promotion failed; falling back to delete+create"
                    );
                }
            }
            // IMPORTANT: do NOT retain the snapshot. The previous version
            // dropped `new_path` entries from the snapshot here, reasoning
            // that the merge below would otherwise see them as "create".
            // But the next step (`tree_studio.clear()` + ingest) then
            // leaves `tree_studio[new_path] = MISSING`, while
            // `tree_base[new_path]` and `tree_fs[new_path]` are present
            // (from handle_studio_renamed). The 3-way merge reads that
            // as "Studio deleted it" and emits `Apply { side: Fs,
            // content: None }` — i.e. *deletes the file we just
            // created via rename*. Keep the snapshot intact so the
            // tree_studio ingest re-adds the new_path entries.
        }
    }

    // 1. Ingest the snapshot into tree_studio.
    {
        let mut guard = state.write().await;
        guard.tree_studio.clear();
        for snap in &snapshot {
            guard.tree_studio.insert(
                snap.path.clone(),
                TreeEntry {
                    kind: snap.kind,
                    content: snap.content.clone(),
                    sha256: snap.sha256.clone(),
                },
            );
        }
        // Cold boot: seed tree_studio with tree_base so subsequent merges
        // (from bootstrap-diverge echoes or bulk reconcile commands) have an
        // entry per known path. An empty tree_studio would make `merge_file`
        // interpret missing paths as "Studio deleted it", which is a
        // destructive mis-read. The plugin's divergence echoes override the
        // clone for paths where Studio's actual source differs.
        if cold_boot {
            let inner = &mut *guard;
            inner.tree_studio.clone_from(&inner.tree_base);
        }
    }

    if cold_boot {
        // Skip the merge entirely. Build `initial_files` straight from
        // `tree_fs` — running apply_outcome here would clobber tree_base
        // with disk content before the plugin's divergence echo arrives,
        // masking real conflicts. Divergences get discovered when the
        // plugin's TreeBuilder sees mismatches and echoes back.
        let (initial_files, project) = {
            let guard = state.read().await;
            let out: Vec<FileSnapshot> = guard
                .tree_fs
                .iter()
                .map(|(path, entry)| FileSnapshot {
                    path: path.clone(),
                    kind: entry.kind,
                    content: entry.content.clone(),
                    sha256: entry.sha256.clone(),
                })
                .collect();
            (out, guard.project.clone())
        };
        // Cold-boot still benefits from the sweep: the daemon may have
        // started up against a project where the user previously had
        // package-manager landings (Wally/pesde) full of `init.meta.json`
        // orphans and stale empty dirs. Without this, the user-visible
        // tree on first reconnect still shows ghost folders even though
        // the daemon could have cleaned them.
        sweep_project_after_handshake(state).await;
        info!(
            files = initial_files.len(),
            "handshake: cold boot, skipping merge (plugin will echo diverges)"
        );
        return Ok((Vec::new(), initial_files, project));
    }

    // 2. Merge every path we know about.
    let paths: Vec<String> = {
        let guard = state.read().await;
        let mut set: HashSet<&String> = HashSet::new();
        set.extend(guard.tree_base.keys());
        set.extend(guard.tree_studio.keys());
        set.extend(guard.tree_fs.keys());
        set.into_iter().cloned().collect()
    };

    let mut outcomes: Vec<(String, MergeOutcome)> = Vec::with_capacity(paths.len());
    {
        let guard = state.read().await;
        for path in paths {
            let outcome = merge_file(
                &path,
                guard.tree_base.get(&path),
                guard.tree_studio.get(&path),
                guard.tree_fs.get(&path),
            );
            outcomes.push((path, outcome));
        }
    }

    // 3. Apply each outcome. Conflicts get collected for the handshake reply
    //    rather than broadcasting — we bundle them into one ConflictDetected.
    let mut conflict_views: Vec<FileConflictView> = Vec::new();
    for (path, outcome) in outcomes {
        if let MergeOutcome::Conflict(c) = outcome {
            conflict_views.push(conflict_to_view(&c));
            let mut guard = state.write().await;
            guard.pending_conflicts.insert(path.clone(), c);
        } else {
            apply_outcome(state, &path, outcome, bcast_tx).await?;
        }
    }

    // Final sweep — see `sweep_project_after_handshake` for the rationale.
    sweep_project_after_handshake(state).await;

    // 4. Build initial_files. For non-conflict paths, send fs content (which
    //    is now merged). For conflict paths, send the plugin's own studio
    //    content back so the resolver UI doesn't implicitly accept fs's
    //    version before the user picks.
    let (initial_files, project) = {
        let guard = state.read().await;
        let mut out: Vec<FileSnapshot> = Vec::new();
        for (path, entry) in &guard.tree_fs {
            if guard.pending_conflicts.contains_key(path) {
                continue;
            }
            out.push(FileSnapshot {
                path: path.clone(),
                kind: entry.kind,
                content: entry.content.clone(),
                sha256: entry.sha256.clone(),
            });
        }
        for (path, conflict) in &guard.pending_conflicts {
            if let Some(content) = conflict.studio_content.as_ref() {
                let sha = sha256_hex(content.as_bytes());
                out.push(FileSnapshot {
                    path: path.clone(),
                    kind: conflict.script_kind,
                    content: content.clone(),
                    sha256: sha,
                });
            }
        }
        (out, guard.project.clone())
    };

    Ok((conflict_views, initial_files, project))
}

// ─── I/O helpers ────────────────────────────────────────────────────────────

/// Write `bytes` to `path` atomically by creating a sibling `.yeet.tmp` file
/// and renaming it over the target. `fs::rename` on all supported platforms
/// replaces the destination. If the daemon dies between create and rename the
/// stray tmp file is harmless and easy to spot.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = {
        let mut os = path.as_os_str().to_owned();
        os.push(".yeet.tmp");
        PathBuf::from(os)
    };
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("write body {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn log_echo(debug_echo: bool, args: std::fmt::Arguments<'_>) {
    if debug_echo {
        info!(target: "yeet::echo", "{args}");
    } else {
        trace!(target: "yeet::echo", "{args}");
    }
}

// ─── Connection lifecycle ───────────────────────────────────────────────────

/// RAII guard that decrements the live-connection counter when dropped.
/// Held by each `handle_connection` task so the counter stays accurate
/// regardless of which exit path the task takes (clean close, error,
/// panic — drop runs on every path). Incrementing happens in the
/// accept loop before `tokio::spawn` so that a flood of accept events
/// can't briefly run the count above the cap before the spawned tasks
/// observe each other.
struct ConnGuard(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

async fn accept_loop(
    listener: TcpListener,
    state: SharedState,
    sessions: Arc<Mutex<SyncbackSessions>>,
    bcast_tx: broadcast::Sender<Arc<ServerMsg>>,
    enforce_loopback_host: bool,
) -> Result<()> {
    // Counter of currently-open WebSocket connections. Bounded by
    // `MAX_CONCURRENT_CONNECTIONS` to prevent a malicious local client
    // from stacking N × WS_MAX_FRAME_BYTES allocations. Held in an
    // `Arc` so each spawned task can carry its own decrement-on-drop
    // guard.
    let live = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                error!(error = %e, "accept failed");
                continue;
            }
        };
        // Check-and-increment under AcqRel so two concurrent accepts
        // can't both observe `n < cap` and both spawn. AtomicUsize CAS
        // would be cleaner but `fetch_add` + revert is simpler and the
        // worst-case overshoot is still bounded (the next accept sees
        // the inflated count and rejects).
        let prev = live.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if prev >= MAX_CONCURRENT_CONNECTIONS {
            live.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            warn!(
                %peer,
                live_connections = prev,
                cap = MAX_CONCURRENT_CONNECTIONS,
                "rejecting connection: concurrent-connection cap reached"
            );
            // Drop the stream — closes the TCP socket without sending
            // a WebSocket close frame (we never upgraded). Caller sees
            // a clean RST/FIN.
            drop(stream);
            continue;
        }
        let guard = ConnGuard(live.clone());
        let state = state.clone();
        let sessions = sessions.clone();
        let bcast_tx = bcast_tx.clone();
        // Note: we deliberately do NOT subscribe to bcast_tx up-front. The
        // plugin session subscribes inside its drain lock so resume-replay
        // never overlaps with broadcast events that have already been
        // recorded into the delta buffer (E7).
        tokio::spawn(async move {
            // `guard` lives for the lifetime of this task; the counter
            // is decremented on every exit path via Drop.
            let _guard = guard;
            if let Err(e) =
                handle_connection(stream, peer, state, sessions, bcast_tx, enforce_loopback_host)
                    .await
            {
                error!(%peer, error = ?e, "connection closed with error");
            }
        });
    }
}

/// Strips the `:port` (or `]:port` for a bracketed IPv6 literal) from a
/// `host[:port]` authority, returning just the host. IPv6 literals keep their
/// brackets so `host_is_loopback` can strip them uniformly.
fn split_host(authority: &str) -> &str {
    if let Some(close) = authority.find(']') {
        // Bracketed IPv6 literal: the host is `[..]`; drop any trailing `:port`.
        return &authority[..=close];
    }
    match authority.rsplit_once(':') {
        Some((host, _port)) => host,
        None => authority,
    }
}

/// True iff `host` (no port; IPv6 may be bracketed) is a loopback host: the
/// literal `localhost`, or an IP that parses into the loopback range
/// (127.0.0.0/8 or ::1). Parsing as an IP is what defeats DNS-rebinding
/// look-alikes — `127.0.0.1.evil.com` and `localhost.evil.com` are neither
/// `localhost` nor a valid loopback IP, so they fail.
fn host_is_loopback(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let bare = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    bare.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Origin allowlist for the WS upgrade (AUDITORIA-YEET.md M24). An absent /
/// empty / `null` Origin passes — native clients (the Studio plugin) send no
/// browser Origin. A present http(s) Origin passes ONLY when its host is an
/// exact loopback host; every other http(s) Origin (any routable or look-alike
/// host, e.g. `localhost.evil.com`) is rejected. A non-http(s) Origin
/// (`file://`, an app scheme) passes — it is not a browser page on a routable
/// host. The old code used `host.starts_with("localhost")`/`"127."`, which
/// accepted rebinding look-alikes.
fn origin_is_allowed(origin: &str) -> bool {
    let origin = origin.trim();
    if origin.is_empty() || origin.eq_ignore_ascii_case("null") {
        return true;
    }
    let lower = origin.to_ascii_lowercase();
    let rest = match lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
    {
        Some(rest) => rest,
        None => return true,
    };
    let authority = rest
        .split(|c| c == '/' || c == '?' || c == '#')
        .next()
        .unwrap_or("");
    // Drop any userinfo (`user:pass@host`).
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    host_is_loopback(split_host(hostport))
}

/// Validates the WS upgrade `Host` header is loopback — defence against a
/// DNS-rebound name reaching a loopback-bound daemon. An absent/empty Host
/// passes (the Origin check is the primary gate and an absent Host is not a
/// rebinding vector). Only enforced when the daemon is bound to loopback;
/// `--allow-remote` deliberately opts out.
fn host_header_is_loopback(host: Option<&str>) -> bool {
    match host.map(str::trim) {
        None | Some("") => true,
        Some(h) => host_is_loopback(split_host(h)),
    }
}

/// True iff a `--bind` address targets a loopback interface only. Gates the
/// `--allow-remote` requirement (B13) and decides whether the loopback `Host`
/// header check (M24) is enforced.
fn is_loopback_bind_addr(addr: &str) -> bool {
    host_is_loopback(split_host(addr.trim()))
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    state: SharedState,
    sessions: Arc<Mutex<SyncbackSessions>>,
    bcast_tx: broadcast::Sender<Arc<ServerMsg>>,
    enforce_loopback_host: bool,
) -> Result<()> {
    // Pin the frame/message cap explicitly. `WS_MAX_FRAME_BYTES` is set
    // tighter than tungstenite's defaults to bound peak attacker-
    // controlled buffering — see the const's docstring. The cap applies
    // to BOTH directions (incoming reads and outgoing writes); the
    // accept-loop's connection-count guard is what bounds N × frame_size
    // worst case.
    let ws_config = WebSocketConfig {
        max_frame_size: Some(WS_MAX_FRAME_BYTES),
        max_message_size: Some(WS_MAX_MESSAGE_BYTES),
        ..WebSocketConfig::default()
    };
    // Origin header allowlist. Browsers ALWAYS attach `Origin: <page>` when
    // opening a WebSocket; native clients (Roblox Studio's WebStreamClient,
    // the extension, curl) send either no Origin, `null`, or a non-http(s)
    // scheme — all of which pass. A present http(s) Origin passes ONLY when
    // its host is an EXACT loopback host (`origin_is_allowed`): the realistic
    // DNS-rebinding attack lands on a rebound name like `localhost.evil.com`
    // whose Origin host is not loopback, so it is rejected. The old code
    // matched hosts by prefix (`starts_with("localhost")`), which let
    // `localhost.evil.com` through.
    //
    // When bound to loopback we additionally require the `Host` header to be
    // loopback — a rebound name shows up there too. This is skipped under
    // `--allow-remote`, where a non-loopback Host is expected.
    let origin_check = move |req: &Request, response: Response| -> Result<Response, ErrorResponse> {
        let origin_str = req
            .headers()
            .get("origin")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        // Always log so future debugging has visibility into what
        // Studio / extensions / ad-hoc clients send.
        info!(origin = %origin_str, "ws upgrade origin");
        if !origin_is_allowed(origin_str) {
            let body = format!("yeet-daemon: refusing non-loopback browser origin ({origin_str})");
            let mut err = ErrorResponse::new(Some(body));
            *err.status_mut() = StatusCode::FORBIDDEN;
            return Err(err);
        }
        if enforce_loopback_host {
            let host_hdr = req.headers().get("host").and_then(|v| v.to_str().ok());
            if !host_header_is_loopback(host_hdr) {
                let body = format!(
                    "yeet-daemon: refusing non-loopback Host header ({})",
                    host_hdr.unwrap_or("")
                );
                let mut err = ErrorResponse::new(Some(body));
                *err.status_mut() = StatusCode::FORBIDDEN;
                return Err(err);
            }
        }
        Ok(response)
    };
    let ws = accept_hdr_async_with_config(stream, origin_check, Some(ws_config))
        .await
        .context("ws handshake")?;
    info!(%peer, "client connected");
    let (mut writer, mut reader) = ws.split();

    let first = reader
        .next()
        .await
        .context("connection closed before hello")??;
    let (version, snapshot, role, claimed_session, request_bootstrap_preview, claimed_auth) =
        match parse_client_msg(&first)? {
            ClientMsg::Hello {
                version,
                studio_snapshot,
                role,
                session_id,
                request_bootstrap_preview,
                auth_token,
            } => (
                version,
                studio_snapshot,
                role,
                session_id,
                request_bootstrap_preview,
                auth_token,
            ),
            other => {
                bail!("expected Hello as first frame, got {other:?}");
            }
        };

    // None or "plugin" → existing plugin flow. "extension" → new control
    // channel. Any other string is rejected so a typo doesn't silently get
    // treated as a plugin and corrupt the merge state.
    let role_label = role.as_deref().unwrap_or("plugin");
    info!(
        %peer,
        role = role_label,
        client_version = %version,
        snapshot_files = snapshot.len(),
        resume = claimed_session.is_some(),
        bootstrap_preview = request_bootstrap_preview,
        "handshake"
    );
    // Reject too-old clients up front. The previous behaviour was to log
    // `client_version = X` and proceed regardless, which let an out-of-
    // date plugin handshake against a newer daemon and silently
    // misinterpret fields the daemon had renamed/reshaped — a recipe for
    // the wrong content landing in Studio. With the check, the user sees
    // an immediate "incompatible plugin version" error and rebuilds.
    // Parse client version with `semver`. The crate respects the full
    // SemVer 2.0 ordering rules — including pre-release tags ranking
    // BELOW release versions ("0.3.0-beta" < "0.3.0") and major/minor/
    // patch components compared numerically. A malformed version string
    // (empty, garbled, missing dots) fails the parse and is rejected
    // with a clear error rather than silently slipping through under
    // the old lexicographic comparison.
    let plugin_v = match semver::Version::parse(version.as_str()) {
        Ok(v) => v,
        Err(e) => {
            warn!(
                %peer,
                client_version = %version,
                error = %e,
                "rejecting handshake: client version is not valid semver"
            );
            bail!(
                "client version {:?} is not a valid semver (expected e.g. \"0.3.0\")",
                version
            );
        }
    };
    let min_v = semver::Version::parse(MIN_COMPATIBLE_PLUGIN_VERSION)
        .expect("MIN_COMPATIBLE_PLUGIN_VERSION must be valid semver — fix the const");
    if plugin_v < min_v {
        warn!(
            %peer,
            client_version = %version,
            min_required = MIN_COMPATIBLE_PLUGIN_VERSION,
            "rejecting handshake: client version below minimum"
        );
        bail!(
            "incompatible client version {} (daemon requires >= {})",
            version,
            MIN_COMPATIBLE_PLUGIN_VERSION
        );
    }
    // Auth gate (after version check so an incompatible client gets
    // a fast loud disconnect instead of an AuthChallenge it can't
    // act on). Plugin clients without a token go through a pair
    // dance (the function may consume additional frames from
    // `reader` and write to `writer`); extension/CLI clients must
    // present a valid token up-front.
    authenticate_or_pair(
        &state,
        &claimed_auth,
        role_label,
        &peer,
        &mut reader,
        &mut writer,
    )
    .await
    .context("auth")?;

    // `request_bootstrap_preview` is advisory — the daemon's behavior is the
    // same either way. A plugin that promises a preview follows `ProjectOpened`
    // with a `StudioSnapshotReport`; one that doesn't simply never sends it.
    // The field is logged so a mismatched flow is traceable in the journal.
    let _ = request_bootstrap_preview;

    match role_label {
        "extension" => run_extension_session(state, bcast_tx, peer, writer, reader).await,
        "plugin" => {
            run_plugin_session(
                state,
                sessions,
                bcast_tx,
                peer,
                writer,
                reader,
                snapshot,
                claimed_session,
            )
            .await
        }
        other => bail!("unknown client role: {other}"),
    }
}

type WsWriter = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<TcpStream>,
    Message,
>;
type WsReader = futures_util::stream::SplitStream<tokio_tungstenite::WebSocketStream<TcpStream>>;

/// Auth decision, factored out of `authenticate_or_pair` so the policy is
/// unit-testable without a live socket. The I/O (sending `AuthGranted`,
/// deleting the breadcrumb, closing the connection) is the caller's job.
#[derive(Debug, PartialEq, Eq)]
enum AuthOutcome {
    /// Client is authorized; proceed to the session without sending a frame.
    Proceed,
    /// A plugin presented no token but a fresh pairing breadcrumb vouches for
    /// it: send `AuthGranted` (so it can cache the token) then proceed.
    GrantAndPair,
    /// Reject the connection. The caller closes the socket WITHOUT sending the
    /// server token; the `&'static str` is the log reason.
    Reject(&'static str),
}

/// Pure auth policy (AUDITORIA-YEET.md A16). Rules:
///   * A supplied token must match (constant-time). A mismatch is `Reject` —
///     the server token is NEVER echoed back. That echo was the A16 leak:
///     any client that guessed wrong received the real credential.
///   * `role == "extension"` (and any non-plugin role) MUST present a valid
///     token up-front. Those clients can read `.yeet/auth-token`, so there is
///     no breadcrumb fallback for them.
///   * A plugin with no token pairs via a fresh breadcrumb, or proceeds
///     tokenless when none is present (first-run Studio before the extension
///     is up — a legitimate flow we must keep).
fn decide_auth(
    claimed_auth: Option<&str>,
    role: &str,
    server_token: &str,
    breadcrumb_valid: bool,
) -> AuthOutcome {
    if let Some(token) = claimed_auth.filter(|t| !t.is_empty()) {
        return if auth::constant_time_eq(token, server_token) {
            AuthOutcome::Proceed
        } else {
            AuthOutcome::Reject("auth token mismatch")
        };
    }
    // No token offered. Breadcrumb pairing is a plugin-only convenience.
    if role != "plugin" {
        return AuthOutcome::Reject("missing auth token (required for this role)");
    }
    if breadcrumb_valid {
        AuthOutcome::GrantAndPair
    } else {
        AuthOutcome::Proceed
    }
}

/// Auth gate, run after the version check and before the role session starts.
/// Threat model: the realistic attacker at this layer is a remote browser tab
/// (DNS rebinding), blocked by the Origin/Host allowlist during the WS upgrade
/// (`handle_connection`) plus the loopback bind. The token/breadcrumb gate is
/// defence-in-depth so a local process still has to read a per-project file
/// rather than merely open the socket.
///
/// On rejection the connection is closed (`bail`) and NOTHING is written — in
/// particular the server token is never sent to a client that presented a
/// wrong one, and the session never reaches `ProjectOpened { initial_files }`
/// (that frame is emitted only inside the role session, after this returns
/// `Ok`).
async fn authenticate_or_pair(
    state: &SharedState,
    claimed_auth: &Option<String>,
    role: &str,
    peer: &SocketAddr,
    _reader: &mut WsReader,
    writer: &mut WsWriter,
) -> Result<()> {
    let (server_token, project_root_path) = {
        let guard = state.read().await;
        (guard.auth_token.clone(), guard.root.clone())
    };
    // The breadcrumb is consulted only for the plugin's no-token path; skip
    // the filesystem read entirely for every other role.
    let breadcrumb_valid =
        role == "plugin" && auth::pairing_breadcrumb_valid(&project_root_path);
    match decide_auth(claimed_auth.as_deref(), role, &server_token, breadcrumb_valid) {
        AuthOutcome::Proceed => {
            info!(%peer, %role, "auth: proceeding");
            Ok(())
        }
        AuthOutcome::GrantAndPair => {
            if let Err(e) = write_frame(
                writer,
                &ServerMsg::AuthGranted {
                    auth_token: server_token,
                },
            )
            .await
            {
                warn!(error = ?e, "auth: failed to send opportunistic auth_granted");
            } else {
                info!(%peer, %role, "auth: paired via breadcrumb (no token, fresh breadcrumb)");
            }
            // One-shot breadcrumb: delete after use so it can't pair a second
            // client. The extension's refresh timer recreates it within ~30s.
            if let Err(e) = auth::delete_pairing_breadcrumb(&project_root_path) {
                warn!(error = ?e, "failed to delete pairing breadcrumb after pair");
            }
            Ok(())
        }
        AuthOutcome::Reject(reason) => {
            warn!(
                %peer,
                %role,
                reason,
                "auth: rejecting connection (closing socket, server token NOT sent)"
            );
            bail!("auth rejected: {reason}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_plugin_session(
    state: SharedState,
    sessions: Arc<Mutex<SyncbackSessions>>,
    bcast_tx: broadcast::Sender<Arc<ServerMsg>>,
    peer: SocketAddr,
    mut writer: WsWriter,
    mut reader: WsReader,
    snapshot: Vec<StudioFileSnapshot>,
    claimed_session: Option<String>,
) -> Result<()> {
    // E7 resume path: if the plugin echoes back the current session_id, we
    // skip bootstrap and replay buffered events. The drain *and* the new
    // broadcast subscription must be created under the same write lock that
    // `broadcast_server_msg` takes — otherwise an event sent between drain
    // and subscribe would either be delivered twice (in both the drain Vec
    // and the new subscription) or lost entirely.
    let resume_outcome: Option<(Vec<ServerMsg>, broadcast::Receiver<Arc<ServerMsg>>)> =
        if let Some(claimed) = claimed_session.as_deref() {
            let mut guard = state.write().await;
            if guard.session_id == claimed {
                let frames = guard.drain_deltas();
                let rx = bcast_tx.subscribe();
                Some((frames, rx))
            } else {
                None
            }
        } else {
            None
        };

    let mut bcast_rx = if let Some((frames, rx)) = resume_outcome {
        let count = u32::try_from(frames.len()).unwrap_or(u32::MAX);
        info!(%peer, replayed = count, "plugin resumed");
        write_frame(
            &mut writer,
            &ServerMsg::Resumed {
                session_id: claimed_session.unwrap_or_default(),
                replayed: count,
            },
        )
        .await?;
        for frame in frames {
            write_frame(&mut writer, &frame).await?;
        }
        rx
    } else {
        if claimed_session.is_some() {
            info!(%peer, "claimed session_id did not match; falling back to full handshake");
        }
        // Fresh plugin session. Rotate the session id (and clear the buffer)
        // so an old plugin holding the previous id can't accidentally resume
        // into a stale view. Subscribe inside the same lock so handshake-time
        // broadcasts land in the new bcast_rx (the plugin de-dups via sha).
        let (new_session_id, rx, project_root) = {
            let mut guard = state.write().await;
            guard.rotate_session_id("plugin handshake (no resume)");
            let root = guard.root.display().to_string();
            (guard.session_id.clone(), bcast_tx.subscribe(), root)
        };
        let (conflicts, initial_files, project) = handshake(&state, snapshot, &bcast_tx).await?;
        if !conflicts.is_empty() {
            write_frame(&mut writer, &ServerMsg::ConflictDetected { conflicts }).await?;
        }
        write_frame(
            &mut writer,
            &ServerMsg::ProjectOpened {
                project,
                initial_files,
                session_id: new_session_id,
                // CARGO_PKG_VERSION is the daemon's own version baked
                // in at compile time — no runtime config needed.
                daemon_version: format!("{}+{}", env!("CARGO_PKG_VERSION"), BUILD_STAMP),
                project_root,
            },
        )
        .await?;
        rx
    };

    let idle_timeout = std::time::Duration::from_secs(WS_IDLE_TIMEOUT_SECS);
    let mut idle_check =
        tokio::time::interval(std::time::Duration::from_secs(WS_IDLE_CHECK_SECS));
    // Skip missed ticks instead of bursting (a future async stall could
    // otherwise queue up multiple ticks back-to-back and kill the loop).
    idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_activity = std::time::Instant::now();
    // Per-connection token bucket to bound how many inbound frames the
    // daemon's main loop has to parse per second. See RATE_LIMIT_*.
    let mut limiter_tokens: f64 = RATE_LIMIT_CAPACITY;
    let mut limiter_last_refill = std::time::Instant::now();
    loop {
        tokio::select! {
            msg = bcast_rx.recv() => match msg {
                Ok(m) => {
                    if let Err(e) = write_frame(&mut writer, &m).await {
                        warn!(%peer, error = ?e, "send failed; dropping client");
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // The broadcast ring overran this client. Events are gone
                    // from `bcast_rx`, but `record_delta` stored every one in
                    // `pending_deltas` under the same write lock as the send,
                    // so closing here triggers the plugin's Reconnect, which
                    // resumes with the current session_id and drains the
                    // missed frames via `Resumed`. If `pending_deltas` itself
                    // overflowed (>MAX_PENDING_DELTAS), `record_delta` will
                    // have rotated the session_id and the resume falls back to
                    // a full bootstrap — also correct.
                    warn!(
                        %peer,
                        skipped = n,
                        "broadcast lag; closing client to force resume from pending_deltas"
                    );
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            incoming = reader.next() => match incoming {
                None | Some(Ok(Message::Close(_))) => break,
                Some(Err(e)) => {
                    warn!(%peer, error = %e, "ws read error");
                    break;
                }
                Some(Ok(frame)) => {
                    last_activity = std::time::Instant::now();
                    // Refill the token bucket using elapsed wall-clock,
                    // then consume one token. A starved bucket means we
                    // received >RATE_LIMIT_PER_SEC frames in the last
                    // second — way past anything organic, almost
                    // certainly a buggy or hostile client. Drop the
                    // frame with a warn instead of letting it dominate
                    // the daemon's main loop.
                    let now = std::time::Instant::now();
                    let elapsed = now
                        .duration_since(limiter_last_refill)
                        .as_secs_f64();
                    limiter_tokens = (limiter_tokens
                        + elapsed * RATE_LIMIT_PER_SEC)
                        .min(RATE_LIMIT_CAPACITY);
                    limiter_last_refill = now;
                    if limiter_tokens < 1.0 {
                        warn!(
                            %peer,
                            cap = RATE_LIMIT_PER_SEC,
                            "client frame dropped: per-connection rate limit exceeded"
                        );
                        continue;
                    }
                    limiter_tokens -= 1.0;
                    if let Err(e) =
                        dispatch_client_frame(&state, &sessions, &frame, &bcast_tx).await
                    {
                        warn!(%peer, error = ?e, "client frame handling failed");
                        // Surface the failure to the plugin dock instead of
                        // letting it die in stderr. Without this, any `?`
                        // that propagates out of a handler (fs::rename
                        // refusing, deadlock recovery, etc.) is invisible.
                        broadcast_server_msg(
                            &state,
                            &bcast_tx,
                            ServerMsg::SyncError {
                                kind: SyncErrorKind::HandlerFailed,
                                path: String::new(),
                                reason: format!("client frame handler failed: {e:#}"),
                            },
                        )
                        .await;
                    }
                }
            },
            _ = idle_check.tick() => {
                // Connection has been silent (no inbound frame) for the
                // whole timeout window. Either the plugin VM died without
                // sending Close (Studio crashed mid-edit), the socket is a
                // zombie (TCP keepalive hasn't kicked in yet), or the
                // user simply walked away — in all three cases dropping
                // is safe: the plugin's Reconnect.luau will bring the
                // connection back when there's something to do.
                if last_activity.elapsed() > idle_timeout {
                    warn!(
                        %peer,
                        idle_secs = last_activity.elapsed().as_secs(),
                        "closing idle plugin connection (no inbound frame inside timeout)"
                    );
                    break;
                }
            }
        }
    }

    info!(%peer, "plugin disconnected");
    Ok(())
}

/// Per-extension session: registers an mpsc tx in shared state so other tasks
/// can address this extension specifically (E5 `OpenProjectRequest`, E4
/// `PickFolderRequest`), and listens for inbound frames. E3 ignores everything
/// inbound; E4 wires `PickFolderResponse` here.
async fn run_extension_session(
    state: SharedState,
    bcast_tx: broadcast::Sender<Arc<ServerMsg>>,
    peer: SocketAddr,
    mut writer: WsWriter,
    mut reader: WsReader,
) -> Result<()> {
    let (ext_tx, mut ext_rx) = mpsc::unbounded_channel::<ServerMsg>();
    {
        let mut guard = state.write().await;
        if guard.extension_tx.is_some() {
            warn!(%peer, "replacing previously-registered extension channel");
        }
        guard.extension_tx = Some(ext_tx);
    }
    info!(%peer, "extension channel registered");

    loop {
        tokio::select! {
            msg = ext_rx.recv() => match msg {
                Some(m) => {
                    if let Err(e) = write_frame(&mut writer, &m).await {
                        warn!(%peer, error = ?e, "extension send failed; closing");
                        break;
                    }
                }
                // Sender side closed (no one holds the tx anymore) — shouldn't
                // happen while we're alive, but exit cleanly if it does.
                None => break,
            },
            incoming = reader.next() => match incoming {
                None | Some(Ok(Message::Close(_))) => break,
                Some(Err(e)) => {
                    warn!(%peer, error = %e, "extension ws read error");
                    break;
                }
                Some(Ok(frame)) => {
                    if let Err(e) = dispatch_extension_frame(&state, &frame, &bcast_tx).await {
                        warn!(%peer, error = ?e, "extension frame handling failed");
                    }
                }
            },
        }
    }

    // Best-effort cleanup. If a newer extension session has overwritten the
    // slot in the meantime we leave its tx alone — the receiver-closed signal
    // on the older tx is harmless.
    {
        let mut guard = state.write().await;
        if let Some(current) = guard.extension_tx.as_ref()
            && current.is_closed()
        {
            guard.extension_tx = None;
        }
    }
    info!(%peer, "extension disconnected");
    Ok(())
}

/// Parses inbound extension frames and forwards them to whoever is waiting.
/// `PickFolderResponse` becomes a broadcast `PickFolderResult` so the plugin
/// that issued the prompt can resume on its side (filtered by `request_id`).
async fn dispatch_extension_frame(
    state: &SharedState,
    frame: &Message,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    match frame {
        Message::Text(_) => {}
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) | Message::Close(_) => {
            return Ok(());
        }
        Message::Binary(_) => {
            warn!("unexpected binary frame from extension");
            return Ok(());
        }
    }
    let msg = parse_client_msg(frame)?;
    match msg {
        ClientMsg::Hello { .. } => {
            warn!("received unexpected Hello from extension mid-session; ignoring");
        }
        ClientMsg::PickFolderResponse { request_id, path } => {
            trace!(request_id, has_path = path.is_some(), "extension folder pick received");
            broadcast_server_msg(
                state,
                bcast_tx,
                ServerMsg::PickFolderResult { request_id, path },
            )
            .await;
        }
        ClientMsg::BulkSyncFromStudioRequest {} => {
            info!("extension: sync from studio");
            if let Err(e) = start_bulk_sync_preview(state, bcast_tx, BulkDirection::FromStudio).await {
                warn!(error = ?e, "bulk sync preview (from studio) failed");
            }
        }
        ClientMsg::BulkSyncFromIdeRequest {} => {
            info!("extension: sync from ide");
            if let Err(e) = start_bulk_sync_preview(state, bcast_tx, BulkDirection::FromIde).await {
                warn!(error = ?e, "bulk sync preview (from ide) failed");
            }
        }
        other => {
            warn!("extension sent an unsupported frame: {other:?}");
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum BulkDirection {
    /// `Yeet: Sync From Studio` — conceptually "pull Studio into the IDE".
    FromStudio,
    /// `Yeet: Sync From Ide` — conceptually "push the IDE into Studio".
    FromIde,
    /// Plugin-initiated cold-boot preview. No "apply all" default — the user
    /// must pick per-file in the preview dock, and `BulkSyncConfirm` carries
    /// the resolutions.
    Bootstrap,
}

impl BulkDirection {
    fn label(self) -> &'static str {
        match self {
            BulkDirection::FromStudio => "from_studio",
            BulkDirection::FromIde => "from_ide",
            BulkDirection::Bootstrap => "bootstrap",
        }
    }

    fn from_label(label: &str) -> Option<Self> {
        match label {
            "from_studio" => Some(BulkDirection::FromStudio),
            "from_ide" => Some(BulkDirection::FromIde),
            "bootstrap" => Some(BulkDirection::Bootstrap),
            _ => None,
        }
    }

    fn to_wire(self) -> BulkSyncDirection {
        match self {
            BulkDirection::FromStudio => BulkSyncDirection::FromStudio,
            BulkDirection::FromIde => BulkSyncDirection::FromIde,
            BulkDirection::Bootstrap => BulkSyncDirection::Bootstrap,
        }
    }
}

/// Walks every path known to the daemon (union of `tree_base`, `tree_studio`,
/// `tree_fs`) and calls `reconcile_path` on each, letting the existing 3-way
/// merge decide between `FileChanged`, `ConflictDetected`, and `Noop`. The
/// `direction` is stored for telemetry; the actual merge logic is symmetric —
/// conflicts open the resolver regardless of who triggered the command.
/// Iterates a snapshot (not a live `keys()` view) so the write locks
/// reconciliation takes don't invalidate the iterator.
async fn run_bulk_reconcile(
    state: &SharedState,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
    direction: BulkDirection,
) -> Result<()> {
    let paths: Vec<String> = {
        let guard = state.read().await;
        let mut set: HashSet<&String> = HashSet::new();
        set.extend(guard.tree_base.keys());
        set.extend(guard.tree_studio.keys());
        set.extend(guard.tree_fs.keys());
        set.into_iter().cloned().collect()
    };
    let label = direction.label();
    info!(direction = label, paths = paths.len(), "bulk reconcile begin");
    for path in paths {
        if let Err(e) = reconcile_path(state, &path, bcast_tx).await {
            warn!(path = %path, error = ?e, "bulk reconcile: reconcile failed");
        }
    }
    info!(direction = label, "bulk reconcile end");
    Ok(())
}

/// Kicks off a bulk sync. Enumerates paths where `tree_studio` and `tree_fs`
/// disagree (or exist only on one side), stores the pending request so the
/// plugin's `BulkSyncConfirm` can find it, and broadcasts a
/// `BulkSyncPreview` so the Studio plugin opens its review dock.
async fn start_bulk_sync_preview(
    state: &SharedState,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
    direction: BulkDirection,
) -> Result<()> {
    let (request_id, direction_wire, entries) = {
        let mut guard = state.write().await;
        let request_id = uuid::Uuid::new_v4().to_string();
        let direction_label = direction.label();
        let direction_wire = direction.to_wire();
        let mut paths: HashSet<&String> = HashSet::new();
        paths.extend(guard.tree_studio.keys());
        paths.extend(guard.tree_fs.keys());
        let mut paths: Vec<&String> = paths.into_iter().collect();
        sort_paths_for_apply(&mut paths);
        let mut entries: Vec<BulkSyncEntry> = Vec::new();
        let mut count_modified = 0usize;
        let mut count_studio_only = 0usize;
        let mut count_ide_only = 0usize;
        let mut count_in_sync = 0usize;
        for path in &paths {
            let studio = guard.tree_studio.get(*path);
            let fs = guard.tree_fs.get(*path);
            match (studio, fs) {
                (Some(s), Some(f)) => {
                    if s.sha256 != f.sha256 {
                        count_modified += 1;
                        entries.push(BulkSyncEntry {
                            path: (*path).clone(),
                            status: BulkSyncEntryStatus::Modified,
                            studio_content: Some(s.content.clone()),
                            fs_content: Some(f.content.clone()),
                        });
                    } else {
                        count_in_sync += 1;
                    }
                }
                (Some(s), None) => {
                    count_studio_only += 1;
                    entries.push(BulkSyncEntry {
                        path: (*path).clone(),
                        status: BulkSyncEntryStatus::StudioOnly,
                        studio_content: Some(s.content.clone()),
                        fs_content: None,
                    });
                }
                (None, Some(f)) => {
                    count_ide_only += 1;
                    entries.push(BulkSyncEntry {
                        path: (*path).clone(),
                        status: BulkSyncEntryStatus::IdeOnly,
                        studio_content: None,
                        fs_content: Some(f.content.clone()),
                    });
                }
                (None, None) => {}
            }
        }
        info!(
            request_id,
            direction = direction_label,
            divergent = entries.len(),
            modified = count_modified,
            studio_only = count_studio_only,
            ide_only = count_ide_only,
            in_sync = count_in_sync,
            tree_studio_size = guard.tree_studio.len(),
            tree_fs_size = guard.tree_fs.len(),
            "bulk sync preview emitted"
        );
        guard
            .pending_bulk_sync
            .insert(request_id.clone(), direction_label.to_owned());
        (request_id, direction_wire, entries)
    };
    broadcast_server_msg(
        state,
        bcast_tx,
        ServerMsg::BulkSyncPreview {
            request_id,
            direction: direction_wire,
            entries,
        },
    )
    .await;
    Ok(())
}

async fn handle_bulk_sync_confirm(
    state: &SharedState,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
    request_id: String,
    resolutions: Vec<BulkSyncResolution>,
) -> Result<()> {
    let direction_label = {
        let mut guard = state.write().await;
        guard.pending_bulk_sync.remove(&request_id)
    };
    let Some(label) = direction_label else {
        warn!(request_id, "bulk sync confirm for unknown request_id; ignoring");
        return Ok(());
    };
    let direction = BulkDirection::from_label(&label).unwrap_or_else(|| {
        warn!(request_id, label = %label, "unknown bulk direction label; defaulting to from_ide");
        BulkDirection::FromIde
    });
    info!(
        request_id,
        direction = %label,
        resolutions = resolutions.len(),
        "bulk sync confirm"
    );
    if !resolutions.is_empty() {
        return apply_bulk_resolutions(state, bcast_tx, &request_id, resolutions).await;
    }
    if matches!(direction, BulkDirection::Bootstrap) {
        // A Bootstrap preview with an empty resolution list means the plugin
        // (or its test harness) confirmed with no picks; applying a global
        // default here would silently overwrite one side, defeating the
        // purpose of the preview. Fall through to no-op.
        warn!(
            request_id,
            "bootstrap bulk confirm with no resolutions; applying nothing"
        );
        return Ok(());
    }
    run_bulk_reconcile(state, bcast_tx, direction).await
}

/// Applies per-file resolutions collected from the preview UI. Each resolution
/// mutates exactly one side of the pair — Studio-affecting actions route
/// through the same `ServerMsg` broadcasts the plugin already consumes for
/// live reconciliation; disk-affecting actions take the same code paths as
/// `run_bulk_reconcile` does per path. Any unrecognized or inapplicable
/// combination is logged and skipped rather than aborting the batch — a bad
/// row shouldn't drag the rest of the sync down.
async fn apply_bulk_resolutions(
    state: &SharedState,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
    request_id: &str,
    mut resolutions: Vec<BulkSyncResolution>,
) -> Result<()> {
    info!(count = resolutions.len(), "apply_bulk_resolutions: start");
    // Defensive: re-sort here in case a future caller passes resolutions
    // unsorted (today they always come from `start_bulk_sync_preview`
    // which already sorts, but the contract should hold regardless).
    // Without init-first ordering the plugin's TreeBuilder builds a
    // Folder for the parent directory, then has to swap it to a
    // ModuleScript when the init.luau resolution arrives later — a
    // chain that breaks under load.
    resolutions.sort_by(|a, b| {
        let depth_a = a.path.matches('/').count();
        let depth_b = b.path.matches('/').count();
        let init_a = is_init_filename(&a.path);
        let init_b = is_init_filename(&b.path);
        depth_a
            .cmp(&depth_b)
            .then(init_b.cmp(&init_a))
            .then(a.path.cmp(&b.path))
    });
    // Collect per-resolution failures rather than aborting the batch — the
    // user may have OKed a 100-file sync where one file is locked but the
    // other 99 should still go through. But unlike the old behaviour, we
    // surface the collected failures via `BulkSyncError` instead of
    // letting the user see "all done" while half the files were skipped.
    // This matches the audit-log "we never lie about what happened"
    // contract.
    let mut failures: Vec<BulkSyncFailure> = Vec::new();
    for resolution in resolutions {
        info!(path = %resolution.path, action = ?resolution.action, "apply_bulk_resolution: begin");
        if let Err(e) =
            apply_bulk_resolution(state, bcast_tx, &resolution.path, resolution.action).await
        {
            let reason = format!("{e:#}");
            warn!(path = %resolution.path, action = ?resolution.action, error = %reason,
                "bulk resolution apply failed");
            // Audit so the post-mortem sees the per-file failure even if
            // the user dismissed the modal before reading it.
            {
                let guard = state.read().await;
                audit::record(
                    &guard.root,
                    &audit::Entry {
                        ts: audit::now_rfc3339(),
                        kind: audit::Kind::BulkFailure,
                        path: &resolution.path,
                        sha_before: None,
                        sha_after: None,
                        session_id: &guard.session_id,
                        note: Some(&reason),
                    },
                );
            }
            failures.push(BulkSyncFailure {
                path: resolution.path,
                reason,
            });
        }
    }
    // Final sweep — catches dirs that became empty mid-batch where the
    // per-file `prune_empty_dirs` call gave up at a parent that still
    // had a sibling file (later deleted by another resolution in the
    // same loop). Cheap because we only hit dirs the walker visits.
    // Aggressive mode descends into package-manager landings; the
    // junction guard handles safety.
    let (canonical_root, mapping_roots, dirty) = {
        let guard = state.read().await;
        let root = std::fs::canonicalize(&guard.root).unwrap_or_else(|_| guard.root.clone());
        (
            root,
            guard.mapping_roots_canonical.clone(),
            guard.project_dirty,
        )
    };
    if dirty {
        warn!("skipping post-bulk sweep: project file dirty (restart daemon)");
    } else {
        let removed =
            prune_all_empty_subdirs_with(&canonical_root, &mapping_roots, /*aggressive=*/ true);
        if removed > 0 {
            info!(removed, "apply_bulk_resolutions: pruned empty dirs");
        }
    }
    if !failures.is_empty() {
        warn!(
            request_id,
            failure_count = failures.len(),
            "bulk sync apply finished with per-file failures; emitting BulkSyncError"
        );
        broadcast_server_msg(
            state,
            bcast_tx,
            ServerMsg::BulkSyncError {
                request_id: request_id.to_owned(),
                failed: failures,
            },
        )
        .await;
    }
    Ok(())
}

async fn apply_bulk_resolution(
    state: &SharedState,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
    path: &str,
    action: BulkSyncAction,
) -> Result<()> {
    // Read the current entries once so the rest of the handler can decide
    // which side has the content without re-acquiring the lock. The clones
    // are cheap (TreeEntry is small) and keep the subsequent `apply_outcome`
    // call working on a stable view even if the watcher fires mid-resolution.
    let (studio, fs) = {
        let guard = state.read().await;
        (guard.tree_studio.get(path).cloned(), guard.tree_fs.get(path).cloned())
    };
    match action {
        BulkSyncAction::Skip => Ok(()),
        // Studio is authoritative: write tree_studio's content to disk. Build
        // the outcome explicitly rather than calling `reconcile_path` — the
        // 3-way merge could pick FS if tree_base matches Studio, flipping the
        // user's intent. Same for the other `Apply { side: Fs }` branches.
        BulkSyncAction::KeepStudio | BulkSyncAction::PushToIde => {
            let Some(s) = studio else {
                warn!(path = %path, "bulk resolution: Studio side missing; skipping");
                return Ok(());
            };
            let outcome = MergeOutcome::Apply {
                side: Side::Fs,
                content: Some(s.content.clone()),
                kind: s.kind,
            };
            apply_outcome(state, path, outcome, bcast_tx).await
        }
        // Disk wins: push tree_fs's content back into Studio.
        BulkSyncAction::KeepIde | BulkSyncAction::PullToStudio => {
            let Some(f) = fs else {
                warn!(path = %path, "bulk resolution: IDE side missing; skipping");
                return Ok(());
            };
            let outcome = MergeOutcome::Apply {
                side: Side::Studio,
                content: Some(f.content.clone()),
                kind: f.kind,
            };
            apply_outcome(state, path, outcome, bcast_tx).await
        }
        BulkSyncAction::DeleteFromStudio => {
            let Some(s) = studio else {
                warn!(path = %path, "bulk resolution: delete-from-studio on missing Studio entry; skipping");
                return Ok(());
            };
            let outcome = MergeOutcome::Apply {
                side: Side::Studio,
                content: None,
                kind: s.kind,
            };
            apply_outcome(state, path, outcome, bcast_tx).await
        }
        BulkSyncAction::DeleteFromIde => {
            let Some(f) = fs else {
                warn!(path = %path, "bulk resolution: delete-from-ide on missing IDE entry; skipping");
                return Ok(());
            };
            let outcome = MergeOutcome::Apply {
                side: Side::Fs,
                content: None,
                kind: f.kind,
            };
            apply_outcome(state, path, outcome, bcast_tx).await
        }
    }
}

async fn handle_bulk_sync_cancel(state: &SharedState, request_id: String) -> Result<()> {
    let mut guard = state.write().await;
    if guard.pending_bulk_sync.remove(&request_id).is_some() {
        info!(request_id, "bulk sync cancel");
    } else {
        warn!(request_id, "bulk sync cancel for unknown request_id");
    }
    Ok(())
}

async fn dispatch_client_frame(
    state: &SharedState,
    sessions: &Arc<Mutex<SyncbackSessions>>,
    frame: &Message,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    match frame {
        Message::Text(_) => {}
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) | Message::Close(_) => {
            return Ok(());
        }
        Message::Binary(_) => {
            warn!("unexpected binary frame from client");
            return Ok(());
        }
    }
    let msg = match parse_client_msg(frame) {
        Ok(m) => m,
        Err(e) => {
            // Surface parse failures back to the plugin so the dock log
            // shows them — otherwise an outdated daemon silently drops
            // every frame it doesn't recognize and the user sees no
            // feedback (exactly the rename bug we're chasing).
            let preview = match frame {
                Message::Text(t) => t.chars().take(160).collect::<String>(),
                _ => String::new(),
            };
            warn!(error = ?e, preview = %preview, "parse_client_msg failed");
            broadcast_server_msg(
                state,
                bcast_tx,
                ServerMsg::SyncError {
                    kind: SyncErrorKind::UnsafePath,
                    path: String::new(),
                    reason: format!("daemon rejected client frame: {e:#}"),
                },
            )
            .await;
            return Ok(());
        }
    };
    let msg_kind_label = match &msg {
        ClientMsg::FileRenamed { .. } => "file_renamed",
        ClientMsg::FileChanged { .. } => "file_changed",
        ClientMsg::FileCreated { .. } => "file_created",
        ClientMsg::FileDeleted { .. } => "file_deleted",
        ClientMsg::NameCollision { .. } => "name_collision",
        ClientMsg::Hello { .. } => "hello",
        _ => "other",
    };
    info!(kind = msg_kind_label, "dispatch_client_frame");
    match msg {
        ClientMsg::Hello { .. } => {
            warn!("received unexpected Hello mid-session; ignoring");
            Ok(())
        }
        ClientMsg::PairRequest {} => {
            // PairRequest is only meaningful during the handshake-time
            // auth dance. A frame arriving mid-session is either a
            // protocol bug or a malicious replay; either way ignore.
            warn!("received unexpected PairRequest mid-session; ignoring");
            Ok(())
        }
        ClientMsg::FileChanged {
            path,
            content,
            sha256,
            bootstrap_diverge,
        } => {
            handle_studio_changed(state, path, content, sha256, None, bootstrap_diverge, bcast_tx)
                .await
        }
        ClientMsg::FileCreated {
            path,
            kind,
            content,
            sha256,
        } => {
            handle_studio_changed(state, path, content, sha256, Some(kind), false, bcast_tx).await
        }
        ClientMsg::FileDeleted { path } => handle_studio_deleted(state, path, bcast_tx).await,
        ClientMsg::FileRenamed {
            old_path,
            new_path,
            kind,
            content,
            sha256,
        } => {
            handle_studio_renamed(state, old_path, new_path, kind, content, sha256, bcast_tx).await
        }
        ClientMsg::NameCollision { path, sha256 } => {
            handle_name_collision(state, path, sha256, bcast_tx).await
        }
        ClientMsg::ConflictResolved { resolutions } => {
            handle_conflict_resolved(state, resolutions, bcast_tx).await
        }
        ClientMsg::ConflictResolvedManual {
            path,
            content,
            sha256,
        } => handle_conflict_resolved_manual(state, path, content, sha256, bcast_tx).await,
        ClientMsg::ConflictAbandoned { paths } => {
            // Plugin closed the conflict dock with `paths` still in the
            // queue. The pending_conflicts entries for those paths must
            // stay so the next reconcile re-emits them — the alternative
            // (dropping them or silently auto-resolving) leaves the
            // Studio side and disk side disagreeing forever. Today
            // pending_conflicts is already keyed by path and we only
            // remove an entry when the user resolves it (handle_conflict
            // _resolved), so this handler only needs to log: the entries
            // are already preserved. Logging makes the abandonment
            // visible in audit/debug output and keeps the contract
            // explicit if a future refactor removes the pending entry on
            // any other path.
            let guard = state.read().await;
            let still_pending: Vec<&String> = paths
                .iter()
                .filter(|p| guard.pending_conflicts.contains_key(p.as_str()))
                .collect();
            info!(
                requested = paths.len(),
                still_pending = still_pending.len(),
                "conflict abandonment received"
            );
            for path in &paths {
                if !guard.pending_conflicts.contains_key(path.as_str()) {
                    warn!(
                        %path,
                        "abandonment names a path with no pending conflict — desync candidate"
                    );
                }
                audit::record(
                    &guard.root,
                    &audit::Entry {
                        ts: audit::now_rfc3339(),
                        kind: audit::Kind::ConflictAbandoned,
                        path,
                        sha_before: None,
                        sha_after: None,
                        session_id: &guard.session_id,
                        note: None,
                    },
                );
            }
            Ok(())
        }
        ClientMsg::SyncbackBegin {
            request_id,
            target_path,
            mode,
            include_non_script,
            include_binary,
            template,
            project_name,
        } => {
            handle_syncback_begin(
                state,
                sessions,
                request_id,
                target_path,
                mode,
                include_non_script,
                include_binary,
                template,
                project_name,
                bcast_tx,
            )
            .await
        }
        ClientMsg::SyncbackChunk {
            request_id,
            seq,
            instances,
        } => handle_syncback_chunk(state, sessions, request_id, seq, instances, bcast_tx).await,
        ClientMsg::SyncbackFinalize {
            request_id,
            total_seq,
        } => handle_syncback_finalize(state, sessions, request_id, total_seq, bcast_tx).await,
        ClientMsg::OpenProjectRequest { path } => {
            forward_open_project(state, path).await;
            Ok(())
        }
        ClientMsg::PickFolderResponse { request_id, .. } => {
            // Only the extension is supposed to send this; ignore on the
            // plugin channel rather than treating it as a protocol error so
            // a buggy plugin can't crash the daemon.
            warn!(
                request_id,
                "PickFolderResponse received on plugin channel; ignoring"
            );
            Ok(())
        }
        ClientMsg::PickFolderPrompt { request_id, prompt } => {
            handle_pick_folder_prompt(state, bcast_tx, request_id, prompt).await
        }
        ClientMsg::BulkSyncFromStudioRequest {} | ClientMsg::BulkSyncFromIdeRequest {} => {
            warn!("bulk sync request received on plugin channel; ignoring (only extension may send it)");
            Ok(())
        }
        ClientMsg::BulkSyncConfirm {
            request_id,
            resolutions,
        } => handle_bulk_sync_confirm(state, bcast_tx, request_id, resolutions).await,
        ClientMsg::BulkSyncCancel { request_id } => {
            handle_bulk_sync_cancel(state, request_id).await
        }
        ClientMsg::StudioSnapshotReport { snapshot } => {
            handle_studio_snapshot_report(state, bcast_tx, snapshot).await
        }
        ClientMsg::SessionEnd {} => {
            // Plugin announced clean shutdown (Studio reload, dock close,
            // place close). Rotate the session id immediately so the next
            // reconnect is forced through a full handshake instead of
            // attempting a resume that would replay deltas at instances
            // the new plugin instance does not know about.
            let mut guard = state.write().await;
            guard.rotate_session_id("plugin SessionEnd received");
            Ok(())
        }
        ClientMsg::Ping { seq } => {
            // Heartbeat probe. Reply via the broadcast channel so the
            // existing per-client write loop carries it back — keeps the
            // pong on the same back-pressure path as every other frame.
            // Pongs aren't recorded into pending_deltas (they're useless
            // on resume) but using broadcast is fine because the channel
            // can absorb the trickle.
            broadcast_server_msg(state, bcast_tx, ServerMsg::Pong { seq }).await;
            Ok(())
        }
    }
}

/// Ingests the plugin's post-handshake Studio snapshot into `tree_studio`
/// and emits a `BulkSyncPreview` with direction = `Bootstrap`. The snapshot
/// carries every LuaSourceContainer the plugin saw under a `$path`-mapped
/// root, so the diff against `tree_fs` covers Modified / StudioOnly /
/// IdeOnly exhaustively. Paths not present in the snapshot are treated as
/// absent from Studio — they surface as IdeOnly if `tree_fs` has them.
async fn handle_studio_snapshot_report(
    state: &SharedState,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
    snapshot: Vec<StudioFileSnapshot>,
) -> Result<()> {
    let received = snapshot.len();
    let mut accepted = 0usize;
    // Collect rather than fire-per-entry so a snapshot with N bad rows
    // doesn't spam the plugin with N modals — the plugin gets one summary
    // SyncError listing the count, plus the per-path tracing logs for
    // operators tailing the daemon.
    let mut dropped_paths: Vec<String> = Vec::new();
    let fs_size = state.read().await.tree_fs.len();
    {
        let mut guard = state.write().await;
        guard.tree_studio.clear();
        // Validate every entry's path + size before inserting so a single
        // bad entry can't poison tree_studio and trip the diff that follows.
        for snap in snapshot {
            if snap.content.len() > MAX_CONTENT_BYTES {
                warn!(
                    path = %snap.path,
                    bytes = snap.content.len(),
                    cap = MAX_CONTENT_BYTES,
                    "dropping snapshot entry: exceeds MAX_CONTENT_BYTES"
                );
                dropped_paths.push(snap.path);
                continue;
            }
            if let Err(e) = resolve_inside(&guard.root, &snap.path) {
                warn!(path = %snap.path, error = ?e, "dropping snapshot entry: unsafe path");
                dropped_paths.push(snap.path);
                continue;
            }
            // Defence in depth: also reject paths inside project root
            // but outside every declared `$path` mapping. A
            // misbehaving plugin (or a forged snapshot from an
            // unauthorised client) shouldn't be able to seed
            // `tree_studio` with paths the FS watcher would refuse to
            // mirror — that would leak into BulkSyncPreview and
            // potentially trick the user into approving a write to a
            // protected file.
            if !guard.is_under_mapping(&snap.path) {
                warn!(
                    path = %snap.path,
                    "dropping snapshot entry: path not under any mapping"
                );
                dropped_paths.push(snap.path);
                continue;
            }
            guard.tree_studio.insert(
                snap.path,
                TreeEntry {
                    kind: snap.kind,
                    content: snap.content,
                    sha256: snap.sha256,
                },
            );
            accepted += 1;
        }
    }
    if !dropped_paths.is_empty() {
        // The user just clicked "Connect" expecting the daemon to see all
        // their Studio scripts. Surface a single summary frame so they know
        // the count is short — without this, they'd see the bulk-sync
        // preview missing N rows with no explanation.
        let preview: String = dropped_paths.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
        let extras = if dropped_paths.len() > 5 {
            format!(" (+{} more)", dropped_paths.len() - 5)
        } else {
            String::new()
        };
        broadcast_server_msg(
            state,
            bcast_tx,
            ServerMsg::SyncError {
                kind: SyncErrorKind::SnapshotEntryDropped,
                path: String::new(),
                reason: format!(
                    "{} snapshot entr{} dropped: {preview}{extras}",
                    dropped_paths.len(),
                    if dropped_paths.len() == 1 { "y" } else { "ies" }
                ),
            },
        )
        .await;
    }
    info!(
        received,
        accepted,
        tree_fs_size = fs_size,
        "studio snapshot ingested"
    );
    start_bulk_sync_preview(state, bcast_tx, BulkDirection::Bootstrap).await
}

/// Forwards a plugin-issued folder-picker request to the registered extension.
/// When no extension is live, replies immediately with `path = None` so the
/// plugin's `ask()` doesn't hang waiting for a ghost dialog.
async fn handle_pick_folder_prompt(
    state: &SharedState,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
    request_id: String,
    prompt: String,
) -> Result<()> {
    let tx_snapshot = {
        let guard = state.read().await;
        guard.extension_tx.clone()
    };
    let Some(tx) = tx_snapshot else {
        warn!(
            request_id,
            "no extension registered; short-circuiting folder-picker with None"
        );
        broadcast_server_msg(
            state,
            bcast_tx,
            ServerMsg::PickFolderResult {
                request_id,
                path: None,
            },
        )
        .await;
        return Ok(());
    };
    let req = ServerMsg::PickFolderRequest {
        request_id: request_id.clone(),
        prompt,
    };
    if let Err(e) = tx.send(req) {
        warn!(
            request_id,
            error = %e,
            "extension channel closed mid-flight; replying with None"
        );
        let mut guard = state.write().await;
        if let Some(current) = guard.extension_tx.as_ref()
            && current.is_closed()
        {
            guard.extension_tx = None;
        }
        drop(guard);
        broadcast_server_msg(
            state,
            bcast_tx,
            ServerMsg::PickFolderResult {
                request_id,
                path: None,
            },
        )
        .await;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// Resolves the current user's home directory (`%USERPROFILE%` on Windows,
/// `$HOME` elsewhere). Returns `None` when the variable is unset or empty, in
/// which case a syncback is refused rather than allowed to write anywhere.
fn user_home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Confines a syncback `target_path` to the user's home directory
/// (AUDITORIA-YEET.md A17 — arbitrary file write). The legitimate
/// reverse-bootstrap targets a user-chosen NEW folder, so we cannot require
/// it inside the served project root — but we can require it under `home`,
/// reject `..` traversal, and canonicalize the nearest EXISTING ancestor so a
/// symlinked ancestor cannot escape home. Returns a human-readable reason on
/// rejection.
fn validate_syncback_target(target: &Path, home: &Path) -> Result<(), String> {
    if !target.is_absolute() {
        return Err(format!("target_path must be absolute: {}", target.display()));
    }
    if target
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!(
            "target_path must not contain '..' components: {}",
            target.display()
        ));
    }
    let canonical_home = home
        .canonicalize()
        .map_err(|e| format!("cannot resolve home directory {}: {e}", home.display()))?;
    // Walk up to the nearest existing ancestor (the target leaf is typically
    // new). Canonicalizing it resolves any symlink in the existing portion, so
    // a `home/link -> C:\elsewhere` ancestor is caught here rather than
    // silently followed at write time.
    let mut cursor: &Path = target;
    let existing = loop {
        if cursor.exists() {
            break cursor;
        }
        match cursor.parent() {
            Some(parent) => cursor = parent,
            None => {
                return Err(format!(
                    "target_path has no existing ancestor: {}",
                    target.display()
                ));
            }
        }
    };
    let canonical_existing = existing
        .canonicalize()
        .map_err(|e| format!("cannot resolve {}: {e}", existing.display()))?;
    if !canonical_existing.starts_with(&canonical_home) {
        return Err(format!(
            "target_path {} resolves outside the home directory ({}); refusing arbitrary file write",
            target.display(),
            home.display()
        ));
    }
    Ok(())
}

/// Refuses to overwrite a non-empty target directory unless the message
/// carries explicit overwrite intent (`MergeExisting { overwrite: true }`). A
/// missing / empty / new directory always passes.
fn syncback_overwrite_ok(target: &Path, mode: SyncbackMode) -> Result<(), String> {
    let non_empty = std::fs::read_dir(target)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);
    if non_empty && !matches!(mode, SyncbackMode::MergeExisting { overwrite: true }) {
        return Err(format!(
            "target_path {} is a non-empty directory; refusing to overwrite without \
             explicit merge-overwrite intent",
            target.display()
        ));
    }
    Ok(())
}

async fn handle_syncback_begin(
    state: &SharedState,
    sessions: &Arc<Mutex<SyncbackSessions>>,
    request_id: String,
    target_path: String,
    mode: SyncbackMode,
    include_non_script: bool,
    include_binary: bool,
    template: SyncbackTemplate,
    project_name: Option<String>,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    let target = PathBuf::from(&target_path);
    // A17: the target comes straight off the wire. Confine it to the user's
    // home dir, reject `..` traversal, and refuse to clobber a non-empty
    // directory without explicit overwrite intent, before any write happens.
    let Some(home) = user_home_dir() else {
        send_syncback_error(
            state,
            bcast_tx,
            request_id,
            "daemon cannot determine the user home directory; refusing syncback".to_owned(),
        )
        .await;
        return Ok(());
    };
    if let Err(reason) = validate_syncback_target(&target, &home) {
        send_syncback_error(state, bcast_tx, request_id.clone(), reason).await;
        return Ok(());
    }
    if let Err(reason) = syncback_overwrite_ok(&target, mode) {
        send_syncback_error(state, bcast_tx, request_id.clone(), reason).await;
        return Ok(());
    }
    // Durable record of every accepted destination: reverse-bootstrap is the
    // one path that writes outside the served project on the client's say-so.
    {
        let guard = state.read().await;
        let session_id = guard.session_id.clone();
        let target_str = target.display().to_string();
        audit::record(
            &guard.root,
            &audit::Entry {
                ts: audit::now_rfc3339(),
                kind: audit::Kind::FsWrite,
                path: &target_str,
                sha_before: None,
                sha_after: None,
                session_id: &session_id,
                note: Some("syncback target accepted"),
            },
        );
    }
    let opts = SyncbackOptions {
        target_path: target,
        mode,
        include_non_script,
        include_binary,
        template,
        project_name,
    };
    let session = SyncbackSession::new(request_id.clone(), opts);
    let mut guard = sessions.lock().await;
    if let Err(e) = guard.insert(session) {
        drop(guard);
        send_syncback_error(state, bcast_tx, request_id, e.to_string()).await;
    } else {
        info!(request_id, target = %target_path, "syncback begin");
    }
    Ok(())
}

async fn handle_syncback_chunk(
    state: &SharedState,
    sessions: &Arc<Mutex<SyncbackSessions>>,
    request_id: String,
    seq: u32,
    instances: Vec<SerializedInstance>,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    let mut guard = sessions.lock().await;
    let Some(session) = guard.get_mut(&request_id) else {
        drop(guard);
        send_syncback_error(
            state,
            bcast_tx,
            request_id,
            "no syncback session for this request_id (did you miss SyncbackBegin?)".to_owned(),
        )
        .await;
        return Ok(());
    };
    if let Err(e) = session.ingest_chunk(seq, instances) {
        let msg = e.to_string();
        // Abort the session on ingest failure; keep it out of the map so the
        // plugin must restart cleanly.
        guard.remove(&request_id);
        drop(guard);
        send_syncback_error(state, bcast_tx, request_id, msg).await;
        return Ok(());
    }
    drop(guard);
    broadcast_server_msg(
        state,
        bcast_tx,
        ServerMsg::SyncbackAck {
            request_id,
            seq,
        },
    )
    .await;
    Ok(())
}

// `state` + `stats` trip `clippy::similar_names`; both are load-bearing here
// (state is the shared project, stats comes out of the materializer) so
// neither renames cleanly.
#[allow(clippy::similar_names)]
async fn handle_syncback_finalize(
    state: &SharedState,
    sessions: &Arc<Mutex<SyncbackSessions>>,
    request_id: String,
    total_seq: u32,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
) -> Result<()> {
    let mut guard = sessions.lock().await;
    let Some(session) = guard.remove(&request_id) else {
        drop(guard);
        send_syncback_error(
            state,
            bcast_tx,
            request_id,
            "no syncback session for this request_id at finalize".to_owned(),
        )
        .await;
        return Ok(());
    };
    drop(guard);

    let project_path = session.opts.target_path.clone();
    // Materialization is synchronous and can do blocking fs work; offload.
    let result =
        tokio::task::spawn_blocking(move || syncback::materialize(session, total_seq)).await;
    match result {
        Ok(Ok(stats)) => {
            info!(
                request_id,
                scripts = stats.scripts_written,
                metas = stats.meta_files_written,
                "syncback complete"
            );
            // Sweep the materialized tree before announcing completion. A
            // syncback writes only the instances it visits, so any
            // pre-existing empty directory inside `target_path` (left over
            // from an earlier syncback / manual edit / crashed run) would
            // otherwise survive the operation and clutter the IDE tree.
            // `mapping_roots_canonical` covers the live project; pass the
            // syncback target instead because we may be writing into a
            // subtree the daemon's own state doesn't yet reflect.
            // Aggressive mode is mandatory here — a typical Wally/pesde
            // project has dozens of empty leaves under `Packages/_Index/`
            // that the conservative sweep would refuse to touch by name.
            let canonical_target =
                std::fs::canonicalize(&project_path).unwrap_or_else(|_| project_path.clone());
            let removed = prune_all_empty_subdirs_with(
                &canonical_target,
                &[canonical_target.clone()],
                /*aggressive=*/ true,
            );
            if removed > 0 {
                info!(request_id, removed, "syncback: pruned empty dirs");
            }
            let project_path_str = project_path.to_string_lossy().into_owned();
            broadcast_server_msg(
                state,
                bcast_tx,
                ServerMsg::SyncbackComplete {
                    request_id,
                    project_path: project_path_str.clone(),
                    stats,
                },
            )
            .await;
            // The extension opens the materialized folder automatically so
            // the user lands in it without an extra click. Silent no-op if
            // no extension is registered.
            forward_open_project(state, project_path_str).await;
        }
        Ok(Err(e)) => {
            send_syncback_error(state, bcast_tx, request_id, format!("{e:#}")).await;
        }
        Err(join_err) => {
            send_syncback_error(state, bcast_tx, request_id, format!("join error: {join_err}"))
                .await;
        }
    }
    Ok(())
}

/// Retransmits an open-folder request to the extension, if one is connected.
/// Absence of an extension is an expected state (headless daemon runs, etc.),
/// not a failure — we just log at info level and carry on.
async fn forward_open_project(state: &SharedState, path: String) {
    let tx_snapshot = {
        let guard = state.read().await;
        guard.extension_tx.clone()
    };
    let Some(tx) = tx_snapshot else {
        info!(
            path = %path,
            "open-project skipped: no extension registered"
        );
        return;
    };
    if let Err(e) = tx.send(ServerMsg::OpenProjectRequest { path }) {
        warn!(
            error = %e,
            "extension channel closed; dropping open-project request"
        );
        let mut guard = state.write().await;
        if let Some(current) = guard.extension_tx.as_ref()
            && current.is_closed()
        {
            guard.extension_tx = None;
        }
    }
}

async fn send_syncback_error(
    state: &SharedState,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
    request_id: String,
    message: String,
) {
    warn!(request_id, %message, "syncback error");
    broadcast_server_msg(
        state,
        bcast_tx,
        ServerMsg::SyncbackError {
            request_id,
            message,
        },
    )
    .await;
}

/// Records `msg` into the resume buffer **and** sends it on `bcast_tx`. Both
/// happen under the same write lock so a concurrent reconnect's drain-then-
/// subscribe sequence (in `run_plugin_session`) can never see the same event
/// once via the buffer and once via its broadcast subscription.
async fn broadcast_server_msg(
    state: &SharedState,
    bcast_tx: &broadcast::Sender<Arc<ServerMsg>>,
    msg: ServerMsg,
) {
    let arc = Arc::new(msg);
    let mut guard = state.write().await;
    guard.record_delta((*arc).clone());
    // If no receivers exist (no clients connected), broadcast returns an
    // error — ignore it, the buffered copy is what counts for resume.
    let _ = bcast_tx.send(arc);
}

fn parse_client_msg(frame: &Message) -> Result<ClientMsg> {
    match frame {
        Message::Text(text) => serde_json::from_str::<ClientMsg>(text.as_str())
            .with_context(|| format!("parse client msg: {text}")),
        other => bail!("expected text frame, got {other:?}"),
    }
}

async fn write_frame<S>(writer: &mut S, msg: &ServerMsg) -> Result<()>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let body = serde_json::to_string(msg).context("serialize server msg")?;
    let bytes = body.len();
    let kind = server_msg_kind(msg);
    // Steady-state file edits stay at trace; only handshake-class payloads
    // (ProjectOpened, BulkSyncPreview, StudioSnapshotReport replies) cross
    // the threshold and surface in the journal so we can correlate them
    // against the active WS_MAX_FRAME_BYTES cap when triaging dropped frames.
    if bytes >= WS_LARGE_FRAME_LOG_BYTES {
        warn!(kind, bytes, cap = WS_MAX_MESSAGE_BYTES, "large outbound ws frame");
    } else {
        trace!(kind, bytes, "outbound ws frame");
    }
    writer
        .send(Message::Text(body))
        .await
        .with_context(|| format!("ws write (kind={kind}, bytes={bytes})"))?;
    Ok(())
}

/// Static discriminator string for a `ServerMsg` variant. Mirrors the wire
/// `type` tag emitted by serde so log lines join cleanly to client-side
/// telemetry without paying for a serde round-trip per frame.
fn server_msg_kind(msg: &ServerMsg) -> &'static str {
    match msg {
        ServerMsg::ProjectOpened { .. } => "project_opened",
        ServerMsg::Resumed { .. } => "resumed",
        ServerMsg::FileCreated { .. } => "file_created",
        ServerMsg::FileChanged { .. } => "file_changed",
        ServerMsg::FileDeleted { .. } => "file_deleted",
        ServerMsg::FileRenamed { .. } => "file_renamed",
        ServerMsg::NameCollision { .. } => "name_collision",
        ServerMsg::AttributesChanged { .. } => "attributes_changed",
        ServerMsg::ConflictDetected { .. } => "conflict_detected",
        ServerMsg::SyncbackAck { .. } => "syncback_ack",
        ServerMsg::SyncbackComplete { .. } => "syncback_complete",
        ServerMsg::SyncbackError { .. } => "syncback_error",
        ServerMsg::OpenProjectRequest { .. } => "open_project_request",
        ServerMsg::PickFolderRequest { .. } => "pick_folder_request",
        ServerMsg::PickFolderResult { .. } => "pick_folder_result",
        ServerMsg::BulkSyncPreview { .. } => "bulk_sync_preview",
        ServerMsg::BulkSyncError { .. } => "bulk_sync_error",
        ServerMsg::SyncError { .. } => "sync_error",
        ServerMsg::Pong { .. } => "pong",
        ServerMsg::AuthChallenge { .. } => "auth_challenge",
        ServerMsg::AuthGranted { .. } => "auth_granted",
        ServerMsg::AuthRejected { .. } => "auth_rejected",
    }
}

#[cfg(test)]
mod sort_tests {
    use super::sort_paths_for_apply;

    fn sorted(input: &[&str]) -> Vec<String> {
        let mut owned: Vec<String> = input.iter().map(|s| (*s).to_owned()).collect();
        sort_paths_for_apply(&mut owned);
        owned
    }

    #[test]
    fn shallower_paths_come_before_deeper() {
        let out = sorted(&[
            "src/Foo/Bar.luau",
            "src/Top.luau",
            "src/Foo/Sub/Inner.luau",
        ]);
        // Top.luau (depth 1) before Bar.luau (depth 2) before Inner.luau (depth 3).
        assert_eq!(
            out,
            vec![
                "src/Top.luau",
                "src/Foo/Bar.luau",
                "src/Foo/Sub/Inner.luau",
            ]
        );
    }

    #[test]
    fn init_files_before_non_init_at_same_depth() {
        let out = sorted(&[
            "src/Foo/Bar.luau",
            "src/Foo/init.luau",
            "src/Foo/Aaa.luau",
        ]);
        // init.luau must come before its siblings — that's the whole bug fix.
        assert_eq!(out[0], "src/Foo/init.luau");
        // Tail order is alphabetical among non-init siblings at same depth.
        assert_eq!(out[1], "src/Foo/Aaa.luau");
        assert_eq!(out[2], "src/Foo/Bar.luau");
    }

    #[test]
    fn nested_init_chain_orders_top_down() {
        let out = sorted(&[
            "src/Foo/Sub/Inner.luau",
            "src/Foo/init.luau",
            "src/Foo/Sub/init.luau",
            "src/Foo/Bar.luau",
        ]);
        // Depth 2 first (init then sibling), then depth 3 (init then sibling).
        // Without this ordering, the plugin would create Folder Foo → Folder
        // Sub → ModuleScript Inner, then have to swap Sub and Foo on later
        // events.
        assert_eq!(
            out,
            vec![
                "src/Foo/init.luau",
                "src/Foo/Bar.luau",
                "src/Foo/Sub/init.luau",
                "src/Foo/Sub/Inner.luau",
            ]
        );
    }

    #[test]
    fn init_variants_all_sort_first() {
        // Each Rojo init variant gets the same priority bump. Mixing them
        // shouldn't matter — only "is init" vs "is not".
        let out = sorted(&[
            "src/Foo/Z.luau",
            "src/Foo/init.server.luau",
            "src/Foo/A.luau",
            "src/Bar/init.client.lua",
            "src/Bar/Z.luau",
        ]);
        // Both init files come first (depth 2). Then alphabetical.
        assert!(out[0].ends_with("/init.client.lua") || out[0].ends_with("/init.server.luau"));
        assert!(out[1].ends_with("/init.client.lua") || out[1].ends_with("/init.server.luau"));
        // Then non-init at depth 2 alphabetically.
        assert_eq!(out[2], "src/Bar/Z.luau");
        assert_eq!(out[3], "src/Foo/A.luau");
        assert_eq!(out[4], "src/Foo/Z.luau");
    }

    #[test]
    fn alphabetical_tiebreaker_keeps_output_stable() {
        // Two paths identical in (depth, init) — alphabetical decides.
        // Stability matters for tests and for log readability.
        let out = sorted(&["src/B.luau", "src/A.luau", "src/C.luau"]);
        assert_eq!(out, vec!["src/A.luau", "src/B.luau", "src/C.luau"]);
    }

    #[test]
    fn empty_paths_does_not_panic() {
        // Edge case: a HashSet that ended up empty (no divergences) must
        // sort to an empty Vec without exploding. This was the path the
        // previous `paths.sort()` took too, but a future change to a
        // unstable algorithm could regress it.
        let out = sorted(&[]);
        assert_eq!(out, Vec::<String>::new());
    }

    #[test]
    fn single_element_is_noop() {
        // One-element input — every comparator is irrelevant and the
        // output must equal the input. Edge case for the worst-shape
        // bulk preview ("just this one file diverged").
        let out = sorted(&["src/Foo/Bar.luau"]);
        assert_eq!(out, vec!["src/Foo/Bar.luau"]);
    }

    #[test]
    fn all_init_at_same_depth_falls_back_to_alphabetical() {
        // Every path is an init.luau at depth 2, so the (init vs non-init)
        // tiebreaker is a wash. Pure alphabetical decides. Pins that the
        // sort key chain never oscillates when one of its components is
        // always equal.
        let out = sorted(&[
            "src/Charlie/init.luau",
            "src/Alpha/init.luau",
            "src/Bravo/init.luau",
        ]);
        assert_eq!(
            out,
            vec![
                "src/Alpha/init.luau",
                "src/Bravo/init.luau",
                "src/Charlie/init.luau",
            ]
        );
    }

    #[test]
    fn mixed_init_variants_sort_first_together() {
        // `init.luau` / `init.client.lua` / `init.server.luau` are all
        // "init" — the sort key bucket should be identical, so they
        // jointly precede non-init at the same depth and order
        // alphabetically among themselves.
        let out = sorted(&[
            "src/Foo/Sibling.luau",
            "src/Bar/init.client.lua",
            "src/Baz/init.server.luau",
            "src/Foo/init.luau",
        ]);
        assert_eq!(
            out,
            vec![
                "src/Bar/init.client.lua",
                "src/Baz/init.server.luau",
                "src/Foo/init.luau",
                "src/Foo/Sibling.luau",
            ]
        );
    }
}

#[cfg(test)]
mod prune_tests {
    use super::{
        dir_name_is_skipped, is_safe_to_prune, prune_all_empty_subdirs,
        prune_all_empty_subdirs_with, prune_empty_dirs, MAX_CONTENT_BYTES,
    };
    use std::fs;
    use std::path::PathBuf;

    /// Builds an isolated temp dir, canonicalizes it, and returns both the
    /// `TempDir` guard (kept alive for cleanup) and the canonical path. We
    /// canonicalize because `prune_empty_dirs` itself canonicalizes
    /// candidates and uses `starts_with` against the root — feeding it the
    /// non-canonical temp path on Windows would mismatch UNC prefixes.
    fn make_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = fs::canonicalize(dir.path()).expect("canonicalize");
        (dir, canonical)
    }

    fn touch(path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir -p");
        }
        fs::write(path, b"x").expect("write");
    }

    // ─── is_safe_to_prune ──────────────────────────────────────────────

    #[test]
    fn safe_to_prune_rejects_root_itself() {
        let (_d, root) = make_root();
        assert!(!is_safe_to_prune(&root, &[], &root));
    }

    #[test]
    fn safe_to_prune_rejects_path_outside_root() {
        let (_d, root) = make_root();
        let other = tempfile::tempdir().expect("other tempdir");
        let other_canon = fs::canonicalize(other.path()).expect("canonicalize");
        assert!(!is_safe_to_prune(&root, &[], &other_canon));
    }

    #[test]
    fn safe_to_prune_rejects_mapping_root() {
        let (_d, root) = make_root();
        let mount = root.join("src");
        fs::create_dir(&mount).expect("mkdir mount");
        let mount_canon = fs::canonicalize(&mount).expect("canonicalize mount");
        assert!(!is_safe_to_prune(&root, &[mount_canon.clone()], &mount_canon));
    }

    #[test]
    fn safe_to_prune_accepts_normal_subdir() {
        let (_d, root) = make_root();
        let dir = root.join("src/sub");
        fs::create_dir_all(&dir).expect("mkdir");
        let canon = fs::canonicalize(&dir).expect("canonicalize");
        assert!(is_safe_to_prune(&root, &[], &canon));
    }

    #[cfg(unix)]
    #[test]
    fn safe_to_prune_rejects_symlinked_dir() {
        use std::os::unix::fs::symlink;
        let (_d, root) = make_root();
        let target = tempfile::tempdir().expect("target tempdir");
        let link = root.join("link");
        symlink(target.path(), &link).expect("symlink");
        // canonicalize follows the link, so containment fails first; even if
        // it didn't, the symlink_metadata check catches it.
        assert!(!is_safe_to_prune(&root, &[], &link));
    }

    // ─── prune_empty_dirs (per-file walk) ──────────────────────────────

    #[test]
    fn prune_empty_dirs_stops_at_root() {
        let (_d, root) = make_root();
        let leaf = root.join("a/b/c");
        fs::create_dir_all(&leaf).expect("mkdir -p");
        prune_empty_dirs(&root, &[], Some(&leaf));
        assert!(!leaf.exists(), "leaf survived");
        assert!(!root.join("a/b").exists(), "b survived");
        assert!(!root.join("a").exists(), "a survived");
        assert!(root.exists(), "root was nuked");
    }

    #[test]
    fn prune_empty_dirs_stops_at_mapping_root() {
        let (_d, root) = make_root();
        let mount = root.join("src/server");
        fs::create_dir_all(&mount).expect("mkdir mount");
        let mount_canon = fs::canonicalize(&mount).expect("canonicalize");
        // Pretend the mount is a $path entry: prune from inside it should
        // collapse children but never the mount itself.
        let inner = mount.join("nested/deeper");
        fs::create_dir_all(&inner).expect("mkdir inner");
        prune_empty_dirs(&root, &[mount_canon.clone()], Some(&inner));
        assert!(!inner.exists(), "inner survived");
        assert!(!mount.join("nested").exists(), "nested survived");
        assert!(mount.exists(), "mount root pruned (it shouldn't be)");
    }

    #[test]
    fn prune_empty_dirs_preserves_dirs_with_files() {
        let (_d, root) = make_root();
        let parent = root.join("a/b");
        fs::create_dir_all(&parent).expect("mkdir");
        // sibling file in parent's parent so the walk should stop there.
        touch(&root.join("a/keep.txt"));
        prune_empty_dirs(&root, &[], Some(&parent));
        assert!(!parent.exists(), "empty leaf b survived");
        assert!(root.join("a").exists(), "ancestor a got pruned despite having a sibling file");
    }

    #[test]
    fn prune_empty_dirs_handles_nonexistent_start() {
        let (_d, root) = make_root();
        // Pointing at something that doesn't exist must not panic or remove
        // anything.
        prune_empty_dirs(&root, &[], Some(&root.join("does/not/exist")));
        assert!(root.exists());
    }

    // ─── prune_all_empty_subdirs (sweep) ────────────────────────────────

    #[test]
    fn prune_all_empty_subdirs_collapses_chain() {
        let (_d, root) = make_root();
        let mount = root.join("src");
        fs::create_dir_all(mount.join("a/b/c/d")).expect("mkdir chain");
        let mount_canon = fs::canonicalize(&mount).expect("canonicalize");
        let removed = prune_all_empty_subdirs(&root, &[mount_canon]);
        assert_eq!(removed, 4, "expected to remove a, b, c, d");
        assert!(mount.exists(), "mount itself was pruned");
        assert!(!mount.join("a").exists(), "a still there");
    }

    #[test]
    fn prune_all_empty_subdirs_preserves_dirs_with_files() {
        let (_d, root) = make_root();
        let mount = root.join("src");
        fs::create_dir_all(mount.join("kept")).expect("mkdir kept");
        touch(&mount.join("kept/file.txt"));
        fs::create_dir_all(mount.join("dropped/empty")).expect("mkdir dropped");
        let mount_canon = fs::canonicalize(&mount).expect("canonicalize");
        let removed = prune_all_empty_subdirs(&root, &[mount_canon]);
        assert_eq!(removed, 2, "should remove dropped and dropped/empty only");
        assert!(mount.join("kept").exists());
        assert!(!mount.join("dropped").exists());
    }

    #[test]
    fn prune_all_empty_subdirs_skips_package_dir_names() {
        // Package-manager landings (Packages, _Index, .pesde, etc.) are
        // off-limits even when they look empty: their contents are
        // typically junctions that Windows refuses to remove and that
        // we'd be wrong to touch even if we could. See
        // `SWEEP_SKIP_DIR_NAMES`.
        let (_d, root) = make_root();
        let mount = root.join("src");
        // Create one normal empty dir (should be pruned) and one nested
        // under a Packages folder (must survive).
        fs::create_dir_all(mount.join("normal")).expect("mkdir normal");
        fs::create_dir_all(mount.join("Packages/_Index/some-pkg/sub")).expect("mkdir pkg tree");
        let mount_canon = fs::canonicalize(&mount).expect("canonicalize");
        let removed = prune_all_empty_subdirs(&root, &[mount_canon]);
        assert_eq!(removed, 1, "only `normal/` should be pruned");
        assert!(!mount.join("normal").exists(), "normal/ survived");
        assert!(
            mount.join("Packages/_Index/some-pkg/sub").exists(),
            "package internals must not be touched"
        );
    }

    #[test]
    fn safe_to_prune_rejects_path_under_skipped_ancestor() {
        // Candidate is empty AND not in mapping_roots, but lives under
        // `Packages/_Index/...` — must still be refused so a per-file
        // delete inside a package landing doesn't end up unlinking the
        // package manager's structural folders.
        let (_d, root) = make_root();
        let candidate = root.join("src/Packages/_Index/foo");
        fs::create_dir_all(&candidate).expect("mkdir tree");
        let canon = fs::canonicalize(&candidate).expect("canonicalize");
        assert!(!is_safe_to_prune(&root, &[], &canon));
    }

    #[test]
    fn dir_name_is_skipped_recognizes_well_known_packages() {
        for name in ["Packages", "_Index", ".pesde", "roblox_packages", "node_modules", ".yeet"] {
            assert!(
                dir_name_is_skipped(std::ffi::OsStr::new(name)),
                "{name} should be skipped"
            );
        }
        for name in ["src", "Server", "ConsPackages", ".gitkeep", ""] {
            assert!(
                !dir_name_is_skipped(std::ffi::OsStr::new(name)),
                "{name} should NOT be skipped"
            );
        }
    }

    #[test]
    fn prune_all_empty_subdirs_idempotent() {
        let (_d, root) = make_root();
        let mount = root.join("src");
        fs::create_dir_all(mount.join("kept")).expect("mkdir");
        touch(&mount.join("kept/file.txt"));
        let mount_canon = fs::canonicalize(&mount).expect("canonicalize");
        let first = prune_all_empty_subdirs(&root, &[mount_canon.clone()]);
        let second = prune_all_empty_subdirs(&root, &[mount_canon]);
        assert_eq!(first, 0);
        assert_eq!(second, 0);
        assert!(mount.join("kept/file.txt").exists());
    }

    // ─── prune_all_empty_subdirs (aggressive sweep) ─────────────────────

    #[test]
    fn aggressive_sweep_drops_meta_only_subtree_inside_packages() {
        // Real-world Wally / pesde landings users see after a syncback:
        // a deep Packages/_Index/<pkg>/<sub>/ tree where every leaf carries
        // an `init.meta.json` but no `.luau`. Conservative skip-by-name
        // would refuse this tree; the aggressive sweep recognises it as
        // dead metadata and removes it whole.
        let (_d, root) = make_root();
        let mount = root.join("src");
        let leaf = mount.join("Packages/_Index/foo@1.0.0/foo");
        fs::create_dir_all(&leaf).expect("mkdir tree");
        touch(&leaf.join("init.meta.json"));
        touch(&leaf.parent().unwrap().join("init.meta.json"));
        let mount_canon = fs::canonicalize(&mount).expect("canonicalize");
        let removed = prune_all_empty_subdirs_with(&root, &[mount_canon], /*aggressive=*/ true);
        assert!(removed >= 1, "expected the meta-only tree to be pruned");
        assert!(!mount.join("Packages").exists(), "Packages/ should be gone");
        assert!(mount.exists(), "mount itself preserved");
    }

    #[test]
    fn aggressive_sweep_preserves_subtree_with_code() {
        // Same shape as above but with one `.luau` in the leaf — the
        // aggressive sweep MUST refuse to drop the subtree because that
        // would erase user code. Stricter than what conservative did,
        // since conservative would refuse anything under Packages anyway.
        let (_d, root) = make_root();
        let mount = root.join("src");
        let leaf = mount.join("Packages/_Index/foo@1.0.0/foo");
        fs::create_dir_all(&leaf).expect("mkdir tree");
        touch(&leaf.join("init.meta.json"));
        touch(&leaf.join("Real.luau"));
        let mount_canon = fs::canonicalize(&mount).expect("canonicalize");
        let _ = prune_all_empty_subdirs_with(&root, &[mount_canon], /*aggressive=*/ true);
        assert!(leaf.join("Real.luau").exists(), "user code must survive");
        assert!(leaf.join("init.meta.json").exists(), "sibling meta survives");
    }

    #[test]
    fn aggressive_sweep_drops_dir_with_no_luau_even_if_other_files_present() {
        // The user explicitly asked: any directory whose subtree has no
        // descendant `.luau` / `.lua` file should be pruned, regardless
        // of incidental sentinel files (`.gitkeep`, README placeholders,
        // package-manager metadata). That matches what Rojo / Argon
        // emit on the disk side.
        let (_d, root) = make_root();
        let mount = root.join("src");
        let dir = mount.join("scaffolding");
        fs::create_dir_all(&dir).expect("mkdir");
        touch(&dir.join("README.md"));
        touch(&dir.join(".gitkeep"));
        let mount_canon = fs::canonicalize(&mount).expect("canonicalize");
        let _ = prune_all_empty_subdirs_with(&root, &[mount_canon], /*aggressive=*/ true);
        assert!(!dir.exists(), "directory without code should have been pruned");
    }

    #[test]
    fn aggressive_sweep_collapses_nested_codeless_package_landings() {
        // Reproduces the Wally / pesde landing the user kept seeing:
        // `Satchel/SatchelLoader/Satchel/Packages/_Index/<pkg>@x.y.z/<pkg>/`
        // with empty leaves at multiple depths. The whole subtree carries
        // no `.luau`, so the aggressive sweep should collapse it across
        // the iterative passes — leaving only the mount itself behind.
        let (_d, root) = make_root();
        let mount = root.join("src");
        let trunk = mount
            .join("Satchel/SatchelLoader/Satchel/Packages/_Index/foo@1.2.3/foo");
        fs::create_dir_all(trunk.join("Elements")).expect("mkdir Elements");
        fs::create_dir_all(trunk.join("Features/Themes")).expect("mkdir Features/Themes");
        fs::create_dir_all(trunk.join("Packages")).expect("mkdir Packages");
        fs::create_dir_all(mount.join("Satchel/SatchelLoader/Satchel/Packages/satchel"))
            .expect("mkdir satchel sibling");
        let mount_canon = fs::canonicalize(&mount).expect("canonicalize");
        let removed = prune_all_empty_subdirs_with(&root, &[mount_canon], /*aggressive=*/ true);
        assert!(removed > 0, "expected the codeless landing to be pruned");
        assert!(
            !mount.join("Satchel").exists(),
            "Satchel/ should have been collapsed entirely, found leftovers under {}",
            mount.display()
        );
        assert!(mount.exists(), "mount itself preserved");
    }

    #[test]
    fn aggressive_sweep_preserves_dir_with_luau_among_other_files() {
        // Same shape as the test above but with a `.luau` mixed in. The
        // sweep MUST refuse so the user's code doesn't go missing
        // alongside the scaffolding.
        let (_d, root) = make_root();
        let mount = root.join("src");
        let dir = mount.join("module");
        fs::create_dir_all(&dir).expect("mkdir");
        touch(&dir.join("README.md"));
        touch(&dir.join("init.luau"));
        let mount_canon = fs::canonicalize(&mount).expect("canonicalize");
        let _ = prune_all_empty_subdirs_with(&root, &[mount_canon], /*aggressive=*/ true);
        assert!(dir.join("init.luau").exists(), "user code must survive");
        assert!(dir.join("README.md").exists(), "neighbour file must survive");
    }

    // ─── MAX_CONTENT_BYTES boundary ─────────────────────────────────────

    #[test]
    fn max_content_bytes_is_ten_mib() {
        // Sanity: changing this constant should be a deliberate decision
        // visible in code review, not a silent edit.
        assert_eq!(MAX_CONTENT_BYTES, 10 * 1024 * 1024);
    }

    #[test]
    fn content_size_under_cap_passes() {
        let s = "x".repeat(MAX_CONTENT_BYTES - 1);
        assert!(s.len() < MAX_CONTENT_BYTES);
    }

    #[test]
    fn content_size_at_cap_passes() {
        let s = "x".repeat(MAX_CONTENT_BYTES);
        assert!(s.len() <= MAX_CONTENT_BYTES);
    }

    #[test]
    fn content_size_over_cap_detected() {
        let s = "x".repeat(MAX_CONTENT_BYTES + 1);
        assert!(s.len() > MAX_CONTENT_BYTES);
    }
}

#[cfg(test)]
mod bulk_tests {
    //! Per-variant correctness tests for `apply_bulk_resolution`. Each test
    //! builds a tempdir-backed project, primes `tree_studio` / `tree_fs`,
    //! invokes one resolution, and asserts the resulting on-disk state +
    //! broadcast traffic + tree mutations. Together these cover every arm
    //! of the `BulkSyncAction` match in `apply_bulk_resolution`.

    use super::{
        apply_bulk_resolution, apply_bulk_resolutions, prune_all_empty_subdirs, BulkSyncResolution,
    };
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{broadcast, RwLock};
    use yeet_daemon::project::Project;
    use yeet_daemon::protocol::{BulkSyncAction, ScriptKind, ServerMsg};
    use yeet_daemon::state::{ProjectState, SharedState};
    use yeet_daemon::tree::TreeEntry;

    /// A test environment: temp project root, daemon state, broadcast bus.
    /// Drop order matters — `_root` must outlive `state` (which holds paths
    /// relative to it), so the tempdir guard is the last field.
    struct Env {
        state: SharedState,
        bcast_tx: broadcast::Sender<Arc<ServerMsg>>,
        _root: tempfile::TempDir,
    }

    /// Builds a project with a single `src` mount under `ServerScriptService`,
    /// writes `disk_files` to disk so `rescan_fs` ingests them, and bootstraps
    /// `ProjectState`. The returned env's `tree_fs` is populated; `tree_studio`
    /// and `tree_base` start equal to `tree_fs` (cold-boot semantics).
    async fn make_env(disk_files: &[(&str, &str)]) -> Env {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let project_json = r#"{
            "name": "BulkTest",
            "tree": {
                "$className": "DataModel",
                "ServerScriptService": {
                    "$className": "ServerScriptService",
                    "$path": "src"
                }
            }
        }"#;
        std::fs::write(root.join("default.project.json"), project_json).expect("write project");
        std::fs::create_dir(root.join("src")).expect("mkdir src");
        for (rel, content) in disk_files {
            let abs = root.join(rel);
            if let Some(p) = abs.parent() {
                std::fs::create_dir_all(p).expect("mkdir -p");
            }
            std::fs::write(&abs, content).expect("write file");
        }
        let project = Project::load(&root.join("default.project.json")).expect("load project");
        let state_inner =
            ProjectState::bootstrap(root, project, false, false).expect("bootstrap");
        let state: SharedState = Arc::new(RwLock::new(state_inner));
        let (bcast_tx, _) = broadcast::channel(64);
        Env {
            state,
            bcast_tx,
            _root: dir,
        }
    }

    /// Drains everything currently in `rx` with a short timeout. Tests use
    /// this after triggering a resolution to inspect what got broadcast.
    async fn drain(rx: &mut broadcast::Receiver<Arc<ServerMsg>>) -> Vec<ServerMsg> {
        let mut out = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_millis(20), rx.recv()).await {
                Ok(Ok(m)) => out.push((*m).clone()),
                _ => break,
            }
        }
        out
    }

    fn root_of(env: &Env) -> std::path::PathBuf {
        env._root.path().to_path_buf()
    }

    /// Inserts a synthetic Studio-side entry without going through the
    /// merge engine — used to set up "Studio differs from disk" cases that
    /// the rescan can't construct on its own.
    async fn set_studio(env: &Env, path: &str, content: &str, kind: ScriptKind) {
        let mut guard = env.state.write().await;
        let sha = yeet_daemon::state::sha256_hex(content.as_bytes());
        guard.tree_studio.insert(
            path.to_owned(),
            TreeEntry {
                kind,
                content: content.to_owned(),
                sha256: sha,
            },
        );
    }

    // ─── KeepStudio: Studio content overwrites disk ──────────────────────

    #[tokio::test]
    async fn keep_studio_writes_studio_content_to_disk() {
        let env = make_env(&[("src/Foo.luau", "disk_version")]).await;
        set_studio(&env, "src/Foo.luau", "studio_version", ScriptKind::ModuleScript).await;
        let mut rx = env.bcast_tx.subscribe();
        apply_bulk_resolution(
            &env.state,
            &env.bcast_tx,
            "src/Foo.luau",
            BulkSyncAction::KeepStudio,
        )
        .await
        .expect("apply ok");
        let _ = drain(&mut rx).await;
        let on_disk = std::fs::read_to_string(root_of(&env).join("src/Foo.luau")).expect("read");
        assert_eq!(on_disk, "studio_version");
    }

    #[tokio::test]
    async fn keep_studio_with_missing_studio_entry_skips_silently() {
        let env = make_env(&[("src/Foo.luau", "disk_version")]).await;
        // Tree_studio empty for this path → guard kicks in.
        let res = apply_bulk_resolution(
            &env.state,
            &env.bcast_tx,
            "src/Missing.luau",
            BulkSyncAction::KeepStudio,
        )
        .await;
        assert!(res.is_ok(), "skip should not error");
        // Disk untouched.
        let on_disk =
            std::fs::read_to_string(root_of(&env).join("src/Foo.luau")).expect("read");
        assert_eq!(on_disk, "disk_version");
    }

    // ─── KeepIde / PullToStudio: disk content pushed into Studio ─────────

    #[tokio::test]
    async fn keep_ide_pushes_disk_content_to_studio_via_broadcast() {
        let env = make_env(&[("src/Foo.luau", "disk_authoritative")]).await;
        set_studio(&env, "src/Foo.luau", "stale_studio", ScriptKind::ModuleScript).await;
        let mut rx = env.bcast_tx.subscribe();
        apply_bulk_resolution(
            &env.state,
            &env.bcast_tx,
            "src/Foo.luau",
            BulkSyncAction::KeepIde,
        )
        .await
        .expect("apply ok");
        let msgs = drain(&mut rx).await;
        let pushed = msgs.iter().any(|m| matches!(m, ServerMsg::FileChanged { path, content, .. } if path == "src/Foo.luau" && content == "disk_authoritative"));
        assert!(pushed, "expected FileChanged with disk content; got {msgs:?}");
        let guard = env.state.read().await;
        let entry = guard.tree_studio.get("src/Foo.luau").expect("studio entry");
        assert_eq!(entry.content, "disk_authoritative");
    }

    #[tokio::test]
    async fn pull_to_studio_creates_studio_entry_for_ide_only_file() {
        let env = make_env(&[("src/NewScript.luau", "fresh from ide")]).await;
        // tree_studio has no entry for this path — cold boot before snapshot.
        {
            let mut g = env.state.write().await;
            g.tree_studio.clear();
        }
        let mut rx = env.bcast_tx.subscribe();
        apply_bulk_resolution(
            &env.state,
            &env.bcast_tx,
            "src/NewScript.luau",
            BulkSyncAction::PullToStudio,
        )
        .await
        .expect("apply ok");
        let msgs = drain(&mut rx).await;
        let created = msgs.iter().any(|m| matches!(m, ServerMsg::FileCreated { path, content, .. } if path == "src/NewScript.luau" && content == "fresh from ide"));
        assert!(created, "expected FileCreated for ide_only pull; got {msgs:?}");
    }

    #[tokio::test]
    async fn pull_to_studio_with_missing_disk_entry_skips() {
        let env = make_env(&[]).await;
        let res = apply_bulk_resolution(
            &env.state,
            &env.bcast_tx,
            "src/Nope.luau",
            BulkSyncAction::PullToStudio,
        )
        .await;
        assert!(res.is_ok(), "missing-side should be a no-op, not an error");
    }

    // ─── PushToIde: Studio-only file written to disk ─────────────────────

    #[tokio::test]
    async fn push_to_ide_creates_file_on_disk_for_studio_only() {
        let env = make_env(&[]).await;
        set_studio(
            &env,
            "src/StudioOnly.luau",
            "from_studio",
            ScriptKind::ModuleScript,
        )
        .await;
        apply_bulk_resolution(
            &env.state,
            &env.bcast_tx,
            "src/StudioOnly.luau",
            BulkSyncAction::PushToIde,
        )
        .await
        .expect("apply ok");
        let on_disk = std::fs::read_to_string(root_of(&env).join("src/StudioOnly.luau"))
            .expect("file should exist");
        assert_eq!(on_disk, "from_studio");
    }

    // ─── DeleteFromStudio: emits FileDeleted, keeps disk untouched ───────

    #[tokio::test]
    async fn delete_from_studio_emits_filedeleted_and_keeps_disk() {
        let env = make_env(&[("src/Both.luau", "disk_kept")]).await;
        set_studio(&env, "src/Both.luau", "studio_to_drop", ScriptKind::ModuleScript).await;
        let mut rx = env.bcast_tx.subscribe();
        apply_bulk_resolution(
            &env.state,
            &env.bcast_tx,
            "src/Both.luau",
            BulkSyncAction::DeleteFromStudio,
        )
        .await
        .expect("apply ok");
        let msgs = drain(&mut rx).await;
        let deleted = msgs.iter().any(|m| matches!(m, ServerMsg::FileDeleted { path } if path == "src/Both.luau"));
        assert!(deleted, "expected FileDeleted broadcast; got {msgs:?}");
        // Disk still has its copy.
        assert!(root_of(&env).join("src/Both.luau").exists());
    }

    // ─── DeleteFromIde: removes file + prunes parent dir ────────────────

    #[tokio::test]
    async fn delete_from_ide_removes_file_and_prunes_empty_parent() {
        let env = make_env(&[("src/lonely/Single.luau", "ide_only")]).await;
        let lonely = root_of(&env).join("src/lonely");
        assert!(lonely.exists());
        apply_bulk_resolution(
            &env.state,
            &env.bcast_tx,
            "src/lonely/Single.luau",
            BulkSyncAction::DeleteFromIde,
        )
        .await
        .expect("apply ok");
        assert!(
            !root_of(&env).join("src/lonely/Single.luau").exists(),
            "file still present"
        );
        assert!(!lonely.exists(), "now-empty parent should have been pruned");
        // Mount root preserved.
        assert!(root_of(&env).join("src").exists(), "$path mount got nuked");
    }

    // ─── Skip: noop, no broadcast, no disk change ───────────────────────

    #[tokio::test]
    async fn skip_is_pure_noop() {
        let env = make_env(&[("src/Foo.luau", "untouched")]).await;
        let mut rx = env.bcast_tx.subscribe();
        apply_bulk_resolution(
            &env.state,
            &env.bcast_tx,
            "src/Foo.luau",
            BulkSyncAction::Skip,
        )
        .await
        .expect("apply ok");
        let msgs = drain(&mut rx).await;
        assert!(msgs.is_empty(), "Skip emitted {} messages", msgs.len());
        let on_disk = std::fs::read_to_string(root_of(&env).join("src/Foo.luau")).expect("read");
        assert_eq!(on_disk, "untouched");
    }

    // ─── End-to-end: Plan A bootstrap sweep ──────────────────────────────

    #[tokio::test]
    async fn apply_bulk_resolutions_sweep_drops_emptied_dirs_post_batch() {
        // Two siblings in the same dir; both deleted in one batch. The per-
        // file walk on the FIRST delete sees a non-empty parent and gives
        // up; the post-batch sweep MUST catch it and drop the parent.
        let env = make_env(&[
            ("src/cluster/A.luau", "a"),
            ("src/cluster/B.luau", "b"),
        ])
        .await;
        let cluster = root_of(&env).join("src/cluster");
        assert!(cluster.exists());
        let resolutions = vec![
            BulkSyncResolution {
                path: "src/cluster/A.luau".into(),
                action: BulkSyncAction::DeleteFromIde,
            },
            BulkSyncResolution {
                path: "src/cluster/B.luau".into(),
                action: BulkSyncAction::DeleteFromIde,
            },
        ];
        apply_bulk_resolutions(&env.state, &env.bcast_tx, "test-request", resolutions)
            .await
            .expect("ok");
        assert!(
            !cluster.exists(),
            "cluster/ should have been swept after the batch"
        );
        assert!(root_of(&env).join("src").exists(), "$path root preserved");
    }

    #[tokio::test]
    async fn bootstrap_sweep_drops_preexisting_empty_dirs() {
        // Project scan ignored an already-empty `src/orphan/` dir at
        // startup. Plan A's sweep at bootstrap should clean it.
        let env = make_env(&[("src/keep/Keep.luau", "x")]).await;
        let orphan = root_of(&env).join("src/orphan/deep");
        std::fs::create_dir_all(&orphan).expect("mkdir orphan");
        assert!(orphan.exists());
        let canonical_root = std::fs::canonicalize(root_of(&env)).expect("canon");
        let mapping_roots = env.state.read().await.mapping_roots_canonical.clone();
        let removed = prune_all_empty_subdirs(&canonical_root, &mapping_roots);
        assert!(removed >= 2, "expected to prune orphan + orphan/deep, got {removed}");
        assert!(!root_of(&env).join("src/orphan").exists());
        assert!(root_of(&env).join("src/keep/Keep.luau").exists());
    }

    // ─── End-to-end: mixed Bootstrap resolutions ─────────────────────────

    #[tokio::test]
    async fn mixed_bulk_resolutions_apply_independently() {
        // Six paths covering every interesting BulkSyncAction. Each
        // post-state assertion is independent so a regression in one
        // variant doesn't mask others.
        let env = make_env(&[
            ("src/Modified.luau", "ide_modified"),
            ("src/IdeOnly.luau", "fresh_from_ide"),
            ("src/ToDelete.luau", "ide_to_drop"),
            ("src/SharedKeep.luau", "ide_keeps"),
            ("src/Untouched.luau", "left_alone"),
        ])
        .await;
        set_studio(
            &env,
            "src/Modified.luau",
            "studio_wins",
            ScriptKind::ModuleScript,
        )
        .await;
        set_studio(
            &env,
            "src/StudioOnly.luau",
            "studio_alone",
            ScriptKind::ModuleScript,
        )
        .await;
        set_studio(
            &env,
            "src/SharedKeep.luau",
            "studio_loses",
            ScriptKind::ModuleScript,
        )
        .await;
        set_studio(
            &env,
            "src/Untouched.luau",
            "studio_unchanged",
            ScriptKind::ModuleScript,
        )
        .await;

        let resolutions = vec![
            BulkSyncResolution {
                path: "src/Modified.luau".into(),
                action: BulkSyncAction::KeepStudio,
            },
            BulkSyncResolution {
                path: "src/IdeOnly.luau".into(),
                action: BulkSyncAction::PullToStudio,
            },
            BulkSyncResolution {
                path: "src/ToDelete.luau".into(),
                action: BulkSyncAction::DeleteFromIde,
            },
            BulkSyncResolution {
                path: "src/SharedKeep.luau".into(),
                action: BulkSyncAction::KeepIde,
            },
            BulkSyncResolution {
                path: "src/Untouched.luau".into(),
                action: BulkSyncAction::Skip,
            },
            BulkSyncResolution {
                path: "src/StudioOnly.luau".into(),
                action: BulkSyncAction::PushToIde,
            },
        ];
        apply_bulk_resolutions(&env.state, &env.bcast_tx, "test-request", resolutions)
            .await
            .expect("apply ok");

        let root = root_of(&env);
        assert_eq!(
            std::fs::read_to_string(root.join("src/Modified.luau")).expect("read"),
            "studio_wins",
            "KeepStudio should overwrite disk"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src/StudioOnly.luau")).expect("read"),
            "studio_alone",
            "PushToIde should create file on disk"
        );
        assert!(
            !root.join("src/ToDelete.luau").exists(),
            "DeleteFromIde should remove file"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src/Untouched.luau")).expect("read"),
            "left_alone",
            "Skip should not touch disk"
        );

        let guard = env.state.read().await;
        assert_eq!(
            guard
                .tree_studio
                .get("src/IdeOnly.luau")
                .expect("entry")
                .content,
            "fresh_from_ide",
            "PullToStudio should populate tree_studio"
        );
        assert_eq!(
            guard
                .tree_studio
                .get("src/SharedKeep.luau")
                .expect("entry")
                .content,
            "ide_keeps",
            "KeepIde should overwrite tree_studio"
        );
    }
}

#[cfg(test)]
mod fs_removed_pending_tests {
    //! Direct tests of `handle_fs_event`'s `fs_removed_pending` flow — the
    //! fix for AUDITORIA-YEET.md A1 (plus the move-2 and watcher-4 guards).
    //! Mirrors the `bulk_tests` harness style: tempdir-backed project,
    //! bootstrap, fire raw `FileEvent`s, inspect broadcasts + tree state.

    use super::{handle_fs_event, FS_RENAME_PAIR_TTL};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{broadcast, RwLock};
    use yeet_daemon::project::Project;
    use yeet_daemon::protocol::ServerMsg;
    use yeet_daemon::state::{ProjectState, SharedState};
    use yeet_daemon::watcher::FileEvent;

    /// A test environment: temp project root, daemon state, broadcast bus.
    /// Drop order matters — `_root` must outlive `state` (which holds paths
    /// relative to it), so the tempdir guard is the last field.
    struct Env {
        state: SharedState,
        bcast_tx: broadcast::Sender<Arc<ServerMsg>>,
        _root: tempfile::TempDir,
    }

    /// Builds a project with a single `src` mount under `ServerScriptService`,
    /// writes `disk_files` to disk so `rescan_fs` ingests them, and
    /// bootstraps `ProjectState`. `bootstrap` alone leaves `tree_studio`
    /// empty (it only fills in once the plugin reports a snapshot), so this
    /// also seeds `tree_studio` from `tree_fs` to model an already-synced
    /// project — matching the audit's repro, which starts from files
    /// already synced to Studio.
    async fn make_env(disk_files: &[(&str, &str)]) -> Env {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let project_json = r#"{
            "name": "FsRemovedPendingTest",
            "tree": {
                "$className": "DataModel",
                "ServerScriptService": {
                    "$className": "ServerScriptService",
                    "$path": "src"
                }
            }
        }"#;
        std::fs::write(root.join("default.project.json"), project_json).expect("write project");
        std::fs::create_dir(root.join("src")).expect("mkdir src");
        for (rel, content) in disk_files {
            let abs = root.join(rel);
            if let Some(p) = abs.parent() {
                std::fs::create_dir_all(p).expect("mkdir -p");
            }
            std::fs::write(&abs, content).expect("write file");
        }
        let project = Project::load(&root.join("default.project.json")).expect("load project");
        let state_inner =
            ProjectState::bootstrap(root, project, false, false).expect("bootstrap");
        let state: SharedState = Arc::new(RwLock::new(state_inner));
        {
            let mut guard = state.write().await;
            guard.tree_studio = guard.tree_fs.clone();
        }
        let (bcast_tx, _) = broadcast::channel(64);
        Env {
            state,
            bcast_tx,
            _root: dir,
        }
    }

    fn root_of(env: &Env) -> std::path::PathBuf {
        env._root.path().to_path_buf()
    }

    /// Drains everything currently in `rx` with a short timeout. Tests use
    /// this after triggering fs events to inspect what got broadcast.
    async fn drain(rx: &mut broadcast::Receiver<Arc<ServerMsg>>) -> Vec<ServerMsg> {
        let mut out = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_millis(20), rx.recv()).await {
                Ok(Ok(m)) => out.push((*m).clone()),
                _ => break,
            }
        }
        out
    }

    /// Sleeps past the rename-pairing TTL so any deferred reconcile task
    /// spawned by a `Removed` event (see `handle_fs_event`) has run.
    async fn wait_out_pairing_window() {
        tokio::time::sleep(FS_RENAME_PAIR_TTL + Duration::from_millis(400)).await;
    }

    // ─── A1 core: two same-content deletes must NOT clobber each other ───

    #[tokio::test]
    async fn two_identical_content_deletes_within_window_both_propagate() {
        let env = make_env(&[("src/A.luau", "return {}"), ("src/B.luau", "return {}")]).await;
        let mut rx = env.bcast_tx.subscribe();
        let root = root_of(&env);

        std::fs::remove_file(root.join("src/A.luau")).expect("rm A");
        handle_fs_event(
            &env.state,
            FileEvent::Removed(root.join("src/A.luau")),
            &env.bcast_tx,
        )
        .await
        .expect("handle removed A");
        std::fs::remove_file(root.join("src/B.luau")).expect("rm B");
        handle_fs_event(
            &env.state,
            FileEvent::Removed(root.join("src/B.luau")),
            &env.bcast_tx,
        )
        .await
        .expect("handle removed B");

        wait_out_pairing_window().await;
        let msgs = drain(&mut rx).await;

        let deleted_a = msgs
            .iter()
            .any(|m| matches!(m, ServerMsg::FileDeleted { path } if path == "src/A.luau"));
        let deleted_b = msgs
            .iter()
            .any(|m| matches!(m, ServerMsg::FileDeleted { path } if path == "src/B.luau"));
        assert!(deleted_a, "expected FileDeleted for A; got {msgs:?}");
        assert!(
            deleted_b,
            "expected FileDeleted for B too (lost under sha-keying); got {msgs:?}"
        );

        let guard = env.state.read().await;
        assert!(
            !guard.tree_base.contains_key("src/A.luau"),
            "A must not linger as a phantom in tree_base"
        );
        assert!(
            !guard.tree_base.contains_key("src/B.luau"),
            "B must not linger as a phantom in tree_base"
        );
        assert!(!guard.tree_studio.contains_key("src/A.luau"));
        assert!(!guard.tree_studio.contains_key("src/B.luau"));
    }

    // ─── watcher-4: Touched on a PRE-EXISTING file must not pair ─────────

    #[tokio::test]
    async fn remove_a_then_touch_preexisting_b_with_as_content_is_not_a_rename() {
        let env = make_env(&[
            ("src/A.luau", "local A = 1\nreturn A\n"),
            ("src/B.luau", "local B = 2\nreturn B\n"),
        ])
        .await;
        let mut rx = env.bcast_tx.subscribe();
        let root = root_of(&env);

        std::fs::remove_file(root.join("src/A.luau")).expect("rm A");
        handle_fs_event(
            &env.state,
            FileEvent::Removed(root.join("src/A.luau")),
            &env.bcast_tx,
        )
        .await
        .expect("handle removed A");

        // Overwrite pre-existing B with A's old content within the window.
        std::fs::write(root.join("src/B.luau"), "local A = 1\nreturn A\n").expect("overwrite B");
        handle_fs_event(
            &env.state,
            FileEvent::Touched(root.join("src/B.luau")),
            &env.bcast_tx,
        )
        .await
        .expect("handle touched B");

        wait_out_pairing_window().await;
        let msgs = drain(&mut rx).await;

        let renamed = msgs
            .iter()
            .any(|m| matches!(m, ServerMsg::FileRenamed { .. }));
        assert!(
            !renamed,
            "must NOT pair as a rename when B pre-existed; got {msgs:?}"
        );

        let deleted_a = msgs
            .iter()
            .any(|m| matches!(m, ServerMsg::FileDeleted { path } if path == "src/A.luau"));
        assert!(
            deleted_a,
            "expected an independent FileDeleted for A; got {msgs:?}"
        );

        let changed_b = msgs.iter().any(|m| {
            matches!(m, ServerMsg::FileChanged { path, content, .. }
                if path == "src/B.luau" && content == "local A = 1\nreturn A\n")
        });
        assert!(
            changed_b,
            "expected a normal FileChanged for B; got {msgs:?}"
        );

        let guard = env.state.read().await;
        assert_eq!(
            guard
                .tree_studio
                .get("src/B.luau")
                .map(|e| e.content.as_str()),
            Some("local A = 1\nreturn A\n"),
            "B's identity must be its own, not clobbered by A's rename"
        );
    }

    // ─── Regression: a genuine single rename must still pair ─────────────

    #[tokio::test]
    async fn single_rename_still_pairs_as_filerenamed() {
        let env = make_env(&[("src/Old.luau", "hello")]).await;
        let mut rx = env.bcast_tx.subscribe();
        let root = root_of(&env);

        std::fs::remove_file(root.join("src/Old.luau")).expect("rm Old");
        handle_fs_event(
            &env.state,
            FileEvent::Removed(root.join("src/Old.luau")),
            &env.bcast_tx,
        )
        .await
        .expect("handle removed Old");

        std::fs::write(root.join("src/New.luau"), "hello").expect("write New");
        handle_fs_event(
            &env.state,
            FileEvent::Touched(root.join("src/New.luau")),
            &env.bcast_tx,
        )
        .await
        .expect("handle touched New");

        wait_out_pairing_window().await;
        let msgs = drain(&mut rx).await;

        let renames: Vec<_> = msgs
            .iter()
            .filter(|m| {
                matches!(m, ServerMsg::FileRenamed { old_path, new_path, .. }
                    if old_path == "src/Old.luau" && new_path == "src/New.luau")
            })
            .collect();
        assert_eq!(
            renames.len(),
            1,
            "expected exactly one FileRenamed; got {msgs:?}"
        );

        let stray_delete = msgs
            .iter()
            .any(|m| matches!(m, ServerMsg::FileDeleted { path } if path == "src/Old.luau"));
        assert!(
            !stray_delete,
            "rename must not ALSO emit a stray FileDeleted; got {msgs:?}"
        );

        let guard = env.state.read().await;
        assert!(!guard.fs_removed_pending.contains_key("src/Old.luau"));
    }
}

#[cfg(test)]
mod security_tests {
    //! Regression tests for AUDITORIA-YEET.md security findings A16 (auth gate
    //! leaked the token / never rejected), A17 (syncback arbitrary file write),
    //! M24 (Origin/Host prefix-match DNS-rebinding bypass) and B13 (silent
    //! non-loopback `--bind`). The policy for each finding is factored into a
    //! pure function so it can be exercised without a live socket, mirroring
    //! how the rest of the daemon tests its decision cores.

    // ─── A16: auth gate ──────────────────────────────────────────────────
    use super::{decide_auth, AuthOutcome};

    const TOKEN: &str = "0011223344556677889900aabbccddee";

    #[test]
    fn matching_token_proceeds() {
        assert_eq!(
            decide_auth(Some(TOKEN), "plugin", TOKEN, false),
            AuthOutcome::Proceed
        );
        assert_eq!(
            decide_auth(Some(TOKEN), "extension", TOKEN, false),
            AuthOutcome::Proceed
        );
    }

    #[test]
    fn wrong_token_is_rejected_and_never_grants() {
        // The A16 leak: a mismatched token used to be answered with
        // AuthGranted { server token }. It must now close the connection and
        // MUST NOT reach the GrantAndPair (token-sending) arm — even when a
        // breadcrumb happens to be fresh.
        assert_eq!(
            decide_auth(Some("deadbeef"), "plugin", TOKEN, false),
            AuthOutcome::Reject("auth token mismatch")
        );
        assert_eq!(
            decide_auth(Some("deadbeef"), "plugin", TOKEN, true),
            AuthOutcome::Reject("auth token mismatch")
        );
        assert_eq!(
            decide_auth(Some("deadbeef"), "extension", TOKEN, false),
            AuthOutcome::Reject("auth token mismatch")
        );
    }

    #[test]
    fn extension_without_token_is_rejected() {
        // Extension/CLI clients can read `.yeet/auth-token`, so they must
        // present it up-front. No token → refused, breadcrumb or not.
        assert_eq!(
            decide_auth(None, "extension", TOKEN, false),
            AuthOutcome::Reject("missing auth token (required for this role)")
        );
        assert_eq!(
            decide_auth(None, "extension", TOKEN, true),
            AuthOutcome::Reject("missing auth token (required for this role)")
        );
        // Empty string counts as "no token".
        assert_eq!(
            decide_auth(Some(""), "extension", TOKEN, false),
            AuthOutcome::Reject("missing auth token (required for this role)")
        );
    }

    #[test]
    fn plugin_without_token_pairs_via_fresh_breadcrumb() {
        assert_eq!(
            decide_auth(None, "plugin", TOKEN, true),
            AuthOutcome::GrantAndPair
        );
        // Empty string is treated as no token.
        assert_eq!(
            decide_auth(Some(""), "plugin", TOKEN, true),
            AuthOutcome::GrantAndPair
        );
    }

    #[test]
    fn plugin_without_token_or_breadcrumb_still_proceeds() {
        // First-run Studio before the extension is up (no breadcrumb yet)
        // must still connect — the legitimate flow the audit says to keep.
        assert_eq!(
            decide_auth(None, "plugin", TOKEN, false),
            AuthOutcome::Proceed
        );
    }

    // ─── A17: syncback target confinement ────────────────────────────────
    use super::{syncback_overwrite_ok, validate_syncback_target};
    use yeet_daemon::protocol::SyncbackMode;

    #[test]
    fn syncback_accepts_new_folder_under_home() {
        let home = tempfile::tempdir().expect("home");
        // Brand-new leaf directly under home (the reverse-bootstrap case).
        let target = home.path().join("MyNewGame");
        assert!(validate_syncback_target(&target, home.path()).is_ok());
        // Nested new folder whose nearest existing ancestor is still home.
        std::fs::create_dir(home.path().join("Projects")).expect("mkdir");
        let nested = home.path().join("Projects").join("Deep").join("Game");
        assert!(validate_syncback_target(&nested, home.path()).is_ok());
    }

    #[test]
    fn syncback_rejects_target_outside_home() {
        let home = tempfile::tempdir().expect("home");
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        let target = elsewhere.path().join("victim");
        let err = validate_syncback_target(&target, home.path())
            .expect_err("target outside home must be rejected");
        assert!(err.contains("outside the home directory"), "got: {err}");
    }

    #[test]
    fn syncback_rejects_parent_dir_traversal() {
        let home = tempfile::tempdir().expect("home");
        // Absolute path that escapes home via `..` — canonicalize can't
        // resolve it past the non-existent leaf, so it's refused outright.
        let target = home.path().join("..").join("escaped");
        let err = validate_syncback_target(&target, home.path())
            .expect_err("`..` traversal must be rejected");
        assert!(err.contains("must not contain '..'"), "got: {err}");
    }

    #[test]
    fn syncback_rejects_relative_target() {
        let home = tempfile::tempdir().expect("home");
        let target = std::path::Path::new("relative/evil");
        let err = validate_syncback_target(target, home.path())
            .expect_err("relative target must be rejected");
        assert!(err.contains("must be absolute"), "got: {err}");
    }

    #[test]
    fn syncback_overwrite_blocks_nonempty_dir_without_intent() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("keep.luau"), "return {}").expect("seed");
        // No explicit overwrite intent → refused.
        assert!(syncback_overwrite_ok(dir.path(), SyncbackMode::NewProject).is_err());
        assert!(
            syncback_overwrite_ok(dir.path(), SyncbackMode::MergeExisting { overwrite: false })
                .is_err()
        );
        // Explicit intent → allowed.
        assert!(
            syncback_overwrite_ok(dir.path(), SyncbackMode::MergeExisting { overwrite: true })
                .is_ok()
        );
    }

    #[test]
    fn syncback_overwrite_allows_empty_or_missing_dir() {
        let empty = tempfile::tempdir().expect("empty");
        assert!(syncback_overwrite_ok(empty.path(), SyncbackMode::NewProject).is_ok());
        let missing = empty.path().join("does-not-exist-yet");
        assert!(syncback_overwrite_ok(&missing, SyncbackMode::NewProject).is_ok());
    }

    // ─── M24: Origin / Host exact-loopback matching (DNS rebinding) ───────
    use super::{host_header_is_loopback, is_loopback_bind_addr, origin_is_allowed};

    #[test]
    fn origin_absent_or_non_browser_is_allowed() {
        // The Studio plugin sends no browser Origin; these must all pass.
        assert!(origin_is_allowed(""));
        assert!(origin_is_allowed("null"));
        assert!(origin_is_allowed("NULL"));
        // Non-http(s) scheme (file://, app scheme) is not a routable browser
        // page — passes.
        assert!(origin_is_allowed("file://"));
        assert!(origin_is_allowed("roblox-studio://plugin"));
    }

    #[test]
    fn origin_exact_loopback_hosts_pass() {
        for ok in [
            "http://localhost",
            "http://localhost:34872",
            "https://localhost",
            "http://127.0.0.1",
            "http://127.0.0.1:34872",
            "http://[::1]",
            "http://[::1]:34872",
            "http://user:pass@localhost:34872",
        ] {
            assert!(origin_is_allowed(ok), "should accept {ok}");
        }
    }

    #[test]
    fn origin_rebinding_lookalikes_are_rejected() {
        // The M24 bug: `starts_with("localhost")` / `"127."` accepted these.
        for bad in [
            "http://localhost.evil.com",
            "http://localhost.evil.com:34872",
            "http://127.0.0.1.evil.com",
            "http://localhostx",
            "http://evil.com",
            "https://evil.com:34872",
            "http://0.0.0.0",
            "http://169.254.0.1",
        ] {
            assert!(!origin_is_allowed(bad), "should reject {bad}");
        }
    }

    #[test]
    fn host_header_loopback_matching() {
        assert!(host_header_is_loopback(None));
        assert!(host_header_is_loopback(Some("")));
        assert!(host_header_is_loopback(Some("127.0.0.1:34872")));
        assert!(host_header_is_loopback(Some("localhost:34872")));
        assert!(host_header_is_loopback(Some("[::1]:34872")));
        assert!(!host_header_is_loopback(Some("localhost.evil.com:34872")));
        assert!(!host_header_is_loopback(Some("evil.com")));
        assert!(!host_header_is_loopback(Some("192.168.1.5:34872")));
    }

    #[test]
    fn loopback_bind_addr_matching() {
        assert!(is_loopback_bind_addr("127.0.0.1:34872"));
        assert!(is_loopback_bind_addr("127.0.0.1:0"));
        assert!(is_loopback_bind_addr("localhost:34872"));
        assert!(is_loopback_bind_addr("[::1]:0"));
        assert!(!is_loopback_bind_addr("0.0.0.0:34872"));
        assert!(!is_loopback_bind_addr("192.168.1.5:34872"));
    }

    // ─── B13: --bind requires --allow-remote for non-loopback ────────────
    use super::validate_bind_addr;

    #[test]
    fn bind_default_and_loopback_pass_without_flag() {
        // No override → default 127.0.0.1 behaviour, unchanged.
        assert!(validate_bind_addr(None, false).is_ok());
        assert!(validate_bind_addr(Some("127.0.0.1:34872"), false).is_ok());
        assert!(validate_bind_addr(Some("127.0.0.1:0"), false).is_ok());
        assert!(validate_bind_addr(Some("[::1]:0"), false).is_ok());
    }

    #[test]
    fn bind_non_loopback_refused_without_allow_remote() {
        assert!(validate_bind_addr(Some("0.0.0.0:34872"), false).is_err());
        assert!(validate_bind_addr(Some("192.168.1.5:34872"), false).is_err());
    }

    #[test]
    fn bind_non_loopback_allowed_with_flag() {
        assert!(validate_bind_addr(Some("0.0.0.0:34872"), true).is_ok());
        assert!(validate_bind_addr(Some("192.168.1.5:34872"), true).is_ok());
    }
}
