--!strict

-- Plugin-side bridge to the IDE's native folder-picker dialog.
--
-- Flow:
--   1. caller runs `FolderPicker.ask(conn, prompt)` on a non-engine thread
--      (click handlers qualify — Studio wraps each one in its own task)
--   2. we generate a request_id, register a BindableEvent keyed by it, and
--      send a `pick_folder_prompt` frame on the wire
--   3. daemon forwards the prompt to the extension; extension opens
--      `vscode.window.showOpenDialog` and sends back `pick_folder_response`
--   4. daemon broadcasts `pick_folder_result` — Applier routes it to our
--      dispatcher, which looks up the BindableEvent and fires it
--   5. `ask` unblocks and returns the path (or nil when the user cancelled)
--
-- `attach` must be called once per Applier lifetime so the `pick_folder_result`
-- dispatcher slot is occupied. Calling `ask` without attach first will just
-- log a warning on Applier and the wait will hang — attach is non-optional.

local HttpService = game:GetService("HttpService")

-- Redeclared locally: Luau strict does not reliably propagate generic types
-- across require boundaries, so each consumer restates the shape it needs.
type ConnectionHandle = {
	send: (self: ConnectionHandle, payload: { [string]: any }) -> (),
	close: (self: ConnectionHandle) -> (),
}

type ApplierHandle = {
	setPickFolderDispatcher: (
		self: ApplierHandle,
		dispatcher: ((payload: { [string]: any }) -> ())?
	) -> (),
}

-- BindableEvent fires with { path } rather than `path` directly so Event:Wait
-- unambiguously returns a value for both "cancelled" (path = nil) and "not
-- cancelled" paths. Firing a bare nil makes Event:Wait's return count fragile.
type Result = { path: string? }

local waiters: { [string]: BindableEvent } = {}

local FolderPicker = {}

function FolderPicker.attach(applier: ApplierHandle)
	applier:setPickFolderDispatcher(function(payload: { [string]: any })
		local id = payload.request_id
		if typeof(id) ~= "string" then
			return
		end
		local waiter = waiters[id]
		if waiter == nil then
			-- Result for a request we never asked for — typical if two plugins
			-- share a daemon and this one isn't the originator. Drop silently.
			return
		end
		waiters[id] = nil
		local raw = payload.path
		local path: string? = if typeof(raw) == "string" then raw else nil
		waiter:Fire({ path = path } :: Result)
		waiter:Destroy()
	end)
end

function FolderPicker.detach(applier: ApplierHandle)
	applier:setPickFolderDispatcher(nil)
	for id, waiter in waiters do
		waiters[id] = nil
		-- Unblock anyone still waiting so they don't deadlock on teardown.
		waiter:Fire({ path = nil } :: Result)
		waiter:Destroy()
	end
end

function FolderPicker.ask(connection: ConnectionHandle, prompt: string): string?
	local id = HttpService:GenerateGUID(false)
	local event = Instance.new("BindableEvent")
	waiters[id] = event

	connection:send({
		type = "pick_folder_prompt",
		request_id = id,
		prompt = prompt,
	})

	local result = event.Event:Wait() :: Result
	return result.path
end

return FolderPicker
