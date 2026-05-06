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

local CHUNK_SIZE = 100
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

function ChunkedSender.run(params: RunParams): Session
	local instances = params.walk_result.instances
	local total = #instances
	local totalSeq = math.max(1, math.ceil(total / CHUNK_SIZE))

	local connection = params.connection
	local opts = params.opts

	local cancelled = false
	local finished = false
	local lastSentSeq = -1
	local lastAckedSeq = -1
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
		local startIdx = seq * CHUNK_SIZE + 1
		local endIdx = math.min(startIdx + CHUNK_SIZE - 1, total)
		local count = endIdx - startIdx + 1
		local slice = table.create(count)
		for i = 1, count do
			slice[i] = instances[startIdx + i - 1]
		end
		connection:send({
			type = "syncback_chunk",
			request_id = opts.request_id,
			seq = seq,
			instances = slice,
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
			local sentCount = math.min((seq + 1) * CHUNK_SIZE, total)
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
			params.onComplete(stats :: SyncbackStats)
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
