--!strict

-- Streams a `WalkResult` to the daemon as a SyncbackBegin frame, one or more
-- SyncbackChunk frames, and a final SyncbackFinalize. Flow-control is 1-in-
-- flight: chunk N is only sent after the daemon acks N-1, which keeps the
-- plugin's memory footprint steady on large places and lets the daemon
-- backpressure without us having to buffer locally.
--
-- Ownership of the dispatcher is NOT taken here: the caller (Widget) is
-- responsible for routing inbound syncback_* frames into `session:handle` and
-- for unwiring that routing in `onComplete` / `onError`. This keeps the
-- Applier's demux as the single source of truth for message dispatch.

-- Hard cap on instances per chunk — a secondary bound alongside the byte
-- budget below, so a chunk of tiny instances still can't balloon in *count*.
local CHUNK_SIZE = 100

-- Soft cap on serialized bytes per chunk. `SerializedInstance.binary`
-- (rbxm_bytes_base64) and script `Source` text are the only fields that can
-- grow unbounded, and a single MeshPart/UnionOperation blob can be many MiB
-- once base64-inflated (see A5 — Source now rides in `properties` too). 8 MiB
-- leaves the daemon's WS_MAX_FRAME_BYTES (16 MiB, see main.rs) a wide margin
-- for the JSON envelope and string-escaping overhead, so a chunk built to
-- this budget never gets near the wire cap.
local CHUNK_BYTE_BUDGET = 8 * 1024 * 1024

-- Flat allowance per instance for everything `estimateInstanceBytes` doesn't
-- size exactly: JSON punctuation/keys, non-string properties, attributes,
-- and tags. Generous on purpose — undercounting here only eats into the
-- margin between the 8 MiB budget and the 16 MiB hard cap, never crosses it
-- on its own.
local INSTANCE_OVERHEAD_BYTES = 512

local ACK_TIMEOUT_SECONDS = 10

-- Wire-format types are redeclared locally; Luau --!strict does not carry
-- exported types across `require()` boundaries (see project feedback memory).
-- Keep in sync with `PropertyTable.luau` and `SerializedProperty` in
-- `yeet-daemon/src/protocol.rs`.
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

type BinaryPayload = {
	class_name: string,
	rbxm_bytes_base64: string,
}

type SerializedInstance = {
	id: number,
	parent_id: number?,
	class_name: string,
	name: string,
	properties: { [string]: Value },
	attributes: { [string]: Value },
	tags: { string },
	binary: BinaryPayload?,
}

type WalkResult = {
	instances: { SerializedInstance },
	services_included: { string },
	root_id: number,
}

-- One `syncback_chunk` frame's worth of instances, as decided by
-- `partitionIntoChunks`.
type Chunk = { SerializedInstance }

type Payload = { [string]: any }

type ConnectionHandle = {
	send: (self: ConnectionHandle, payload: Payload) -> (),
	close: (self: ConnectionHandle) -> (),
}

export type SyncbackStats = {
	scripts_written: number,
	non_script_instances_written: number,
	meta_files_written: number,
	total_bytes: number,
	services_included: { string },
	warnings: { string },
}

export type Mode = "new_project" | { kind: "merge_existing", overwrite: boolean }

export type Options = {
	request_id: string,
	target_path: string,
	mode: Mode,
	include_non_script: boolean,
	include_binary: boolean,
	template: "minimal",
	project_name: string?,
}

export type RunParams = {
	connection: ConnectionHandle,
	walk_result: WalkResult,
	opts: Options,
	onProgress: (sent: number, total: number) -> (),
	onComplete: (stats: SyncbackStats) -> (),
	onError: (message: string) -> (),
}

export type Session = {
	handle: (self: Session, payload: Payload) -> (),
	cancel: (self: Session) -> (),
}

local ChunkedSender = {}

-- The daemon's `SyncbackMode` is a serde-tagged enum; both variants use the
-- same `kind` discriminator. Callers hand us the string form for the simple
-- case and a table for the one with a field, so we normalize here.
local function encodeMode(mode: Mode): { [string]: any }
	if typeof(mode) == "string" then
		return { kind = mode }
	end
	local obj = mode :: { kind: string, overwrite: boolean }
	return { kind = obj.kind, overwrite = obj.overwrite }
end

-- Rough byte-size estimate for one instance's contribution to the JSON wire
-- payload. A full JSONEncode just to measure size would double the cost of
-- every syncback, so instead we sum the fields that can actually grow
-- unbounded — string property/attribute values (Source, MeshId, ...) and the
-- base64 `.rbxm` blob — and add `INSTANCE_OVERHEAD_BYTES` for everything
-- else (keys, punctuation, numeric/vector fields, tags).
local function estimateInstanceBytes(instance: SerializedInstance): number
	local size = INSTANCE_OVERHEAD_BYTES + #instance.class_name + #instance.name
	for _, value in instance.properties do
		if value.type == "string" then
			size += #value.value
		end
	end
	for _, value in instance.attributes do
		if value.type == "string" then
			size += #value.value
		end
	end
	for _, tag in instance.tags do
		size += #tag
	end
	if instance.binary ~= nil then
		size += #instance.binary.rbxm_bytes_base64
	end
	return size
end

-- Groups `instances` into wire-sized chunks: accumulate into the current
-- chunk until either the byte budget or the count cap would be exceeded
-- (whichever comes first), then flush and start a new one. A single instance
-- whose own estimate already exceeds the budget is never placed alone in an
-- over-cap frame — it's skipped and reported back via the returned warning
-- list so the caller can surface it instead of losing it silently (A14).
local function partitionIntoChunks(instances: { SerializedInstance }): ({ Chunk }, { string })
	local chunks: { Chunk } = {}
	local warnings: { string } = {}
	local current: Chunk = {}
	local currentBytes = 0

	local function flush()
		if #current > 0 then
			table.insert(chunks, current)
			current = {}
			currentBytes = 0
		end
	end

	for _, instance in instances do
		local size = estimateInstanceBytes(instance)
		if size > CHUNK_BYTE_BUDGET then
			table.insert(
				warnings,
				string.format(
					"skipped %s (%s, id %d): ~%d MiB exceeds the %d MiB syncback chunk budget",
					instance.name,
					instance.class_name,
					instance.id,
					math.ceil(size / (1024 * 1024)),
					CHUNK_BYTE_BUDGET / (1024 * 1024)
				)
			)
			continue
		end
		if #current >= CHUNK_SIZE or currentBytes + size > CHUNK_BYTE_BUDGET then
			flush()
		end
		table.insert(current, instance)
		currentBytes += size
	end
	flush()

	if #chunks == 0 then
		-- Preserve the "always at least one chunk before finalize" contract
		-- the fixed-size sender had, even for an empty walk result or one
		-- where every instance was skipped.
		table.insert(chunks, {})
	end

	return chunks, warnings
end

function ChunkedSender.run(params: RunParams): Session
	local chunks, skipWarnings = partitionIntoChunks(params.walk_result.instances)
	local totalSeq = #chunks
	-- Sendable total (post-skip) so `onProgress` reaches 100% at completion
	-- even when `partitionIntoChunks` had to drop an oversized instance.
	local total = 0
	for _, chunk in chunks do
		total += #chunk
	end

	local connection = params.connection
	local opts = params.opts

	local cancelled = false
	local finished = false
	local lastSentSeq = -1
	local lastAckedSeq = -1
	local sentCount = 0
	-- Monotonic token so older ack-timeouts don't fire after a newer chunk
	-- has already been ack'd. Equivalent to a cancellation cookie per wait.
	local ackTimeoutToken = 0

	local function finish()
		finished = true
	end

	local function fail(message: string)
		if finished then
			return
		end
		finish()
		params.onError(message)
	end

	local function scheduleAckTimeout(seq: number)
		ackTimeoutToken += 1
		local myToken = ackTimeoutToken
		task.delay(ACK_TIMEOUT_SECONDS, function()
			if finished or cancelled then
				return
			end
			if myToken ~= ackTimeoutToken then
				return
			end
			if lastAckedSeq >= seq then
				return
			end
			fail(string.format("ack timeout on seq %d", seq))
		end)
	end

	local function sendChunk(seq: number)
		if cancelled or finished then
			return
		end
		connection:send({
			type = "syncback_chunk",
			request_id = opts.request_id,
			seq = seq,
			instances = chunks[seq + 1],
		})
		lastSentSeq = seq
		scheduleAckTimeout(seq)
	end

	local function sendFinalize()
		if cancelled or finished then
			return
		end
		connection:send({
			type = "syncback_finalize",
			request_id = opts.request_id,
			total_seq = totalSeq,
		})
	end

	local function sendBegin()
		local payload: { [string]: any } = {
			type = "syncback_begin",
			request_id = opts.request_id,
			target_path = opts.target_path,
			mode = encodeMode(opts.mode),
			include_non_script = opts.include_non_script,
			include_binary = opts.include_binary,
			template = opts.template,
		}
		if opts.project_name ~= nil then
			payload.project_name = opts.project_name
		end
		connection:send(payload)
	end

	sendBegin()
	sendChunk(0)

	local function handle(payload: Payload)
		if finished or cancelled then
			return
		end
		if payload.request_id ~= opts.request_id then
			return
		end
		local kind = payload.type
		if kind == "syncback_ack" then
			local seq = payload.seq
			if typeof(seq) ~= "number" then
				return
			end
			if seq <= lastAckedSeq then
				return
			end
			lastAckedSeq = seq
			sentCount += #chunks[seq + 1]
			params.onProgress(sentCount, total)
			local nextSeq = seq + 1
			if nextSeq < totalSeq then
				sendChunk(nextSeq)
			else
				sendFinalize()
			end
		elseif kind == "syncback_complete" then
			finish()
			local stats = payload.stats
			if typeof(stats) ~= "table" then
				params.onError("syncback_complete without stats")
				return
			end
			local typedStats = stats :: SyncbackStats
			if #skipWarnings > 0 then
				-- Surface plugin-side skips (oversized instances dropped by
				-- `partitionIntoChunks`) through the same warnings list the
				-- daemon already uses for its own non-fatal ingestion issues,
				-- so SyncbackModal's completion screen shows them without any
				-- change to the public callback shape.
				if typeof(typedStats.warnings) ~= "table" then
					typedStats.warnings = {}
				end
				for _, warning in skipWarnings do
					table.insert(typedStats.warnings, warning)
				end
			end
			params.onComplete(typedStats)
		elseif kind == "syncback_error" then
			fail(tostring(payload.message or "unknown syncback error"))
		end
	end

	local session = {}
	function session.handle(_self: Session, payload: Payload)
		handle(payload)
	end
	function session.cancel(_self: Session)
		if finished then
			return
		end
		cancelled = true
		finish()
	end
	return session :: Session
end

return ChunkedSender
