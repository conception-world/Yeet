//! `~/.yeet/daemons.json` — the cross-project index of running daemons.
//!
//! Distinct from the per-project `<root>/.yeet/` directory (`auth-token`,
//! `pairing`, `base-tree.msgpack`): this one file lists every daemon on the
//! machine, keyed by the project each serves.
//!
//! ## Who this is for
//!
//! The **extension**, not the plugin. Before multi-place the extension found
//! "is a daemon already running?" by probing the fixed port 34872 and refusing
//! to spawn when it answered. With one daemon per project that question becomes
//! "is a daemon already running *for my project root*?", which a port probe
//! cannot answer. The registry can.
//!
//! It also restores orphan detection. A force-quit daemon used to be obvious
//! because it squatted the well-known port; with dynamic ports nothing collides
//! and an orphan is otherwise invisible.
//!
//! The Studio plugin never reads this file — it has no filesystem access at all
//! (see `net/Connection.luau`: `CreateWebStreamClient` is its only I/O). The
//! plugin finds daemons by scanning the port window instead. That split is
//! deliberate: it means a broken, stale, or unwritable registry can never stop
//! the plugin from connecting.
//!
//! ## Advisory, never authoritative
//!
//! Concurrent daemons can race on the write and lose an entry. That is
//! tolerated: the worst case is the extension spawning a second daemon for a
//! project that already had one (they land on different ports and both work),
//! and the plugin's scan finds both regardless. Nothing that matters for
//! correctness may depend on this file being complete — which is precisely why
//! a failed registry write is logged and ignored rather than aborting startup.
//!
//! ## Liveness
//!
//! Pruning is by **port probe only**. The obvious alternative — checking the
//! PID — needs `kill(pid, 0)` on Unix, and this crate sets
//! `unsafe_code = "forbid"`; pulling in a process-inspection dependency to
//! delete stale JSON lines is not worth it. `pid` is therefore recorded for
//! humans (it names the process to kill in a bug report) and explicitly NOT
//! used to decide liveness.
//!
//! A successful TCP connect proves *something* listens, not that it is our
//! daemon — a recycled port could be anything. That is good enough for pruning.
//! A caller that must be certain (the extension deciding whether to reuse a
//! daemon rather than spawn one) follows up with a `role="discover"` handshake
//! and compares `daemon_id`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::state::sha256_hex;

/// Schema version of `daemons.json`. Bumped only for a breaking shape change;
/// a file whose version we do not recognize is treated as empty rather than
/// misparsed, so an older daemon never corrupts a newer file's entries.
pub const REGISTRY_VERSION: u32 = 1;

/// How long to wait for a TCP connect when probing whether a registered daemon
/// is still alive. Loopback either answers immediately or refuses immediately;
/// this budget only matters for a port stuck in a half-open state. Mirrors the
/// extension's own `DAEMON_PROBE_TIMEOUT_MS`.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Length of a `daemon_id` in hex characters (64 bits). This is a namespacing
/// key, not a secret: the only collision that matters is between two of one
/// user's own projects, which 64 bits makes impossible in practice.
const DAEMON_ID_HEX_LEN: usize = 16;

/// One running daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Stable per-project id — `daemon_id_for(project_root)`. Survives daemon
    /// restarts for the same project, which is what lets the plugin's picker
    /// remember a choice and key a per-project auth token.
    pub daemon_id: String,
    /// OS process id. Diagnostic ONLY — see the module docs on liveness. Kept
    /// so a user chasing an orphan has something to kill.
    pub pid: u32,
    pub port: u16,
    /// Absolute, canonical project root, with Windows' `\\?\` verbatim prefix
    /// stripped (see `normalize_root_for_display`).
    pub project_root: String,
    /// `Project.name` from `default.project.json`. Shown in UI.
    pub project_name: String,
    pub daemon_version: String,
    /// Unix seconds. Tie-breaker when two entries somehow share a root.
    pub started_at: u64,
}

/// On-disk shape. A struct rather than a bare array so the version rides along.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryFile {
    version: u32,
    daemons: Vec<Entry>,
}

impl Default for RegistryFile {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            daemons: Vec::new(),
        }
    }
}

/// Stable per-project identifier: the first 16 hex chars of the SHA-256 of the
/// normalized project root.
///
/// Derived from the root rather than randomized per boot, because it has to
/// survive a daemon restart: the plugin keys both its remembered picker choice
/// and its per-project auth-token setting on this value.
///
/// Computed daemon-side and sent over the wire because the plugin has no
/// SHA-256 — Luau ships no hash function, and vendoring one purely to derive a
/// settings key is not justified.
#[must_use]
pub fn daemon_id_for(project_root: &Path) -> String {
    let normalized = normalize_root_for_display(project_root);
    // Case-fold: Windows paths are case-insensitive, so `C:\Dev\Game` and
    // `c:\dev\game` are the same project and must hash alike. Unix paths are
    // case-sensitive, but two roots differing only by case there would be a
    // pathological setup, and collapsing them is far less harmful than the
    // Windows case splitting one project into two identities.
    let mut hex = sha256_hex(normalized.to_lowercase().as_bytes());
    hex.truncate(DAEMON_ID_HEX_LEN);
    hex
}

/// Strips Windows' `\\?\` verbatim prefix and normalizes separators to `\` on
/// Windows / leaves them alone elsewhere.
///
/// `ProjectState::bootstrap` canonicalizes the root, and on Windows
/// `fs::canonicalize` returns a verbatim path (`\\?\C:\Users\...`). That prefix
/// is correct for the OS but leaks into anything that displays or compares the
/// path: the extension holds the plain `C:\Users\...` form from VS Code, so a
/// naive string compare against the canonical form never matches and the
/// extension would spawn a duplicate daemon every time.
#[must_use]
pub fn normalize_root_for_display(root: &Path) -> String {
    let s = root.display().to_string();
    // `\\?\UNC\server\share` -> `\\server\share`; `\\?\C:\x` -> `C:\x`.
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_owned()
}

/// `~/.yeet` — the machine-wide Yeet directory.
///
/// Uses `USERPROFILE` on Windows and `HOME` elsewhere rather than per-OS config
/// directories (`%APPDATA%`, `~/.config`). Two reasons: `.yeet/` is already the
/// established Yeet directory name in every project, so users recognize it; and
/// a single rule avoids adding a `dirs`-style dependency just to locate one
/// JSON file.
pub fn registry_dir() -> Result<PathBuf> {
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    };
    let home = home.context(
        "cannot locate the home directory (USERPROFILE on Windows, HOME elsewhere) \
         to place ~/.yeet/daemons.json",
    )?;
    Ok(PathBuf::from(home).join(".yeet"))
}

/// `~/.yeet/daemons.json`.
pub fn registry_path() -> Result<PathBuf> {
    Ok(registry_dir()?.join("daemons.json"))
}

/// Unix seconds, saturating to 0 before the epoch.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reads the registry at `path`. A missing file is an empty registry. A file we
/// cannot parse, or one written by a schema version we do not know, is ALSO
/// treated as empty: the registry is advisory, so silently starting over beats
/// failing a daemon's startup over a corrupt cache.
fn read_at(path: &Path) -> RegistryFile {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return RegistryFile::default();
    };
    match serde_json::from_str::<RegistryFile>(&raw) {
        Ok(file) if file.version == REGISTRY_VERSION => file,
        Ok(file) => {
            tracing::warn!(
                found = file.version,
                expected = REGISTRY_VERSION,
                "daemons.json has an unrecognized schema version — ignoring its contents"
            );
            RegistryFile::default()
        }
        Err(e) => {
            tracing::warn!(error = %e, "daemons.json is unreadable — starting a fresh registry");
            RegistryFile::default()
        }
    }
}

/// Serializes and atomically replaces the registry at `path`.
///
/// The temp file carries the pid so two daemons writing at once cannot clobber
/// each other's partial file; the rename itself is atomic on both NTFS and
/// POSIX. A concurrent write can still lose an entry (last writer wins), which
/// the module docs accept as tolerable.
fn write_at(path: &Path, file: &RegistryFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let mut bytes = serde_json::to_vec_pretty(file).context("serialize daemons.json")?;
    bytes.push(b'\n');
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, &bytes).with_context(|| format!("write {}", tmp.display()))?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Do not leave the temp file behind on a failed rename.
            let _ = std::fs::remove_file(&tmp);
            Err(e).with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))
        }
    }
}

/// True when something accepts a TCP connection on `127.0.0.1:port`.
///
/// Proof that the port is occupied, NOT that our daemon occupies it — see the
/// module docs. Used only to decide whether a registry entry is stale.
#[must_use]
pub fn port_is_live(port: u16) -> bool {
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok()
}

/// Drops entries whose port no longer answers.
fn prune(entries: Vec<Entry>) -> Vec<Entry> {
    entries.into_iter().filter(|e| port_is_live(e.port)).collect()
}

/// Adds (or replaces) this daemon's entry and prunes dead ones.
///
/// Replacing by `daemon_id` rather than appending keeps the file from growing a
/// duplicate row every time a daemon restarts on the same project — the old
/// entry's port is usually dead by then anyway, but a fast restart can reuse the
/// same port and slip past the prune.
pub fn register(entry: &Entry) -> Result<()> {
    let path = registry_path()?;
    register_at(&path, entry)
}

/// `register`, with an explicit path. Exists so tests do not touch the real
/// `~/.yeet/daemons.json`.
pub fn register_at(path: &Path, entry: &Entry) -> Result<()> {
    let mut file = read_at(path);
    file.daemons = prune(file.daemons);
    file.daemons.retain(|e| e.daemon_id != entry.daemon_id);
    file.daemons.push(entry.clone());
    file.version = REGISTRY_VERSION;
    write_at(path, &file)
}

/// Removes this daemon's entry. Best-effort: called on clean shutdown, but a
/// SIGKILL or a panic skips it, which is exactly why pruning exists.
pub fn unregister(daemon_id: &str) -> Result<()> {
    let path = registry_path()?;
    unregister_at(&path, daemon_id)
}

/// `unregister`, with an explicit path.
pub fn unregister_at(path: &Path, daemon_id: &str) -> Result<()> {
    let mut file = read_at(path);
    let before = file.daemons.len();
    file.daemons.retain(|e| e.daemon_id != daemon_id);
    if file.daemons.len() == before {
        return Ok(());
    }
    write_at(path, &file)
}

/// Every registered daemon that still answers on its port, pruning the rest and
/// rewriting the file when anything was dropped.
pub fn read_live() -> Result<Vec<Entry>> {
    let path = registry_path()?;
    read_live_at(&path)
}

/// `read_live`, with an explicit path.
pub fn read_live_at(path: &Path) -> Result<Vec<Entry>> {
    let file = read_at(path);
    let before = file.daemons.len();
    let live = prune(file.daemons);
    if live.len() != before {
        let pruned = RegistryFile {
            version: REGISTRY_VERSION,
            daemons: live.clone(),
        };
        // A failed prune-write is not fatal: the caller still gets the correct
        // live list, the stale rows just linger for the next run to clean.
        if let Err(e) = write_at(path, &pruned) {
            tracing::warn!(error = ?e, "could not rewrite daemons.json after pruning");
        }
    }
    Ok(live)
}

/// Builds the entry describing the currently-running daemon.
#[must_use]
pub fn entry_for(
    project_root: &Path,
    project_name: &str,
    port: u16,
    daemon_version: &str,
) -> Entry {
    Entry {
        daemon_id: daemon_id_for(project_root),
        pid: std::process::id(),
        port,
        project_root: normalize_root_for_display(project_root),
        project_name: project_name.to_owned(),
        daemon_version: daemon_version.to_owned(),
        started_at: now_secs(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id_root: &str, port: u16) -> Entry {
        Entry {
            daemon_id: daemon_id_for(Path::new(id_root)),
            pid: 1234,
            port,
            project_root: id_root.to_owned(),
            project_name: "P".to_owned(),
            daemon_version: "0.5.0".to_owned(),
            started_at: 1_700_000_000,
        }
    }

    #[test]
    fn daemon_id_is_stable_and_distinct_per_root() {
        let a = daemon_id_for(Path::new("/projects/alpha"));
        let b = daemon_id_for(Path::new("/projects/beta"));
        assert_eq!(
            a,
            daemon_id_for(Path::new("/projects/alpha")),
            "the same root must always hash to the same id — the plugin keys its \
             stored auth token and remembered picker choice on this"
        );
        assert_ne!(a, b, "different projects must not share an id");
        assert_eq!(a.len(), DAEMON_ID_HEX_LEN);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn daemon_id_ignores_case_so_windows_roots_do_not_split() {
        assert_eq!(
            daemon_id_for(Path::new(r"C:\Dev\Game")),
            daemon_id_for(Path::new(r"c:\dev\game")),
            "Windows paths are case-insensitive; the same project must not get two ids"
        );
    }

    #[test]
    fn normalize_strips_the_windows_verbatim_prefix() {
        // `fs::canonicalize` hands back this shape on Windows. The extension
        // compares against VS Code's plain path, so the prefix has to go or the
        // match silently fails and a duplicate daemon gets spawned.
        assert_eq!(
            normalize_root_for_display(Path::new(r"\\?\C:\Users\me\Game")),
            r"C:\Users\me\Game"
        );
        assert_eq!(
            normalize_root_for_display(Path::new(r"\\?\UNC\server\share\Game")),
            r"\\server\share\Game"
        );
        // A path without the prefix is untouched.
        assert_eq!(
            normalize_root_for_display(Path::new(r"C:\Users\me\Game")),
            r"C:\Users\me\Game"
        );
        assert_eq!(
            normalize_root_for_display(Path::new("/home/me/game")),
            "/home/me/game"
        );
    }

    #[test]
    fn verbatim_and_plain_roots_share_one_id() {
        // The daemon canonicalizes (verbatim on Windows) while the extension
        // holds the plain form. Both must resolve to the same daemon.
        assert_eq!(
            daemon_id_for(Path::new(r"\\?\C:\Users\me\Game")),
            daemon_id_for(Path::new(r"C:\Users\me\Game")),
        );
    }

    #[test]
    fn register_round_trips_and_replaces_rather_than_duplicates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemons.json");

        // A live listener so the entry survives pruning.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();

        let mut e = entry("/projects/alpha", port);
        register_at(&path, &e).expect("register");
        let live = read_live_at(&path).expect("read");
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].port, port);

        // A restart of the SAME project must update its row, not append one.
        e.started_at += 60;
        register_at(&path, &e).expect("re-register");
        let live = read_live_at(&path).expect("read");
        assert_eq!(live.len(), 1, "a restart must not duplicate the project's row");
        assert_eq!(live[0].started_at, e.started_at);
    }

    #[test]
    fn dead_entries_are_pruned_on_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemons.json");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let live_port = listener.local_addr().expect("addr").port();

        // Bind and immediately drop, so this port is (almost certainly) dead.
        let dead_port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };

        register_at(&path, &entry("/projects/alive", live_port)).expect("register alive");
        register_at(&path, &entry("/projects/dead", dead_port)).expect("register dead");

        let live = read_live_at(&path).expect("read");
        assert_eq!(live.len(), 1, "the orphaned entry must be pruned");
        assert_eq!(live[0].port, live_port);

        // The prune is persisted, not just filtered in memory.
        let reread: RegistryFile =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read file"))
                .expect("parse");
        assert_eq!(reread.daemons.len(), 1);
    }

    #[test]
    fn unregister_removes_only_the_named_daemon() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemons.json");
        let l1 = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let l2 = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let p1 = l1.local_addr().expect("addr").port();
        let p2 = l2.local_addr().expect("addr").port();

        register_at(&path, &entry("/projects/one", p1)).expect("register one");
        register_at(&path, &entry("/projects/two", p2)).expect("register two");

        unregister_at(&path, &daemon_id_for(Path::new("/projects/one"))).expect("unregister");
        let live = read_live_at(&path).expect("read");
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].port, p2);
    }

    #[test]
    fn a_corrupt_or_future_registry_is_treated_as_empty_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemons.json");

        std::fs::write(&path, "{ not json at all").expect("write garbage");
        assert!(
            read_live_at(&path).expect("must not fail on garbage").is_empty(),
            "an unparseable registry must degrade to empty, never abort startup"
        );

        std::fs::write(&path, r#"{"version":9999,"daemons":[]}"#).expect("write future");
        assert!(
            read_live_at(&path).expect("must not fail on a future version").is_empty(),
            "a newer schema must be ignored rather than misparsed"
        );

        // And a daemon can still register over either of them.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        register_at(&path, &entry("/projects/x", port)).expect("register over a bad file");
        assert_eq!(read_live_at(&path).expect("read").len(), 1);
    }

    #[test]
    fn missing_registry_reads_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nope").join("daemons.json");
        assert!(read_live_at(&path).expect("missing is not an error").is_empty());
    }

    #[test]
    fn entry_for_normalizes_the_root_it_records() {
        let e = entry_for(Path::new(r"\\?\C:\Users\me\Game"), "Game", 34872, "0.5.0");
        assert_eq!(e.project_root, r"C:\Users\me\Game");
        assert_eq!(e.daemon_id, daemon_id_for(Path::new(r"C:\Users\me\Game")));
        assert_eq!(e.port, 34872);
        assert_eq!(e.pid, std::process::id());
    }

    #[test]
    fn no_temp_files_linger_after_a_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemons.json");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        register_at(&path, &entry("/projects/tmp", port)).expect("register");

        let stray: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(stray.is_empty(), "temp files left behind: {stray:?}");
    }
}
