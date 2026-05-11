use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::merge::{ConflictHunk, ConflictKind, HunkResolution};
use crate::project::Project;

/// Script instance classes we know how to materialize in Phase 1.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptKind {
    ModuleScript,
    Script,
    LocalScript,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSnapshot {
    pub path: String,
    pub kind: ScriptKind,
    pub content: String,
    pub sha256: String,
}

/// What the plugin knows about one file at the moment of Hello. Carrying the
/// full content means the daemon can run its 3-way merge without a follow-up
/// round-trip. At steady state the plugin's content matches `Tree_Base` so
/// this snapshot compresses trivially on the wire.
#[derive(Debug, Clone, Deserialize)]
pub struct StudioFileSnapshot {
    pub path: String,
    pub kind: ScriptKind,
    pub content: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// First frame from any client. `role` distinguishes plugin from extension;
    /// missing/`null` defaults to `"plugin"` for backcompat with pre-E3 plugins.
    /// `studio_snapshot` is only meaningful for `"plugin"` clients — for
    /// `"extension"` it should be empty (and is ignored by the daemon either way).
    /// On a cold plugin boot it's empty too; populated when the plugin reconnects
    /// with locally-edited scripts.
    Hello {
        version: String,
        #[serde(default)]
        studio_snapshot: Vec<StudioFileSnapshot>,
        #[serde(default)]
        role: Option<String>,
        /// When the plugin reconnects after a transient disconnect, it sends
        /// back the `session_id` it last received in `ProjectOpened` (E7).
        /// If the daemon's current session matches, it skips bootstrap and
        /// drains buffered events; otherwise it falls through to a full
        /// handshake.
        #[serde(default)]
        session_id: Option<String>,
        /// Opt-in: when true, the plugin promises to follow `ProjectOpened`
        /// with a `StudioSnapshotReport` listing every LuaSourceContainer it
        /// sees under a `$path`-mapped root. The daemon waits for that
        /// report, runs the bootstrap diff against `tree_fs`, and emits a
        /// `BulkSyncPreview` (direction = `Bootstrap`) so the user can pick
        /// per-file what should be created, overwritten, or deleted on each
        /// side. When false (pre-E-this plugins), daemon falls back to the
        /// legacy cold-boot auto-apply path.
        #[serde(default)]
        request_bootstrap_preview: bool,
        /// Random hex token the daemon issued at startup (extension reads
        /// it from `<root>/.yeet/auth-token` and forwards it; plugin
        /// reads it from `plugin:GetSetting("yeet.authToken")` if it has
        /// paired before, else sends empty and goes through the
        /// AuthChallenge → PairRequest dance). Daemon does constant-time
        /// comparison against the token in state and bails the
        /// handshake if mismatched (with the auth-challenge fallback
        /// for empty plugin tokens, see daemon `handle_connection`).
        #[serde(default)]
        auth_token: Option<String>,
    },
    /// Second half of the auth dance: plugin sends this AFTER it
    /// received `AuthChallenge` and the user clicked "Pair" in the
    /// dock. Daemon checks for a valid `<root>/yeet-pairing.txt`
    /// breadcrumb (extension wrote it via `Yeet: Pair Studio`,
    /// timestamp <60s) and replies with `AuthGranted` + the auth
    /// token, OR `AuthRejected` if no breadcrumb is present. No
    /// fields needed in the request itself — the breadcrumb on disk
    /// is the proof.
    PairRequest {},
    /// Second half of the bootstrap handshake when `request_bootstrap_preview`
    /// was true. Plugin walks the DataModel under mapped `$path` roots, hashes
    /// each script's `Source`, and sends the results here so the daemon can
    /// diff against its `tree_fs` without guessing. Daemon replies with a
    /// `BulkSyncPreview { direction: Bootstrap }` describing the divergent
    /// files.
    StudioSnapshotReport {
        snapshot: Vec<StudioFileSnapshot>,
    },
    /// User edited a tracked script's `Source` in Studio, OR the plugin is
    /// echoing Studio's live `Source` back during bootstrap because the
    /// `TreeBuilder` detected a divergence from the `initial_files` snapshot.
    /// The `bootstrap_diverge` flag routes the latter through a 2-way
    /// compare (Studio vs disk only) instead of the usual 3-way merge — the
    /// user's intent in a fresh bootstrap is "show me both sides, let me
    /// pick", not "auto-apply whichever side disagrees with the base".
    FileChanged {
        path: String,
        content: String,
        sha256: String,
        #[serde(default)]
        bootstrap_diverge: bool,
    },
    /// User created a new script inside a `$path`-mapped directory in Studio.
    FileCreated {
        path: String,
        kind: ScriptKind,
        content: String,
        sha256: String,
    },
    /// User deleted a tracked script in Studio.
    FileDeleted {
        path: String,
    },
    /// User renamed, moved, or promoted/demoted a tracked script in Studio,
    /// changing its path while the same `LuaSourceContainer` instance lives
    /// on. The plugin emits this when its `SourceWatcher` detects a
    /// `Name`/`AncestryChanged` event, or when a child added/removed flips
    /// a leaf script into a `Foo/init.luau` (or back). `content` carries
    /// the current `Source` so the daemon doesn't need a follow-up
    /// `FileChanged` for renames that coincide with edits — the rename
    /// frame is authoritative for both fields.
    FileRenamed {
        old_path: String,
        new_path: String,
        kind: ScriptKind,
        content: String,
        sha256: String,
    },
    /// Plugin detected two `LuaSourceContainer` instances that resolve to
    /// the same project-relative path (two scripts with the same name under
    /// the same parent in Studio). Sync for that path is paused until one
    /// is renamed in Studio; this frame just informs the daemon so it can
    /// reject incoming edits for `path` with `SyncErrorKind::NameCollisionPending`
    /// and broadcast a `NameCollision` warning to the extension.
    NameCollision {
        path: String,
        sha256: String,
    },
    /// User picked resolutions in the conflict UI for one or more files.
    ConflictResolved {
        resolutions: Vec<FileResolution>,
    },
    /// User hand-built the merged content for `path` via the line-level
    /// merge picker (or another freeform editor surface). Carries the
    /// final file content directly instead of a per-hunk choice list.
    /// Daemon writes `content` to disk + Studio, removes the path from
    /// `pending_conflicts`, audits with `Kind::ConflictResolved` +
    /// `note: "manual"`. `sha256` is verified against the recomputed
    /// hash the same way `FileChanged` is — mismatch yields a
    /// `SyncError { kind: HashMismatch }`.
    ConflictResolvedManual {
        path: String,
        content: String,
        sha256: String,
    },
    /// User closed the conflict dock with N files still unresolved. Daemon
    /// must keep these in `pending_conflicts` so they re-surface on the next
    /// reconcile, instead of silently moving `tree_base` forward as if they
    /// had been resolved. Without this signal the daemon's view (3 of 5
    /// resolved) and the plugin's view (queue cleared) drift permanently.
    ConflictAbandoned {
        paths: Vec<String>,
    },
    /// Plugin is starting a reverse-bootstrap. Opens a new session keyed by
    /// `request_id`; subsequent `SyncbackChunk` frames will carry instances.
    SyncbackBegin {
        request_id: String,
        target_path: String,
        mode: SyncbackMode,
        #[serde(default = "default_true")]
        include_non_script: bool,
        #[serde(default)]
        include_binary: bool,
        #[serde(default)]
        template: SyncbackTemplate,
        /// Optional — when set, overrides `project.name` in the generated
        /// `default.project.json`. Defaults to the target directory name.
        #[serde(default)]
        project_name: Option<String>,
    },
    /// One page of instances for an in-flight syncback session. `seq` starts
    /// at 0 and increments monotonically; daemon replies with
    /// `SyncbackAck { seq }` after persisting each chunk.
    SyncbackChunk {
        request_id: String,
        seq: u32,
        instances: Vec<SerializedInstance>,
    },
    /// All instances sent; daemon should materialize and reply with
    /// `SyncbackComplete` or `SyncbackError`.
    SyncbackFinalize {
        request_id: String,
        total_seq: u32,
    },
    /// Reply from the extension to a `PickFolderRequest`. `path` is `None`
    /// when the user dismissed the dialog. The plugin never sends this — it
    /// is wired in E4 so the extension can satisfy a folder-picker request
    /// the plugin issued via the daemon.
    PickFolderResponse {
        request_id: String,
        #[serde(default)]
        path: Option<String>,
    },
    /// Plugin asking the daemon to pop the native folder-picker dialog via
    /// the registered extension. Daemon matches a response by `request_id`
    /// and broadcasts a `PickFolderResult` back. If no extension is registered
    /// the daemon answers immediately with `path = None` so the plugin
    /// doesn't hang.
    PickFolderPrompt {
        request_id: String,
        prompt: String,
    },
    /// Plugin asking the daemon to have the extension reopen a materialized
    /// project folder — the "Open in IDE" button after a completed syncback,
    /// when the user closed the auto-opened window and wants it back. Daemon
    /// just retransmits as `ServerMsg::OpenProjectRequest` to the registered
    /// extension; silently dropped if no extension is live.
    OpenProjectRequest {
        path: String,
    },
    /// Extension command `Yeet: Sync From Studio`. Daemon enumerates divergent
    /// paths between `tree_studio` and `tree_fs`, sends `BulkSyncPreview` to
    /// the plugin, and waits for the user to confirm or cancel before
    /// touching anything.
    BulkSyncFromStudioRequest {},
    /// Extension command `Yeet: Sync From Ide`. Same preview-then-confirm
    /// dance as `BulkSyncFromStudioRequest` with the direction label
    /// adjusted for plugin UI purposes.
    BulkSyncFromIdeRequest {},
    /// Plugin confirms a pending bulk sync identified by `request_id`. When
    /// `resolutions` is non-empty, the daemon applies exactly those per-file
    /// decisions and ignores the pending request's `direction`. When empty
    /// (legacy `Sync From Studio` / `Sync From Ide` extension commands), the
    /// daemon falls back to the stored direction and reconciles every path
    /// in bulk. `Bootstrap` previews always require non-empty resolutions —
    /// a "apply all with one direction" default would silently overwrite
    /// either side, which is exactly what the preview exists to prevent.
    BulkSyncConfirm {
        request_id: String,
        #[serde(default)]
        resolutions: Vec<BulkSyncResolution>,
    },
    /// Plugin cancelled the preview dock. Daemon drops the pending entry.
    BulkSyncCancel {
        request_id: String,
    },
    /// Plugin announces it is going away cleanly (Studio reload, dock close,
    /// place close, etc.). Daemon should rotate `session_id` and drop
    /// `pending_deltas` for this session immediately. Without this signal the
    /// daemon keeps buffering events for a session that's gone, and the next
    /// reconnect (likely with a fresh plugin instance) replays deltas
    /// targeting instances that no longer exist — silent corruption or hard
    /// errors. Best-effort: a missed `SessionEnd` is no worse than the
    /// pre-existing behaviour (silent disconnect).
    SessionEnd {},
    /// Plugin liveness probe sent every `HEARTBEAT_INTERVAL_SEC`. Daemon
    /// replies with `ServerMsg::Pong`. The plugin tracks the round-trip and
    /// forces a reconnect if no Pong arrives within the missed-window — the
    /// canonical signal for "the WebSocket is wedged but neither end's TCP
    /// stack noticed yet". Carrying a sequence number lets the plugin
    /// detect out-of-order or duplicated pongs in chaotic conditions.
    Ping { seq: u64 },
}

/// One row in the preview dock: a path that currently differs between
/// `tree_studio` and `tree_fs`, optionally with both sides' content so the
/// plugin can show a See Code peek without a follow-up round-trip.
#[derive(Debug, Clone, Serialize)]
pub struct BulkSyncEntry {
    pub path: String,
    pub status: BulkSyncEntryStatus,
    /// Present whenever Studio has a version of this file. Absent for
    /// `IdeOnly` (Studio doesn't have it yet — the confirm would create it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub studio_content: Option<String>,
    /// Present whenever disk has a version. Absent for `StudioOnly`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fs_content: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BulkSyncEntryStatus {
    /// Both sides have the file but the content differs.
    Modified,
    /// Studio has it; disk doesn't. Confirm would create it on disk.
    StudioOnly,
    /// Disk has it; Studio doesn't. Confirm would create it in Studio.
    IdeOnly,
}

/// Per-file failure record sent inside `BulkSyncError` so the plugin UI can
/// list exactly which files were skipped during apply, with the daemon-side
/// reason. Without this the daemon's "100 files synced" message can hide
/// that 50 of them silently failed mid-batch — production-game corruption
/// scenario the audit log alone wouldn't catch in real time.
#[derive(Debug, Clone, Serialize)]
pub struct BulkSyncFailure {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BulkSyncDirection {
    FromStudio,
    FromIde,
    /// Cold-boot preview driven by the Hello handshake. Unlike the two
    /// one-sided variants, Bootstrap does not have a meaningful "apply all"
    /// default — the plugin is expected to collect per-file resolutions and
    /// pass them back in `BulkSyncConfirm.resolutions`.
    Bootstrap,
}

/// Per-file decision the user picks in the preview dock. Each variant names
/// the side that "wins" and implicitly the side that changes:
///
/// * `KeepStudio` — Modified: write Studio content to disk.
/// * `KeepIde`    — Modified: overwrite Studio `Source` with disk content.
/// * `PushToIde`  — StudioOnly: create the file on disk.
/// * `DeleteFromStudio` — StudioOnly: remove the instance from Studio.
/// * `PullToStudio` — IdeOnly: materialize the file as a Studio instance.
/// * `DeleteFromIde` — IdeOnly: unlink the file on disk.
/// * `Skip` — leave both sides untouched (divergence persists; next connect
///   will flag it again).
#[derive(Debug, Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BulkSyncAction {
    Skip,
    KeepStudio,
    KeepIde,
    PushToIde,
    DeleteFromStudio,
    PullToStudio,
    DeleteFromIde,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BulkSyncResolution {
    pub path: String,
    pub action: BulkSyncAction,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncbackMode {
    NewProject,
    MergeExisting {
        /// When true, existing files at the target are overwritten without
        /// going through the conflict-resolution UI.
        #[serde(default)]
        overwrite: bool,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncbackTemplate {
    /// Only services that actually have children are emitted in the project
    /// tree. Fase 4 v1 ships only this template; `FullServices` and `Custom`
    /// are placeholders for a later phase.
    #[default]
    Minimal,
}

fn default_true() -> bool {
    true
}

/// A single `DataModel` instance as the plugin sees it. Identity is by
/// plugin-assigned `id`: `parent_id == None` marks the `game` root so the
/// daemon can thread the tree without relying on insertion order.
///
/// The contents of `properties`, `attributes`, and `tags` are whitelisted by
/// the plugin against its curated property table — the daemon never needs to
/// know which properties exist on which class.
///
/// When `binary` is `Some` and `SyncbackOptions::include_binary` is true, the
/// daemon writes the payload as a `<Name>.rbxm` file and skips recursion into
/// children (the binary blob carries the whole subtree). When the option is
/// off the field is ignored and we fall back to the directory pipeline.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SerializedInstance {
    pub id: u64,
    pub parent_id: Option<u64>,
    pub class_name: String,
    pub name: String,
    #[serde(default)]
    pub properties: HashMap<String, SerializedProperty>,
    #[serde(default)]
    pub attributes: HashMap<String, SerializedProperty>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Pre-serialized `.rbxm` bytes for this instance and its subtree, ready
    /// to drop on disk. The plugin produces this from `plugin:ExportSelection`
    /// (or equivalent) for classes the curated property table can't round-trip
    /// losslessly. Daemon never opens the bytes — it only writes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<BinaryPayload>,
}

/// Opaque pre-serialized `.rbxm` payload. `class_name` is informational (kept
/// for parity with the parent instance and for future Rojo interop); the bytes
/// in `rbxm_bytes_base64` are written verbatim to disk after base64 decode.
///
/// JSON is the wire format for Fase 4-5; once we cut over to `MessagePack` the
/// `Vec<u8>` representation can be surfaced directly without the base64 step.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BinaryPayload {
    pub class_name: String,
    pub rbxm_bytes_base64: String,
}

/// Value of a Roblox property or attribute, encoded in a small wire-friendly
/// union. The plugin's curated table picks the variant per property; the
/// daemon just round-trips it into `.meta.json`.
///
/// `CFrame` is encoded as `[px, py, pz, r00, r01, r02, r10, r11, r12,
/// r20, r21, r22]` — position followed by a row-major 3×3 rotation matrix.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
pub enum SerializedProperty {
    String(String),
    Bool(bool),
    Number(f64),
    Vector2([f64; 2]),
    Vector3([f64; 3]),
    CFrame([f64; 12]),
    Color3([f64; 3]),
    UDim([f64; 2]),
    UDim2([f64; 4]),
    /// Fully-qualified enum item, e.g. `"Material.Plastic"` (the `Enum.`
    /// prefix is implied).
    Enum(String),
    BrickColor(u16),
}

/// Plugin-side resolution for a single conflict file. `hunks` maps
/// conflict-hunk ids to the user's choice; non-conflict hunks are applied
/// server-side from the daemon's own record and don't appear here.
#[derive(Debug, Clone, Deserialize)]
pub struct FileResolution {
    pub path: String,
    pub hunks: HashMap<String, HunkResolution>,
}

/// What the plugin sees for one file in a `ConflictDetected` frame.
#[derive(Debug, Clone, Serialize)]
pub struct FileConflictView {
    pub path: String,
    pub script_kind: ScriptKind,
    pub conflict_kind: ConflictKind,
    pub base_content: Option<String>,
    pub studio_content: Option<String>,
    pub fs_content: Option<String>,
    pub hunks: Vec<ConflictHunk>,
}

/// The server speaks these to the plugin. `ProjectOpened` carries the initial
/// snapshot in `initial_files` so the plugin can wrap the whole bootstrap in
/// a single `ChangeHistoryService` recording.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Sent to a plugin client whose `Hello.auth_token` was empty.
    /// Plugin shows a dock prompt: "Run `Yeet: Pair Studio` in your
    /// IDE within 60s, then click 'Try pairing'." When the user does
    /// so, plugin replies with `PairRequest`. Extension and CLI clients
    /// that already hold a valid token never see this message — they
    /// proceed straight to `ProjectOpened`.
    AuthChallenge {
        /// Absolute project root the daemon is serving. Plugin shows
        /// it in the prompt so the user can verify they're pairing
        /// with the right project (defends against the "two daemons,
        /// pair into the wrong one" UX trap).
        project_root: String,
        /// Time-to-live in seconds for a `yeet-pairing.txt`
        /// breadcrumb. Plugin uses this to format the prompt.
        pair_ttl_secs: u64,
    },
    /// Sent in response to a successful `PairRequest`. Plugin stores
    /// `auth_token` in `plugin:SetSetting("yeet.authToken", ...)` and
    /// re-sends `Hello` with it (or the daemon may proceed directly
    /// to `ProjectOpened` after this — see daemon implementation).
    AuthGranted {
        auth_token: String,
    },
    /// `PairRequest` couldn't be honoured: either no breadcrumb file
    /// found, or breadcrumb timestamp expired. Plugin shows the
    /// reason in the dock log and offers a retry.
    AuthRejected {
        reason: String,
    },
    ProjectOpened {
        project: Project,
        initial_files: Vec<FileSnapshot>,
        /// Daemon-assigned session identifier. Plugins should remember it and
        /// echo it back in `Hello.session_id` on reconnect to skip the
        /// bootstrap path and resume from the buffered delta queue (E7).
        session_id: String,
        /// Daemon's `CARGO_PKG_VERSION` at the time it answered the
        /// handshake. Plugins compare against `MIN_COMPATIBLE_DAEMON_VERSION`
        /// and disconnect with a clear message if the daemon is older
        /// than the protocol the plugin expects. Without this, a newer
        /// plugin against an older daemon would silently drop unknown
        /// frames (the daemon emits the old shape) and converge on
        /// half-functional state.
        daemon_version: String,
        /// Absolute path of the project root the daemon is serving. The
        /// plugin caches this on first connection and refuses to
        /// reconnect to a daemon serving a different project — protects
        /// against the "Studio in project B reconnects to a daemon
        /// still mounted on project A" scenario, which silently writes
        /// project A's files into project B's DataModel.
        project_root: String,
    },
    /// Daemon recognized the `session_id` the plugin sent on `Hello` and is
    /// replaying buffered events instead of bootstrapping fresh. Sent before
    /// the buffered frames so the plugin knows to suppress its own
    /// `ProjectOpened` handling.
    Resumed {
        session_id: String,
        /// How many frames the daemon is about to replay from the buffer.
        /// The plugin uses this for telemetry; counting received frames isn't
        /// reliable since some message types may have been coalesced.
        replayed: u32,
    },
    FileCreated {
        path: String,
        kind: ScriptKind,
        content: String,
        sha256: String,
    },
    FileChanged {
        path: String,
        content: String,
        sha256: String,
    },
    FileDeleted {
        path: String,
    },
    /// Daemon completed a `fs::rename` (real, history-preserving) on behalf
    /// of a `ClientMsg::FileRenamed` from the plugin, or detected an offline
    /// rename via the handshake content-matching heuristic. Plugin applies
    /// this in a single `withRecording("YeetFileRenamed", ...)` so the user
    /// can undo the rename in one step.
    FileRenamed {
        old_path: String,
        new_path: String,
        content: String,
        sha256: String,
        kind: ScriptKind,
    },
    /// Two scripts collide at `path`. `message` is the human-readable hint
    /// the plugin surfaces in its dock log; the extension may treat it as
    /// a generic warning. Sent on every collision detection and again when
    /// the collision is resolved (the daemon sends `path` empty in that
    /// case — plugin clears any active warning UI). Routed via broadcast
    /// so both plugin and extension surfaces stay in sync.
    NameCollision {
        path: String,
        message: String,
    },
    /// Emitted when a `.meta.json` on disk changes the `attributes` block of
    /// an instance. `path` points at the *associated* tracked entry (the
    /// script path for a `Foo.meta.json` sidecar), not at the `.meta.json`
    /// file itself, so the plugin can reuse its existing `fileToInstance`
    /// map. An empty `attributes` map means the user cleared every
    /// attribute — the plugin should remove any it had previously applied.
    AttributesChanged {
        path: String,
        attributes: HashMap<String, SerializedProperty>,
    },
    /// One or more files need human intervention. Plugin opens its resolver
    /// UI; no sync for those paths happens until `ConflictResolved` arrives.
    ConflictDetected {
        conflicts: Vec<FileConflictView>,
    },
    /// Per-chunk flow-control signal. Plugins should wait for the ack of
    /// chunk `seq` before sending chunk `seq + N` when streaming at a
    /// rate the daemon can't keep up with.
    SyncbackAck {
        request_id: String,
        seq: u32,
    },
    /// Materialization succeeded; `stats` describes what was written.
    SyncbackComplete {
        request_id: String,
        project_path: String,
        stats: SyncbackStats,
    },
    /// Any fatal error during any stage. `request_id` may be empty if the
    /// error fired before the begin frame was parsed.
    SyncbackError {
        request_id: String,
        message: String,
    },
    /// Tells the extension to open `path` in a new editor window. Routed only
    /// to the registered extension client, never to the plugin. Used by E5
    /// after a `SyncbackComplete` so the user lands in the freshly
    /// materialized project without an extra click.
    OpenProjectRequest {
        path: String,
    },
    /// Tells the extension to pop the native folder-picker dialog. The
    /// extension replies with a `PickFolderResponse` carrying the same
    /// `request_id`. Routed only to the registered extension client; the
    /// plugin's matching outbound trigger lands in E4.
    PickFolderRequest {
        request_id: String,
        prompt: String,
    },
    /// Daemon's reply back to the plugin for a `PickFolderPrompt`. Broadcast
    /// rather than point-to-point for parity with the syncback ack/complete
    /// flow; plugins filter by `request_id` and ignore everyone else's.
    /// `path` is `None` when the user cancelled, or when no extension was
    /// registered to satisfy the request.
    PickFolderResult {
        request_id: String,
        #[serde(default)]
        path: Option<String>,
    },
    /// Fired to the plugin when the extension triggers a bulk sync. Plugin
    /// opens its preview dock; on Apply it sends `BulkSyncConfirm` back,
    /// which lets the daemon run the actual reconcile.
    BulkSyncPreview {
        request_id: String,
        direction: BulkSyncDirection,
        entries: Vec<BulkSyncEntry>,
    },
    /// Sent after a `BulkSyncConfirm` whose apply loop hit per-file errors.
    /// `failed` lists exactly which files were skipped and why. The plugin
    /// surfaces this in the activity log (and ideally a modal) so the user
    /// can investigate the half-applied state instead of trusting the
    /// silent "all done" the previous protocol implied.
    BulkSyncError {
        request_id: String,
        failed: Vec<BulkSyncFailure>,
    },
    /// Generic "your frame was rejected" notice, broadcast whenever the
    /// daemon dropped a `ClientMsg` for any reason short of crashing — bad
    /// path (sandbox escape), oversized content, sha mismatch, malformed
    /// snapshot entry. Without this the daemon silently swallowed those
    /// frames and the user only noticed when the affected file mysteriously
    /// failed to sync. The kind discriminator lets the plugin decide
    /// whether to log, modal, or both. `path` is empty when the rejection
    /// isn't tied to a single file (e.g. a malformed snapshot dropped a
    /// dozen entries — surface a single `SyncError` summarising the count).
    SyncError {
        kind: SyncErrorKind,
        path: String,
        reason: String,
    },
    /// Reply to `ClientMsg::Ping`. Echoes the same `seq` so the plugin can
    /// pair request to response under high churn.
    Pong { seq: u64 },
}

/// Discriminator for `ServerMsg::SyncError` so the plugin can route by
/// failure class without parsing a free-form `reason` string.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncErrorKind {
    /// Path failed `resolve_inside` — sandbox escape, NUL bytes, etc.
    UnsafePath,
    /// `content.len() > MAX_CONTENT_BYTES`.
    OversizedContent,
    /// Plugin-supplied sha256 didn't match the recomputed hash.
    HashMismatch,
    /// One or more entries in a `StudioSnapshotReport` were dropped.
    SnapshotEntryDropped,
    /// `path` is currently in `pending_collisions` — two `LuaSourceContainer`s
    /// in Studio resolve to the same path and sync is paused until one is
    /// renamed. Frames touching the colliding path are rejected with this
    /// kind so the plugin can show "this script's sync is on hold".
    NameCollisionPending,
    /// A handler (rename, write, delete, etc.) returned an `Err` that the
    /// daemon couldn't recover from. The `reason` field carries the
    /// `format!("{e:#}")` of the original error so the user has a real
    /// failure mode to chase instead of "the rename silently didn't
    /// happen". Used whenever an I/O operation (fs::rename,
    /// create_dir_all, atomic_write, etc.) fails inside a Studio→IDE
    /// sync path.
    HandlerFailed,
}

/// Summary of one completed syncback. Returned inside `SyncbackComplete` so
/// the plugin can show the "147 scripts / 12 services / 3 warnings" summary
/// screen without a follow-up round-trip.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncbackStats {
    pub scripts_written: u32,
    pub non_script_instances_written: u32,
    pub meta_files_written: u32,
    pub total_bytes: u64,
    pub services_included: Vec<String>,
    pub warnings: Vec<String>,
}

/// Determines a `ScriptKind` from the file name suffix, matching Rojo's rules:
///
/// - `foo.server.lua(u)` → `Script`
/// - `foo.client.lua(u)` → `LocalScript`
/// - `foo.lua(u)`        → `ModuleScript`
///
/// Returns `None` for anything that is not a recognized Luau source file.
pub fn classify(file_name: &str) -> Option<(String, ScriptKind)> {
    let (stem, ext) = split_ext(file_name)?;
    // Case-insensitive: Windows filesystems are case-preserving but
    // case-insensitive, so a file written as `Foo.LUAU` or `Foo.Lua` is the
    // same file as `Foo.luau`. Comparing literally would silently drop those.
    if !ext.eq_ignore_ascii_case("luau") && !ext.eq_ignore_ascii_case("lua") {
        return None;
    }
    // Suffix detection is also case-insensitive (`.SERVER.luau` is valid).
    // We compare on a lowercased copy but slice the original `stem` so the
    // returned base name preserves the on-disk casing.
    let stem_lower = stem.to_ascii_lowercase();
    if let Some(prefix) = stem_lower.strip_suffix(".server") {
        return Some((stem[..prefix.len()].to_owned(), ScriptKind::Script));
    }
    if let Some(prefix) = stem_lower.strip_suffix(".client") {
        return Some((stem[..prefix.len()].to_owned(), ScriptKind::LocalScript));
    }
    Some((stem.to_owned(), ScriptKind::ModuleScript))
}

fn split_ext(file_name: &str) -> Option<(&str, &str)> {
    let idx = file_name.rfind('.')?;
    Some((&file_name[..idx], &file_name[idx + 1..]))
}

/// True when the basename of `path` matches one of Rojo's `init` patterns:
/// `init.luau`, `init.lua`, `init.server.{luau,lua}`, `init.client.{luau,lua}`.
/// These files have load-order significance: their parent directory must
/// already exist as a script of the matching `kind` so siblings can be
/// reparented under it. Bulk apply must process them BEFORE non-init
/// siblings to avoid the "ModuleScript with children landed as Folder"
/// bug — see the sort in `apply_bulk_resolutions`. Comparison is
/// case-insensitive to match `classify()`.
pub fn is_init_filename(path: &str) -> bool {
    let basename = path.rsplit('/').next().unwrap_or(path);
    let lower = basename.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "init.luau"
            | "init.lua"
            | "init.server.luau"
            | "init.server.lua"
            | "init.client.luau"
            | "init.client.lua"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_suffixes() {
        assert_eq!(
            classify("Foo.luau"),
            Some(("Foo".to_owned(), ScriptKind::ModuleScript))
        );
        assert_eq!(
            classify("Foo.server.luau"),
            Some(("Foo".to_owned(), ScriptKind::Script))
        );
        assert_eq!(
            classify("Foo.client.lua"),
            Some(("Foo".to_owned(), ScriptKind::LocalScript))
        );
        assert_eq!(classify("notes.txt"), None);
        assert_eq!(classify("no_ext"), None);
    }

    #[test]
    fn detects_init_filenames() {
        // Bare basenames — every Rojo init variant accepted, case-insensitive.
        assert!(is_init_filename("init.luau"));
        assert!(is_init_filename("init.lua"));
        assert!(is_init_filename("init.server.luau"));
        assert!(is_init_filename("init.server.lua"));
        assert!(is_init_filename("init.client.luau"));
        assert!(is_init_filename("init.client.lua"));
        assert!(is_init_filename("INIT.LUAU"));
        assert!(is_init_filename("Init.Server.Lua"));

        // Full paths — only the basename matters.
        assert!(is_init_filename("src/Foo/init.luau"));
        assert!(is_init_filename("src/Sub/init.client.lua"));

        // Near-misses must NOT match (these are regular module scripts).
        assert!(!is_init_filename("init.spec.luau"));
        assert!(!is_init_filename("Initial.luau"));
        assert!(!is_init_filename("init_helper.luau"));
        assert!(!is_init_filename("src/Foo/Bar.luau"));
        assert!(!is_init_filename(""));
        assert!(!is_init_filename("src/init"));
    }
}
