--!strict

-- Resilient WebSocket wrapper. Wraps net/Connection with exponential backoff
-- and session-resume bookkeeping: we remember the `session_id` the daemon
-- issued on `ProjectOpened` (or echoed on `Resumed`) and feed it into the
-- next `Hello` so the daemon can replay buffered deltas instead of forcing
-- a full bootstrap (E7).
--
-- Backoff: 1s, 2s, 4s, 8s, 16s, capped at 30s. The cap matters because
-- Studio reload-plugin cycles are typically 2-5 minutes — beyond 30s we
-- add noise without changing behavior.

local Connection = require(script.Parent.Connection)

-- Locally redeclared (types don't reliably cross `require` in strict mode).
export type Payload = { [string]: any }

type ConnectionHandle = {
	send: (self: ConnectionHandle, payload: Payload) -> (),
	close: (self: ConnectionHandle) -> (),
}

export type Handlers = {
	-- Fires once the WebSocket handshake completed AND the hello frame has
	-- been queued. `attempt` is 0 on first connect and >=1 on reconnects;
	-- `sessionId` is the id echoed back on the next ProjectOpened/Resumed.
	onOpen: ((attempt: number, sessionId: string?) -> ())?,
	onMessage: (Payload) -> (),
	onError: (string) -> (),
	-- Fires on every socket close — userClosed or daemon-side. The Reconnect
	-- module decides whether to retry afterwards; callers that need to know
	-- "we are permanently done" should rely on `userClosed` via close().
	onClose: (() -> ())?,
	-- Fires before each retry sleep so the UI can display
	-- "reconnecting (attempt N in Xs)".
	onReconnecting: ((attempt: number, delaySec: number) -> ())?,
	-- Fires when the daemon sends `Resumed`; distinct from the cold-bootstrap
	-- `ProjectOpened`. The widget uses this to suppress TreeBuilder rebuilds.
	onResumed: ((sessionId: string, replayed: number) -> ())?,
	-- Called on each (re)open to produce the hello payload. The caller owns
	-- the shape (e.g. `version`, `studio_snapshot`); we just inject
	-- `session_id` when one has been captured.
	buildHelloPayload: (sessionId: string?) -> Payload,
	-- Optional Connection-level options forwarded into every
	-- `Connection.connect` call (initial + reconnects). Today the
	-- only field is `skipHttpEnabledCheck`; future options should
	-- live here too so plumbing is one place to change.
	connectionOptions: { skipHttpEnabledCheck: boolean? }?,
}

export type Handle = {
	send: (self: Handle, payload: Payload) -> (),
	close: (self: Handle) -> (),
	getSessionId: (self: Handle) -> string?,
}

local BASE_BACKOFF_SEC = 1
local MAX_BACKOFF_SEC = 30
-- Heartbeat liveness probe. The plugin sends a Ping every HEARTBEAT_SEC;
-- the daemon replies with Pong. If the plugin doesn't hear a Pong within
-- HEARTBEAT_TIMEOUT_SEC, the connection is treated as dead and the
-- onClose callback fires — Reconnect's existing backoff/retry path
-- re-establishes a fresh connection. Without this a wedged WebSocket
-- (TCP socket alive, daemon process hung) could go undetected for
-- minutes until the kernel keepalive fires.
local HEARTBEAT_SEC = 30
local HEARTBEAT_TIMEOUT_SEC = 90

local Reconnect = {}

function Reconnect.run(url: string, handlers: Handlers): Handle
	local userClosed = false
	-- 0 on first attempt, incremented for every retry after an unintended close.
	local attempt = 0
	local sessionId: string? = nil
	local currentConn: ConnectionHandle? = nil
	-- Heartbeat state. `heartbeatGen` is bumped on every (re)connect so
	-- old timer tasks from previous connections become silent no-ops once
	-- their generation no longer matches. `lastPongAt` is updated on
	-- every Pong arrival.
	local heartbeatGen = 0
	local lastPongAt = 0
	local pingSeq: number = 0

	local tryConnect: () -> ()

	local function scheduleReconnect()
		if userClosed then
			return
		end
		-- `attempt` was just incremented by the caller; display that as 1-indexed
		-- so users see "attempt 1" right after the first unexpected close.
		local exponent = attempt - 1
		local delay = math.min(MAX_BACKOFF_SEC, BASE_BACKOFF_SEC * (2 ^ exponent))
		if handlers.onReconnecting then
			handlers.onReconnecting(attempt, delay)
		end
		task.delay(delay, function()
			if userClosed then
				return
			end
			tryConnect()
		end)
	end

	tryConnect = function()
		local thisAttempt = attempt
		-- `conn` must exist before `onOpen` can use it. Connection.connect
		-- returns synchronously and Opened fires async, so the assignment
		-- below races safely ahead of any callback.
		local conn: ConnectionHandle? = Connection.connect(url, {
			onOpen = function()
				local current = currentConn
				if current == nil then
					return
				end
				local payload = handlers.buildHelloPayload(sessionId)
				current:send(payload)
				-- Arm the heartbeat for this connection. Bumping the gen
				-- invalidates any previous-connection timer task that
				-- might still be sleeping. Seed lastPongAt to "now" so
				-- the timeout window opens fresh.
				heartbeatGen += 1
				local myGen = heartbeatGen
				lastPongAt = os.clock()
				task.spawn(function()
					-- Wait BEFORE sending the first ping. This avoids
					-- racing the daemon's auth dance: right after Hello
					-- the daemon may send `auth_challenge` and read the
					-- next frame expecting `pair_request`. If our first
					-- ping arrives before the plugin has a chance to
					-- dispatch the challenge → reply, the daemon bails
					-- ("expected PairRequest, got Ping"). Waiting one
					-- HEARTBEAT_SEC interval gives auth + pair time to
					-- complete first; the timeout window is unaffected
					-- because we seeded `lastPongAt = os.clock()` above.
					while heartbeatGen == myGen and not userClosed do
						task.wait(HEARTBEAT_SEC)
						if heartbeatGen ~= myGen or userClosed then
							return
						end
						local socket = currentConn
						if socket == nil then
							return
						end
						pingSeq += 1
						socket:send({ type = "ping", seq = pingSeq })
						if (os.clock() - lastPongAt) > HEARTBEAT_TIMEOUT_SEC then
							-- No pong within the window; treat the
							-- WebSocket as wedged and force a close.
							-- The Connection's onClose handler will
							-- bump `attempt` and schedule a reconnect.
							handlers.onError(string.format(
								"heartbeat timeout: no pong in %ds",
								HEARTBEAT_TIMEOUT_SEC
							))
							local dead = currentConn
							currentConn = nil
							if dead ~= nil then
								pcall(function() dead:close() end)
							end
							return
						end
					end
				end)
				if handlers.onOpen ~= nil then
					handlers.onOpen(thisAttempt, sessionId)
				end
			end,
			onMessage = function(payload: Payload)
				-- Intercept for session tracking; never drop the frame, even
				-- if we didn't understand the id shape.
				local kind = payload["type"]
				if kind == "project_opened" then
					local sid = payload["session_id"]
					if typeof(sid) == "string" then
						sessionId = sid
					end
				elseif kind == "resumed" then
					local sid = payload["session_id"]
					if typeof(sid) == "string" then
						sessionId = sid
					end
					if handlers.onResumed ~= nil then
						local replayed = payload["replayed"]
						local count: number = if typeof(replayed) == "number" then replayed else 0
						handlers.onResumed(sessionId :: string, count)
					end
				elseif kind == "pong" then
					-- Heartbeat reply. Update the watchdog timestamp and
					-- swallow the frame — downstream handlers don't care
					-- about pongs.
					lastPongAt = os.clock()
					return
				end
				handlers.onMessage(payload)
			end,
			onError = function(err: string)
				handlers.onError(err)
			end,
			onClose = function()
				currentConn = nil
				if handlers.onClose ~= nil then
					handlers.onClose()
				end
				if not userClosed then
					attempt += 1
					scheduleReconnect()
				end
			end,
		}, handlers.connectionOptions)
		if conn == nil then
			-- Connection.connect already surfaced the error via onError. Treat
			-- as a failed open: bump attempt and back off like a normal close.
			if not userClosed then
				attempt += 1
				scheduleReconnect()
			end
			return
		end
		currentConn = conn
	end

	tryConnect()

	local handle: Handle
	handle = {
		send = function(_self: Handle, payload: Payload)
			local conn = currentConn
			if conn ~= nil then
				conn:send(payload)
			else
				handlers.onError("send while disconnected")
			end
		end,
		close = function(_self: Handle)
			userClosed = true
			local conn = currentConn
			currentConn = nil
			if conn ~= nil then
				conn:close()
			end
		end,
		getSessionId = function(_self: Handle): string?
			return sessionId
		end,
	}
	return handle
end

return Reconnect
