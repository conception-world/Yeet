--!strict

-- Curated whitelist of properties the plugin serializes during reverse
-- bootstrap. The daemon is class-agnostic and just round-trips whatever it
-- receives into `.meta.json`, so every property we list here must map to one
-- of the wire variants in `SerializedProperty` (protocol.rs):
--   string | bool | number | vector2 | vector3 | cframe | color3
--   | udim | udim2 | enum | brickcolor
--
-- Inheritance is handled at encode time via `inst:IsA(class)` — an entry for
-- "BasePart.Anchored" applies to every subclass without us listing them.
--
-- `Source` is intentionally absent from this whitelist: for incremental
-- FS<->Studio sync, script bodies travel as file content via the existing
-- `FileChanged`/`FileCreated` channel, not as a property. Reverse-bootstrap
-- syncback has no such channel, though — `Walker.serialize` injects
-- `properties.Source` directly for every `LuaSourceContainer` instead of
-- routing it through this table (see Walker.luau `encodeSource`), and
-- `syncback.rs` drops it again before persisting `.meta.json`.

export type Tag =
	"string" | "bool" | "number"
	| "vector2" | "vector3" | "cframe"
	| "color3" | "udim" | "udim2"
	| "enum" | "brickcolor"

export type Entry = {
	class: string,
	prop: string,
	tag: Tag,
}

export type Value =
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

local ENTRIES: { Entry } = {
	-- BasePart (applies to Part, MeshPart, TrussPart, WedgePart, UnionOperation, ...)
	{ class = "BasePart", prop = "Anchored",     tag = "bool" },
	{ class = "BasePart", prop = "CanCollide",   tag = "bool" },
	{ class = "BasePart", prop = "CanTouch",     tag = "bool" },
	{ class = "BasePart", prop = "CanQuery",     tag = "bool" },
	{ class = "BasePart", prop = "CastShadow",   tag = "bool" },
	{ class = "BasePart", prop = "Massless",     tag = "bool" },
	{ class = "BasePart", prop = "Size",         tag = "vector3" },
	{ class = "BasePart", prop = "CFrame",       tag = "cframe" },
	{ class = "BasePart", prop = "Color",        tag = "color3" },
	{ class = "BasePart", prop = "Transparency", tag = "number" },
	{ class = "BasePart", prop = "Reflectance",  tag = "number" },
	{ class = "BasePart", prop = "Material",     tag = "enum" },

	-- Part-only
	{ class = "Part", prop = "Shape", tag = "enum" },

	-- MeshPart
	{ class = "MeshPart", prop = "MeshId",    tag = "string" },
	{ class = "MeshPart", prop = "TextureID", tag = "string" },

	-- Decal / Texture
	{ class = "Decal", prop = "Texture",      tag = "string" },
	{ class = "Decal", prop = "Transparency", tag = "number" },
	{ class = "Decal", prop = "Color3",       tag = "color3" },
	{ class = "Decal", prop = "Face",         tag = "enum" },

	-- Script-family (Source is excluded on purpose — flows as file content)
	{ class = "BaseScript",   prop = "Disabled",   tag = "bool" },
	{ class = "Script",       prop = "RunContext", tag = "enum" },
	{ class = "LocalScript",  prop = "Disabled",   tag = "bool" },
	{ class = "ModuleScript", prop = "Disabled",   tag = "bool" },

	-- GuiObject (Frame, TextLabel, ImageLabel, ScrollingFrame, ...)
	{ class = "GuiObject", prop = "Size",                 tag = "udim2" },
	{ class = "GuiObject", prop = "Position",             tag = "udim2" },
	{ class = "GuiObject", prop = "AnchorPoint",          tag = "vector2" },
	{ class = "GuiObject", prop = "BackgroundColor3",     tag = "color3" },
	{ class = "GuiObject", prop = "BackgroundTransparency", tag = "number" },
	{ class = "GuiObject", prop = "BorderSizePixel",      tag = "number" },
	{ class = "GuiObject", prop = "Visible",              tag = "bool" },
	{ class = "GuiObject", prop = "ZIndex",               tag = "number" },
	{ class = "GuiObject", prop = "Rotation",             tag = "number" },
	{ class = "GuiObject", prop = "ClipsDescendants",     tag = "bool" },

	-- Text-bearing GUIs
	{ class = "TextLabel",  prop = "Text",          tag = "string" },
	{ class = "TextLabel",  prop = "TextColor3",    tag = "color3" },
	{ class = "TextLabel",  prop = "TextSize",      tag = "number" },
	{ class = "TextLabel",  prop = "TextScaled",    tag = "bool" },
	{ class = "TextLabel",  prop = "TextWrapped",   tag = "bool" },
	{ class = "TextLabel",  prop = "RichText",      tag = "bool" },
	{ class = "TextLabel",  prop = "TextXAlignment", tag = "enum" },
	{ class = "TextLabel",  prop = "TextYAlignment", tag = "enum" },
	{ class = "TextButton", prop = "Text",          tag = "string" },
	{ class = "TextButton", prop = "TextColor3",    tag = "color3" },
	{ class = "TextButton", prop = "TextSize",      tag = "number" },
	{ class = "TextBox",    prop = "Text",          tag = "string" },
	{ class = "TextBox",    prop = "TextColor3",    tag = "color3" },
	{ class = "TextBox",    prop = "TextSize",      tag = "number" },

	-- Image GUIs
	{ class = "ImageLabel",  prop = "Image",             tag = "string" },
	{ class = "ImageLabel",  prop = "ImageColor3",       tag = "color3" },
	{ class = "ImageLabel",  prop = "ImageTransparency", tag = "number" },
	{ class = "ImageLabel",  prop = "ScaleType",         tag = "enum" },
	{ class = "ImageButton", prop = "Image",             tag = "string" },

	-- Layout helpers
	{ class = "ScrollingFrame",  prop = "CanvasSize",        tag = "udim2" },
	{ class = "ScrollingFrame",  prop = "ScrollBarThickness", tag = "number" },
	{ class = "UIListLayout",    prop = "Padding",    tag = "udim" },
	{ class = "UIListLayout",    prop = "SortOrder",  tag = "enum" },
	{ class = "UIGridLayout",    prop = "CellSize",   tag = "udim2" },
	{ class = "UIGridLayout",    prop = "CellPadding", tag = "udim2" },
	{ class = "UIPadding",       prop = "PaddingTop",    tag = "udim" },
	{ class = "UIPadding",       prop = "PaddingBottom", tag = "udim" },
	{ class = "UIPadding",       prop = "PaddingLeft",   tag = "udim" },
	{ class = "UIPadding",       prop = "PaddingRight",  tag = "udim" },

	-- Attachment
	{ class = "Attachment", prop = "CFrame",  tag = "cframe" },
	{ class = "Attachment", prop = "Visible", tag = "bool" },

	-- Lights
	{ class = "Light", prop = "Brightness", tag = "number" },
	{ class = "Light", prop = "Color",      tag = "color3" },
	{ class = "Light", prop = "Enabled",    tag = "bool" },
	{ class = "Light", prop = "Shadows",    tag = "bool" },
	{ class = "PointLight",   prop = "Range", tag = "number" },
	{ class = "SpotLight",    prop = "Range", tag = "number" },
	{ class = "SurfaceLight", prop = "Range", tag = "number" },

	-- Sound
	{ class = "Sound", prop = "SoundId",       tag = "string" },
	{ class = "Sound", prop = "Volume",        tag = "number" },
	{ class = "Sound", prop = "PlaybackSpeed", tag = "number" },
	{ class = "Sound", prop = "Looped",        tag = "bool" },
	{ class = "Sound", prop = "Playing",       tag = "bool" },
}

local PropertyTable = {}

PropertyTable.entries = ENTRIES

-- Encode a single live value into the wire format. Returns nil when the type
-- coming back from `inst[prop]` doesn't match the declared tag — callers
-- should treat that as "skip this property" rather than abort the walk, since
-- property types can change across engine versions.
function PropertyTable.encodeValue(tag: Tag, raw: any): Value?
	if tag == "string" then
		if type(raw) ~= "string" then return nil end
		return { type = "string", value = raw }
	elseif tag == "bool" then
		if type(raw) ~= "boolean" then return nil end
		return { type = "bool", value = raw }
	elseif tag == "number" then
		if type(raw) ~= "number" then return nil end
		return { type = "number", value = raw }
	elseif tag == "vector2" then
		if typeof(raw) ~= "Vector2" then return nil end
		return { type = "vector2", value = { raw.X, raw.Y } }
	elseif tag == "vector3" then
		if typeof(raw) ~= "Vector3" then return nil end
		return { type = "vector3", value = { raw.X, raw.Y, raw.Z } }
	elseif tag == "cframe" then
		if typeof(raw) ~= "CFrame" then return nil end
		-- GetComponents returns (x,y,z, r00,r01,r02, r10,r11,r12, r20,r21,r22)
		-- which matches the wire layout exactly.
		return { type = "cframe", value = { raw:GetComponents() } }
	elseif tag == "color3" then
		if typeof(raw) ~= "Color3" then return nil end
		return { type = "color3", value = { raw.R, raw.G, raw.B } }
	elseif tag == "udim" then
		if typeof(raw) ~= "UDim" then return nil end
		return { type = "udim", value = { raw.Scale, raw.Offset } }
	elseif tag == "udim2" then
		if typeof(raw) ~= "UDim2" then return nil end
		return {
			type = "udim2",
			value = { raw.X.Scale, raw.X.Offset, raw.Y.Scale, raw.Y.Offset },
		}
	elseif tag == "enum" then
		if typeof(raw) ~= "EnumItem" then return nil end
		-- Wire format drops the leading "Enum." — e.g. "Material.Plastic".
		return { type = "enum", value = raw.EnumType.Name .. "." .. raw.Name }
	elseif tag == "brickcolor" then
		if typeof(raw) ~= "BrickColor" then return nil end
		return { type = "brickcolor", value = raw.Number }
	end
	return nil
end

-- Decodes a wire-format Value into its Roblox runtime representation. This
-- is the inverse of `encodeValue`: given the shape produced by the syncback
-- pipeline (plus anything the daemon emits on FS→Studio events), rebuild
-- the Luau value the engine expects. Returns nil for any malformed or
-- unsupported payload — callers should treat nil as "skip this entry",
-- not as a hard error, so one bad attribute can't freeze the dispatcher.
function PropertyTable.decodeValue(encoded: Value): any?
	local tag = encoded.type
	if tag == "string" then
		return encoded.value
	elseif tag == "bool" then
		return encoded.value
	elseif tag == "number" then
		return encoded.value
	elseif tag == "vector2" then
		local v = encoded.value
		if typeof(v) ~= "table" or #v ~= 2 then return nil end
		return Vector2.new(v[1], v[2])
	elseif tag == "vector3" then
		local v = encoded.value
		if typeof(v) ~= "table" or #v ~= 3 then return nil end
		return Vector3.new(v[1], v[2], v[3])
	elseif tag == "cframe" then
		local v = encoded.value
		if typeof(v) ~= "table" or #v ~= 12 then return nil end
		return CFrame.new(
			v[1], v[2], v[3],
			v[4], v[5], v[6],
			v[7], v[8], v[9],
			v[10], v[11], v[12]
		)
	elseif tag == "color3" then
		local v = encoded.value
		if typeof(v) ~= "table" or #v ~= 3 then return nil end
		return Color3.new(v[1], v[2], v[3])
	elseif tag == "udim" then
		local v = encoded.value
		if typeof(v) ~= "table" or #v ~= 2 then return nil end
		return UDim.new(v[1], v[2])
	elseif tag == "udim2" then
		local v = encoded.value
		if typeof(v) ~= "table" or #v ~= 4 then return nil end
		return UDim2.new(v[1], v[2], v[3], v[4])
	elseif tag == "enum" then
		-- Wire format is "Material.Plastic"; split and look up via Enum lib.
		local raw = encoded.value
		if typeof(raw) ~= "string" then return nil end
		local dot = raw:find("%.")
		if dot == nil then return nil end
		local enumName = raw:sub(1, dot - 1)
		local itemName = raw:sub(dot + 1)
		local ok, enumType = pcall(function()
			return (Enum :: any)[enumName]
		end)
		if not ok or enumType == nil then return nil end
		local okItem, item = pcall(function()
			return enumType[itemName]
		end)
		if not okItem then return nil end
		return item
	elseif tag == "brickcolor" then
		local raw = encoded.value
		if typeof(raw) ~= "number" then return nil end
		local ok, brickColor = pcall(BrickColor.new, raw)
		if not ok then return nil end
		return brickColor
	end
	return nil
end

-- Per-ClassName cache of which ENTRIES apply. `inst:IsA(entry.class)` depends
-- only on the instance's ClassName — a static fact about the engine's class
-- hierarchy, never on runtime state — so two instances sharing a ClassName
-- always get the same applicable subset. Without this, encodeInstance ran
-- the full `#ENTRIES` (~70-95) `IsA` sweep on every single instance in the
-- walk instead of once per distinct class. The cache only ever grows to the
-- fixed number of ClassNames the engine defines, so it's never cleared.
local applicableEntriesByClassName: { [string]: { Entry } } = {}

local function applicableEntriesFor(inst: Instance): { Entry }
	local className = inst.ClassName
	local cached = applicableEntriesByClassName[className]
	if cached ~= nil then
		return cached
	end
	local applicable: { Entry } = {}
	for _, entry in ENTRIES do
		if inst:IsA(entry.class) then
			table.insert(applicable, entry)
		end
	end
	applicableEntriesByClassName[className] = applicable
	return applicable
end

-- Walk the whitelist for one instance and encode every applicable property.
-- Read errors are swallowed (security-sandboxed props, deprecated accessors)
-- so a single bad property doesn't abort the whole syncback.
function PropertyTable.encodeInstance(inst: Instance): { [string]: Value }
	local out: { [string]: Value } = {}
	for _, entry in applicableEntriesFor(inst) do
		local ok, raw = pcall(function()
			return (inst :: any)[entry.prop]
		end)
		if not ok then
			continue
		end
		local encoded = PropertyTable.encodeValue(entry.tag, raw)
		if encoded ~= nil then
			out[entry.prop] = encoded
		end
	end
	return out
end

return PropertyTable
