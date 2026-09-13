--!strict

-- THROWAWAY SPIKE — not part of the plugin. Delete once its questions are answered.
--
-- ## What this is for
--
-- The project picker depends on scanning a window of loopback ports from inside
-- Studio. Two things about `HttpService:CreateWebStreamClient` are undocumented,
-- and both change how the real scanner must be written:
--
--   Q1. How many WebSocket clients can exist at once? If Studio caps it lower
--       than the batch size in net/Discovery.luau, probes silently fail and the
--       picker looks empty for no visible reason.
--
--   Q2. When nothing is listening on a port, does `Error`/`Closed` fire
--       promptly, or does the socket hang until our own timeout? This decides
--       whether a scan settles in milliseconds or always costs the full budget.
--
-- Answering these by guessing is exactly the failure this spike exists to
-- avoid, so it measures rather than assumes.
--
-- ## How to run it
--
--   1. Start at least one Yeet daemon (any project): `yeet-daemon <project>`.
--      Note the `yeet-port:` line it prints. Two daemons is better than one.
--   2. In Studio: File -> Game Settings -> Security -> Allow HTTP Requests = ON.
--   3. Open the Command Bar (View -> Command Bar) and paste this whole file.
--   4. Read the Output window.
--
-- It only opens sockets and prints. It never writes to the DataModel, never
-- touches the filesystem, and never sends anything but a discovery Hello.

local HttpService = game:GetService("HttpService")

local PORT_WINDOW_BASE = 34872
local PORT_WINDOW_SIZE = 10
-- Deliberately ALL AT ONCE, unlike the real scanner's batching: the point is to
-- find out whether Studio tolerates it.
local PROBE_TIMEOUT_SEC = 3

type Result = {
	port: number,
	opened: boolean,
	answered: boolean,
	projectName: string?,
	-- Seconds from CreateWebStreamClient to the first terminal signal.
	elapsed: number,
	outcome: string,
}

local results: { Result } = {}
local constructed = 0
local constructFailures: { string } = {}

print("─────────────────────────────────────────────")
print("[spike] Yeet discovery — probing all "
	.. tostring(PORT_WINDOW_SIZE)
	.. " ports simultaneously")
print("[spike] HttpEnabled = " .. tostring(HttpService.HttpEnabled))
if not HttpService.HttpEnabled then
	warn("[spike] HTTP is OFF — every probe will fail. "
		.. "File -> Game Settings -> Security -> Allow HTTP Requests.")
end
print("─────────────────────────────────────────────")

local startedAll = os.clock()

for offset = 0, PORT_WINDOW_SIZE - 1 do
	local port = PORT_WINDOW_BASE + offset
	local url = string.format("ws://127.0.0.1:%d", port)
	local started = os.clock()
	local settled = false

	local result: Result = {
		port = port,
		opened = false,
		answered = false,
		projectName = nil,
		elapsed = 0,
		outcome = "no signal (still pending at report time)",
	}
	table.insert(results, result)

	local function settle(outcome: string)
		if settled then
			return
		end
		settled = true
		result.elapsed = os.clock() - started
		result.outcome = outcome
	end

	-- Q1: does constructing the Nth client throw?
	local ok, socketOrErr = pcall(function()
		return (HttpService :: any):CreateWebStreamClient(
			Enum.WebStreamClientType.WebSocket,
			{ Url = url }
		)
	end)

	if not ok or socketOrErr == nil then
		local message = tostring(socketOrErr)
		table.insert(
			constructFailures,
			string.format(":%d %s", port, message)
		)
		settle("CreateWebStreamClient failed: " .. message)
		continue
	end

	constructed += 1
	local socket = socketOrErr

	socket.Opened:Connect(function()
		result.opened = true
		local sendOk, sendErr = pcall(function()
			socket:Send(HttpService:JSONEncode({
				type = "hello",
				version = "0.6.0",
				role = "discover",
			}))
		end)
		if not sendOk then
			settle("opened but Send threw: " .. tostring(sendErr))
		end
	end)

	socket.MessageReceived:Connect(function(raw: string)
		local decodeOk, payload = pcall(function()
			return HttpService:JSONDecode(raw)
		end)
		if decodeOk and typeof(payload) == "table" and payload.type == "daemon_info" then
			result.answered = true
			result.projectName = tostring(payload.project_name)
			settle(string.format(
				"daemon_info: %s (in use: %s)",
				tostring(payload.project_name),
				tostring(payload.plugin_connected)
			))
		else
			local kind = if decodeOk and typeof(payload) == "table"
				then tostring(payload.type)
				else "undecodable"
			settle("replied but not daemon_info (" .. kind .. ")")
		end
		pcall(function()
			socket:Close()
		end)
	end)

	-- Q2: how quickly does a dead port report itself?
	socket.Error:Connect(function(statusCode: number, message: string)
		settle(string.format("Error [%d] %s", statusCode, tostring(message)))
	end)

	socket.Closed:Connect(function()
		settle("Closed")
	end)
end

print(string.format(
	"[spike] constructed %d/%d clients simultaneously",
	constructed,
	PORT_WINDOW_SIZE
))
if #constructFailures > 0 then
	warn("[spike] Q1 ANSWERED — Studio refused some concurrent clients:")
	for _, failure in constructFailures do
		warn("   " .. failure)
	end
else
	print("[spike] Q1: no construction failures at "
		.. tostring(PORT_WINDOW_SIZE)
		.. " concurrent clients")
end

task.wait(PROBE_TIMEOUT_SEC)

print("─────────────────────────────────────────────")
print("[spike] RESULTS")
print("─────────────────────────────────────────────")

local answered = 0
local slowest = 0
local pending = 0
for _, r in results do
	if r.answered then
		answered += 1
	end
	if r.outcome:match("^no signal") then
		pending += 1
	elseif r.elapsed > slowest then
		slowest = r.elapsed
	end
	print(string.format(
		"  :%d  %6.0fms  %s",
		r.port,
		r.elapsed * 1000,
		r.outcome
	))
end

print("─────────────────────────────────────────────")
print(string.format("[spike] daemons found: %d", answered))
print(string.format(
	"[spike] total wall clock: %.0fms",
	(os.clock() - startedAll) * 1000
))
print(string.format("[spike] slowest settled probe: %.0fms", slowest * 1000))

if pending > 0 then
	warn(string.format(
		"[spike] Q2 ANSWERED (badly) — %d port(s) never signalled at all. "
			.. "Dead ports do NOT report themselves, so the scanner MUST rely on "
			.. "its per-probe deadline and the scan will always cost the full budget.",
		pending
	))
else
	print(string.format(
		"[spike] Q2 ANSWERED (well) — every port signalled within %.0fms. "
			.. "Dead ports report promptly, so the scan settles fast and the "
			.. "timeout is only a backstop.",
		slowest * 1000
	))
end
print("─────────────────────────────────────────────")
print("[spike] Report back: the Q1 line, the Q2 line, and the slowest probe.")
