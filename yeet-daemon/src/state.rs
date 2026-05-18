use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tokio::sync::{RwLock, mpsc};

use crate::merge::FileConflict;
use crate::project::Project;
use crate::protocol::{SerializedProperty, ServerMsg, classify};
use crate::tree::{self, Tree, TreeEntry};

const SESSION_FILE: &str = ".yeet/session.json";

/// Cap on the per-session delta buffer. When pushing past this point we
/// rotate `session_id`, which forces the next reconnect to fall back to a
/// full handshake instead of trying to resume from a buffer that's missing
/// frames. 1000 events is enough for ~hours of typical edits.
const MAX_PENDING_DELTAS: usize = 1000;

/// How long the daemon remembers that it just did a `fs::rename`, so the
/// matching `notify::Remove(old) + Create(new)` pair from the filesystem
/// watcher can be suppressed and not echoed back to the plugin. Sized to
/// cover the watcher's 100 ms debounce window plus channel slack.
const RENAME_ECHO_TTL: Duration = Duration::from_millis(500);

/// A `tree_fs` entry that just disappeared from the watcher's point of
/// view but might be the source half of a rename. Held briefly so a
/// matching `Touched` (with the same `sha256`) can promote the pair into
/// a `ServerMsg::FileRenamed` instead of a destructive delete + create.
#[derive(Debug, Clone)]
pub struct FsRemovedPending {
    pub path: String,
    pub entry: TreeEntry,
    pub meta: FileMeta,
    pub since: Instant,
}

/// How the file encoded line endings on disk. We canonicalize to LF internally;
/// this is kept per-file so we can write back in the same flavor the file was
/// originally in, rather than forcing one on the user.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

#[derive(Debug, Clone, Copy)]
pub struct FileMeta {
    pub line_ending: LineEnding,
    pub has_bom: bool,
}

impl FileMeta {
    /// Reasonable default for files we are about to create fresh — the user
    /// hasn't established a flavor preference yet, so we pick the portable one.
    pub const fn default_for_new_file() -> Self {
        Self {
            line_ending: LineEnding::Lf,
            has_bom: false,
        }
    }
}

#[derive(Debug)]
pub struct ProjectState {
    pub root: PathBuf,
    pub project: Project,
    /// What the plugin last told us about the Studio `DataModel`.
    pub tree_studio: Tree,
    /// What the filesystem currently holds.
    pub tree_fs: Tree,
    /// Last state confirmed by both sides; persisted to `.yeet/base-tree.msgpack`.
    pub tree_base: Tree,
    /// Line-ending/BOM flavor per path, mirrored from disk. Paths missing
    /// from this map get `FileMeta::default_for_new_file` when we have to
    /// write something new.
    pub meta: HashMap<String, FileMeta>,
    /// Files currently waiting for a `ConflictResolved` from the plugin.
    pub pending_conflicts: HashMap<String, FileConflict>,
    /// When true, echo-detection decisions are promoted from trace-level to
    /// info-level so they show up without a custom `RUST_LOG` filter.
    pub debug_echo: bool,
    /// Channel of the currently-registered extension client, if any. Last
    /// extension to send `Hello { role = "extension" }` wins. Cleared on
    /// disconnect (or on the next failed send, whichever comes first).
    pub extension_tx: Option<mpsc::UnboundedSender<ServerMsg>>,
    /// Daemon-assigned identifier for this run's "session". Persisted to
    /// `.yeet/session.json` so it survives a daemon crash + restart only when
    /// no buffer overflow occurred. Plugins echo it back in `Hello.session_id`
    /// to skip the bootstrap path on reconnect.
    pub session_id: String,
    /// Monotonic counter incremented on every `rotate_session_id`. Surfaced
    /// in logs and the audit trail so a post-mortem can spot rotation
    /// pressure (e.g. "session rotated 50 times in an hour" → broadcast
    /// channel sized too small for the project).
    pub rotation_count: u64,
    /// Set when the watcher observes a change to `default.project.json`
    /// after startup. The daemon doesn't hot-reload the project file (path
    /// mappings are baked into `mapping_roots_canonical` at bootstrap and
    /// changing them mid-flight is a much larger refactor), but it MUST
    /// stop pruning empty directories — a freshly-added `$path` mount with
    /// no files yet would otherwise be deleted by the next aggressive
    /// sweep. Cleared only by daemon restart.
    pub project_dirty: bool,
    /// Set by `--dry-run` at startup. When true, every mutation path
    /// (write_to_fs, delete_from_fs, push_to_studio, delete_on_studio)
    /// records its intended change to the audit log and then returns
    /// without touching disk, Studio, or in-memory tree state. The
    /// daemon never converges in dry-run mode — every reconcile cycle
    /// rediscovers the same divergences and re-emits the same "would
    /// have done X" audit entries — which is the point.
    pub dry_run: bool,
    /// Ring buffer of `ServerMsg` events broadcast since the daemon started.
    /// On plugin reconnect with matching `session_id`, drained into the
    /// connection in order. Bounded by `MAX_PENDING_DELTAS`; when full, the
    /// oldest entry is evicted **and** `session_id` is rotated so the plugin
    /// can't resume into a gap.
    pub pending_deltas: VecDeque<ServerMsg>,
    /// Last known attribute map per script path (E8). Keyed by the
    /// *associated tracked entry*, not the `.meta.json` path — e.g. for
    /// `src/Foo.meta.json` paired with `src/Foo.luau`, the key is
    /// `src/Foo.luau`. Used to diff on disk changes so we only emit
    /// `AttributesChanged` when something actually moved.
    pub meta_attributes: HashMap<String, HashMap<String, SerializedProperty>>,
    /// In-flight bulk-sync sessions, keyed by `request_id`. Inserted when
    /// the extension triggers `Yeet: Sync From *`, removed on the plugin's
    /// `BulkSyncConfirm` or `BulkSyncCancel`. The stored direction label
    /// lets the daemon know which wording to surface in logs on confirm.
    pub pending_bulk_sync: HashMap<String, String>,
    /// Canonicalized absolute paths of every directory the project's tree
    /// declares as a `$path` mount point. Empty-folder cleanup
    /// (`prune_empty_dirs`, `prune_all_empty_subdirs` in main.rs) refuses
    /// to delete any of these even when they end up empty — they're
    /// first-class project structure and Rojo expects them to exist for
    /// the mapping to keep resolving.
    ///
    /// Mappings whose disk path doesn't exist at startup are skipped (with
    /// a `warn!`); they re-enter the list on the next bootstrap if the
    /// user creates the directory.
    pub mapping_roots_canonical: Vec<PathBuf>,
    /// Random hex token (256 bits, 64 hex chars) generated at daemon
    /// startup and written to `<root>/.yeet/auth-token` (perm 0o600).
    /// Clients prove they have local-FS read access by reading the
    /// file and echoing the token in `Hello.auth_token`. Plugin-side
    /// clients that lack file access go through the `AuthChallenge` →
    /// `PairRequest` dance, which validates a `<root>/yeet-pairing.txt`
    /// breadcrumb the extension writes for them.
    ///
    /// Held as a String (not zeroized on drop) because the token is
    /// also reachable from the on-disk file with the same lifetime —
    /// process-memory zeroization gives no extra protection here.
    pub auth_token: String,
    /// Suppresses the watcher echo from a daemon-initiated `fs::rename`.
    /// Populated immediately before the rename, consumed by `handle_fs_event`
    /// when the `Remove(old) + Touched(new)` pair surfaces. Entries older than
    /// `RENAME_ECHO_TTL` are dropped opportunistically — no background sweep.
    pub recently_renamed: HashMap<(String, String), Instant>,
    /// Paths currently flagged as colliding (two Studio instances resolving
    /// to the same project-relative path). Sync for these paths is paused
    /// until the plugin notifies a rename that clears the collision. Used
    /// by the studio-side mutation handlers to reject incoming `FileChanged`
    /// / `FileRenamed` with `SyncErrorKind::NameCollisionPending`.
    pub pending_collisions: HashSet<String>,
    /// Recently observed filesystem removes, indexed by `sha256` of the
    /// content that disappeared. When a `Touched` event later carries the
    /// same hash, the pair is promoted to a `FileRenamed` instead of a
    /// destructive Delete+Create — preserves Studio-side state
    /// (attributes, tags, non-script children) across IDE-side renames.
    pub fs_removed_pending: HashMap<String, FsRemovedPending>,
}

pub type SharedState = Arc<RwLock<ProjectState>>;

impl ProjectState {
    pub fn bootstrap(
        root: &Path,
        project: Project,
        debug_echo: bool,
        dry_run: bool,
    ) -> Result<Self> {
        // The session id from disk is only meaningful while the daemon's
        // event buffer is intact — and that buffer lives in memory, so a
        // restart wipes it. We always rotate to a fresh id on bootstrap;
        // the persisted file is more of a debugging breadcrumb than a
        // resume primitive across daemon restarts.
        let session_id = uuid::Uuid::new_v4().to_string();
        // Pre-compute the canonical absolute path of every `$path` mount.
        // We canonicalize so prune-time `strip_prefix` checks compare like
        // shapes (Windows UNC vs non-UNC bites otherwise). Missing mounts
        // are skipped — `rescan_fs` already warns about them.
        let mapping_roots_canonical: Vec<PathBuf> = project
            .path_mappings()
            .into_iter()
            .filter_map(|(_, rel)| {
                let abs = root.join(&rel);
                match std::fs::canonicalize(&abs) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::warn!(
                            path = %abs.display(),
                            error = ?e,
                            "mapping root not on disk; pruning will not protect it"
                        );
                        None
                    }
                }
            })
            .collect();
        // Auth token: 256 bits of entropy (two stacked v4 UUIDs).
        // `Uuid::new_v4()` uses `getrandom` under the hood, which on
        // every supported OS pulls from a CSPRNG (Windows BCrypt,
        // Linux /dev/urandom, macOS arc4random). Two UUIDs give ~244
        // bits of randomness which clears every realistic
        // brute-force threshold. Persisted to disk by the caller in
        // main.rs (so this constructor stays platform-neutral and the
        // file path remains a `main.rs` concern).
        let auth_token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let mut state = Self {
            root: root.to_path_buf(),
            project,
            tree_studio: Tree::new(),
            tree_fs: Tree::new(),
            tree_base: Tree::new(),
            meta: HashMap::new(),
            pending_conflicts: HashMap::new(),
            debug_echo,
            extension_tx: None,
            session_id: session_id.clone(),
            rotation_count: 0,
            project_dirty: false,
            dry_run,
            pending_deltas: VecDeque::new(),
            meta_attributes: HashMap::new(),
            pending_bulk_sync: HashMap::new(),
            mapping_roots_canonical,
            auth_token,
            recently_renamed: HashMap::new(),
            pending_collisions: HashSet::new(),
            fs_removed_pending: HashMap::new(),
        };
        state.rescan_fs()?;
        if let Some(persisted) = tree::load_base_tree(root)? {
            state.tree_base = persisted;
        } else {
            // First run: trust whatever's on disk as the canonical base.
            state.tree_base.clone_from(&state.tree_fs);
            tree::save_base_tree(root, &state.tree_base)
                .context("initial base-tree write")?;
        }
        if let Err(e) = persist_session_id(root, &session_id) {
            tracing::warn!(error = ?e, "could not persist session id");
        }
        Ok(state)
    }

    /// Appends `msg` to the delta ring. When at capacity, drops the oldest
    /// entry and rotates `session_id` so plugins reconnecting with the old
    /// id are forced into a full bootstrap instead of resuming with a gap.
    pub fn record_delta(&mut self, msg: ServerMsg) {
        if self.pending_deltas.len() >= MAX_PENDING_DELTAS {
            self.pending_deltas.pop_front();
            self.rotate_session_id("pending_deltas overflow");
        }
        self.pending_deltas.push_back(msg);
    }

    /// Drains the entire delta queue to the caller. Used on a successful
    /// resume so the plugin sees every event the daemon emitted while it
    /// was disconnected.
    pub fn drain_deltas(&mut self) -> Vec<ServerMsg> {
        self.pending_deltas.drain(..).collect()
    }

    /// Forgets the buffer and assigns a new id. Called when the buffer
    /// overflows or when a fresh handshake is in progress and old deltas
    /// would be misleading.
    ///
    /// Race analysis: this method takes `&mut self` and every caller goes
    /// through `state.write().await` on the surrounding `RwLock`, which
    /// serializes all rotations and resume-checks. There is no window in
    /// which a resume can read a partially-rotated state — the resume's
    /// `if guard.session_id == claimed` runs under the same write lock.
    /// The audit-trail entries below give post-mortem visibility when the
    /// plugin reports "lost state" — pairing a session-end timestamp to
    /// the rotation reason explains why a resume was rejected.
    pub fn rotate_session_id(&mut self, reason: &str) {
        let new_id = uuid::Uuid::new_v4().to_string();
        let old_id = std::mem::replace(&mut self.session_id, new_id.clone());
        self.pending_deltas.clear();
        self.rotation_count = self.rotation_count.saturating_add(1);
        tracing::info!(
            old = %old_id,
            new = %new_id,
            reason,
            count = self.rotation_count,
            "session id rotated"
        );
        if let Err(e) = persist_session_id(&self.root, &self.session_id) {
            tracing::warn!(error = ?e, "could not persist rotated session id");
        }
        // Audit so "session rotated; resume rejected" is reconstructable
        // from disk after the daemon has long since exited. `path` field
        // doubles as the rotation reason — slightly abusing the schema
        // but keeps the audit log a single uniform sequence.
        crate::audit::record(
            &self.root.clone(),
            &crate::audit::Entry {
                ts: crate::audit::now_rfc3339(),
                kind: crate::audit::Kind::SessionRotated,
                path: reason,
                sha_before: Some(&old_id),
                sha_after: Some(&new_id),
                session_id: &new_id,
                note: None,
            },
        );
    }

    /// Walks every `$path`-mapped directory and seeds `tree_fs` + `meta`.
    /// `tree_base` and `tree_studio` are left untouched.
    fn rescan_fs(&mut self) -> Result<()> {
        self.tree_fs.clear();
        self.meta.clear();
        self.meta_attributes.clear();
        for (_, rel_dir) in self.project.path_mappings() {
            let dir = self.root.join(&rel_dir);
            if !dir.exists() {
                tracing::warn!(path = %dir.display(), "project $path not found on disk, skipping");
                continue;
            }
            for entry in walkdir::WalkDir::new(&dir) {
                let entry = entry.context("walkdir")?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                if is_meta_file(path) {
                    // Capture the starting state so the first watcher tick
                    // that changes a meta file can diff against something.
                    let _ = self.ingest_meta_file(path)?;
                } else {
                    let _ = self.ingest_fs_file(path)?;
                }
            }
        }
        Ok(())
    }

    /// Reads a file, classifies it, stores it in `tree_fs` + `meta`, and
    /// returns the project-relative path. Returns `Ok(None)` if the file is
    /// not a supported source or lives outside every mapped `$path`.
    pub fn ingest_fs_file(&mut self, abs: &Path) -> Result<Option<String>> {
        let Some(rel) = self.relative(abs) else {
            return Ok(None);
        };
        let file_name = abs
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let Some((_name, kind)) = classify(file_name) else {
            return Ok(None);
        };

        let raw = std::fs::read_to_string(abs)
            .with_context(|| format!("read {}", abs.display()))?;
        let (content, meta) = normalize_from_disk(&raw);
        let sha256 = sha256_hex(content.as_bytes());
        self.tree_fs.insert(
            rel.clone(),
            TreeEntry {
                kind,
                content,
                sha256,
            },
        );
        self.meta.insert(rel.clone(), meta);
        Ok(Some(rel))
    }

    /// Removes the entry for `abs` from `tree_fs` + `meta`. Returns the
    /// path we removed, if any.
    pub fn forget_fs_file(&mut self, abs: &Path) -> Option<String> {
        let rel = self.relative(abs)?;
        self.meta.remove(&rel);
        self.meta_attributes.remove(&rel);
        self.tree_fs.remove(&rel).map(|_| rel)
    }

    /// Parses `abs` as a `.meta.json`, resolves the *tracked-entry path*
    /// (e.g. the paired script), stores the attributes map, and returns
    /// `(tracked_path, new_attributes)`. Returns `Ok(None)` when the file
    /// does not pair with any tracked entry (for example `init.meta.json`
    /// in a folder with no scripts — tracked instance maps don't cover
    /// folders yet, so we simply ignore the meta in that case).
    pub fn ingest_meta_file(
        &mut self,
        abs: &Path,
    ) -> Result<Option<(String, HashMap<String, SerializedProperty>)>> {
        let Some((tracked_rel, attributes)) = self.resolve_meta(abs)? else {
            return Ok(None);
        };
        self.meta_attributes.insert(tracked_rel.clone(), attributes.clone());
        Ok(Some((tracked_rel, attributes)))
    }

    /// Drops the attribute record for the entry associated with `abs` and
    /// returns the tracked-entry path it was keyed under. When the meta
    /// file was never pairable (unknown sibling script), returns `None`.
    pub fn forget_meta_file(&mut self, abs: &Path) -> Option<String> {
        let tracked_rel = self.tracked_path_for_meta(abs)?;
        self.meta_attributes.remove(&tracked_rel);
        Some(tracked_rel)
    }

    fn resolve_meta(
        &self,
        abs: &Path,
    ) -> Result<Option<(String, HashMap<String, SerializedProperty>)>> {
        let Some(tracked_rel) = self.tracked_path_for_meta(abs) else {
            return Ok(None);
        };
        let raw = std::fs::read_to_string(abs)
            .with_context(|| format!("read {}", abs.display()))?;
        let attributes = parse_meta_attributes(&raw)
            .with_context(|| format!("parse {}", abs.display()))?;
        Ok(Some((tracked_rel, attributes)))
    }

    /// Maps a `.meta.json` on disk back to the project-relative *tracked*
    /// entry it describes — currently only a paired sibling script, since
    /// that's what the plugin's Applier can resolve. `Foo.meta.json` pairs
    /// with `Foo.luau` / `Foo.server.luau` / `Foo.client.luau` in the
    /// same directory; `init.meta.json` pairs with `init.luau` in its
    /// directory. Non-script instance meta (e.g. `init.meta.json` in a
    /// scriptless folder) returns `None` and the event is dropped — we
    /// can't apply it without a broader instance-path tracker.
    fn tracked_path_for_meta(&self, abs: &Path) -> Option<String> {
        let file_name = abs.file_name().and_then(|s| s.to_str())?;
        let stem = file_name.strip_suffix(".meta.json")?;
        if stem.is_empty() {
            return None;
        }
        let parent = abs.parent()?;
        // Probe each supported script suffix in order. We short-circuit on
        // the first sibling that exists and classifies as a source file.
        let suffixes = [
            "luau",
            "lua",
            "server.luau",
            "server.lua",
            "client.luau",
            "client.lua",
        ];
        for suffix in suffixes {
            let candidate = parent.join(format!("{stem}.{suffix}"));
            if !candidate.is_file() {
                continue;
            }
            let candidate_name = candidate.file_name()?.to_str()?;
            if classify(candidate_name).is_some() {
                return self.relative(&candidate);
            }
        }
        None
    }

    /// Converts an absolute path into a project-relative, forward-slashed path,
    /// or `None` if the path is outside the project root.
    pub fn relative(&self, abs: &Path) -> Option<String> {
        let rel = abs.strip_prefix(&self.root).ok()?;
        let mut s = String::with_capacity(rel.as_os_str().len());
        for (i, comp) in rel.components().enumerate() {
            if i > 0 {
                s.push('/');
            }
            s.push_str(comp.as_os_str().to_str()?);
        }
        Some(s)
    }

    pub fn is_under_mapping(&self, rel: &str) -> bool {
        // Case-insensitive: Windows and macOS filesystems are case-preserving
        // but case-insensitive. If `default.project.json` declares `"src"` and
        // the disk directory is actually `Src/` (e.g. user renamed it via
        // explorer, or git checked out a branch with different casing), the
        // file lives at the same physical location but the literal string
        // comparison would drop it. Linux is case-sensitive so distinct dirs
        // `src` and `Src` could exist — but no real project does that, and
        // accepting both is the friendlier failure mode.
        let rel_lower = rel.to_ascii_lowercase();
        self.project.path_mappings().iter().any(|(_, dir)| {
            let dir_lower = dir
                .to_string_lossy()
                .replace('\\', "/")
                .to_ascii_lowercase();
            rel_lower == dir_lower || rel_lower.starts_with(&format!("{dir_lower}/"))
        })
    }

    /// Records that the daemon is about to perform a `fs::rename` from
    /// `old_path` to `new_path`. The watcher will emit `Remove(old) +
    /// Touched(new)` shortly after; `consume_rename_echo` swallows that
    /// pair so the daemon does not re-broadcast its own work.
    pub fn note_rename_echo(&mut self, old_path: String, new_path: String) {
        self.prune_rename_echoes();
        self.recently_renamed
            .insert((old_path, new_path), Instant::now());
    }

    /// Checks whether a watcher event for `path` (either side of a recent
    /// rename) should be suppressed. Removes the matching entry so the
    /// echo is one-shot. Stale entries are pruned opportunistically.
    pub fn consume_rename_echo(&mut self, path: &str) -> bool {
        self.prune_rename_echoes();
        let hit = self
            .recently_renamed
            .keys()
            .find(|(old, new)| old == path || new == path)
            .cloned();
        if let Some(key) = hit {
            self.recently_renamed.remove(&key);
            true
        } else {
            false
        }
    }

    fn prune_rename_echoes(&mut self) {
        let now = Instant::now();
        self.recently_renamed
            .retain(|_, ts| now.duration_since(*ts) < RENAME_ECHO_TTL);
    }

    /// Convenience accessor: meta for `path`, or a sensible default if the
    /// file has never been on disk (delete-vs-edit resolution, for instance).
    pub fn meta_for(&self, path: &str) -> FileMeta {
        self.meta
            .get(path)
            .copied()
            .unwrap_or_else(FileMeta::default_for_new_file)
    }
}

/// Strips a UTF-8 BOM and collapses CRLF to LF, recording what was there so
/// we can round-trip when writing back.
pub fn normalize_from_disk(raw: &str) -> (String, FileMeta) {
    let (body, has_bom) = match raw.strip_prefix('\u{feff}') {
        Some(rest) => (rest, true),
        None => (raw, false),
    };
    let line_ending = if body.contains("\r\n") {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    };
    let content = match line_ending {
        LineEnding::Crlf => body.replace("\r\n", "\n"),
        LineEnding::Lf => body.to_owned(),
    };
    (
        content,
        FileMeta {
            line_ending,
            has_bom,
        },
    )
}

/// Inverse of `normalize_from_disk`: re-applies BOM + the file's original line
/// ending flavor to canonical (LF, no BOM) content.
pub fn encode_for_disk(canonical: &str, meta: FileMeta) -> String {
    let body = match meta.line_ending {
        LineEnding::Crlf => canonical.replace('\n', "\r\n"),
        LineEnding::Lf => canonical.to_owned(),
    };
    if meta.has_bom {
        let mut out = String::with_capacity(body.len() + 3);
        out.push('\u{feff}');
        out.push_str(&body);
        out
    } else {
        body
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

/// Returns true for files ending in `.meta.json` (case-sensitive, matches
/// what `syncback.rs` writes). Used by FS event routing to split the
/// meta-attributes path from the source-file path.
/// Resolves a project-relative path to an absolute `PathBuf` under `root`,
/// rejecting any attempt to escape the project sandbox. Accepts only
/// `Normal` (filename) and `CurDir` (`.`) components — absolute paths,
/// parent-dir traversals (`..`), Windows prefix components, and embedded
/// NUL bytes are all rejected.
///
/// This is the single chokepoint every client-supplied `path` must go
/// through before it reaches `std::fs`. A malicious or buggy client
/// sending `../../etc/passwd` gets a hard error here instead of having
/// the daemon happily write outside the project tree.
pub fn resolve_inside(root: &Path, rel: &str) -> Result<PathBuf> {
    use std::path::Component;
    if rel.as_bytes().contains(&0) {
        anyhow::bail!("rejected path with NUL byte: {rel:?}");
    }
    if rel.is_empty() {
        anyhow::bail!("rejected empty path");
    }
    let requested = PathBuf::from(rel);
    let mut out = root.to_path_buf();
    for component in requested.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                anyhow::bail!("rejected traversal (..) in path: {rel}");
            }
            Component::RootDir => {
                anyhow::bail!("rejected absolute path: {rel}");
            }
            Component::Prefix(_) => {
                anyhow::bail!("rejected Windows prefix in path: {rel}");
            }
        }
    }
    // Defensive: `root.join(…)` with only Normal/CurDir components should
    // already stay inside `root`, but a canonicalized check catches
    // symlinks inside the tree that point elsewhere. We can't canonicalize
    // `out` because it may not exist yet (writes create new files), so we
    // only canonicalize root and assert `out`'s ancestor chain includes
    // it. `strip_prefix` is the cheap way to assert containment.
    let canonical_root = std::fs::canonicalize(root)
        .with_context(|| format!("canonicalize root {}", root.display()))?;
    let check_base = canonical_root.clone();
    // For the target, walk up to the first existing ancestor and
    // canonicalize that — catches any symlink shenanigans anywhere along
    // the chain while tolerating not-yet-created leaves.
    let mut existing = out.as_path();
    loop {
        if existing.exists() {
            break;
        }
        match existing.parent() {
            Some(p) if p != existing => existing = p,
            _ => break,
        }
    }
    if existing.as_os_str().is_empty() {
        return Ok(out);
    }
    let canonical_existing = std::fs::canonicalize(existing)
        .with_context(|| format!("canonicalize {}", existing.display()))?;
    if canonical_existing.strip_prefix(&check_base).is_err() {
        anyhow::bail!(
            "resolved path {} escapes project root {}",
            canonical_existing.display(),
            canonical_root.display()
        );
    }
    Ok(out)
}

pub fn is_meta_file(abs: &Path) -> bool {
    abs.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|name| name.ends_with(".meta.json"))
}

/// Extracts the `attributes` block from a `.meta.json` body. Missing blocks
/// parse as empty — an instance with no attributes is indistinguishable
/// from "user removed the `attributes` key", which is the behavior we
/// want: both cases should apply as `SetAttribute(name, nil)` for any
/// previously-known attribute.
pub fn parse_meta_attributes(body: &str) -> Result<HashMap<String, SerializedProperty>> {
    #[derive(serde::Deserialize)]
    struct PartialMeta {
        #[serde(default)]
        attributes: Option<HashMap<String, SerializedProperty>>,
    }
    let parsed: PartialMeta = serde_json::from_str(body).context("meta.json deserialize")?;
    Ok(parsed.attributes.unwrap_or_default())
}

/// Writes `id` to `<root>/.yeet/session.json`. Failure is non-fatal — the
/// session id only matters for the current daemon process; the file is a
/// breadcrumb for `rg` debugging and tooling that wants to verify a
/// reconnect attached to the right backend.
fn persist_session_id(root: &Path, id: &str) -> Result<()> {
    let path = root.join(SESSION_FILE);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    }
    let body = format!("{{\"session_id\":\"{id}\"}}\n");
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_lf_no_bom() {
        let raw = "local x = 1\nlocal y = 2\n";
        let (content, meta) = normalize_from_disk(raw);
        assert_eq!(content, raw);
        assert_eq!(meta.line_ending, LineEnding::Lf);
        assert!(!meta.has_bom);
        assert_eq!(encode_for_disk(&content, meta), raw);
    }

    #[test]
    fn roundtrip_crlf_with_bom() {
        let raw = "\u{feff}local x = 1\r\nlocal y = 2\r\n";
        let (content, meta) = normalize_from_disk(raw);
        assert_eq!(content, "local x = 1\nlocal y = 2\n");
        assert_eq!(meta.line_ending, LineEnding::Crlf);
        assert!(meta.has_bom);
        assert_eq!(encode_for_disk(&content, meta), raw);
    }

    #[test]
    fn roundtrip_crlf_no_bom() {
        let raw = "a\r\nb";
        let (content, meta) = normalize_from_disk(raw);
        assert_eq!(content, "a\nb");
        assert_eq!(meta.line_ending, LineEnding::Crlf);
        assert!(!meta.has_bom);
        assert_eq!(encode_for_disk(&content, meta), raw);
    }

    #[test]
    fn is_meta_file_matches_suffix() {
        assert!(is_meta_file(Path::new("src/Foo.meta.json")));
        assert!(is_meta_file(Path::new("init.meta.json")));
        assert!(!is_meta_file(Path::new("Foo.luau")));
        assert!(!is_meta_file(Path::new("notes.json")));
    }

    #[test]
    fn parse_meta_attributes_picks_only_attributes_block() {
        let body = r#"{
            "className": "ModuleScript",
            "properties": { "Name": { "type": "string", "value": "Hi" } },
            "attributes": {
                "MyTag": { "type": "string", "value": "hello" },
                "Count": { "type": "number", "value": 42 }
            }
        }"#;
        let attrs = parse_meta_attributes(body).expect("parse");
        assert_eq!(attrs.len(), 2);
        match attrs.get("MyTag").expect("tag present") {
            SerializedProperty::String(s) => assert_eq!(s, "hello"),
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn parse_meta_attributes_handles_missing_block() {
        let body = r#"{"className":"Folder"}"#;
        let attrs = parse_meta_attributes(body).expect("parse");
        assert!(attrs.is_empty());
    }

    // ─── resolve_inside: path traversal & sandbox guard ───────────────────

    fn make_sandbox() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn resolve_inside_accepts_simple_relative() {
        let sandbox = make_sandbox();
        let resolved = resolve_inside(sandbox.path(), "src/Foo.luau").expect("ok");
        assert!(resolved.starts_with(sandbox.path()));
        assert!(resolved.ends_with("src/Foo.luau") || resolved.ends_with("src\\Foo.luau"));
    }

    #[test]
    fn resolve_inside_accepts_curdir() {
        let sandbox = make_sandbox();
        let resolved = resolve_inside(sandbox.path(), "./a/b").expect("ok");
        assert!(resolved.starts_with(sandbox.path()));
    }

    #[test]
    fn resolve_inside_rejects_parent_traversal_simple() {
        let sandbox = make_sandbox();
        assert!(resolve_inside(sandbox.path(), "../etc/passwd").is_err());
    }

    #[test]
    fn resolve_inside_rejects_parent_traversal_nested() {
        let sandbox = make_sandbox();
        assert!(resolve_inside(sandbox.path(), "a/../../b").is_err());
    }

    #[test]
    fn resolve_inside_rejects_parent_only() {
        let sandbox = make_sandbox();
        assert!(resolve_inside(sandbox.path(), "..").is_err());
    }

    #[test]
    fn resolve_inside_rejects_nul_byte() {
        let sandbox = make_sandbox();
        assert!(resolve_inside(sandbox.path(), "a\0b").is_err());
    }

    #[test]
    fn resolve_inside_rejects_empty() {
        let sandbox = make_sandbox();
        assert!(resolve_inside(sandbox.path(), "").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_inside_rejects_absolute_unix() {
        let sandbox = make_sandbox();
        assert!(resolve_inside(sandbox.path(), "/etc/passwd").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn resolve_inside_rejects_absolute_windows_drive() {
        let sandbox = make_sandbox();
        assert!(resolve_inside(sandbox.path(), "C:\\Windows\\System32").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn resolve_inside_rejects_absolute_windows_unc() {
        let sandbox = make_sandbox();
        assert!(resolve_inside(sandbox.path(), "\\\\server\\share\\foo").is_err());
    }

    // Symlink-based escape: create a symlink inside the sandbox that
    // points to /tmp; ensure the canonicalized-ancestor check inside
    // `resolve_inside` rejects requests that traverse through it.
    #[cfg(unix)]
    #[test]
    fn resolve_inside_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let sandbox = make_sandbox();
        let link = sandbox.path().join("escape");
        symlink("/tmp", &link).expect("create symlink");
        // The link itself canonicalizes to /tmp which is outside root.
        assert!(resolve_inside(sandbox.path(), "escape").is_err());
    }
}
