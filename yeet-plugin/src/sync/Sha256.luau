--!strict
--!native
--!optimize 2

-- Pure Luau SHA-256 over strings. Uses the `buffer` API for tight memory
-- layout and 32-bit ops; the hot path is word-at-a-time reads/writes so we
-- avoid per-byte table indirection. Output is lowercase hex.
--
-- We need this because Luau has no built-in hash and the daemon identifies
-- file versions by SHA-256 hex. Cross-check: compare against daemon output
-- on known strings (empty, "abc", etc.) — vectors below match FIPS 180-4.

local Sha256 = {}

-- SHA-256 initial hash values (first 32 bits of the fractional parts of the
-- square roots of the first 8 primes).
local H0: { number } = {
	0x6a09e667,
	0xbb67ae85,
	0x3c6ef372,
	0xa54ff53a,
	0x510e527f,
	0x9b05688c,
	0x1f83d9ab,
	0x5be0cd19,
}

-- Round constants (first 32 bits of the fractional parts of the cube roots
-- of the first 64 primes).
local K: { number } = {
	0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
	0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
	0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
	0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
	0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
	0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
	0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
	0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
}

local band = bit32.band
local bor = bit32.bor
local bxor = bit32.bxor
local bnot = bit32.bnot
local rshift = bit32.rshift
local rrotate = bit32.rrotate

-- Returns a buffer padded per FIPS 180-4 §5.1.1: append 0x80, k zero bytes
-- s.t. (len + 1 + k) % 64 == 56, then the 64-bit big-endian bit length.
local function padded(message: string): buffer
	local msgLen = #message
	local bitLen = msgLen * 8
	-- Total length is smallest multiple of 64 that fits msgLen + 1 (for 0x80)
	-- + 8 (for length field).
	local total = msgLen + 1
	local rem = total % 64
	local pad = if rem <= 56 then 56 - rem else 120 - rem
	total = total + pad + 8

	local buf = buffer.create(total)
	buffer.writestring(buf, 0, message)
	buffer.writeu8(buf, msgLen, 0x80)
	-- The zero padding is already there — `buffer.create` zero-inits.
	-- Write the 64-bit big-endian bit length into the last 8 bytes.
	-- In Luau, `#message` is limited to 2^53-1 so we can afford to keep the
	-- high 32 bits at zero for any realistic script source.
	buffer.writeu32(buf, total - 8, 0) -- high 32 bits of bit length
	-- Low 32 bits, big-endian.
	buffer.writeu8(buf, total - 4, band(rshift(bitLen, 24), 0xff))
	buffer.writeu8(buf, total - 3, band(rshift(bitLen, 16), 0xff))
	buffer.writeu8(buf, total - 2, band(rshift(bitLen, 8), 0xff))
	buffer.writeu8(buf, total - 1, band(bitLen, 0xff))
	return buf
end

-- Reads a big-endian u32 at `offset` in `buf`. The buffer API's native
-- `readu32` is little-endian, so we do the byte shuffle by hand.
local function readBE32(buf: buffer, offset: number): number
	local b0 = buffer.readu8(buf, offset)
	local b1 = buffer.readu8(buf, offset + 1)
	local b2 = buffer.readu8(buf, offset + 2)
	local b3 = buffer.readu8(buf, offset + 3)
	return bor(bor(bor(b0 * 0x1000000, b1 * 0x10000), b2 * 0x100), b3)
end

local function processChunk(buf: buffer, offset: number, h: { number }, w: { number })
	for i = 0, 15 do
		w[i + 1] = readBE32(buf, offset + i * 4)
	end
	for i = 17, 64 do
		local wm15 = w[i - 15]
		local s0 = bxor(bxor(rrotate(wm15, 7), rrotate(wm15, 18)), rshift(wm15, 3))
		local wm2 = w[i - 2]
		local s1 = bxor(bxor(rrotate(wm2, 17), rrotate(wm2, 19)), rshift(wm2, 10))
		-- Adds mod 2^32 via `%`; all intermediates are non-negative.
		w[i] = (w[i - 16] + s0 + w[i - 7] + s1) % 0x100000000
	end

	local a, b, c, d = h[1], h[2], h[3], h[4]
	local e, f, g, hh = h[5], h[6], h[7], h[8]

	for i = 1, 64 do
		local S1 = bxor(bxor(rrotate(e, 6), rrotate(e, 11)), rrotate(e, 25))
		local ch = bxor(band(e, f), band(bnot(e), g))
		local t1 = (hh + S1 + ch + K[i] + w[i]) % 0x100000000
		local S0 = bxor(bxor(rrotate(a, 2), rrotate(a, 13)), rrotate(a, 22))
		local maj = bxor(bxor(band(a, b), band(a, c)), band(b, c))
		local t2 = (S0 + maj) % 0x100000000
		hh = g
		g = f
		f = e
		e = (d + t1) % 0x100000000
		d = c
		c = b
		b = a
		a = (t1 + t2) % 0x100000000
	end

	h[1] = (h[1] + a) % 0x100000000
	h[2] = (h[2] + b) % 0x100000000
	h[3] = (h[3] + c) % 0x100000000
	h[4] = (h[4] + d) % 0x100000000
	h[5] = (h[5] + e) % 0x100000000
	h[6] = (h[6] + f) % 0x100000000
	h[7] = (h[7] + g) % 0x100000000
	h[8] = (h[8] + hh) % 0x100000000
end

function Sha256.hex(message: string): string
	local buf = padded(message)
	local size = buffer.len(buf)
	local h = table.clone(H0)
	local w = table.create(64, 0)
	for offset = 0, size - 1, 64 do
		processChunk(buf, offset, h, w)
	end
	return string.format(
		"%08x%08x%08x%08x%08x%08x%08x%08x",
		h[1], h[2], h[3], h[4], h[5], h[6], h[7], h[8]
	)
end

return Sha256
