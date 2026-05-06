--!strict

-- User-facing plugin settings: persisted via `plugin:SetSetting` so
-- they survive plugin reloads / Studio restarts (Studio writes them
-- into the user's profile directory, NOT the place file). Each
-- setting has a stable key prefixed with `yeet.setting.` so a future
-- migration can grep for "ours" without colliding with the auth
-- token (`yeet.authToken`).
--
-- ## Why every value is JSON-encoded
--
-- An earlier implementation passed booleans / strings directly to
-- `plugin:SetSetting(key, value)`. Empirically this didn't survive
-- a dock close + reopen on the user's Studio build — the next
-- `GetSetting(key)` returned `nil`, even though the SetSetting call
-- didn't error and the in-memory state had updated. Whether that's
-- a Studio bug or a sandboxing quirk, we don't have visibility.
--
-- Wrapping every value in `HttpService:JSONEncode({ v = value })` and
-- always reading it back via `JSONDecode` sidesteps the problem
-- entirely: every key is now stored as a single canonical string,
-- which Studio is observed to persist correctly. The wrapper table
-- (`{ v = ... }`) avoids JSONDecode of a bare boolean / null edge
-- cases on older Roblox HttpService versions.
--
-- The legacy direct-encoding path (boolean / number / string with
-- relaxed parsing) is kept inside `decodeStored` as a fallback for
-- any setting written by a previous plugin version, so users who
-- upgrade don't lose their existing config silently. New writes
-- always go through the JSON wrapper.

local HttpService = game:GetService("HttpService")

export type Settings = {
	-- When true, the plugin auto-clicks Connect once it finishes
	-- loading — useful for users who always want sync on, removing
	-- one click from their daily flow. Off by default so a fresh
	-- install never opens a network connection without consent.
	autoConnect: boolean,
	-- When true, skip the `HttpService.HttpEnabled` pre-flight check
	-- in Connection.connect. Power-users who know HTTP is enabled
	-- (e.g. they use HttpService for other systems too) can suppress
	-- the warning and the extra check. Misconfiguration falls back
	-- to the daemon's connection error — louder but later.
	skipHttpEnabledCheck: boolean,
	-- When true, the plugin's Activity log accepts trace-level
	-- breadcrumbs (echo decisions, frame sizes, watcher debounce
	-- ticks). Off keeps the log focused on user-meaningful events.
	verboseLogging: boolean,
	-- When true, the BulkSyncPreview dock auto-clicks "Apply Sync"
	-- on the daemon's default resolutions instead of waiting for
	-- the user. Intended for headless / CI flows; a typical user
	-- should leave this off so they review every bulk apply.
	autoConfirmBulkSync: boolean,
	-- Override for the daemon's WebSocket URL. Default is the same
	-- localhost port the extension's daemon binds. Devs running a
	-- second daemon on a custom port can point the plugin at it
	-- without recompiling.
	daemonUrl: string,
}

local Settings = {}

local DEFAULT_DAEMON_URL = "ws://127.0.0.1:34872"

-- All keys live under this prefix so a `plugin:GetSetting` listing
-- (if Roblox ever adds one) makes the namespace obvious. Underscores
-- only — earlier versions used dotted keys (`yeet.setting.foo`) but
-- empirically Studio's `plugin:SetSetting` either rejects or
-- silently fails to persist keys containing `.`. The same Studio
-- build round-trips alphanumeric+underscore keys reliably, so
-- switching is the simplest fix that doesn't require pivoting the
-- whole settings layer onto daemon-mediated storage.
local KEY_AUTO_CONNECT = "yeet_setting_autoConnect"
local KEY_SKIP_HTTP_CHECK = "yeet_setting_skipHttpEnabledCheck"
local KEY_VERBOSE = "yeet_setting_verboseLogging"
local KEY_AUTO_CONFIRM_BULK = "yeet_setting_autoConfirmBulkSync"
local KEY_DAEMON_URL = "yeet_setting_daemonUrl"

-- Decodes a value `plugin:GetSetting` returned. Three formats are
-- recognized, in order:
--   1. JSON-wrapped string `{"v": <value>}` — what `Settings.save`
--      writes today. Returns the unwrapped value.
--   2. Raw boolean / number / string — what older plugin versions
--      wrote directly. Returns as-is.
--   3. Anything else (nil, table, etc.) — returns nil so the
--      caller falls back to the default.
-- Logs every read for debugging "settings don't persist" reports.
local function decodeStored(p: Plugin, key: string): any
	local ok, raw = pcall(function() return p:GetSetting(key) end)
	if not ok then
		print(string.format(
			"[Yeet:settings] read %s: GetSetting threw (%s)", key, tostring(raw)
		))
		return nil
	end
	-- Common case: stored as JSON-wrapped string by `Settings.save`.
	if typeof(raw) == "string" then
		local decodeOk, decoded = pcall(function()
			return HttpService:JSONDecode(raw)
		end)
		if decodeOk and typeof(decoded) == "table" and decoded.v ~= nil then
			print(string.format(
				"[Yeet:settings] read %s: type=%s, value=%s (decoded from JSON wrapper)",
				key, typeof(decoded.v), tostring(decoded.v)
			))
			return decoded.v
		end
		-- Plain string from a legacy plugin version.
		print(string.format(
			"[Yeet:settings] read %s: type=string, value=%s (legacy plain)",
			key, tostring(raw)
		))
		return raw
	end
	-- Legacy boolean / number from a plugin version before
	-- the JSON-wrapping rewrite.
	print(string.format(
		"[Yeet:settings] read %s: type=%s, value=%s (legacy raw)",
		key, typeof(raw), tostring(raw)
	))
	return raw
end

local function readBool(p: Plugin, key: string, default: boolean): boolean
	local v = decodeStored(p, key)
	if typeof(v) == "boolean" then
		return v
	end
	if typeof(v) == "number" then
		return v ~= 0
	end
	if typeof(v) == "string" then
		local lower = string.lower(v)
		if lower == "true" or lower == "1" then return true end
		if lower == "false" or lower == "0" then return false end
	end
	return default
end

local function readString(p: Plugin, key: string, default: string): string
	local v = decodeStored(p, key)
	if typeof(v) == "string" and v ~= "" then
		return v
	end
	return default
end

-- Loads the settings struct from `plugin:GetSetting`. Missing or
-- malformed values fall back to the defaults declared in this
-- module. Never throws — caller can use the result immediately.
function Settings.load(p: Plugin): Settings
	return {
		autoConnect = readBool(p, KEY_AUTO_CONNECT, false),
		skipHttpEnabledCheck = readBool(p, KEY_SKIP_HTTP_CHECK, false),
		verboseLogging = readBool(p, KEY_VERBOSE, false),
		autoConfirmBulkSync = readBool(p, KEY_AUTO_CONFIRM_BULK, false),
		daemonUrl = readString(p, KEY_DAEMON_URL, DEFAULT_DAEMON_URL),
	}
end

-- Result of a `Settings.save` attempt. `ok` is true iff every field
-- successfully persisted (write succeeded AND readback matched).
-- `errors` carries one human-readable line per failed field (label
-- + reason) so the dock can surface a specific message instead of
-- a green "saved" banner that contradicts the on-disk state.
export type SaveResult = {
	ok: boolean,
	errors: { string },
}

-- Persists the supplied settings struct. Every field is JSON-
-- encoded as `{"v": <value>}` and stored as a single canonical
-- string per key. This sidesteps the empirical bug where Studio
-- did not persist boolean values across dock-reopen cycles for
-- this user — strings appear to round-trip reliably.
--
-- Returns a `SaveResult` so the caller (SettingsDock) can show an
-- accurate success/failure banner. Three classes of failure are
-- accumulated:
--   1. JSONEncode threw (e.g. NaN, Inf, recursive table reference)
--   2. plugin:SetSetting threw (sandbox, profile read-only, ...)
--   3. Readback returned nil (Studio "accepted" the write but
--      didn't actually persist it — observed on some builds with
--      dotted keys / boolean values; we still emit this so the
--      user sees a clear "did not persist" warning rather than a
--      false-positive success banner)
-- Each failure also `warn(...)`s to Studio Output for offline
-- triage; the returned `errors` table is for the in-dock UI.
function Settings.save(p: Plugin, s: Settings): SaveResult
	local errors: { string } = {}
	local function trySet(key: string, value: any, label: string)
		local jsonOk, encoded = pcall(function()
			return HttpService:JSONEncode({ v = value })
		end)
		if not jsonOk then
			local msg = string.format(
				"%s: JSON-encode failed (%s)",
				label, tostring(encoded)
			)
			warn("[Yeet] " .. msg)
			table.insert(errors, msg)
			return
		end
		local setOk, err = pcall(function() p:SetSetting(key, encoded) end)
		if not setOk then
			local msg = string.format(
				"%s: SetSetting threw (%s)",
				label, tostring(err)
			)
			warn("[Yeet] " .. msg)
			table.insert(errors, msg)
			return
		end
		-- Read-back same-tick to confirm the write actually persisted.
		local readOk, readBack = pcall(function() return p:GetSetting(key) end)
		print(string.format(
			"[Yeet:settings] wrote %s = %s; readback ok=%s, type=%s, value=%s",
			key, encoded,
			tostring(readOk), typeof(readBack), tostring(readBack)
		))
		if not readOk or readBack == nil then
			local msg = string.format(
				"%s: did not persist (readback returned nil — Studio's "
					.. "plugin:SetSetting may not work in this build)",
				label
			)
			warn("[Yeet] " .. msg)
			table.insert(errors, msg)
		end
	end
	trySet(KEY_AUTO_CONNECT, s.autoConnect, "autoConnect")
	trySet(KEY_SKIP_HTTP_CHECK, s.skipHttpEnabledCheck, "skipHttpEnabledCheck")
	trySet(KEY_VERBOSE, s.verboseLogging, "verboseLogging")
	trySet(KEY_AUTO_CONFIRM_BULK, s.autoConfirmBulkSync, "autoConfirmBulkSync")
	trySet(KEY_DAEMON_URL, s.daemonUrl, "daemonUrl")
	return { ok = #errors == 0, errors = errors }
end

-- Defaults factory used by the dock's "Reset to defaults" button.
function Settings.defaults(): Settings
	return {
		autoConnect = false,
		skipHttpEnabledCheck = false,
		verboseLogging = false,
		autoConfirmBulkSync = false,
		daemonUrl = DEFAULT_DAEMON_URL,
	}
end

return Settings
