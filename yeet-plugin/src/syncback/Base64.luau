--!strict

-- Pure-Luau base64 encoder. The plugin needs this to wrap raw `.rbxm` bytes
-- (produced by `plugin:ExportSelection` or `BinaryStringValue.Value`) for
-- transport across our JSON wire format. Once Fase 5+ moves to MessagePack
-- the call sites can hand `Vec<u8>` straight through and this module goes
-- away — keeping the API minimal so that swap stays cheap.
--
-- We don't ship a decoder: the daemon owns base64 decoding, and the plugin
-- never needs to ingest binary blobs (FS→Studio is text-only today).

local ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"

-- Materialize a numeric index → character lookup once. `string.byte` on the
-- alphabet would also work, but a flat table dodges the per-call call cost.
local CHARS: { string } = table.create(64)
for i = 1, 64 do
	CHARS[i] = ALPHABET:sub(i, i)
end

local Base64 = {}

-- Encodes a byte string (each character treated as one octet, exactly how
-- `BinaryStringValue.Value` and `plugin:ExportSelection` already hand bytes
-- back) into standard base64 with `=` padding. Input is allowed to contain
-- nulls and arbitrary high bytes.
function Base64.encode(bytes: string): string
	local len = #bytes
	if len == 0 then
		return ""
	end

	-- Pre-size the output: every 3 input bytes produce 4 output chars,
	-- rounding up. `table.create` skips the rehash that would otherwise fire
	-- a few times on large inputs.
	local outLen = math.ceil(len / 3) * 4
	local out: { string } = table.create(outLen)
	local outIdx = 0

	-- Walk full 3-byte triplets. The trailing 1- or 2-byte tail is handled
	-- below so the hot loop stays branch-free.
	local i = 1
	local fullChunks = len - (len % 3)
	while i <= fullChunks do
		local b1, b2, b3 = string.byte(bytes, i, i + 2)
		local n = b1 * 65536 + b2 * 256 + b3
		outIdx += 1
		out[outIdx] = CHARS[bit32.rshift(n, 18) + 1]
		outIdx += 1
		out[outIdx] = CHARS[bit32.band(bit32.rshift(n, 12), 0x3F) + 1]
		outIdx += 1
		out[outIdx] = CHARS[bit32.band(bit32.rshift(n, 6), 0x3F) + 1]
		outIdx += 1
		out[outIdx] = CHARS[bit32.band(n, 0x3F) + 1]
		i += 3
	end

	local rem = len - fullChunks
	if rem == 1 then
		local b1 = string.byte(bytes, i)
		local n = b1 * 65536
		outIdx += 1
		out[outIdx] = CHARS[bit32.rshift(n, 18) + 1]
		outIdx += 1
		out[outIdx] = CHARS[bit32.band(bit32.rshift(n, 12), 0x3F) + 1]
		outIdx += 1
		out[outIdx] = "="
		outIdx += 1
		out[outIdx] = "="
	elseif rem == 2 then
		local b1, b2 = string.byte(bytes, i, i + 1)
		local n = b1 * 65536 + b2 * 256
		outIdx += 1
		out[outIdx] = CHARS[bit32.rshift(n, 18) + 1]
		outIdx += 1
		out[outIdx] = CHARS[bit32.band(bit32.rshift(n, 12), 0x3F) + 1]
		outIdx += 1
		out[outIdx] = CHARS[bit32.band(bit32.rshift(n, 6), 0x3F) + 1]
		outIdx += 1
		out[outIdx] = "="
	end

	return table.concat(out)
end

return Base64
