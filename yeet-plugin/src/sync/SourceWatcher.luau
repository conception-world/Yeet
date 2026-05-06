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

export type EditHandler = (path: string, content: string, hash: string) -> ()
export type DeleteHandler = (path: string) -> ()
export type EchoPredicate = (path: string, hash: string) -> boolean
export type Hasher = (content: string) -> string
export type Log = (line: string) -> ()

type Entry = {
	instance: Instance,
	connection: RBXScriptConnection,
	-- Fires once when the user deletes the tracked script in Studio; we
	-- forward that as a `file_deleted` upward to the daemon.
	destroyConnection: RBXScriptConnection,
	-- Incremented on every non-echo signal fire; the scheduled task carries
	-- a copy of the value at the time it was enqueued and bails out if the
	-- entry's gen has advanced (meaning another fire superseded it).
	gen: number,
}

type Options = {
	log: Log,
	onEdit: EditHandler,
	onDelete: DeleteHandler,
	isEcho: EchoPredicate,
	hash: Hasher,
}

local SourceWatcher = {}
SourceWatcher.__index = SourceWatcher

type self = {
	byPath: { [string]: Entry },
	log: Log,
	onEdit: EditHandler,
	onDelete: DeleteHandler,
	isEcho: EchoPredicate,
	hash: Hasher,
	destroyed: boolean,
}

export type SourceWatcher = typeof(setmetatable({} :: self, SourceWatcher))

function SourceWatcher.new(opts: Options): SourceWatcher
	return setmetatable({
		byPath = {},
		log = opts.log,
		onEdit = opts.onEdit,
		onDelete = opts.onDelete,
		isEcho = opts.isEcho,
		hash = opts.hash,
		destroyed = false,
	}, SourceWatcher)
end

local function readSource(instance: Instance): string?
	-- Only LuaSourceContainer descendants carry `Source`. Everything we track
	-- is created by TreeBuilder, but type-narrow defensively anyway.
	if instance:IsA("LuaSourceContainer") then
		return (instance :: LuaSourceContainer).Source
	end
	return nil
end

function SourceWatcher.track(self: SourceWatcher, path: string, instance: Instance)
	if self.destroyed then
		return
	end
	-- Re-tracking an already-tracked path must drop the old connection first,
	-- otherwise we'd fire twice per edit and double-send.
	local existing = self.byPath[path]
	if existing ~= nil then
		existing.connection:Disconnect()
		existing.destroyConnection:Disconnect()
	end

	local entry: Entry = {
		instance = instance,
		connection = nil :: any,
		destroyConnection = nil :: any,
		gen = 0,
	}
	self.byPath[path] = entry

	entry.destroyConnection = instance.Destroying:Connect(function()
		local current = self.byPath[path]
		if current ~= entry then
			return
		end
		-- Forward the deletion before untracking so the handler still sees
		-- consistent state.
		self.log(`-> file_deleted {path}`)
		self.onDelete(path)
		self:untrack(path)
	end)

	entry.connection = instance:GetPropertyChangedSignal("Source"):Connect(function()
		local current = readSource(instance)
		if current == nil then
			return
		end
		local syncHash = self.hash(current)
		if self.isEcho(path, syncHash) then
			-- Our own write; Applier already seeded `recentlyApplied` with this
			-- hash, so the change is a no-op from the sync perspective.
			return
		end

		entry.gen += 1
		local myGen = entry.gen
		task.delay(DEBOUNCE_SECONDS, function()
			local stillTracked = self.byPath[path]
			if stillTracked == nil or stillTracked ~= entry or entry.gen ~= myGen then
				-- Superseded by a later signal fire or by untrack.
				return
			end
			local latest = readSource(instance)
			if latest == nil then
				return
			end
			local finalHash = self.hash(latest)
			-- Re-check: the daemon may have pushed a FileChanged during the
			-- 200ms window; if our current Source already matches what the
			-- Applier last wrote, there is nothing new to report.
			if self.isEcho(path, finalHash) then
				return
			end
			self.log(`-> file_changed {path}`)
			self.onEdit(path, latest, finalHash)
		end)
	end)
end

function SourceWatcher.untrack(self: SourceWatcher, path: string)
	local entry = self.byPath[path]
	if entry == nil then
		return
	end
	entry.connection:Disconnect()
	entry.destroyConnection:Disconnect()
	-- Bump gen so any in-flight debounced task for this entry exits early.
	entry.gen += 1
	self.byPath[path] = nil
end

function SourceWatcher.destroy(self: SourceWatcher)
	if self.destroyed then
		return
	end
	self.destroyed = true
	for _, entry in self.byPath do
		entry.connection:Disconnect()
		entry.destroyConnection:Disconnect()
		entry.gen += 1
	end
	self.byPath = {}
end

return SourceWatcher
