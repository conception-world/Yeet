--!strict

-- Consumes server messages decoded by `net.Connection` and mutates the
-- DataModel to match. Every apply is wrapped in a single ChangeHistoryService
-- recording so Ctrl+Z rolls back the whole server-side event as one step.
--
-- Anti-echo cooperation: before each mutation of `Source`, we stash the new
-- hash in `recentlyApplied`. The SourceWatcher consults `isEcho` when its
-- property-changed signal fires and drops the event if the observed hash
-- matches. This must be set *before* the assignment because
-- `GetPropertyChangedSignal` fires synchronously.

local ChangeHistoryService = game:GetService("ChangeHistoryService")

-- How long an entry in `recentlyApplied` is considered an "echo" of a write
-- the Applier just made. Beyond this window, a Source-property-changed event
-- whose hash happens to match a previous Applier write is treated as a real
-- user edit (e.g. the user typed AB and undid it back to A — A may match the
-- last Applier write, but the user's intent is to send the revert). Sized
-- well above the SourceWatcher's 200ms debounce so a legitimate echo always
-- lands inside the window, but well below typical user-revert latencies
-- (multiple seconds) so deliberate reverts always land outside.
local ECHO_WINDOW_SEC = 0.5

-- Soft cap on `recentlyApplied` entries before we run a sweep. With ECHO_
-- WINDOW_SEC at 0.5s, anything older than that is dead weight (isEcho
-- already rejects it on the timestamp check) — the cap exists to bound
-- memory across long-running sessions where files are renamed/deleted
-- and their stale entries would otherwise accumulate forever. A 500-file
-- project with 10 edits/file/hour for 8h hits ~40K inserts but never has
-- more than ~500 live entries at any moment, so 10K is a generous
-- overhead before the sweep kicks in.
local MAX_RECENTLY_APPLIED = 10000

local TreeBuilder = require(script.Parent.TreeBuilder)
local Sha256 = require(script.Parent.Sha256)
local PropertyTable = require(script.Parent.Parent.syncback.PropertyTable)

-- Types are redeclared locally; cross-file generic types don't propagate
-- reliably in Luau strict mode (see project feedback memory).
type ScriptKind = "module_script" | "script" | "local_script"

type FileSnapshot = {
	path: string,
	kind: ScriptKind,
	content: string,
	sha256: string,
}

type ProjectTreeNode = {
	["$className"]: string?,
	["$path"]: string?,
	[string]: any,
}

type Project = {
	name: string,
	tree: ProjectTreeNode,
}

type Mappings = { [string]: Instance }

type Log = (string) -> ()
type TrackHook = (path: string, instance: Instance) -> ()
type UntrackHook = (path: string) -> ()
-- Fired during bootstrap for every file whose live `Source` in Studio
-- disagreed with the snapshot the daemon sent in `ProjectOpened`. The Widget
-- echoes these back as `file_changed` so the daemon's merge engine gets a
-- chance to surface the conflict instead of the bootstrap silently winning.
type BootstrapDivergeHook = (
	path: string,
	kind: ScriptKind,
	studioContent: string,
	studioSha256: string
) -> ()

-- Fired after bootstrap whenever a new `LuaSourceContainer` appears under a
-- mapped `$path`. The Widget sends it to the daemon as `file_created` so the
-- normal reconcile pipeline materializes the file on disk. Without this, a
-- script the user creates in Studio mid-session is invisible to the daemon
-- until the next full reconnect.
type StudioCreatedHook = (
	path: string,
	kind: ScriptKind,
	studioContent: string,
	studioSha256: string
) -> ()

-- Redeclared locally to match the wire shape of ConflictDetected. Full field
-- definitions live in ui/ConflictResolver.luau; this is just the minimum the
-- Applier needs to forward the payload unchanged.
type ConflictDispatcher = (conflicts: { any }) -> ()

-- Routes syncback_* frames (ack/complete/error) into an active upload session
-- when one is installed. Forwards the raw payload so the session can inspect
-- `request_id` and ignore frames aimed at a different upload.
type SyncbackDispatcher = (payload: { [string]: any }) -> ()

-- Routes `pick_folder_result` frames to whoever is waiting on that request_id.
-- The FolderPicker module installs this once per Applier lifetime and keeps
-- an internal registry of pending waiters; multiple concurrent ask() calls
-- are safe because each carries a unique request_id.
type PickFolderDispatcher = (payload: { [string]: any }) -> ()

-- Routes `bulk_sync_preview` frames into the Widget so it can open the
-- preview dock. Payload carries `request_id`, `direction`, and `entries`.
type BulkSyncPreviewDispatcher = (payload: { [string]: any }) -> ()

-- Fired on receipt of `ProjectOpened` when bootstrap-preview mode is on. The
-- Widget is expected to package the items as a `studio_snapshot_report` and
-- send it down to the daemon, which replies with a `BulkSyncPreview`. Each
-- entry mirrors the wire shape of `StudioFileSnapshot` on the Rust side.
type BootstrapSnapshotHook = (
	snapshot: { { path: string, kind: ScriptKind, content: string, sha256: string } }
) -> ()

local Applier = {}
Applier.__index = Applier

type self = {
	fileToInstance: { [string]: Instance },
	mappings: Mappings,
	bootstrapped: boolean,
	log: Log,
	-- Hash of the content we last wrote into `Source` for each path.
	-- Used by SourceWatcher to reject self-triggered property changes.
	-- Map from path → { hash, sentAt } pair recording the last Applier-driven
	-- Source write. `isEcho` rejects matches older than ECHO_WINDOW_SEC so a
	-- user reverting to the same content (e.g. via Ctrl+Z) is not silently
	-- swallowed as our own bounce.
	recentlyApplied: { [string]: { hash: string, sentAt: number } },
	-- Live entry count for `recentlyApplied`. Tracked explicitly because
	-- Luau hash tables don't expose `#table` — we'd otherwise have to
	-- iterate to know when to sweep. Updated in `seatEcho`, reset on
	-- bookkeeping wipes (`reset`, bootstrap-preview).
	recentlyAppliedCount: number,
	-- Set of attribute names we currently have applied to each path. On a
	-- fresh `AttributesChanged` frame any name present here but missing
	-- from the incoming map is cleared via `SetAttribute(name, nil)` so
	-- deletions on disk propagate cleanly (E8).
	appliedAttributes: { [string]: { [string]: true } },
	onTracked: TrackHook?,
	onUntracked: UntrackHook?,
	onConflicts: ConflictDispatcher?,
	onSyncback: SyncbackDispatcher?,
	onPickFolderResult: PickFolderDispatcher?,
	onBootstrapDiverge: BootstrapDivergeHook?,
	onBulkSyncPreview: BulkSyncPreviewDispatcher?,
	-- Buffer for `bulk_sync_preview` frames that arrived before the
	-- dispatcher was wired. Drained immediately by
	-- `setBulkSyncPreviewDispatcher`. The race the user reports as
	-- "interface de sync não aparece no bootstrap" matches this shape:
	-- if the daemon's reply somehow races ahead of the dispatcher
	-- setup, the frame would be silently dropped without this buffer.
	-- A normal boot keeps this empty.
	bufferedBulkSyncPreviews: { any },
	onStudioCreated: StudioCreatedHook?,
	-- Auth-flow hooks. The daemon sends `auth_challenge` when a Hello
	-- arrives without a usable token (typical first-pair from Studio);
	-- `auth_granted` when a `pair_request` validates against a fresh
	-- `yeet-pairing.txt` breadcrumb; `auth_rejected` on any failure.
	-- Each hook receives the raw payload so the Widget can pull
	-- `project_root` / `pair_ttl_secs` / `auth_token` / `reason`.
	onAuthChallenge: ((any) -> ())?,
	onAuthGranted: ((any) -> ())?,
	onAuthRejected: ((any) -> ())?,
	-- When true, `applyProjectOpened` skips the normal materialize-everything
	-- pass. It instead runs `TreeBuilder.discover` (pure read of the DataModel
	-- under each `$path`), bundles the Studio side into a snapshot, and hands
	-- it off via `onBootstrapSnapshot` so the Widget can forward it to the
	-- daemon as a `StudioSnapshotReport`. The daemon then drives the preview
	-- UI through the existing `BulkSyncPreview`/`BulkSyncConfirm` messages.
	bootstrapPreviewMode: boolean,
	onBootstrapSnapshot: BootstrapSnapshotHook?,
	-- `DescendantAdded` listeners rooted on each `$path` mapping parent.
	-- Kept so `reset()` can disconnect them alongside the rest of the
	-- bootstrap-owned state.
	liveConnections: { RBXScriptConnection },
}

export type Applier = typeof(setmetatable({} :: self, Applier))

function Applier.new(log: Log): Applier
	return setmetatable({
		fileToInstance = {},
		mappings = {},
		bootstrapped = false,
		log = log,
		recentlyApplied = {},
		recentlyAppliedCount = 0,
		appliedAttributes = {},
		onTracked = nil :: TrackHook?,
		onUntracked = nil :: UntrackHook?,
		onConflicts = nil :: ConflictDispatcher?,
		onSyncback = nil :: SyncbackDispatcher?,
		onPickFolderResult = nil :: PickFolderDispatcher?,
		onBootstrapDiverge = nil :: BootstrapDivergeHook?,
		onBulkSyncPreview = nil :: BulkSyncPreviewDispatcher?,
		bufferedBulkSyncPreviews = {} :: { any },
		onStudioCreated = nil :: StudioCreatedHook?,
		onAuthChallenge = nil :: ((any) -> ())?,
		onAuthGranted = nil :: ((any) -> ())?,
		onAuthRejected = nil :: ((any) -> ())?,
		bootstrapPreviewMode = false,
		onBootstrapSnapshot = nil :: BootstrapSnapshotHook?,
		liveConnections = {} :: { RBXScriptConnection },
	}, Applier)
end

function Applier.setBootstrapPreviewMode(self: Applier, enabled: boolean)
	self.bootstrapPreviewMode = enabled
end

function Applier.setBootstrapSnapshotHook(self: Applier, hook: BootstrapSnapshotHook?)
	self.onBootstrapSnapshot = hook
end

function Applier.setTrackHooks(self: Applier, onTracked: TrackHook, onUntracked: UntrackHook)
	self.onTracked = onTracked
	self.onUntracked = onUntracked
end

function Applier.setConflictDispatcher(self: Applier, dispatcher: ConflictDispatcher)
	self.onConflicts = dispatcher
end

-- Auth-flow setters. Mirror `setConflictDispatcher`: each one stores a
-- single most-recent handler. The Widget wires all three on connect;
-- `reset()` clears them along with the rest of the bootstrap-owned
-- state.
function Applier.setAuthChallengeHandler(self: Applier, h: ((any) -> ())?)
	self.onAuthChallenge = h
end

function Applier.setAuthGrantedHandler(self: Applier, h: ((any) -> ())?)
	self.onAuthGranted = h
end

function Applier.setAuthRejectedHandler(self: Applier, h: ((any) -> ())?)
	self.onAuthRejected = h
end

function Applier.setSyncbackDispatcher(self: Applier, dispatcher: SyncbackDispatcher?)
	self.onSyncback = dispatcher
end

function Applier.setPickFolderDispatcher(self: Applier, dispatcher: PickFolderDispatcher?)
	self.onPickFolderResult = dispatcher
end

function Applier.setBootstrapDivergeHook(self: Applier, hook: BootstrapDivergeHook?)
	self.onBootstrapDiverge = hook
end

function Applier.setBulkSyncPreviewDispatcher(
	self: Applier,
	dispatcher: BulkSyncPreviewDispatcher?
)
	self.onBulkSyncPreview = dispatcher
	-- Drain anything that arrived before the dispatcher was wired. The
	-- normal connect order in Widget._onConnectPressed wires this BEFORE
	-- Reconnect.run, so the buffer should be empty here in steady state.
	-- But the user reports "interface de sync não aparece no bootstrap"
	-- which can only happen if this dispatcher was nil at the moment
	-- the daemon's frame arrived — this drain closes that hole.
	if dispatcher ~= nil and #self.bufferedBulkSyncPreviews > 0 then
		local buffered = self.bufferedBulkSyncPreviews
		self.bufferedBulkSyncPreviews = {}
		self.log(`draining {#buffered} buffered bulk_sync_preview frame(s)`)
		for _, payload in buffered do
			dispatcher(payload)
		end
	end
end

function Applier.setStudioCreatedHook(self: Applier, hook: StudioCreatedHook?)
	self.onStudioCreated = hook
end

function Applier.isEcho(self: Applier, path: string, hash: string): boolean
	local entry = self.recentlyApplied[path]
	if entry == nil or entry.hash ~= hash then
		return false
	end
	-- Hash matches a previous self-write — but only treat it as an echo if
	-- that write happened recently. A stale match (user reverted to old
	-- content via undo, or rewrote the same lines) must propagate to the
	-- daemon as a real edit.
	return (os.clock() - entry.sentAt) < ECHO_WINDOW_SEC
end

-- Symmetrical to `seatEcho`: drops the entry and keeps the live count
-- accurate. Sites used to do `self.recentlyApplied[path] = nil` directly,
-- which would leave the counter stale and trigger the sweep early.
local function forgetEcho(self: Applier, path: string)
	if self.recentlyApplied[path] ~= nil then
		self.recentlyApplied[path] = nil
		self.recentlyAppliedCount -= 1
	end
end

-- Drops every entry older than 2× ECHO_WINDOW_SEC. Anything beyond that
-- can never satisfy `isEcho`'s timestamp check, so it's dead weight.
-- Returns the new live count for caller logging.
local function sweepStaleEchos(self: Applier): number
	local cutoff = os.clock() - (ECHO_WINDOW_SEC * 2)
	local kept = 0
	for path, entry in pairs(self.recentlyApplied) do
		if entry.sentAt < cutoff then
			self.recentlyApplied[path] = nil
		else
			kept += 1
		end
	end
	self.recentlyAppliedCount = kept
	return kept
end

-- Centralised "I just wrote `hash` to `path`" bookkeeping. Always pairs the
-- hash with the current clock so `isEcho` can window-check it. All sites
-- that previously did `self.recentlyApplied[path] = hash` route through
-- here so the timestamp invariant is impossible to forget.
--
-- Also tracks a soft entry cap (`MAX_RECENTLY_APPLIED`): when crossed, runs
-- a sweep that drops every entry already past the echo window. Without
-- this, a long-running session that touches files faster than they go
-- stale (renames, deletions whose paths never come back) would let the
-- table grow without bound — irrelevant for memory in any short session
-- but a real production-soak risk.
local function seatEcho(self: Applier, path: string, hash: string)
	if self.recentlyApplied[path] == nil then
		self.recentlyAppliedCount += 1
	end
	self.recentlyApplied[path] = { hash = hash, sentAt = os.clock() }
	if self.recentlyAppliedCount >= MAX_RECENTLY_APPLIED then
		local kept = sweepStaleEchos(self)
		self.log(`recentlyApplied swept: kept {kept} of {MAX_RECENTLY_APPLIED} entries`)
	end
end

local function withRecording(name: string, action: () -> ()): (boolean, string?)
	local identifier = ChangeHistoryService:TryBeginRecording(name, `Yeet: {name}`)
	if identifier == nil then
		-- Another recording is already open (user is dragging a handle, say).
		-- Refusing is safer than silently committing changes that won't undo
		-- atomically with whatever Studio is currently recording.
		return false, "ChangeHistoryService busy"
	end
	local ok, err: any = pcall(action)
	if ok then
		ChangeHistoryService:FinishRecording(identifier, Enum.FinishRecordingOperation.Commit)
		return true, nil
	end
	ChangeHistoryService:FinishRecording(identifier, Enum.FinishRecordingOperation.Cancel)
	return false, tostring(err)
end

-- Seeds `recentlyApplied` for every path we just materialized and notifies the
-- watcher. Called inside the bootstrap/create recording so the initial Source
-- assignment in `TreeBuilder` is already committed when we reach here.
local function registerTracked(self: Applier, path: string, inst: Instance, hash: string)
	seatEcho(self, path, hash)
	local hook = self.onTracked
	if hook ~= nil then
		hook(path, inst)
	end
end

local function applyProjectOpenedForPreview(self: Applier, project: Project)
	-- Wipe per-session state the same way the apply path does — the preview
	-- will drive fresh materializations as the user confirms resolutions.
	self.recentlyApplied = {}
	self.recentlyAppliedCount = 0
	self.fileToInstance = {}
	self.mappings = {}
	self.appliedAttributes = {}

	local result = TreeBuilder.discover(project)
	self.mappings = result.mappings
	-- Mark as bootstrapped so that `applyFileCreated` / `applyFileChanged` /
	-- `applyFileDeleted` triggered by the daemon's per-resolution broadcasts
	-- after `BulkSyncConfirm` aren't rejected with "ignored before bootstrap".
	self.bootstrapped = true

	-- Package the Studio side for the wire AND populate `fileToInstance`
	-- in lockstep. Wiring up the discovered instances right here is what
	-- makes post-bootstrap IDE→Studio edits land: a daemon-side
	-- `FileChanged` arriving for `src/Foo.luau` looks up
	-- `fileToInstance["src/Foo.luau"]`, finds the script, and assigns
	-- `.Source`. Without this seeding the Applier prints "~ {path}
	-- (unknown; skipped)" and the Studio side stays stale.
	local snapshot: {
		{ path: string, kind: ScriptKind, content: string, sha256: string }
	} = {}
	for _, entry in result.scripts do
		local sha = Sha256.hex(entry.studio_content)
		table.insert(snapshot, {
			path = entry.path,
			kind = entry.kind,
			content = entry.studio_content,
			sha256 = sha,
		})
		self.fileToInstance[entry.path] = entry.instance
		registerTracked(self, entry.path, entry.instance, sha)
	end

	self:startLiveWatcher()

	local hook = self.onBootstrapSnapshot
	if hook == nil then
		self.log(
			`bootstrap preview: {#snapshot} studio script(s) discovered but no hook wired`
		)
		return
	end
	self.log(`bootstrap preview: reporting {#snapshot} studio script(s) to daemon`)
	hook(snapshot)
end

-- Minimum daemon `CARGO_PKG_VERSION` the plugin's wire schema is built
-- to talk to. Bump in lockstep with the daemon's
-- `MIN_COMPATIBLE_PLUGIN_VERSION` so the three components (daemon /
-- plugin / extension) refuse to silently misinterpret each other's
-- frames after a breaking change.
local MIN_COMPATIBLE_DAEMON_VERSION = "0.3.0"

-- Cheap semver compare. Roblox Luau has no semver crate; we only need
-- the strict precedence on `major.minor.patch` numerics. Pre-release
-- tags (`-beta` etc.) are stripped — pre-release of a target version
-- is treated as that version for compatibility checks here. Returns
-- `true` iff `a` is strictly older than `b`. Malformed inputs return
-- `false` (don't reject on parse failure — bias toward "let the user
-- proceed and find out later via runtime errors" rather than locking
-- them out for a missing dot).
type SemverParts = { major: number, minor: number, patch: number }
local function parseSemver(v: string): SemverParts?
	local m1, m2, m3 = string.match(v, "^(%d+)%.(%d+)%.(%d+)")
	if m1 == nil then return nil end
	local maj = tonumber(m1)
	local min = tonumber(m2)
	local pat = tonumber(m3)
	if maj == nil or min == nil or pat == nil then return nil end
	return { major = maj, minor = min, patch = pat }
end

local function semverLess(a: string, b: string): boolean
	local av = parseSemver(a)
	local bv = parseSemver(b)
	if av == nil or bv == nil then return false end
	if av.major ~= bv.major then return av.major < bv.major end
	if av.minor ~= bv.minor then return av.minor < bv.minor end
	return av.patch < bv.patch
end

local function applyProjectOpened(self: Applier, msg: any)
	-- Daemon advertises its own CARGO_PKG_VERSION in ProjectOpened. If
	-- the daemon is older than what the plugin's wire schema needs, log
	-- a loud warning so the user knows to update — better than silently
	-- mis-handling unknown frames later in the session. We do NOT force
	-- a disconnect here (would need a connection callback the Applier
	-- doesn't own); the daemon will reject the next breaking-changed
	-- frame and the user reaches the same end-state with a clear log.
	local daemonVersion = msg.daemon_version
	if typeof(daemonVersion) ~= "string" or daemonVersion == "" then
		self.log(
			`!! daemon did not advertise daemon_version (likely older than `
				.. MIN_COMPATIBLE_DAEMON_VERSION
				.. `). Update the daemon before relying on this connection.`
		)
	elseif semverLess(daemonVersion, MIN_COMPATIBLE_DAEMON_VERSION) then
		self.log(
			`!! daemon version {daemonVersion} is older than required `
				.. MIN_COMPATIBLE_DAEMON_VERSION
				.. `. Update the daemon — frames may misalign and writes can be lost.`
		)
	end
	local project: Project = msg.project
	local initialFiles: { FileSnapshot } = msg.initial_files
	if self.bootstrapPreviewMode then
		-- Bootstrap-preview flow owns its own lifecycle: discover mappings,
		-- ship the Studio snapshot to the daemon, and wait for the preview
		-- dock to drive per-file apply. The daemon's `initial_files` is
		-- ignored here — we don't want a materialize-all side effect before
		-- the user has chosen.
		applyProjectOpenedForPreview(self, project)
		return
	end
	-- Re-bootstrap path (daemon restarted without a matching session_id).
	-- `TreeBuilder.build` reuses same-class children by name, so the second
	-- pass is a no-op for instances that already existed; wiping our own
	-- bookkeeping first just keeps `recentlyApplied` honest.
	if self.bootstrapped then
		self.recentlyApplied = {}
		self.recentlyAppliedCount = 0
		self.fileToInstance = {}
		self.mappings = {}
		self.appliedAttributes = {}
	end
	local divergent: { { path: string, kind: ScriptKind, studio_content: string } } = {}
	local untracked: { { path: string, kind: ScriptKind, studio_content: string } } = {}
	local ok, err = withRecording("YeetBootstrap", function()
		local result = TreeBuilder.build(project, initialFiles)
		self.fileToInstance = result.fileToInstance
		self.mappings = result.mappings
		self.bootstrapped = true
		divergent = result.divergent
		untracked = result.untracked
	end)
	if not ok then
		self.log(`bootstrap failed: {err}`)
		return
	end
	-- Register tracking outside the recording: SourceWatcher connects signals
	-- but does not mutate Source, so there is nothing to roll back.
	for _, file in initialFiles do
		local inst = self.fileToInstance[file.path]
		if inst ~= nil then
			registerTracked(self, file.path, inst, file.sha256)
		end
	end
	self.log(`bootstrap: {#initialFiles} file(s) materialized`)
	-- Start live tracking of new scripts under every `$path` mapping, so
	-- scripts the user creates in Studio mid-session show up on disk via
	-- the normal reconcile path instead of staying invisible to the daemon
	-- until the next reconnect.
	self:startLiveWatcher()
	-- Echo every Studio-side divergence back to the daemon. Without this, a
	-- script that was edited in Studio while the daemon was off would get
	-- silently clobbered with the disk version (TreeBuilder now preserves
	-- Studio's `Source`, so clobbering was only half the bug — we also have
	-- to surface the diff to the merge engine). Each echo primes the
	-- `recentlyApplied` hash with Studio's content so the SourceWatcher
	-- doesn't double-fire on the same value.
	if #divergent > 0 or #untracked > 0 then
		local hook = self.onBootstrapDiverge
		if hook == nil then
			self.log(`bootstrap: {#divergent + #untracked} diverge/untracked but no hook wired`)
		else
			-- Divergent first so the Studio-vs-disk conflicts (if any) land
			-- before the Studio-only registrations; order doesn't really
			-- matter to the daemon but the log reads top-down.
			for _, entry in divergent do
				local sha = Sha256.hex(entry.studio_content)
				seatEcho(self, entry.path, sha)
				hook(entry.path, entry.kind, entry.studio_content, sha)
			end
			for _, entry in untracked do
				local sha = Sha256.hex(entry.studio_content)
				seatEcho(self, entry.path, sha)
				hook(entry.path, entry.kind, entry.studio_content, sha)
			end
			self.log(
				`bootstrap: {#divergent} divergent + {#untracked} studio-only file(s) echoed`
			)
		end
	end
end

local function applyFileCreated(self: Applier, msg: any)
	if not self.bootstrapped then
		self.log(`ignored FileCreated before bootstrap: {msg.path}`)
		return
	end
	local snap: FileSnapshot = {
		path = msg.path,
		kind = msg.kind,
		content = msg.content,
		sha256 = msg.sha256,
	}
	local created: Instance? = nil
	local ok, err = withRecording("YeetFileCreated", function()
		created = TreeBuilder.materializeFile(snap, self.mappings)
		if created ~= nil then
			self.fileToInstance[snap.path] = created
		end
	end)
	if not ok then
		self.log(`create failed {snap.path}: {err}`)
		return
	end
	if created ~= nil then
		registerTracked(self, snap.path, created, snap.sha256)
		self.log(`+ {snap.path}`)
	else
		self.log(`skipped (no mapping): {snap.path}`)
	end
end

local function applyFileChanged(self: Applier, msg: any)
	if not self.bootstrapped then
		self.log(`ignored FileChanged before bootstrap: {msg.path}`)
		return
	end
	local inst = self.fileToInstance[msg.path]
	if inst == nil then
		-- Unknown path: we can't re-materialize without the `kind` field,
		-- which FileChanged doesn't carry. Logged so the mismatch surfaces.
		self.log(`~ {msg.path} (unknown; skipped)`)
		return
	end
	-- Validate the cached reference is still live and still the right kind
	-- before assigning to Source. The reference was captured at bootstrap or
	-- by the live-tree watcher, but the user can have removed/replaced the
	-- instance in Studio between then and now (e.g. deleted the Script and
	-- created a ModuleScript with the same name, or moved it to nil parent).
	-- Without this check we'd silently write `content` to an orphaned
	-- instance — invisible in the DataModel but appearing successful.
	-- Self-heals: drop the stale mapping, log loudly, and let the next
	-- reconcile cycle (daemon notices path missing, sends FileCreated)
	-- re-materialize the instance.
	if inst.Parent == nil or not inst:IsA("LuaSourceContainer") then
		self.log(
			`~ {msg.path} (stale ref dropped: parent={tostring(inst.Parent)} class={inst.ClassName}); awaiting re-materialize`
		)
		self.fileToInstance[msg.path] = nil
		forgetEcho(self, msg.path)
		return
	end
	local content: string = msg.content
	local hash: string = msg.sha256
	-- Seat the hash BEFORE the assignment: `GetPropertyChangedSignal` fires
	-- synchronously during the write, and the watcher will read
	-- `recentlyApplied` inside that callback.
	seatEcho(self, msg.path, hash)
	local ok, err = withRecording("YeetFileChanged", function()
		(inst :: LuaSourceContainer).Source = content
	end)
	if ok then
		self.log(`~ {msg.path}`)
	else
		self.log(`change failed {msg.path}: {err}`)
	end
end

local function applyFileDeleted(self: Applier, msg: any)
	if not self.bootstrapped then
		return
	end
	local inst = self.fileToInstance[msg.path]
	if inst == nil then
		self.log(`- {msg.path} (unknown)`)
		return
	end
	local hook = self.onUntracked
	if hook ~= nil then
		hook(msg.path)
	end
	forgetEcho(self, msg.path)
	local ok, err = withRecording("YeetFileDeleted", function()
		inst:Destroy()
		self.fileToInstance[msg.path] = nil
	end)
	if ok then
		self.log(`- {msg.path}`)
	else
		self.log(`delete failed {msg.path}: {err}`)
	end
end

local function applyAttributesChanged(self: Applier, msg: any)
	if not self.bootstrapped then
		return
	end
	local path = msg.path
	if typeof(path) ~= "string" then
		return
	end
	local inst = self.fileToInstance[path]
	if inst == nil then
		-- The daemon only emits these for script sidecars today, but a path
		-- could still be stale (renamed mid-flight, or the sibling .luau is
		-- missing). Drop rather than fail — next file event will re-align.
		self.log(`attributes_changed for untracked path: {path}`)
		return
	end
	local incoming = msg.attributes
	if typeof(incoming) ~= "table" then
		self.log(`attributes_changed: malformed payload for {path}`)
		return
	end
	local previous = self.appliedAttributes[path] or {}
	local nextSet: { [string]: true } = {}
	local ok, err = withRecording("YeetAttributesChanged", function()
		for name, encoded in incoming do
			if typeof(name) ~= "string" or typeof(encoded) ~= "table" then
				continue
			end
			local decoded = PropertyTable.decodeValue(encoded)
			-- `nil` here means the wire shape was unsupported; leave the
			-- attribute untouched rather than clearing it.
			if decoded == nil then
				continue
			end
			inst:SetAttribute(name, decoded)
			nextSet[name] = true
		end
		-- Clear attributes that we applied last time but aren't in the new map.
		for name in previous do
			if nextSet[name] == nil then
				inst:SetAttribute(name, nil)
			end
		end
	end)
	if ok then
		self.appliedAttributes[path] = nextSet
		self.log(`= attributes: {path}`)
	else
		self.log(`attributes_changed failed {path}: {err}`)
	end
end

-- Called by SourceWatcher right after it has queued a FileChanged frame to
-- the daemon. Keeps `recentlyApplied` in sync with the content we just sent,
-- so the daemon's echoed `file_changed` broadcast (if it ever gets one) is
-- absorbed cleanly.
function Applier.recordSelfEdit(self: Applier, path: string, hash: string)
	seatEcho(self, path, hash)
end

-- Called by SourceWatcher when the user deletes a tracked script in Studio.
-- Drops local bookkeeping; the Widget still needs to forward the
-- `file_deleted` frame separately, because Applier doesn't own the socket.
function Applier.forgetLocalDelete(self: Applier, path: string)
	self.fileToInstance[path] = nil
	forgetEcho(self, path)
end

-- Exposed so Widget/Watcher can compute hashes using the same implementation
-- Applier uses internally. Keeping it here avoids leaking `Sha256` into
-- every consumer's require list.
function Applier.hash(_self: Applier, content: string): string
	return Sha256.hex(content)
end

function Applier.dispatch(self: Applier, payload: { [string]: any })
	local kind = payload["type"]
	if typeof(kind) ~= "string" then
		self.log(`dropped msg without 'type': {tostring(kind)}`)
		return
	end
	if kind == "project_opened" then
		applyProjectOpened(self, payload)
	elseif kind == "resumed" then
		-- Session matched — daemon is replaying buffered deltas right after
		-- this frame. No local state needs to move; subsequent FileChanged /
		-- FileCreated / FileDeleted frames land in the existing dispatch
		-- arms. Reconnect.luau already captured `session_id`.
		local replayed = payload["replayed"]
		if typeof(replayed) == "number" then
			self.log(`resumed: replaying {replayed} frame(s)`)
		else
			self.log("resumed")
		end
	elseif kind == "file_created" then
		applyFileCreated(self, payload)
	elseif kind == "file_changed" then
		applyFileChanged(self, payload)
	elseif kind == "file_deleted" then
		applyFileDeleted(self, payload)
	elseif kind == "attributes_changed" then
		applyAttributesChanged(self, payload)
	elseif kind == "conflict_detected" then
		local conflicts = payload.conflicts
		local count = typeof(conflicts) == "table" and #(conflicts :: { any }) or 0
		self.log(`conflict_detected received: {count} file(s)`)
		local dispatcher = self.onConflicts
		if dispatcher == nil then
			self.log(`conflict_detected but no dispatcher wired`)
			return
		end
		if typeof(conflicts) ~= "table" then
			self.log(`conflict_detected: malformed payload`)
			return
		end
		dispatcher(conflicts :: { any })
	elseif kind == "syncback_ack" or kind == "syncback_complete" or kind == "syncback_error" then
		local dispatcher = self.onSyncback
		if dispatcher == nil then
			self.log(`{kind} received with no syncback dispatcher installed`)
			return
		end
		dispatcher(payload)
	elseif kind == "pick_folder_result" then
		local dispatcher = self.onPickFolderResult
		if dispatcher == nil then
			-- Arrives when a prompt goes out without FolderPicker attached;
			-- log and drop so the daemon's broadcast doesn't wedge anything.
			self.log(`pick_folder_result received with no picker dispatcher installed`)
			return
		end
		dispatcher(payload)
	elseif kind == "bulk_sync_preview" then
		local entries = payload.entries
		local count = typeof(entries) == "table" and #(entries :: { any }) or 0
		self.log(`bulk_sync_preview received: {count} file(s)`)
		local dispatcher = self.onBulkSyncPreview
		if dispatcher == nil then
			-- Buffer the frame instead of dropping it. setBulkSyncPreview
			-- Dispatcher will drain on wire-up. This protects against any
			-- timing where the daemon's reply outraces the dispatcher
			-- setup — without this, "interface de sync não aparece no
			-- bootstrap" silently happens whenever the race fires.
			table.insert(self.bufferedBulkSyncPreviews, payload)
			self.log(
				`bulk_sync_preview buffered (no dispatcher yet); now {#self.bufferedBulkSyncPreviews} pending`
			)
			return
		end
		dispatcher(payload)
	elseif kind == "bulk_sync_error" then
		-- Daemon hit per-file failures in the apply loop. Logging here is
		-- the user-visible signal — without this they'd see "all done"
		-- in the preview and only discover the half-applied state when
		-- production breaks. Each failure line includes the path and the
		-- daemon's reason string so the user can investigate directly.
		local failed = payload.failed
		local count = typeof(failed) == "table" and #(failed :: { any }) or 0
		local requestId = tostring(payload.request_id or "<no-id>")
		self.log(`!! bulk sync apply had {count} failure(s) (request={requestId}):`)
		if typeof(failed) == "table" then
			for _, fail in failed :: { any } do
				if typeof(fail) == "table" then
					self.log(`   {tostring(fail.path)}: {tostring(fail.reason)}`)
				end
			end
		end
	elseif kind == "sync_error" then
		-- Daemon dropped a frame for safety reasons (unsafe path, oversize,
		-- sha mismatch, snapshot entry filter). Surface loudly so the user
		-- knows the file did NOT sync — silent drop was the previous
		-- behaviour, exactly the production-game-corruption scenario the
		-- T1.5 fix exists to close.
		local subKind = tostring(payload.kind or "<no-kind>")
		local path = tostring(payload.path or "")
		local reason = tostring(payload.reason or "<no-reason>")
		if path == "" then
			self.log(`!! sync error [{subKind}]: {reason}`)
		else
			self.log(`!! sync error [{subKind}] {path}: {reason}`)
		end
	else
		if kind == "auth_challenge" or kind == "auth_granted" or kind == "auth_rejected" then
			-- Auth-flow frames. Forwarded to Widget-owned handlers so
			-- the dock can show the pairing UI / persist the token /
			-- show the rejection reason. Falls into this catch-all
			-- branch (rather than a dedicated `elseif` higher up)
			-- because Luau's `elseif kind == "auth_challenge"` chain
			-- gets fragile when re-edited; centralising auth dispatch
			-- here keeps the rest of dispatch untouched.
			local dispatcher: ((any) -> ())?
			if kind == "auth_challenge" then
				dispatcher = self.onAuthChallenge
			elseif kind == "auth_granted" then
				dispatcher = self.onAuthGranted
			else
				dispatcher = self.onAuthRejected
			end
			if dispatcher == nil then
				self.log(`!! {kind} received but no handler installed; drop`)
				return
			end
			dispatcher(payload)
			return
		end
		self.log(`unknown msg type: {kind}`)
	end
end

-- Starts listening for new `LuaSourceContainer` descendants under each
-- `$path`-mapped parent. When a user creates a Script/LocalScript/ModuleScript
-- in Studio mid-session, this fires `onStudioCreated` so the Widget can tell
-- the daemon (which then goes through the normal reconcile → creates the
-- file on disk). Without this, such scripts stay invisible to the daemon
-- until the next full reconnect. Call once per bootstrap — the connections
-- persist until `reset()`.
function Applier.startLiveWatcher(self: Applier)
	-- Drop any previous batch in case bootstrap was rerun without reset.
	for _, conn in self.liveConnections do
		conn:Disconnect()
	end
	table.clear(self.liveConnections)

	for mapPath, mapParent in pairs(self.mappings) do
		local conn = mapParent.DescendantAdded:Connect(function(desc: Instance)
			if not desc:IsA("LuaSourceContainer") then
				return
			end
			local path, kind = TreeBuilder.computePath(mapPath, mapParent, desc)
			if path == nil or kind == nil then
				return
			end
			if self.fileToInstance[path] ~= nil then
				-- Already tracked (bootstrap materialized it, or an earlier
				-- DescendantAdded fire beat us here for a reparent dance).
				return
			end
			local hook = self.onStudioCreated
			if hook == nil then
				self.log(`studio script created but no hook wired: {path}`)
				return
			end
			local ok, source = pcall(function()
				return (desc :: LuaSourceContainer).Source
			end)
			if not ok then
				return
			end
			local sha = Sha256.hex(source)
			-- Seat the hash in `recentlyApplied` BEFORE the hook fires its
			-- `file_created` — our eventual daemon FileChanged echo (if any)
			-- would otherwise be treated as a user edit and rebound.
			seatEcho(self, path, sha)
			self.fileToInstance[path] = desc
			hook(path, kind, source, sha)
			-- Register the fresh instance with SourceWatcher so subsequent
			-- edits to its Source flow through the normal pipeline.
			local trackHook = self.onTracked
			if trackHook ~= nil then
				trackHook(path, desc)
			end
		end)
		table.insert(self.liveConnections, conn)
	end
end

function Applier.reset(self: Applier)
	for _, conn in self.liveConnections do
		conn:Disconnect()
	end
	table.clear(self.liveConnections)
	self.fileToInstance = {}
	self.mappings = {}
	self.bootstrapped = false
	self.recentlyApplied = {}
	self.recentlyAppliedCount = 0
	self.appliedAttributes = {}
	-- Drop any unprocessed bulk_sync_preview frames from the previous
	-- session — they reference a request_id the daemon already retired.
	self.bufferedBulkSyncPreviews = {}
end

return Applier
