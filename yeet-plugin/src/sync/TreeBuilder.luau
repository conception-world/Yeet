--!strict

-- Given a Rojo project tree and a set of file snapshots, materializes the
-- corresponding instance tree inside the DataModel. Pure functions only —
-- state (file -> instance map) lives in Applier.
--
-- The traversal honors a deliberate subset of Rojo's rules for Phase 1:
--   * `$path` on a node maps the node's Instance to a filesystem directory
--     (or a single file). Every snapshot below that path becomes a descendant
--     Script/ModuleScript/LocalScript of that Instance.
--   * `$className` overrides the default "Folder" for intermediate nodes.
--   * Top-level children of DataModel are resolved via `game:GetService`.
--   * `init.luau` / `init.client.luau` / `init.server.luau` promote the
--     parent directory to a Script/LocalScript/ModuleScript whose name is
--     the directory's name (matches Rojo, Argon, and the daemon's syncback
--     output in `syncback.rs`). Sibling files become children of that
--     promoted script.
--   * `.meta.json` / `$properties` / `$ignoreUnknownInstances` are NOT
--     honored here. If they matter, the daemon's `project.rs` must grow
--     the same support first.

local TreeBuilder = {}

export type ScriptKind = "module_script" | "script" | "local_script"

export type FileSnapshot = {
	path: string,
	kind: ScriptKind,
	content: string,
	sha256: string,
}

export type ProjectTreeNode = {
	["$className"]: string?,
	["$path"]: string?,
	[string]: any,
}

export type Project = {
	name: string,
	tree: ProjectTreeNode,
}

export type Mappings = { [string]: Instance }

-- When a Script already existed in the `DataModel` with a `Source` that
-- disagrees with the daemon's snapshot, `build` records it here instead of
-- clobbering. Caller is expected to echo the Studio `Source` back to the
-- daemon via `FileChanged`, so the merge engine surfaces the divergence
-- (usually as a conflict — that's the whole point).
export type Divergent = {
	path: string,
	kind: ScriptKind,
	studio_content: string,
}

-- Scripts present in the `DataModel` under a mapped `$path` but absent from
-- the daemon's `initial_files` snapshot — i.e. scripts the user created in
-- Studio while the daemon was offline. Caller echoes each back so it shows
-- up in the bulk sync preview as `studio_only`.
export type Untracked = {
	path: string,
	kind: ScriptKind,
	studio_content: string,
}

-- `untracked` is deliberately NOT part of the build result: it is discovered
-- read-only via `TreeBuilder.findUntracked`, which the caller runs OUTSIDE the
-- ChangeHistoryService recording so its yields don't hold the recording open
-- across frames (M9).
export type BuildResult = {
	fileToInstance: { [string]: Instance },
	mappings: Mappings,
	divergent: { Divergent },
}

local function classFor(kind: ScriptKind): string
	if kind == "module_script" then
		return "ModuleScript"
	elseif kind == "script" then
		return "Script"
	elseif kind == "local_script" then
		return "LocalScript"
	end
	error(`unknown script kind: {kind}`)
end

-- File-name suffix the daemon's `syncback.rs::script_suffix` emits per kind.
-- Used by `computePath` / `fileNameFor` to round-trip Studio→disk paths.
local function suffixFor(kind: ScriptKind): string
	if kind == "module_script" then
		return ".luau"
	elseif kind == "local_script" then
		return ".client.luau"
	end
	return ".server.luau"
end

-- `init.luau` / `init.client.luau` / `init.server.luau` (and `.lua` variants)
-- promote the *containing directory* into a script of the matching kind. This
-- is the pattern Rojo, Argon, and the daemon's syncback all emit, and is what
-- finally routes a `ModuleScript Foo` with children to `Foo/init.luau` instead
-- of a sibling `Foo.luau`. Returns `nil` for non-init file names.
local INIT_FILE_KINDS: { [string]: ScriptKind } = {
	["init.luau"] = "module_script",
	["init.lua"] = "module_script",
	["init.server.luau"] = "script",
	["init.server.lua"] = "script",
	["init.client.luau"] = "local_script",
	["init.client.lua"] = "local_script",
}

local function initKind(fileName: string): ScriptKind?
	return INIT_FILE_KINDS[fileName]
end

-- True when the basename of `path` matches one of Rojo's init patterns.
-- Mirrors the daemon's `protocol::is_init_filename` so client- and
-- server-side sort keys agree. Case-insensitive: a Windows-renamed
-- `Init.luau` resolves to the same file as `init.luau`, so the sort
-- must treat them identically.
local function isInitFilename(path: string): boolean
	local basename = path:match("([^/]+)$") or path
	return INIT_FILE_KINDS[basename:lower()] ~= nil
end

-- `foo.server.luau` / `foo.client.lua` / `foo.luau` -> `foo`.
-- The daemon already classified the kind; here we just recover the instance name.
local function scriptNameFromFile(fileName: string): string
	local stem = (fileName:gsub("%.luau?$", ""))
	stem = (stem:gsub("%.server$", ""))
	stem = (stem:gsub("%.client$", ""))
	return stem
end

local function splitPath(path: string): { string }
	local parts: { string } = {}
	for part in path:gmatch("[^/]+") do
		table.insert(parts, part)
	end
	return parts
end

-- Creates a child of `className` named `name` under `parent`, reusing an
-- existing child if its ClassName matches. When the ClassName differs we
-- destroy and replace — mirroring what Rojo would do for a filesystem-driven
-- rename of the underlying file. Returns the instance and a flag telling the
-- caller whether the instance is fresh or pre-existing; the distinction
-- matters for bootstrap because we must not overwrite a pre-existing
-- `Source` until the merge engine has had its say.
local function ensureChild(parent: Instance, name: string, className: string): (Instance, boolean)
	local existing = parent:FindFirstChild(name)
	if existing and existing.ClassName == className then
		return existing, true
	end
	if existing then
		existing:Destroy()
	end
	local child = Instance.new(className)
	child.Name = name
	child.Parent = parent
	return child, false
end

-- Reuses any existing child by `name` regardless of class — used for
-- intermediate path components. Coexists with `ensureChild` because here
-- the script kind isn't known until we see whether the directory contains
-- `init.[ext]`: a sibling `Foo/Helpers.luau` arriving before `Foo/init.luau`
-- mustn't destroy `Foo` if it's already a ModuleScript created by an earlier
-- snapshot. Falls back to creating a Folder when no child exists yet.
local function ensureContainer(parent: Instance, name: string): Instance
	local existing = parent:FindFirstChild(name)
	if existing ~= nil then
		return existing
	end
	local fresh = Instance.new("Folder")
	fresh.Name = name
	fresh.Parent = parent
	return fresh
end

-- Promotes (or creates) `parent/name` to a script of `className`, preserving
-- any pre-existing children. Three cases:
--   1. No child by that name yet → create one with `className`.
--   2. Child exists with matching `className` → reuse (returns `preExisting=true`
--      so the caller can fold the divergence detection through).
--   3. Child exists with a different class (typically Folder, because sibling
--      files were materialized first) → instantiate the new script, reparent
--      every child of the old node into it, then destroy the old one. The
--      reparent ordering matters: we rename the fresh instance after the
--      original is gone so name collisions can't fire mid-swap.
local function ensureContainerAsScript(
	parent: Instance,
	name: string,
	className: string
): (Instance, boolean)
	local existing = parent:FindFirstChild(name)
	if existing == nil then
		local fresh = Instance.new(className)
		fresh.Name = name
		fresh.Parent = parent
		return fresh, false
	end
	if existing.ClassName == className then
		return existing, true
	end
	local fresh = Instance.new(className)
	fresh.Name = name .. "__yeet_swap"
	fresh.Parent = parent
	for _, child in existing:GetChildren() do
		child.Parent = fresh
	end
	existing:Destroy()
	fresh.Name = name
	return fresh, false
end

-- Resolves a named child of the *tree root* (DataModel). Services must be
-- accessed via `GetService`, not `Instance.new`, so they get their own path.
local function ensureNode(parent: Instance, name: string, node: ProjectTreeNode): Instance
	local className = node["$className"] or "Folder"
	if parent == game then
		local ok, service = pcall(function()
			return game:GetService(name :: any)
		end)
		if ok then
			return service
		end
		local child, _ = ensureChild(parent, name, className)
		return child
	end
	local child, _ = ensureChild(parent, name, className)
	return child
end

local function traverseTree(node: ProjectTreeNode, parent: Instance, mappings: Mappings)
	local path = node["$path"]
	if path ~= nil then
		mappings[path] = parent
	end
	for key, child in pairs(node :: any) do
		if typeof(key) ~= "string" then
			continue
		end
		if (key :: string):sub(1, 1) == "$" then
			continue
		end
		local childNode = child :: ProjectTreeNode
		local childInstance = ensureNode(parent, key :: string, childNode)
		traverseTree(childNode, childInstance, mappings)
	end
end

-- Picks the longest `$path` that is a prefix of `snap.path`. Matters when one
-- mapping nests inside another's directory on disk (rare but legal in Rojo).
local function bestMapping(snap: FileSnapshot, mappings: Mappings): (string?, Instance?)
	local bestKey: string? = nil
	local bestInst: Instance? = nil
	local bestLen = -1
	for mapPath, parent in pairs(mappings) do
		local isExact = snap.path == mapPath
		local isDescendant = snap.path:sub(1, #mapPath + 1) == (mapPath .. "/")
		if (isExact or isDescendant) and #mapPath > bestLen then
			bestKey = mapPath
			bestInst = parent
			bestLen = #mapPath
		end
	end
	return bestKey, bestInst
end

-- Honors a nested-project `$path` mount whose package module maps directly onto
-- the mapping Instance itself (AUDITORIA-YEET.md A10 `wally-1`): a Wally package
-- `default.project.json` (`{name, tree:{$path:"src"}}`) makes the package folder
-- THE ModuleScript. When the mapped Instance is already a `LuaSourceContainer`
-- we set its `Source` (reporting a divergence rather than clobbering, exactly as
-- the leaf path does). A `Folder`/service mount can't be re-classed here, so it
-- returns nil and the snapshot is dropped — unchanged from before A10.
local function materializeAtMount(parent: Instance, snap: FileSnapshot): (Instance?, string?)
	if not parent:IsA("LuaSourceContainer") then
		return nil, nil
	end
	local existingSource = (parent :: LuaSourceContainer).Source
	if existingSource ~= snap.content then
		return parent, existingSource
	end
	;(parent :: LuaSourceContainer).Source = snap.content
	return parent, nil
end

-- Materializes a single file snapshot as an Instance in the tree, returning the
-- resulting Script-ish Instance and the `Source` that was present before we
-- touched it (nil when the instance was fresh). Intermediate directory
-- components become Folders, except those promoted by an `init.[ext]` sibling
-- to a Script of the matching kind. A file landing directly on a `$path` mount
-- whose Instance is already a script sets that Instance's Source in place
-- (nested-project package mount, A10). Returns nil + nil when the file falls
-- outside every `$path` mapping or lands on a mount that is a Folder/service
-- (which can't be re-classed here).
local function materializeFile(
	snap: FileSnapshot,
	mappings: Mappings
): (Instance?, string?)
	local mapPath, parent = bestMapping(snap, mappings)
	if mapPath == nil or parent == nil then
		return nil, nil
	end
	local relPath = if snap.path == mapPath then "" else snap.path:sub(#mapPath + 2)
	local parts = splitPath(relPath)
	if #parts == 0 then
		-- `$path` pointing directly at a single file/module: the Instance at
		-- this node *is* the script (a nested-project package folder that
		-- became the ModuleScript, A10). Honor it in place when it's a script.
		return materializeAtMount(parent, snap)
	end

	local fileName = parts[#parts]
	local promotedKind = initKind(fileName)

	if promotedKind ~= nil then
		-- `init.[ext]` promotes its parent directory into a script of
		-- `promotedKind`. With only one component (the init file itself) the
		-- package module maps onto the mapping Instance itself — a nested-project
		-- mount (A10). Promote in place when it's already a script; a
		-- service / Folder `$path` mount can't be re-classed, so it still drops.
		if #parts < 2 then
			return materializeAtMount(parent, snap)
		end
		local container: Instance = parent
		for i = 1, #parts - 2 do
			container = ensureContainer(container, parts[i])
		end
		local scriptName = parts[#parts - 1]
		local className = classFor(promotedKind)
		local scriptInstance, preExisting = ensureContainerAsScript(container, scriptName, className)
		local priorSource: string? = nil
		if preExisting then
			local existingSource = (scriptInstance :: LuaSourceContainer).Source
			if existingSource ~= snap.content then
				priorSource = existingSource
				return scriptInstance, priorSource
			end
		end
		;(scriptInstance :: LuaSourceContainer).Source = snap.content
		return scriptInstance, priorSource
	end

	local container: Instance = parent
	for i = 1, #parts - 1 do
		container = ensureContainer(container, parts[i])
	end

	local scriptName = scriptNameFromFile(fileName)
	local className = classFor(snap.kind)
	local scriptInstance, preExisting = ensureChild(container, scriptName, className)
	local priorSource: string? = nil
	if preExisting then
		local existingSource = (scriptInstance :: LuaSourceContainer).Source
		if existingSource ~= snap.content then
			-- Studio's in-memory content diverges from what the daemon
			-- expects on disk. Keep the Studio Source intact so the merge
			-- engine, once we echo this back, can surface a proper conflict
			-- instead of the bootstrap silently winning.
			priorSource = existingSource
			return scriptInstance, priorSource
		end
	end
	;(scriptInstance :: LuaSourceContainer).Source = snap.content
	return scriptInstance, priorSource
end

-- Returns the kind of `inst` (script-like) when it is one, plus a flag
-- indicating whether it should land on disk under the `Name/init.[ext]`
-- promotion shape. `nil` when the instance isn't script-like.
--
-- The promotion criterion mirrors the daemon's syncback (see
-- `syncback.rs::write_instance`, branch `(Some(kind), false)`): any direct
-- child is enough, regardless of class. This keeps Studio→disk paths
-- round-trippable — picking a different criterion here would have the
-- plugin emit `Foo.luau` while the next syncback rewrites the same
-- instance to `Foo/init.luau`, churning the file on every reconnect.
local function classifyInstance(inst: Instance): (ScriptKind?, boolean)
	local kind: ScriptKind?
	if inst:IsA("ModuleScript") then
		kind = "module_script"
	elseif inst:IsA("LocalScript") then
		kind = "local_script"
	elseif inst:IsA("Script") then
		kind = "script"
	else
		return nil, false
	end
	return kind, #inst:GetChildren() > 0
end

-- Inverse of `materializeFile`'s naming rules. For a leaf script (no
-- script descendants) returns `<name>[.server|.client].luau`. For a script
-- with descendants returns `init[.server|.client].luau` — caller is
-- responsible for prepending the script's name as a directory.
local function fileNameFor(inst: Instance): (string, ScriptKind, boolean)
	local kind, promote = classifyInstance(inst)
	-- Caller in this module always pre-checks `IsA("LuaSourceContainer")`,
	-- so `kind == nil` indicates a programmer error rather than runtime
	-- input. Surface it instead of returning a bogus name.
	assert(kind ~= nil, "fileNameFor called on non-script instance")
	local extension = suffixFor(kind)
	local fileBaseName = if promote then "init" else inst.Name
	return fileBaseName .. extension, kind, promote
end

-- Walks each `$path`-mapped subtree of the DataModel and collects
-- `LuaSourceContainer` descendants that aren't already in `fileToInstance`.
-- These are "created in Studio while the daemon was off" scripts; without
-- reporting them the bulk sync preview only sees disk-side files and the
-- user's new `SystemTest` (or whatever) stays invisible.
--
-- Read-only. The bootstrap caller runs this OUTSIDE its ChangeHistoryService
-- recording (M9) so the yields below never hold a recording open across
-- frames. Yields every YIELD_EVERY descendants — the same cadence as
-- `discover` — so a place with thousands of instances doesn't freeze the main
-- thread while the daemon is being bootstrapped.
function TreeBuilder.findUntracked(
	mappings: Mappings,
	fileToInstance: { [string]: Instance }
): { Untracked }
	local out: { Untracked } = {}
	local YIELD_EVERY = 200
	local processed = 0
	for mapPath, parent in pairs(mappings) do
		for _, desc in parent:GetDescendants() do
			processed += 1
			if processed % YIELD_EVERY == 0 then
				task.wait()
			end
			if not desc:IsA("LuaSourceContainer") then
				continue
			end
			-- Chain from parent's direct child down to `desc`: append leaf→root
			-- then reverse in place (was O(depth^2) via insert-at-1; A11).
			local chain: { string } = {}
			local cursor: Instance? = desc
			while cursor ~= nil and cursor ~= parent do
				table.insert(chain, cursor.Name)
				cursor = cursor.Parent
			end
			if cursor ~= parent or #chain == 0 then
				continue
			end
			local left = 1
			local right = #chain
			while left < right do
				chain[left], chain[right] = chain[right], chain[left]
				left += 1
				right -= 1
			end
			local leafName, kind, promote = fileNameFor(desc)
			if promote then
				-- `desc.Name` stays as a directory; the file lives inside it.
				table.insert(chain, leafName)
			else
				chain[#chain] = leafName
			end
			local relPath = table.concat(chain, "/")
			local fullPath = mapPath .. "/" .. relPath
			if fileToInstance[fullPath] ~= nil then
				continue
			end
			local ok, source = pcall(function()
				return (desc :: LuaSourceContainer).Source
			end)
			if not ok then
				continue
			end
			table.insert(out, {
				path = fullPath,
				kind = kind,
				studio_content = source,
			})
		end
	end
	return out
end

function TreeBuilder.build(project: Project, snapshots: { FileSnapshot }): BuildResult
	local mappings: Mappings = {}
	traverseTree(project.tree, game, mappings)
	local fileToInstance: { [string]: Instance } = {}
	local divergent: { Divergent } = {}
	-- Sort init files before non-init siblings within each parent dir, and
	-- shallower paths before deeper ones. Mirrors the daemon's
	-- `sort_paths_for_apply`. Without this, a `Foo/Bar.luau` snapshot
	-- materializes BEFORE `Foo/init.luau`, the parent ends up as a Folder,
	-- and the later init promotion has to swap it — a chain that breaks
	-- intermittently when ChangeHistoryService says "busy" mid-bootstrap
	-- and one swap silently no-ops.
	local sorted: { FileSnapshot } = table.clone(snapshots)
	table.sort(sorted, function(a, b)
		local depthA = select(2, a.path:gsub("/", "/"))
		local depthB = select(2, b.path:gsub("/", "/"))
		if depthA ~= depthB then
			return depthA < depthB
		end
		local initA = isInitFilename(a.path)
		local initB = isInitFilename(b.path)
		if initA ~= initB then
			return initA -- true sorts first
		end
		return a.path < b.path
	end)
	for _, snap in ipairs(sorted) do
		local created, priorSource = materializeFile(snap, mappings)
		if created ~= nil then
			fileToInstance[snap.path] = created
			if priorSource ~= nil then
				table.insert(divergent, {
					path = snap.path,
					kind = snap.kind,
					studio_content = priorSource,
				})
			end
		end
	end
	return {
		fileToInstance = fileToInstance,
		mappings = mappings,
		divergent = divergent,
	}
end

-- Computes the script's project-relative path given the mapping parent it
-- sits under. Returns `nil` when `desc` isn't a `LuaSourceContainer`
-- descendant of `mapParent`. Used by the Applier's live watcher to turn a
-- freshly-added Studio instance into a path the daemon can track. Promotes
-- to `Name/init.[ext]` when `desc` already has script descendants, matching
-- the daemon's syncback shape.
local function computePath(
	mapPath: string,
	mapParent: Instance,
	desc: Instance
): (string?, ScriptKind?)
	if not desc:IsA("LuaSourceContainer") then
		return nil, nil
	end
	-- Collect names leaf→root by appending (O(1) each) and reverse in place.
	-- The old `table.insert(chain, 1, ...)` shifted the whole array on every
	-- ancestor, so a deep instance was O(depth^2); when a large moved/deleted
	-- subtree drives this once per descendant that quadratic dominates (A11).
	-- REGRESSION(A11): the reversed chain is byte-identical to what the old
	-- prepend produced — root first, `desc` last — so the emitted path is
	-- unchanged.
	local chain: { string } = {}
	local cursor: Instance? = desc
	while cursor ~= nil and cursor ~= mapParent do
		table.insert(chain, cursor.Name)
		cursor = cursor.Parent
	end
	if cursor ~= mapParent or #chain == 0 then
		return nil, nil
	end
	local left = 1
	local right = #chain
	while left < right do
		chain[left], chain[right] = chain[right], chain[left]
		left += 1
		right -= 1
	end
	local leafName, kind, promote = fileNameFor(desc)
	if promote then
		table.insert(chain, leafName)
	else
		chain[#chain] = leafName
	end
	local relPath = table.concat(chain, "/")
	return mapPath .. "/" .. relPath, kind
end

-- Dry-run counterpart to `traverseTree`: resolves the project tree against the
-- live DataModel *without* creating any missing Folder/service, so a Preview
-- connect doesn't leave orphan containers behind in a place the user then
-- decides to skip. Paths whose chain can't be walked (e.g. a service not yet
-- present) are dropped from `mappings` — they surface as pure IDE-only.
local function traverseTreeDryRun(
    node: ProjectTreeNode,
    parent: Instance?,
    mappings: Mappings
)
    if parent == nil then
        return
    end
    local path = node["$path"]
    if path ~= nil then
        mappings[path] = parent
    end
    for key, child in pairs(node :: any) do
        if typeof(key) ~= "string" then
            continue
        end
        if (key :: string):sub(1, 1) == "$" then
            continue
        end
        local childNode = child :: ProjectTreeNode
        local childInst: Instance? = nil
        if parent == game then
            local ok, svc = pcall(function()
                return game:GetService(key :: any)
            end)
            if ok then
                childInst = svc
            end
        else
            childInst = parent:FindFirstChild(key :: string)
        end
        traverseTreeDryRun(childNode, childInst, mappings)
    end
end

-- Discover the project's `$path` → Instance mappings and collect a snapshot of
-- every `LuaSourceContainer` under them. Used by the bootstrap-preview flow:
-- the plugin hands this snapshot to the daemon, which diffs it against disk
-- and emits `BulkSyncPreview` before any mutation occurs on either side.
--
-- Each `scripts` entry includes the live `Instance` so the Applier can
-- seat it in `fileToInstance` immediately. Skipping that step would mean
-- a follow-up `FileChanged` from the daemon (after the user picks "Keep
-- Studio" / does nothing for an untouched file, then edits it on disk)
-- would log "unknown; skipped" and the Studio source would never update.
export type DiscoveredScript = {
    path: string,
    kind: ScriptKind,
    studio_content: string,
    instance: Instance,
}

function TreeBuilder.discover(project: Project): { mappings: Mappings, scripts: { DiscoveredScript } }
    local mappings: Mappings = {}
    traverseTreeDryRun(project.tree, game, mappings)
    local scripts: { DiscoveredScript } = {}
    -- Yield every YIELD_EVERY descendants so a place with thousands of
    -- instances doesn't block the main thread for several seconds.
    -- Without this the user perceives the plugin as a freeze/crash on
    -- bootstrap-preview against a real production place. 200 is a
    -- compromise: small enough to keep frame budget under ~16ms even on
    -- weak hardware, large enough that the per-yield overhead doesn't
    -- dominate for small projects.
    local YIELD_EVERY = 200
    local processed = 0
    for mapPath, parent in pairs(mappings) do
        for _, desc in parent:GetDescendants() do
            processed += 1
            if processed % YIELD_EVERY == 0 then
                task.wait()
            end
            local path, kind = computePath(mapPath, parent, desc)
            if path == nil or kind == nil then
                continue
            end
            local ok, source = pcall(function()
                return (desc :: LuaSourceContainer).Source
            end)
            if not ok then
                continue
            end
            table.insert(scripts, {
                path = path :: string,
                kind = kind :: ScriptKind,
                studio_content = source,
                instance = desc,
            })
        end
    end
    return { mappings = mappings, scripts = scripts }
end

-- Returns the `Instance.Name` a script should have after being renamed to
-- `path`. For `src/Foo/init.luau`, that's `Foo` (the parent dir); for
-- `src/Foo.luau` or `src/Foo.server.luau`, that's `Foo`. Used by
-- `Applier.applyFileRenamed` to update `instance.Name` after a path change.
local function basenameFor(path: string): string
    local parts = splitPath(path)
    if #parts == 0 then
        return ""
    end
    local last = parts[#parts]
    if initKind(last:lower()) ~= nil then
        if #parts >= 2 then
            return parts[#parts - 1]
        end
        return ""
    end
    return scriptNameFromFile(last)
end

TreeBuilder.classFor = classFor
TreeBuilder.scriptNameFromFile = scriptNameFromFile
TreeBuilder.materializeFile = materializeFile
TreeBuilder.computePath = computePath
TreeBuilder.isInitFilename = isInitFilename
TreeBuilder.basenameFor = basenameFor

return TreeBuilder
