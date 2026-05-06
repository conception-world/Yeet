--!strict

-- Walks the live `DataModel` and produces the flat `SerializedInstance` list
-- the daemon consumes (see protocol.rs `SerializedInstance`). Identity is by
-- plugin-assigned u64 ids; the `DataModel` root gets `parent_id = nil`, which
-- is how the daemon locates it.
--
-- The walker does not split the list into chunks — that's the sender's job.
-- It also does not apply the `include_non_script` filter: the daemon already
-- enforces that server-side (see `write_instance` in syncback.rs), and the
-- plugin keeps the payload class-agnostic. What the walker *does* handle is
-- the Minimal-template shape: services with zero descendants are dropped so
-- we don't materialize 30 empty folders per place.

local CollectionService = game:GetService("CollectionService")
local Selection = game:GetService("Selection")

local Base64 = require(script.Parent.Base64)
local PropertyTable = require(script.Parent.PropertyTable)

-- Classes worth round-tripping as a `.rbxm` blob when the user opts in.
-- BinaryStringValue is special — its `.Value` is already raw bytes, no
-- ExportSelection needed and no Selection mutation. Everything else routes
-- through ExportSelection (only when the caller hands us a `plugin` handle).
local BINARY_EXPORT_CLASSES: { string } = {
	"MeshPart",
	"UnionOperation",
	"NegateOperation",
	"PartOperation",
}

-- Luau's type system doesn't carry exports across `require()` boundaries, so
-- we redeclare the wire-format shapes locally. They must stay in sync with
-- `PropertyTable.luau` and with `SerializedProperty` in `protocol.rs`.
type Tag =
	"string" | "bool" | "number"
	| "vector2" | "vector3" | "cframe"
	| "color3" | "udim" | "udim2"
	| "enum" | "brickcolor"

type Value =
	{ type: "string", value: string }
	| { type: "bool", value: boolean }
	| { type: "number", value: number }
	| { type: "vector2", value: { number } }
	| { type: "vector3", value: { number } }
	| { type: "cframe", value: { number } }
	| { type: "color3", value: { number } }
	| { type: "udim", value: { number } }
	| { type: "udim2", value: { number } }
	| { type: "enum", value: string }
	| { type: "brickcolor", value: number }

export type BinaryPayload = {
	class_name: string,
	rbxm_bytes_base64: string,
}

export type SerializedInstance = {
	id: number,
	parent_id: number?,
	class_name: string,
	name: string,
	properties: { [string]: Value },
	attributes: { [string]: Value },
	tags: { string },
	binary: BinaryPayload?,
}

export type WalkOptions = {
	-- When true (default) empty services are kept; when false, a service with
	-- no descendants is omitted entirely. Minimal template uses `false`.
	keep_empty_services: boolean?,
	-- Opt-in for the binary `.rbxm` pipeline. With this off, classes whose
	-- value can't round-trip through the property table just lose fidelity
	-- (intentional: keeps the wire payload script-only and predictable).
	include_binary: boolean?,
	-- Required for `BINARY_EXPORT_CLASSES` (MeshPart etc.); the plugin handle
	-- is the only path to `plugin:ExportSelection`. BinaryStringValue ignores
	-- this and reads `.Value` directly. When omitted, only BinaryStringValue
	-- gets the binary treatment and richer classes silently fall back.
	plugin: Plugin?,
}

export type WalkResult = {
	instances: { SerializedInstance },
	services_included: { string },
	root_id: number,
}

local Walker = {}

-- Encode attributes by dispatching on Luau's runtime type tag. Unknown types
-- are silently dropped — this mirrors how we handle unknown property types
-- in PropertyTable: one bad attribute should never abort a syncback.
local function tagForAttribute(raw: any): Tag?
	local t = typeof(raw)
	if t == "string" then return "string"
	elseif t == "boolean" then return "bool"
	elseif t == "number" then return "number"
	elseif t == "Vector2" then return "vector2"
	elseif t == "Vector3" then return "vector3"
	elseif t == "CFrame" then return "cframe"
	elseif t == "Color3" then return "color3"
	elseif t == "UDim" then return "udim"
	elseif t == "UDim2" then return "udim2"
	elseif t == "EnumItem" then return "enum"
	elseif t == "BrickColor" then return "brickcolor"
	end
	return nil
end

local function encodeAttributes(inst: Instance): { [string]: Value }
	local out: { [string]: Value } = {}
	local ok, attrs = pcall(function()
		return inst:GetAttributes()
	end)
	if not ok or type(attrs) ~= "table" then
		return out
	end
	for name, raw in attrs do
		local tag = tagForAttribute(raw)
		if tag == nil then
			continue
		end
		local encoded = PropertyTable.encodeValue(tag, raw)
		if encoded ~= nil then
			out[name] = encoded
		end
	end
	return out
end

local function encodeTags(inst: Instance): { string }
	local ok, tags = pcall(function()
		return CollectionService:GetTags(inst)
	end)
	if ok and type(tags) == "table" then
		return tags
	end
	return {}
end

local function isExportClass(class_name: string): boolean
	for _, candidate in BINARY_EXPORT_CLASSES do
		if candidate == class_name then
			return true
		end
	end
	return false
end

-- Tries to extract `.rbxm` bytes for `inst`. Returns nil whenever the API
-- is unavailable or refuses, which lets the caller silently fall back to the
-- regular property pipeline. Selection is saved/restored around any
-- ExportSelection call so the user's UI doesn't visibly flinch mid-walk.
local function extractBinary(inst: Instance, plugin: Plugin?): BinaryPayload?
	if inst:IsA("BinaryStringValue") then
		local ok, raw = pcall(function()
			return (inst :: any).Value
		end)
		if not ok or type(raw) ~= "string" or raw == "" then
			return nil
		end
		return {
			class_name = inst.ClassName,
			rbxm_bytes_base64 = Base64.encode(raw),
		}
	end

	if plugin == nil or not isExportClass(inst.ClassName) then
		return nil
	end

	-- ExportSelection mutates the editor's Selection; restore on the way out
	-- so the user's clicks aren't silently re-routed.
	local previous: { Instance }
	do
		local ok, prev = pcall(function()
			return Selection:Get()
		end)
		previous = if ok and type(prev) == "table" then prev else {}
	end
	local setOk = pcall(function()
		Selection:Set({ inst })
	end)
	if not setOk then
		return nil
	end
	local exportOk, bytes = pcall(function()
		return (plugin :: any):ExportSelection()
	end)
	pcall(function()
		Selection:Set(previous)
	end)
	if not exportOk or type(bytes) ~= "string" or bytes == "" then
		return nil
	end
	return {
		class_name = inst.ClassName,
		rbxm_bytes_base64 = Base64.encode(bytes),
	}
end

type SerializeContext = {
	include_binary: boolean,
	plugin: Plugin?,
}

local function serialize(
	inst: Instance,
	id: number,
	parent_id: number?,
	ctx: SerializeContext
): SerializedInstance
	local out: SerializedInstance = {
		id = id,
		parent_id = parent_id,
		class_name = inst.ClassName,
		name = inst.Name,
		properties = PropertyTable.encodeInstance(inst),
		attributes = encodeAttributes(inst),
		tags = encodeTags(inst),
	}
	if ctx.include_binary then
		local payload = extractBinary(inst, ctx.plugin)
		if payload ~= nil then
			out.binary = payload
		end
	end
	return out
end

-- Returns true iff this subtree contains at least one child — purely a count,
-- used to decide whether a service survives the Minimal filter.
local function hasAnyDescendant(inst: Instance): boolean
	return #inst:GetChildren() > 0
end

function Walker.collect(opts: WalkOptions?): WalkResult
	local keep_empty_services = (opts and opts.keep_empty_services) == true
	local include_binary = (opts and opts.include_binary) == true
	local plugin: Plugin? = opts and opts.plugin

	local serializeCtx: SerializeContext = {
		include_binary = include_binary,
		plugin = plugin,
	}

	local instances: { SerializedInstance } = {}
	local services_included: { string } = {}
	local next_id = 1

	local function allocId(): number
		local id = next_id
		next_id += 1
		return id
	end

	-- DataModel root: parent_id = nil signals "game" to the daemon.
	local root_id = allocId()
	table.insert(instances, serialize(game, root_id, nil, serializeCtx))

	-- Yield every YIELD_EVERY serialized instances. A 50K-instance place
	-- otherwise pegs the main thread for several seconds during syncback,
	-- which Studio surfaces as a frozen UI. 500 is a looser bound than the
	-- discover-side YIELD_EVERY because each `serialize()` call can do
	-- non-trivial property reads — staying under the 16ms frame budget on
	-- the heavy path matters more than absolute throughput.
	local YIELD_EVERY = 500
	local serialized_count = 1

	local function walkDescendants(inst: Instance, parent_id: number)
		for _, child in inst:GetChildren() do
			local id = allocId()
			local serialized = serialize(child, id, parent_id, serializeCtx)
			table.insert(instances, serialized)
			serialized_count += 1
			if serialized_count % YIELD_EVERY == 0 then
				task.wait()
			end
			-- Binary payloads carry their own subtree; recursing would create
			-- duplicate siblings on re-ingest because the .rbxm already nests
			-- the children inside it.
			if serialized.binary == nil then
				walkDescendants(child, id)
			end
		end
	end

	for _, service in game:GetChildren() do
		if not keep_empty_services and not hasAnyDescendant(service) then
			continue
		end
		local service_id = allocId()
		table.insert(instances, serialize(service, service_id, root_id, serializeCtx))
		table.insert(services_included, service.ClassName)
		walkDescendants(service, service_id)
	end

	return {
		instances = instances,
		services_included = services_included,
		root_id = root_id,
	}
end

return Walker
