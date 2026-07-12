--!strict

-- Thin wrapper around Studio's native WebSocket API
-- (`HttpService:CreateWebStreamClient`), shipped in late 2025. This is a
-- Studio-only API: calling it from a published experience is blocked by the
-- engine, which is fine because Yeet only ever runs inside Studio.
--
-- Handlers run on the engine's event thread; keep them fast and defer heavy
-- work to deferred events or task.spawn.
--
-- Upstream docs:
--   https://create.roblox.com/docs/reference/engine/classes/HttpService#CreateWebStreamClient
--   https://create.roblox.com/docs/reference/engine/classes/WebStreamClient

local HttpService = game:GetService("HttpService")

-- When the encoded body crosses this threshold, surface a warn so the Studio
-- Output window flags a candidate for "WebStream rejected as too big". The
-- only payload that realistically gets here is `studio_snapshot_report`, sent
-- once per bootstrap with one entry per script — large projects can blow past
-- a low MiB cap in a single send and the failure mode (Send failed without a
-- byte count) is otherwise indistinguishable from any other error.
local LARGE_FRAME_BYTES = 1024 * 1024

export type Payload = { [string]: any }

export type Handlers = {
	onOpen: (() -> ())?,
	onMessage: (Payload) -> (),
	onError: (string) -> (),
	onClose: (() -> ())?,
}

export type Options = {
	-- When true, skip the `HttpService.HttpEnabled` pre-flight check
	-- (the user toggled "Skip HTTP-enabled check" in Settings).
	-- Misconfiguration falls back to the daemon's connection error
	-- — louder but later. Default false (check runs).
	skipHttpEnabledCheck: boolean?,
}

export type Handle = {
	-- Returns true when the frame was JSON-encoded and handed to the socket
	-- without error, false otherwise (encode failure or socket Send throw).
	-- Callers gate their "sent" bookkeeping/logging on this (B10).
	send: (self: Handle, payload: Payload) -> boolean,
	close: (self: Handle) -> (),
}

local Connection = {}

function Connection.connect(url: string, handlers: Handlers, options: Options?): Handle?
	-- Pre-flight: WebStreamClient internally requires HTTP requests
	-- to be enabled. The setting is per-place (File → Game Settings
	-- → Security → Allow HTTP Requests), and a fresh user often has
	-- it disabled. Without this check, the failure surfaces as a
	-- generic "CreateWebStreamClient failed" string in the dock log
	-- — not actionable. Surfacing the check explicitly here gives
	-- the user a clear instruction to follow. Power-users can
	-- suppress via the `skipHttpEnabledCheck` option (Settings dock).
	local skipCheck = options ~= nil and options.skipHttpEnabledCheck == true
	if not skipCheck and not HttpService.HttpEnabled then
		handlers.onError(
			"HTTP requests are disabled in this place. Open File → "
				.. "Game Settings → Security and toggle 'Allow HTTP "
				.. "Requests' on, then click Connect again."
		)
		return nil
	end

	local ok, socketOrErr = pcall(function()
		return (HttpService :: any):CreateWebStreamClient(
			Enum.WebStreamClientType.WebSocket,
			{ Url = url }
		)
	end)
	if not ok or socketOrErr == nil then
		handlers.onError("CreateWebStreamClient failed: " .. tostring(socketOrErr))
		return nil
	end

	local socket = socketOrErr

	-- `Opened` fires once the HTTP upgrade succeeds; fire `onOpen` only then so
	-- callers don't race by sending before the socket is actually writable.
	-- Signal fires with (statusCode, headers) but we don't use either; a
	-- no-arg callback is legal and keeps the Luau unused-local lint quiet.
	socket.Opened:Connect(function()
		if handlers.onOpen then
			handlers.onOpen()
		end
	end)

	socket.MessageReceived:Connect(function(raw: string)
		local okDecode, payload = pcall(function()
			return HttpService:JSONDecode(raw)
		end)
		if okDecode then
			handlers.onMessage(payload :: Payload)
		else
			handlers.onError("JSONDecode failed: " .. tostring(payload))
		end
	end)

	socket.Error:Connect(function(statusCode: number, message: string)
		handlers.onError(
			string.format("ws error [%d]: %s", statusCode, tostring(message))
		)
	end)

	socket.Closed:Connect(function()
		if handlers.onClose then
			handlers.onClose()
		end
	end)

	local handle: Handle
	handle = {
		send = function(_self: Handle, payload: Payload): boolean
			local payloadType = tostring(payload.type or "<no-type>")
			local okEncode, body = pcall(function()
				return HttpService:JSONEncode(payload)
			end)
			if not okEncode then
				handlers.onError(string.format(
					"JSONEncode failed (type=%s): %s",
					payloadType,
					tostring(body)
				))
				return false
			end
			local bytes = #body
			if bytes >= LARGE_FRAME_BYTES then
				warn(string.format(
					"[Yeet] large outbound ws frame: type=%s bytes=%d",
					payloadType,
					bytes
				))
			end
			local okSend, err = pcall(function()
				socket:Send(body)
			end)
			if not okSend then
				handlers.onError(string.format(
					"Send failed (type=%s, bytes=%d): %s",
					payloadType,
					bytes,
					tostring(err)
				))
				return false
			end
			return true
		end,
		close = function(_self: Handle)
			pcall(function()
				socket:Close()
			end)
		end,
	}
	return handle
end

return Connection
