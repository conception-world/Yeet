--!strict

-- Finds running Yeet daemons by scanning a small window of loopback ports.
--
-- ## Why a port scan
--
-- With multi-place there is one daemon per project, each on its own port, and
-- the daemon writes an index of them to `~/.yeet/daemons.json`. The plugin
-- cannot read that file: inside Studio there is no filesystem access and no
-- HTTP client — `HttpService:CreateWebStreamClient` (see net/Connection) is the
-- only I/O available. So the plugin asks over the one transport it has, opening
-- a short-lived WebSocket to each candidate port and sending a
-- `role = "discover"` Hello. Daemons answer with `daemon_info` and hang up.
--
-- This is also why the daemon prefers a narrow window instead of an OS
-- ephemeral port: scanning ten fixed ports is cheap, scanning the ephemeral
-- range is not. PORT_WINDOW_BASE/SIZE below MUST match the daemon's constants
-- of the same name in yeet-daemon/src/main.rs.
--
-- ## Why this bypasses net/Reconnect
--
-- Reconnect retries forever with exponential backoff. Pointed at nine ports
-- with nothing listening, it would spin indefinitely. Discovery therefore talks
-- to Connection directly: one attempt per port, no retry, no heartbeat, no
-- outgoing buffer, no session bookkeeping.
--
-- ## Probing is safe
--
-- The daemon answers a discover Hello before both its version gate and its auth
-- gate, takes only a read lock, and closes. It never rotates the session id, so
-- scanning cannot disturb a plugin already connected to one of the daemons
-- being scanned — see the tests in yeet-daemon/tests/discovery.rs.

local Connection = require(script.Parent.Connection)

-- Locally redeclared: types do not reliably cross `require` under --!strict.
export type Payload = { [string]: any }

type ConnectionHandle = {
	send: (self: ConnectionHandle, payload: Payload) -> boolean,
	close: (self: ConnectionHandle) -> (),
}

-- One daemon that answered a probe.
export type DaemonEntry = {
	daemonId: string,
	projectName: string,
	projectRoot: string,
	port: number,
	daemonVersion: string,
	-- True when another Studio place already holds a session with this daemon.
	-- The picker surfaces it so the user does not take a connection away from
	-- a place they have open elsewhere.
	pluginConnected: boolean,
	-- Ready-to-use `ws://127.0.0.1:<port>`, so callers never rebuild it.
	url: string,
}

export type Options = {
	-- Forwarded to Connection.connect. Discovery deliberately inherits the
	-- user's `skipHttpEnabledCheck` setting rather than forcing it: if HTTP is
	-- genuinely disabled, every probe fails and the picker can say so, which is
	-- the correct diagnosis rather than a confusing empty list.
	skipHttpEnabledCheck: boolean?,
	-- Optional trace sink (the Widget wires this to its Activity log behind the
	-- `verboseLogging` setting). Per-port outcomes are the only way to diagnose
	-- "the picker is empty" from a user's machine, since the plugin has no test
	-- runner and no console we can inspect.
	log: ((string) -> ())?,
}

export type Handle = {
	-- Abandons an in-flight scan. `onComplete` still fires, with whatever was
	-- found so far, so callers never leak a pending UI state.
	cancel: (self: Handle) -> (),
}

local Discovery = {}

-- Keep in lockstep with PORT_WINDOW_BASE / PORT_WINDOW_SIZE in
-- yeet-daemon/src/main.rs. A daemon outside this window is unreachable by the
-- picker (it is still reachable by setting the Daemon URL by hand, and the
-- daemon logs a warning when it lands outside).
local PORT_WINDOW_BASE = 34872
local PORT_WINDOW_SIZE = 10

-- How many probes may be in flight at once.
--
-- Roblox does not document a concurrent-WebStreamClient limit, so this is
-- deliberately conservative rather than "all ten at once". Four keeps the scan
-- to three quick rounds while staying well inside any plausible cap. If it
-- turns out Studio is happy with more, raising this is a one-line change; if a
-- cap is discovered to be lower, so is lowering it.
local PROBE_BATCH_SIZE = 4

-- Upper bound on a single probe. A port with nothing listening normally fails
-- its TCP connect almost immediately on loopback, so this budget is really for
-- the pathological case: something IS bound but never completes the WebSocket
-- upgrade (an unrelated app squatting a port in our window). Without a
-- per-probe deadline such a port would stall its whole batch.
local PROBE_TIMEOUT_SEC = 1.5

-- Upper bound on the entire scan, as a backstop in case a probe neither
-- succeeds, errors, nor closes. Generous enough not to truncate a slow Studio;
-- short enough that a Refresh click still feels responsive.
local SCAN_TIMEOUT_SEC = 6

local function parseEntry(payload: Payload, port: number): DaemonEntry?
	if payload.type ~= "daemon_info" then
		return nil
	end
	local daemonId = payload.daemon_id
	if typeof(daemonId) ~= "string" or daemonId == "" then
		-- Without an id we cannot key a stored token or a remembered choice,
		-- so the row would be useless even if we rendered it.
		return nil
	end
	-- The daemon reports its own port; trust the dialed one if it is missing or
	-- malformed, since that is what we would connect to anyway.
	local reportedPort = payload.port
	local resolvedPort = if typeof(reportedPort) == "number" then reportedPort else port
	return {
		daemonId = daemonId,
		projectName = if typeof(payload.project_name) == "string"
			then payload.project_name
			else "(unnamed project)",
		projectRoot = if typeof(payload.project_root) == "string" then payload.project_root else "",
		port = resolvedPort,
		daemonVersion = if typeof(payload.daemon_version) == "string"
			then payload.daemon_version
			else "?",
		pluginConnected = payload.plugin_connected == true,
		url = string.format("ws://127.0.0.1:%d", resolvedPort),
	}
end

-- Scans the window and calls `onComplete` exactly once with every daemon that
-- answered, sorted by port. Never throws: a port that errors, times out, or
-- answers something unexpected simply contributes nothing.
--
-- `onProgress` (optional) fires per answer so the picker can fill in
-- incrementally instead of waiting for the whole scan to settle.
function Discovery.scan(
	options: Options?,
	onComplete: ({ DaemonEntry }) -> (),
	onProgress: ((DaemonEntry) -> ())?
): Handle
	local opts: Options = options or {}
	local log = opts.log
	local function trace(message: string)
		if log ~= nil then
			log(message)
		end
	end

	local found: { DaemonEntry } = {}
	-- Dedupe by daemon id. Two ports should never report the same daemon, but a
	-- stale socket or a daemon restarting mid-scan could produce a double, and
	-- a duplicated row in the picker would be actively confusing.
	local seen: { [string]: boolean } = {}
	local cancelled = false
	local settled = false

	local function finish()
		if settled then
			return
		end
		settled = true
		table.sort(found, function(a: DaemonEntry, b: DaemonEntry): boolean
			return a.port < b.port
		end)
		trace(string.format("[discovery] scan complete: %d daemon(s)", #found))
		onComplete(found)
	end

	-- Probes one port and resolves via `done` exactly once, whatever happens.
	local function probe(port: number, done: () -> ())
		local url = string.format("ws://127.0.0.1:%d", port)
		local handle: ConnectionHandle? = nil
		local finished = false
		-- True when `settleOne` ran before `Connection.connect` returned, so the
		-- handle did not exist yet to be closed. `Connection.connect` can invoke
		-- `onError` synchronously (its HttpEnabled pre-flight does exactly
		-- that), and Studio may deliver `Opened` before the assignment below
		-- lands. Without this flag such a probe would leave its socket open for
		-- the lifetime of the plugin VM — ten leaked sockets per scan, and the
		-- user's only symptom would be discovery mysteriously degrading.
		local closePending = false
		local function settleOne(reason: string)
			if finished then
				return
			end
			finished = true
			if reason ~= "" then
				trace(string.format("[discovery] :%d %s", port, reason))
			end
			local current = handle
			if current ~= nil then
				pcall(function()
					current:close()
				end)
			else
				closePending = true
			end
			done()
		end

		handle = Connection.connect(url, {
			onOpen = function()
				if cancelled then
					settleOne("cancelled")
					return
				end
				local current = handle
				if current == nil then
					settleOne("no handle")
					return
				end
				current:send({
					type = "hello",
					-- The daemon answers discovery before its version gate, so
					-- this value never gates the reply; it is sent because
					-- `Hello.version` is a required field.
					version = "0.5.0",
					role = "discover",
				})
			end,
			onMessage = function(payload: Payload)
				local entry = parseEntry(payload, port)
				if entry ~= nil and not seen[entry.daemonId] then
					seen[entry.daemonId] = true
					table.insert(found, entry)
					if onProgress ~= nil then
						-- A throwing picker callback must not abort the scan.
						pcall(onProgress, entry)
					end
					settleOne(string.format("found %s", entry.projectName))
					return
				end
				-- Anything else (an old daemon's `auth_rejected`, a non-Yeet
				-- process) means "not a daemon we can use" — same outcome as
				-- silence, so stop waiting on this port.
				settleOne("not a Yeet daemon")
			end,
			onError = function(_message: string)
				-- Expected for every port with nothing listening: this is the
				-- common case, not a fault. Left untraced to keep the log
				-- readable; the completion line reports the total.
				settleOne("")
			end,
			onClose = function()
				settleOne("")
			end,
		}, {
			skipHttpEnabledCheck = opts.skipHttpEnabledCheck,
		})

		if handle == nil then
			-- `Connection.connect` already reported why through its own
			-- onError (most often: HTTP requests disabled for this place).
			settleOne("connect refused locally")
			return
		end

		-- A callback settled this probe before `connect` returned, so the close
		-- above found no handle. Close it now that we have one.
		if closePending then
			local current = handle
			pcall(function()
				current:close()
			end)
			return
		end

		-- Already settled (a synchronous callback we did handle) — do not arm a
		-- deadline for a probe that is finished.
		if finished then
			return
		end

		-- Per-probe deadline, so one wedged port cannot hold up its batch.
		task.delay(PROBE_TIMEOUT_SEC, function()
			settleOne("")
		end)
	end

	task.spawn(function()
		local offset = 0
		while offset < PORT_WINDOW_SIZE and not cancelled and not settled do
			local remaining = PORT_WINDOW_SIZE - offset
			local batch = if remaining < PROBE_BATCH_SIZE then remaining else PROBE_BATCH_SIZE
			local outstanding = batch
			local batchDone = false
			for i = 0, batch - 1 do
				probe(PORT_WINDOW_BASE + offset + i, function()
					outstanding -= 1
					if outstanding <= 0 then
						batchDone = true
					end
				end)
			end
			-- Wait for the batch to drain. Every probe carries its own
			-- `task.delay` deadline, so this always completes; the wall-clock
			-- bound is a second line of defence in case a probe's callback is
			-- somehow never reached. Measured with `os.clock` rather than by
			-- accumulating `task.wait()` returns, which drift under load.
			local deadline = os.clock() + PROBE_TIMEOUT_SEC + 1
			while not batchDone and not cancelled and os.clock() < deadline do
				task.wait()
			end
			offset += batch
		end
		finish()
	end)

	-- Backstop: fires even if a probe never reports, so the picker always
	-- leaves its "scanning" state.
	task.delay(SCAN_TIMEOUT_SEC, finish)

	local handle: Handle
	handle = {
		cancel = function(_self: Handle)
			cancelled = true
			finish()
		end,
	}
	return handle
end

-- Exposed so the settings UI and diagnostics can describe the scanned range
-- without duplicating the constants.
function Discovery.windowDescription(): string
	return string.format(
		"ports %d-%d",
		PORT_WINDOW_BASE,
		PORT_WINDOW_BASE + PORT_WINDOW_SIZE - 1
	)
end

return Discovery
