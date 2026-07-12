--!strict

-- Watches the `Source` property of every tracked script and reports user
-- edits upward to the daemon via an injected `onEdit` callback.
--
-- Anti-echo has two tiers:
--   1. Synchronous drop: as soon as the property-changed signal fires, we
--      hash the current Source and ask `isEcho(path, hash)`. If the hash
--      matches the value the Applier just stashed, we return immediately
--      without scheduling anything.
--   2. Trailing-edge debounce (200ms): rapid typing would otherwise spam
--      `FileChanged` frames. We delay the send until the user pauses for
--      200ms, then re-read Source + re-check `isEcho`, in case the Applier
--      wrote over the edit in the meantime.

-- The user's spec says "throttle of 200ms" but describes debounce semantics
-- ("avoid spam on rapid typing"). Debounce is what actually matches that
-- requirement, so that's what we implement.

local DEBOUNCE_SECONDS = 0.2

export type ScriptKind = "module_script" | "script" | "local_script"

export type EditHandler = (path: string, content: string, hash: string) -> ()
export type DeleteHandler = (path: string) -> ()
export type RenameHandler = (
	oldPath: string,
	newPath: string,
	kind: ScriptKind,
	content: string,
	hash: string
) -> ()
export type EchoPredicate = (path: string, hash: string) -> boolean
export type Hasher = (content: string) -> string
export type Log = (line: string) -> ()
export type PathResolver = (instance: Instance) -> (string?, ScriptKind?)

type Entry = {
	instance: Instance,
	-- Current project-relative path. Updated in lockstep with `byPath`
	-- whenever the instance's `Name`/`AncestryChanged` listener fires
	-- a rename; the Source debounce closure reads this so it always
	-- reports against the current path even if the user renamed mid-edit.
	path: string,
	connection: RBXScriptConnection,
	-- Fires once when the user deletes the tracked script in Studio; we
	-- forward that as a `file_deleted` upward to the daemon.
	destroyConnection: RBXScriptConnection,
	-- `GetPropertyChangedSignal("Name")` listener — detects in-place
	-- renames (`Foo` → `Bar` under the same parent). Maps directly to
	-- a path change once `resolvePath` agrees.
	nameConnection: RBXScriptConnection,
	-- `AncestryChanged` listener — detects reparenting (moving the
	-- script between mapped containers, or out of every mapping
	-- entirely). The latter is treated as a delete.
	ancestryConnection: RBXScriptConnection,
	-- Incremented on every non-echo signal fire; the scheduled task carries
	-- a copy of the value at the time it was enqueued and bails out if the
	-- entry's gen has advanced (meaning another fire superseded it).
	gen: number,
	-- Content/hash of the most recent non-echo user edit that has NOT yet
	-- been forwarded to the daemon. Captured synchronously when the Source
	-- signal fires (before the debounce) so the trailing-edge send can report
	-- the user's actual edit even if the Applier overwrote `Source` with
	-- daemon content inside the 200ms window (A3). Cleared when the debounce
	-- fires. Also read by `peekPending` so the Applier can avoid clobbering.
	pendingContent: string?,
	pendingHash: string?,
}

type Options = {
	log: Log,
	onEdit: EditHandler,
	onDelete: DeleteHandler,
	-- Optional rename callback. When absent, rename events fall back to
	-- `onDelete(oldPath)` (since the daemon can't be informed of the
	-- new path) — the IDE side will surface the script as `studio_only`
	-- on the next reconcile.
	onRename: RenameHandler?,
	-- Resolves an instance's current `(path, kind)` against the active
	-- `$path` mappings. Injected by the Applier (which owns the
	-- mappings table). Absent → rename detection is disabled and the
	-- watcher behaves like its pre-rename version.
	resolvePath: PathResolver?,
	isEcho: EchoPredicate,
	hash: Hasher,
}

local SourceWatcher = {}
SourceWatcher.__index = SourceWatcher

type self = {
	byPath: { [string]: Entry },
	byInstance: { [Instance]: Entry },
	log: Log,
	onEdit: EditHandler,
	onDelete: DeleteHandler,
	onRename: RenameHandler?,
	resolvePath: PathResolver?,
	isEcho: EchoPredicate,
	hash: Hasher,
	destroyed: boolean,
}

export type SourceWatcher = typeof(setmetatable({} :: self, SourceWatcher))

function SourceWatcher.new(opts: Options): SourceWatcher
	return setmetatable({
		byPath = {},
		byInstance = {},
		log = opts.log,
		onEdit = opts.onEdit,
		onDelete = opts.onDelete,
		onRename = opts.onRename,
		resolvePath = opts.resolvePath,
		isEcho = opts.isEcho,
		hash = opts.hash,
		destroyed = false,
	}, SourceWatcher)
end

function SourceWatcher.setRenameHook(self: SourceWatcher, handler: RenameHandler?)
	self.onRename = handler
end

function SourceWatcher.setResolvePath(self: SourceWatcher, resolver: PathResolver?)
	self.resolvePath = resolver
end

local function readSource(instance: Instance): string?
	-- Only LuaSourceContainer descendants carry `Source`. Everything we track
	-- is created by TreeBuilder, but type-narrow defensively anyway.
	if instance:IsA("LuaSourceContainer") then
		return (instance :: LuaSourceContainer).Source
	end
	return nil
end

-- Internal helper: drop all four connections from an entry. Idempotent.
local function disconnectAll(entry: Entry)
	entry.connection:Disconnect()
	entry.destroyConnection:Disconnect()
	entry.nameConnection:Disconnect()
	entry.ancestryConnection:Disconnect()
	entry.gen += 1
end

-- Internal helper: if the instance's current path resolved by the
-- injected `resolvePath` differs from `entry.path`, emit a rename and
-- update `byPath`/`entry.path` in lockstep so subsequent Source-change
-- callbacks see the new path. Returns the resolved path (nil when the
-- instance escaped every mapping → caller treats as delete).
local function syncPath(self: SourceWatcher, entry: Entry): string?
	local resolver = self.resolvePath
	if resolver == nil then
		self.log(`[debug] syncPath: no resolver wired for {entry.path}`)
		return entry.path
	end
	local newPath, kind = resolver(entry.instance)
	if newPath == nil or kind == nil then
		self.log(`[debug] syncPath: resolver returned nil for {entry.path}`)
		return nil
	end
	if newPath == entry.path then
		return newPath
	end
	self.log(`[debug] syncPath: detected rename {entry.path} -> {newPath}`)
	local oldPath = entry.path
	-- Re-key both maps before invoking the rename hook: the hook may
	-- inspect/mutate watcher state itself, and it must see consistent
	-- maps.
	if self.byPath[oldPath] == entry then
		self.byPath[oldPath] = nil
	end
	self.byPath[newPath] = entry
	entry.path = newPath

	local hook = self.onRename
	if hook ~= nil then
		local source = readSource(entry.instance) or ""
		local hash = self.hash(source)
		self.log(`-> file_renamed {oldPath} -> {newPath}`)
		hook(oldPath, newPath, kind, source, hash)
	else
		-- No rename channel — fall back to a delete so the daemon at
		-- least drops the old path. The new path will surface via the
		-- Applier's live-tree watcher on the next descendant-added.
		self.log(`-> file_deleted {oldPath} (no rename hook; renamed to {newPath})`)
		self.onDelete(oldPath)
	end
	return newPath
end

function SourceWatcher.track(self: SourceWatcher, path: string, instance: Instance)
	if self.destroyed then
		return
	end
	-- Re-tracking an already-tracked path must drop the old connection first,
	-- otherwise we'd fire twice per edit and double-send.
	local existing = self.byPath[path]
	if existing ~= nil then
		disconnectAll(existing)
		self.byInstance[existing.instance] = nil
	end

	local entry: Entry = {
		instance = instance,
		path = path,
		connection = nil :: any,
		destroyConnection = nil :: any,
		nameConnection = nil :: any,
		ancestryConnection = nil :: any,
		gen = 0,
		pendingContent = nil,
		pendingHash = nil,
	}
	self.byPath[path] = entry
	self.byInstance[instance] = entry

	entry.destroyConnection = instance.Destroying:Connect(function()
		local current = self.byInstance[instance]
		if current ~= entry then
			return
		end
		-- Forward the deletion before untracking so the handler still sees
		-- consistent state.
		self.log(`-> file_deleted {entry.path}`)
		self.onDelete(entry.path)
		self:untrack(entry.path)
	end)

	entry.nameConnection = instance:GetPropertyChangedSignal("Name"):Connect(function()
		local current = self.byInstance[instance]
		if current ~= entry then
			return
		end
		self.log(`[debug] Name changed: tracked={entry.path} now={instance.Name}`)
		local newPath = syncPath(self, entry)
		if newPath == nil then
			-- Renaming pushed the instance out of every mapping —
			-- treat as delete.
			local oldPath = entry.path
			self.log(`-> file_deleted {oldPath} (renamed out of mapping)`)
			self.onDelete(oldPath)
			self:untrack(oldPath)
		end
	end)

	entry.ancestryConnection = instance.AncestryChanged:Connect(function(_, parent)
		local current = self.byInstance[instance]
		if current ~= entry then
			return
		end
		if parent == nil then
			-- Detached from the DataModel; `Destroying` will follow if
			-- it's actually being destroyed. Treat as delete now so the
			-- daemon stops broadcasting to a stale path.
			local oldPath = entry.path
			self.log(`-> file_deleted {oldPath} (detached from DataModel)`)
			self.onDelete(oldPath)
			self:untrack(oldPath)
			return
		end
		local newPath = syncPath(self, entry)
		if newPath == nil then
			local oldPath = entry.path
			self.log(`-> file_deleted {oldPath} (moved out of mapping)`)
			self.onDelete(oldPath)
			self:untrack(oldPath)
		end
	end)

	entry.connection = instance:GetPropertyChangedSignal("Source"):Connect(function()
		local current = readSource(instance)
		if current == nil then
			return
		end
		-- Resolve path on every fire — the user can have renamed the
		-- script in the same tick that the Source signal fired (e.g.
		-- script-template insertion ones writes Source then Name in
		-- close succession). Going through `syncPath` first ensures the
		-- daemon receives the rename frame before the file_changed.
		local livePath = syncPath(self, entry)
		if livePath == nil then
			-- Instance escaped every mapping; rename callback already
			-- emitted (if applicable) and the syncPath helper logged.
			return
		end
		local syncHash = self.hash(current)
		if self.isEcho(livePath, syncHash) then
			-- Our own write; Applier already seeded `recentlyApplied` with this
			-- hash, so the change is a no-op from the sync perspective.
			return
		end
		-- REGRESSION(A3): capture the user's edit synchronously, before the
		-- debounce. If a daemon FileChanged overwrites Source with disk content
		-- inside the window, the trailing send still reports THIS captured
		-- value instead of the clobbered one — the user's edit is never lost.
		entry.pendingContent = current
		entry.pendingHash = syncHash

		entry.gen += 1
		local myGen = entry.gen
		task.delay(DEBOUNCE_SECONDS, function()
			if self.byInstance[instance] ~= entry or entry.gen ~= myGen then
				-- Superseded by a later signal fire or by untrack.
				return
			end
			local capturedContent = entry.pendingContent
			local capturedHash = entry.pendingHash
			-- Consume the capture; a later edit re-captures a fresh one.
			entry.pendingContent = nil
			entry.pendingHash = nil
			local latest = readSource(instance)
			if latest == nil then
				return
			end
			local finalPath = syncPath(self, entry)
			if finalPath == nil then
				return
			end
			local latestHash = self.hash(latest)
			if not self.isEcho(finalPath, latestHash) then
				-- Common path: the current Source is a genuine (non-echo) user
				-- state — send it. Correctly reflects an undo/redo the user made
				-- during the debounce window.
				self.log(`-> file_changed {finalPath}`)
				self.onEdit(finalPath, latest, latestHash)
				return
			end
			-- REGRESSION(A3): the current Source matches a recent Applier write
			-- (echo) — a daemon FileChanged clobbered the user's edit inside the
			-- window. Forward the CAPTURED edit so it reaches the daemon
			-- (surfacing as a conflict) instead of vanishing silently.
			if capturedContent ~= nil and capturedHash ~= nil
				and not self.isEcho(finalPath, capturedHash)
			then
				self.log(`-> file_changed {finalPath} (recovered pre-clobber edit)`)
				self.onEdit(finalPath, capturedContent, capturedHash)
			end
		end)
	end)
end

function SourceWatcher.untrack(self: SourceWatcher, path: string)
	local entry = self.byPath[path]
	if entry == nil then
		return
	end
	disconnectAll(entry)
	self.byPath[path] = nil
	self.byInstance[entry.instance] = nil
end

-- Returns the pending, not-yet-forwarded user edit captured for `path` at the
-- last Source-change signal, or (nil, nil) when none is buffered. The Applier
-- consults this before overwriting Source so a daemon FileChanged landing
-- inside the debounce window doesn't silently clobber the user's in-flight
-- edit (A3).
function SourceWatcher.peekPending(self: SourceWatcher, path: string): (string?, string?)
	local entry = self.byPath[path]
	if entry == nil then
		return nil, nil
	end
	return entry.pendingContent, entry.pendingHash
end

function SourceWatcher.destroy(self: SourceWatcher)
	if self.destroyed then
		return
	end
	self.destroyed = true
	for _, entry in self.byPath do
		disconnectAll(entry)
	end
	self.byPath = {}
	self.byInstance = {}
end

return SourceWatcher
