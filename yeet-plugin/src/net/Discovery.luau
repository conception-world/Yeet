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

-- Mirrors `PLUGIN_VERSION` in ui/Widget.luau. Duplicated rather than shared
-- because this module sits below the UI layer and must not depend upward on
-- it; the value only labels the handshake, since discovery is answered before
-- the daemon's version gate.
local PLUGIN_VERSION = "0.6.0"

-- Keep in lockstep with PORT_WINDOW_BASE / PORT_WINDOW_SIZE in
-- yeet-daemon/src/main.rs. A daemon outside this window is unreachable by the
-- picker (it is still reachable by setting the Daemon URL by hand, and the
-- daemon logs a warning when it lands outside).
local PORT_WINDOW_BASE = 34872
local PORT_WINDOW_SIZE = 10

-- How many probes may be in flight at once.
--
-- Roblox does not document a concurrent-WebStreamClient limit, so rather than
-- betting on a number, the scanner starts here and *measures*: a port that
-- fails to construct a client while several are already open is treated as
-- evidence of a cap, the batch size is halved, and that port is retried. See
-- `retryQueue` below. Four is the starting guess; the adaptation is what makes
-- the scanner correct regardless of whether the real cap is 2, 8, or absent.
local PROBE_BATCH_SIZE = 4

-- Floor for the adaptive batch size. At 1 the scan is fully sequential, which
-- is slow but always works; going below would mean not scanning at all.
local MIN_BATCH_SIZE = 1

-- Upper bound on a single probe. A port with nothing listening normally fails
-- its TCP connect almost immediately on loopback, so this budget is really for
-- the pathological case: something IS bound but never completes the WebSocket
-- upgrade (an unrelated app squatting a port in our window). Without a
-- per-probe deadline such a port would stall its whole batch.
--
-- Whether a dead loopback port reports itself promptly is the other thing
-- Roblox does not document. If it does, this timeout is nearly never reached
-- and a scan settles in milliseconds; if it does not, every scan would cost the
-- full budget per batch. `FAST_SETTLE_GRACE_SEC` below removes that worst case
-- without assuming either answer.
local PROBE_TIMEOUT_SEC = 1.5

-- Once ANY port in a batch has settled, the rest get only this much longer.
--
-- All ports in a batch are dialled at the same instant over loopback, so a
-- healthy one answering proves the round trip is fast. Anything still silent
-- well after that is a port with nothing on it, whatever signal Studio does or
-- does not deliver for that case. This turns "dead ports never report" from a
-- worst case that dominates every scan into a fixed, small tax.
local FAST_SETTLE_GRACE_SEC = 0.35

-- Upper bound on the entire scan, as a backstop in case a probe neither
-- succeeds, errors, nor closes.
--
-- Sized for the worst path the adaptive loop can take: every port refused,
-- forcing the batch size down to 1 and a full per-port wait each. Being
-- generous costs nothing in the common case, where the scan settles in well
-- under a second and this timer is simply cancelled by `finish` running first;
-- being too tight would silently truncate a slow scan and show the user an
-- empty picker on a machine that has daemons running.
local SCAN_TIMEOUT_SEC = PORT_WINDOW_SIZE * PROBE_TIMEOUT_SEC + 4

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
	--
	-- `done(constructFailed)` reports whether the failure was Studio refusing to
	-- create the WebSocket client at all, as opposed to the port simply having
	-- nothing on it. The caller uses that to detect a concurrency cap and retry
	-- the port in a smaller batch.
	local function probe(port: number, done: (constructFailed: boolean) -> ())
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
		local function settleOne(reason: string, constructFailed: boolean?)
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
			done(constructFailed == true)
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
					-- `Hello.version` is a required field. Kept accurate anyway
					-- so the daemon's handshake log names the real build.
					version = PLUGIN_VERSION,
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
			-- `Connection.connect` returned nil: either the HttpEnabled
			-- pre-flight refused, or `CreateWebStreamClient` itself failed.
			-- The latter is what a concurrency cap looks like from here, so
			-- flag it and let the caller retry this port in a smaller batch.
			-- A genuinely disabled HttpService retries harmlessly: every port
			-- fails the same way and the scan still ends empty, which is the
			-- correct outcome.
			settleOne("client could not be created", true)
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
		-- Ports still to try. Seeded with the whole window; a port refused
		-- because Studio would not create another client goes back on the end
		-- so a smaller batch can retry it.
		local pending: { number } = {}
		for offset = 0, PORT_WINDOW_SIZE - 1 do
			table.insert(pending, PORT_WINDOW_BASE + offset)
		end
		-- Adapts downward when Studio refuses concurrent clients. Never adapts
		-- back up within a scan: having found a ceiling, re-testing it every
		-- round would just re-fail the same ports.
		local batchSize = PROBE_BATCH_SIZE
		-- Guards against an infinite retry loop when construction fails for a
		-- reason that is NOT concurrency (HttpService disabled being the common
		-- one). Each port earns at most one retry.
		local retried: { [number]: boolean } = {}

		while #pending > 0 and not cancelled and not settled do
			local batch: { number } = {}
			for _ = 1, math.min(batchSize, #pending) do
				table.insert(batch, table.remove(pending, 1) :: number)
			end

			local outstanding = #batch
			local batchDone = false
			local refused: { number } = {}
			-- Set by the first probe in this batch to settle, whatever its
			-- outcome. Loopback dials all of them at once, so one settling
			-- proves the round trip is quick and the stragglers are dead ports.
			local firstSettleAt: number? = nil

			for _, port in batch do
				probe(port, function(constructFailed: boolean)
					if constructFailed and not retried[port] then
						retried[port] = true
						table.insert(refused, port)
					end
					if firstSettleAt == nil then
						firstSettleAt = os.clock()
					end
					outstanding -= 1
					if outstanding <= 0 then
						batchDone = true
					end
				end)
			end

			-- Wait for the batch. Two bounds, whichever comes first: the full
			-- per-probe budget, or a short grace period after the first probe
			-- settled. The grace is what keeps a scan fast if it turns out
			-- Studio never signals a dead loopback port — without it every
			-- batch would cost the full budget.
			local hardDeadline = os.clock() + PROBE_TIMEOUT_SEC + 1
			while not batchDone and not cancelled and os.clock() < hardDeadline do
				local seenAt = firstSettleAt
				if seenAt ~= nil and os.clock() - seenAt > FAST_SETTLE_GRACE_SEC then
					break
				end
				task.wait()
			end

			if #refused > 0 then
				-- Studio would not hand out another client while this many were
				-- open. Halve and retry the refused ports; at batchSize 1 the
				-- refusal is not about concurrency, so stop blaming it.
				if batchSize > MIN_BATCH_SIZE then
					batchSize = math.max(MIN_BATCH_SIZE, batchSize // 2)
					trace(string.format(
						"[discovery] client creation refused; retrying %d port(s) with batch size %d",
						#refused,
						batchSize
					))
					for _, port in refused do
						table.insert(pending, port)
					end
				else
					trace(string.format(
						"[discovery] %d port(s) refused a client even one at a time "
							.. "(is HttpService enabled?)",
						#refused
					))
				end
			end
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
