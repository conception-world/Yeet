--!strict

-- Dedicated dock for plugin settings. Opened from the toolbar
-- "Settings" button (registered in init.server.luau). Renders a
-- list of toggles + a daemon URL field, with Save / Reset.
--
-- Lifecycle: lazy-created on first toolbar click. Persists via the
-- `Settings` module which goes through `plugin:SetSetting` so the
-- values survive plugin reloads. Hot-apply notifies the host
-- (`onSaved` callback) so the Widget can react in real time —
-- toggle Verbose Logging now and the next log line shows.

local Roact = require(script.Parent.Parent.Packages.Roact)
local theme = require(script.Parent.theme)
local Card = require(script.Parent.components.Card)
local Button = require(script.Parent.components.Button)
local Heading = require(script.Parent.components.Heading)

local Settings = require(script.Parent.Settings)
type SettingsT = {
	autoConnect: boolean,
	skipHttpEnabledCheck: boolean,
	verboseLogging: boolean,
	autoConfirmBulkSync: boolean,
	daemonUrl: string,
}

-- Per-VM unique WIDGET_ID. See BulkSyncPreview.luau for rationale.
local BASE_WIDGET_ID = "YeetSettings"
local function freshWidgetId(): string
	local guid = game:GetService("HttpService"):GenerateGUID(false)
	return string.format("%s_%s", BASE_WIDGET_ID, guid)
end
local WIDGET_ID = freshWidgetId()

-- ─── Pure view ────────────────────────────────────────────────────────────

type ToggleRowProps = {
	label: string,
	description: string,
	value: boolean,
	onToggle: (boolean) -> (),
	layoutOrder: number,
}

local function ToggleRow(props: ToggleRowProps)
	-- Two columns: left = label + description (flex-fill), right =
	-- toggle pill button. Click anywhere on the row toggles, but
	-- only the pill shows the on/off state (label area stays neutral
	-- so a misclick on whitespace still flips intentionally).
	return Roact.createElement("Frame", {
		Size = UDim2.new(1, 0, 0, 0),
		AutomaticSize = Enum.AutomaticSize.Y,
		BackgroundTransparency = 1,
		LayoutOrder = props.layoutOrder,
	}, {
		layout = Roact.createElement("UIListLayout", {
			FillDirection = Enum.FillDirection.Horizontal,
			VerticalAlignment = Enum.VerticalAlignment.Center,
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, theme.spacing.md),
		}),
		text = Roact.createElement("Frame", {
			Size = UDim2.fromScale(1, 1),
			BackgroundTransparency = 1,
			AutomaticSize = Enum.AutomaticSize.Y,
			LayoutOrder = 1,
		}, {
			flex = Roact.createElement("UIFlexItem", {
				FlexMode = Enum.UIFlexMode.Fill,
			}),
			layout = Roact.createElement("UIListLayout", {
				FillDirection = Enum.FillDirection.Vertical,
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, 2),
			}),
			label = Roact.createElement("TextLabel", {
				Size = UDim2.new(1, 0, 0, 18),
				BackgroundTransparency = 1,
				Text = props.label,
				TextColor3 = theme.colors.text,
				Font = theme.typography.label.font,
				TextSize = theme.typography.label.size,
				TextXAlignment = Enum.TextXAlignment.Left,
				LayoutOrder = 1,
			}),
			description = Roact.createElement("TextLabel", {
				Size = UDim2.new(1, 0, 0, 0),
				AutomaticSize = Enum.AutomaticSize.Y,
				BackgroundTransparency = 1,
				Text = props.description,
				TextColor3 = theme.colors.textMuted,
				Font = theme.typography.caption.font,
				TextSize = theme.typography.caption.size,
				TextXAlignment = Enum.TextXAlignment.Left,
				TextWrapped = true,
				LayoutOrder = 2,
			}),
		}),
		toggle = Roact.createElement(Button, {
			text = if props.value then "On" else "Off",
			variant = if props.value then "accept" else "ghost",
			size = UDim2.new(0, 60, 0, 28),
			layoutOrder = 2,
			onClick = function()
				props.onToggle(not props.value)
			end,
		}),
	})
end

type ViewProps = {
	visible: boolean,
	settings: SettingsT,
	dirty: boolean,
	-- True for ~3s after a successful Save. Renders a transient
	-- "✓ Settings saved" banner at the top of the dock so the user
	-- has explicit visual confirmation. Without this banner, the
	-- only feedback was the Save button changing label from "Save
	-- changes" to "Saved" — easy to miss; users complained the
	-- settings "weren't saving" when they were just invisible.
	recentlySaved: boolean,
	-- Non-empty when the most recent Save failed, partially or
	-- entirely. Each entry is a "<label>: <reason>" line returned
	-- by `Settings.save`. Renders a red banner that lists the
	-- failures so the user sees which fields didn't persist
	-- instead of getting a false-positive "Saved" banner.
	saveErrors: { string },
	onChange: (key: string, value: any) -> (),
	onSave: () -> (),
	onReset: () -> (),
	onClose: () -> (),
}

local function SettingsView(props: ViewProps)
	if not props.visible then
		return Roact.createElement("Frame", {
			Size = UDim2.fromScale(1, 1),
			BackgroundColor3 = theme.colors.background,
			BorderSizePixel = 0,
		})
	end
	local s = props.settings

	local togglesCard = Roact.createElement(Card, {
		layoutOrder = 2,
		padding = theme.spacing.md,
		children = {
			layout = Roact.createElement("UIListLayout", {
				FillDirection = Enum.FillDirection.Vertical,
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, theme.spacing.md),
			}),
			autoConnect = Roact.createElement(ToggleRow, {
				label = "Auto-connect on plugin load",
				description = "Skip the manual Connect click — Yeet connects to the daemon as soon as the plugin loads. Off by default so a fresh install never opens a network connection without consent.",
				value = s.autoConnect,
				layoutOrder = 1,
				onToggle = function(v) props.onChange("autoConnect", v) end,
			}),
			skipHttp = Roact.createElement(ToggleRow, {
				label = "Skip HTTP-enabled check",
				description = "Suppress the \"HTTP requests are disabled\" pre-flight banner. Use only if you've already enabled HTTP in Game Settings — misconfiguration will fall back to the daemon's connection error instead.",
				value = s.skipHttpEnabledCheck,
				layoutOrder = 2,
				onToggle = function(v) props.onChange("skipHttpEnabledCheck", v) end,
			}),
			verbose = Roact.createElement(ToggleRow, {
				label = "Verbose logging",
				description = "Show trace-level breadcrumbs in the Activity log: echo decisions, frame sizes, watcher debounce ticks. Helpful for diagnosing sync ping-pongs.",
				value = s.verboseLogging,
				layoutOrder = 3,
				onToggle = function(v) props.onChange("verboseLogging", v) end,
			}),
			autoConfirm = Roact.createElement(ToggleRow, {
				label = "Auto-confirm bulk sync",
				description = "On bootstrap, automatically apply the daemon's default resolutions instead of waiting for the user. Intended for headless / CI flows; keep off if you want to review every bulk apply.",
				value = s.autoConfirmBulkSync,
				layoutOrder = 4,
				onToggle = function(v) props.onChange("autoConfirmBulkSync", v) end,
			}),
		},
	})

	-- Daemon URL row uses a TextBox (editable). Roblox TextBox needs
	-- explicit visual chrome to look like an input — UIStroke + a
	-- darker bg sets it apart from the surface card.
	local urlCard = Roact.createElement(Card, {
		layoutOrder = 3,
		padding = theme.spacing.md,
		children = {
			layout = Roact.createElement("UIListLayout", {
				FillDirection = Enum.FillDirection.Vertical,
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, theme.spacing.sm),
			}),
			label = Roact.createElement("TextLabel", {
				Size = UDim2.new(1, 0, 0, 18),
				BackgroundTransparency = 1,
				Text = "Daemon URL",
				TextColor3 = theme.colors.text,
				Font = theme.typography.label.font,
				TextSize = theme.typography.label.size,
				TextXAlignment = Enum.TextXAlignment.Left,
				LayoutOrder = 1,
			}),
			description = Roact.createElement("TextLabel", {
				Size = UDim2.new(1, 0, 0, 0),
				AutomaticSize = Enum.AutomaticSize.Y,
				BackgroundTransparency = 1,
				Text = "WebSocket URL the plugin connects to. Default ws://127.0.0.1:34872 matches what the bundled daemon binds. Change only if you're running a custom daemon on a different port.",
				TextColor3 = theme.colors.textMuted,
				Font = theme.typography.caption.font,
				TextSize = theme.typography.caption.size,
				TextXAlignment = Enum.TextXAlignment.Left,
				TextWrapped = true,
				LayoutOrder = 2,
			}),
			input = Roact.createElement("TextBox", {
				Size = UDim2.new(1, 0, 0, 32),
				BackgroundColor3 = theme.colors.background,
				BorderSizePixel = 0,
				Text = s.daemonUrl,
				PlaceholderText = "ws://127.0.0.1:34872",
				PlaceholderColor3 = theme.colors.textMuted,
				TextColor3 = theme.colors.text,
				Font = theme.typography.code.font,
				TextSize = theme.typography.code.size,
				TextXAlignment = Enum.TextXAlignment.Left,
				ClearTextOnFocus = false,
				LayoutOrder = 3,
				[Roact.Change.Text] = function(rbx)
					props.onChange("daemonUrl", rbx.Text)
				end,
			}, {
				corner = Roact.createElement("UICorner", {
					CornerRadius = UDim.new(0, theme.radius.sm),
				}),
				stroke = Roact.createElement("UIStroke", {
					Color = theme.colors.stroke,
					Thickness = 1,
					ApplyStrokeMode = Enum.ApplyStrokeMode.Border,
				}),
				padding = Roact.createElement("UIPadding", {
					PaddingLeft = UDim.new(0, theme.spacing.sm),
					PaddingRight = UDim.new(0, theme.spacing.sm),
				}),
			}),
		},
	})

	-- Transient "✓ Settings saved" banner shown for ~3s after a
	-- successful save. Renders only when `recentlySaved` is true;
	-- otherwise we show just the heading. Banner is the explicit
	-- visual confirmation users were missing — the bare label
	-- change on the Save button was too subtle.
	local headerChildren: { [string]: any } = {
		layout = Roact.createElement("UIListLayout", {
			FillDirection = Enum.FillDirection.Vertical,
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, theme.spacing.sm),
		}),
		heading = Roact.createElement(Heading, {
			title = "Yeet Settings",
			subtitle = "Changes apply on Save. Persisted across plugin reloads.",
			layoutOrder = 1,
		}),
	}
	if props.recentlySaved and #props.saveErrors == 0 then
		-- Full-success banner. Only shown when EVERY field
		-- persisted; if any failed, the error banner below
		-- replaces this one. Showing both green "Saved" and red
		-- "Did not persist" simultaneously would be incoherent.
		headerChildren.savedBanner = Roact.createElement("Frame", {
			Size = UDim2.new(1, 0, 0, 28),
			BackgroundColor3 = theme.colors.accentAccept,
			BorderSizePixel = 0,
			LayoutOrder = 2,
		}, {
			corner = Roact.createElement("UICorner", {
				CornerRadius = UDim.new(0, theme.radius.sm),
			}),
			label = Roact.createElement("TextLabel", {
				Size = UDim2.fromScale(1, 1),
				BackgroundTransparency = 1,
				Text = "✓ Settings saved",
				TextColor3 = theme.colors.textInverse,
				Font = theme.typography.label.font,
				TextSize = theme.typography.label.size,
				TextXAlignment = Enum.TextXAlignment.Center,
				TextYAlignment = Enum.TextYAlignment.Center,
			}),
		})
	end
	if #props.saveErrors > 0 then
		-- Partial-or-total failure banner. Auto-sizes vertically
		-- so the full list of failed fields is visible without
		-- scrolling — the user has a clear, complete picture of
		-- what didn't stick. Persists until the next Save (no
		-- 3s auto-clear like the success banner) so the user
		-- doesn't miss it.
		local errText = "✕ Save failed for:\n   • " .. table.concat(props.saveErrors, "\n   • ")
		headerChildren.errorBanner = Roact.createElement("Frame", {
			Size = UDim2.new(1, 0, 0, 0),
			AutomaticSize = Enum.AutomaticSize.Y,
			BackgroundColor3 = theme.colors.accentDecline,
			BorderSizePixel = 0,
			LayoutOrder = 2,
		}, {
			corner = Roact.createElement("UICorner", {
				CornerRadius = UDim.new(0, theme.radius.sm),
			}),
			pad = Roact.createElement("UIPadding", {
				PaddingTop = UDim.new(0, theme.spacing.sm),
				PaddingBottom = UDim.new(0, theme.spacing.sm),
				PaddingLeft = UDim.new(0, theme.spacing.md),
				PaddingRight = UDim.new(0, theme.spacing.md),
			}),
			label = Roact.createElement("TextLabel", {
				Size = UDim2.new(1, 0, 0, 0),
				AutomaticSize = Enum.AutomaticSize.Y,
				BackgroundTransparency = 1,
				Text = errText,
				TextColor3 = theme.colors.text,
				Font = theme.typography.body.font,
				TextSize = theme.typography.body.size,
				TextXAlignment = Enum.TextXAlignment.Left,
				TextYAlignment = Enum.TextYAlignment.Top,
				TextWrapped = true,
			}),
		})
	end
	local headerCard = Roact.createElement(Card, {
		layoutOrder = 1,
		padding = theme.spacing.md,
		children = headerChildren,
	})

	local footerCard = Roact.createElement(Card, {
		layoutOrder = 4,
		padding = theme.spacing.md,
		children = {
			layout = Roact.createElement("UIListLayout", {
				FillDirection = Enum.FillDirection.Horizontal,
				HorizontalAlignment = Enum.HorizontalAlignment.Right,
				VerticalAlignment = Enum.VerticalAlignment.Center,
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, theme.spacing.sm),
			}),
			reset = Roact.createElement(Button, {
				text = "Reset to defaults",
				variant = "ghost",
				layoutOrder = 1,
				onClick = props.onReset,
			}),
			close = Roact.createElement(Button, {
				text = "Close",
				variant = "ghost",
				layoutOrder = 2,
				onClick = props.onClose,
			}),
			save = Roact.createElement(Button, {
				text = if props.dirty then "Save changes" else "Saved",
				variant = if props.dirty then "primary" else "accept",
				layoutOrder = 3,
				disabled = not props.dirty,
				onClick = props.onSave,
			}),
		},
	})

	return Roact.createElement("ScrollingFrame", {
		Size = UDim2.fromScale(1, 1),
		BackgroundColor3 = theme.colors.background,
		BorderSizePixel = 0,
		AutomaticCanvasSize = Enum.AutomaticSize.Y,
		CanvasSize = UDim2.new(),
		ScrollBarThickness = 6,
		ScrollBarImageColor3 = theme.colors.surface,
	}, {
		padding = Roact.createElement("UIPadding", {
			PaddingTop = UDim.new(0, theme.spacing.md),
			PaddingBottom = UDim.new(0, theme.spacing.md),
			PaddingLeft = UDim.new(0, theme.spacing.md),
			PaddingRight = UDim.new(0, theme.spacing.md),
		}),
		layout = Roact.createElement("UIListLayout", {
			FillDirection = Enum.FillDirection.Vertical,
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, theme.spacing.md),
		}),
		header = headerCard,
		toggles = togglesCard,
		url = urlCard,
		footer = footerCard,
	})
end

-- ─── Imperative class ────────────────────────────────────────────────────

local SettingsDock = {}
SettingsDock.__index = SettingsDock

type self = {
	plugin: Plugin,
	gui: DockWidgetPluginGui,
	tree: any,
	visible: boolean,
	-- Working copy of the settings the user is currently editing.
	-- `Save` writes this to disk via `Settings.save`; `Close`
	-- without `Save` discards. `dirty` tracks whether `working`
	-- diverges from what's persisted so the Save button can show
	-- "Saved" when there's nothing to commit.
	working: SettingsT,
	dirty: boolean,
	-- True for ~3s after a successful save. Cleared by a deferred
	-- task. The view uses this to render a transient "✓ Settings
	-- saved" banner. Generation counter (`savedGen`) makes sure a
	-- pending clear from a previous save doesn't wipe out a fresh
	-- "saved" state if the user clicks Save twice quickly.
	recentlySaved: boolean,
	savedGen: number,
	-- Non-empty when the most recent Save failed for at least one
	-- field. Drives the red error banner. Cleared on the next
	-- Save attempt (the new attempt's result replaces the old
	-- list). The user must re-click Save to retry; we never
	-- automatically retry, since the failure modes (sandbox, full
	-- disk) usually need user action to fix.
	saveErrors: { string },
	-- Fired after a successful save with the new settings struct.
	-- Host (Widget / init.server.luau) re-reads anything that
	-- depends on settings (logger filter, daemon URL, etc.).
	-- Only fired when `Settings.save` returns ok=true.
	onSaved: ((SettingsT) -> ())?,
}

export type SettingsDock = typeof(setmetatable({} :: self, SettingsDock))

function SettingsDock.new(plugin: Plugin): SettingsDock
	local info = DockWidgetPluginGuiInfo.new(
		Enum.InitialDockState.Float,
		false,
		true,
		520, 480,
		380, 320
	)
	local gui = plugin:CreateDockWidgetPluginGui(WIDGET_ID, info)
	gui.Title = "Yeet — Settings"
	gui.Name = WIDGET_ID
	gui.Enabled = false

	local self = setmetatable({
		plugin = plugin,
		gui = gui,
		tree = nil,
		visible = false,
		working = Settings.load(plugin),
		dirty = false,
		recentlySaved = false,
		savedGen = 0,
		saveErrors = {} :: { string },
		onSaved = nil :: ((SettingsT) -> ())?,
	}, SettingsDock)
	self.tree = Roact.mount(self:_renderTree(), gui, "Content")
	return self
end

function SettingsDock._rerender(self: SettingsDock)
	if self.tree == nil then return end
	Roact.update(self.tree, self:_renderTree())
end

function SettingsDock._renderTree(self: SettingsDock)
	return Roact.createElement(SettingsView, {
		visible = self.visible,
		settings = self.working,
		dirty = self.dirty,
		recentlySaved = self.recentlySaved,
		saveErrors = self.saveErrors,
		onChange = function(key, value)
			-- Dynamic indexing on a typed struct is awkward in Luau
			-- without explicit casts; the cast is intentional and the
			-- keys are validated because we only render rows for known
			-- fields.
			(self.working :: any)[key] = value
			self.dirty = true
			-- Editing after a save dismisses the "Saved" banner —
			-- otherwise the green banner would mislead the user
			-- into thinking their NEW edit was already persisted.
			self.recentlySaved = false
			self:_rerender()
		end,
		onSave = function()
			-- Write to disk and capture per-field success/failure.
			-- Earlier the save was treated as fire-and-hope: if
			-- one field's `SetSetting` failed, the green "Saved"
			-- banner appeared anyway and the user had no signal
			-- that persistence partially failed. Now we read the
			-- `SaveResult` and route to a green or red banner
			-- accordingly.
			local result = Settings.save(self.plugin, self.working)
			self.saveErrors = result.errors
			self.dirty = false
			self.recentlySaved = result.ok
			self.savedGen += 1
			local thisGen = self.savedGen
			if result.ok then
				print(string.format(
					"[Yeet] settings saved: autoConnect=%s, skipHttpEnabledCheck=%s, "
						.. "verboseLogging=%s, autoConfirmBulkSync=%s, daemonUrl=%s",
					tostring(self.working.autoConnect),
					tostring(self.working.skipHttpEnabledCheck),
					tostring(self.working.verboseLogging),
					tostring(self.working.autoConfirmBulkSync),
					self.working.daemonUrl
				))
			else
				warn(string.format(
					"[Yeet] settings save FAILED for %d field(s); see red banner in Settings dock",
					#result.errors
				))
			end
			self:_rerender()
			-- Auto-clear the banner after 3s. Generation guard
			-- prevents a stale clear from wiping a fresh save:
			-- if the user clicks Save again before 3s elapse,
			-- savedGen advances and this old task no-ops. Error
			-- banner has no auto-clear (intentional — failures
			-- need user action to address).
			task.delay(3, function()
				if self.savedGen == thisGen and self.recentlySaved then
					self.recentlySaved = false
					self:_rerender()
				end
			end)
			-- Only forward to the host's `onSaved` hook on a clean
			-- save. If the daemonUrl write failed but the others
			-- succeeded, applying the in-memory `working` would
			-- make the host (Widget) reconnect to a URL that
			-- isn't actually persisted — surprising on next
			-- session. Better to require the user to retry the
			-- save until it goes clean.
			if result.ok then
				local cb = self.onSaved
				if cb ~= nil then
					cb(self.working)
				end
			end
		end,
		onReset = function()
			self.working = Settings.defaults()
			self.dirty = true
			self:_rerender()
		end,
		onClose = function()
			-- Discards any unsaved changes. The dock can be reopened
			-- via the toolbar; reopening reads fresh from disk.
			self.visible = false
			self.gui.Enabled = false
			self:_rerender()
		end,
	})
end

-- Sets the callback fired after a successful Save. Single-handler;
-- caller wires this once at construction time.
function SettingsDock.setOnSaved(self: SettingsDock, cb: ((SettingsT) -> ())?)
	self.onSaved = cb
end

-- Opens the dock. Re-reads from disk so any external edit (e.g.
-- another tool wrote to the same plugin settings keys) is picked up.
function SettingsDock.open(self: SettingsDock)
	self.working = Settings.load(self.plugin)
	self.dirty = false
	self.visible = true
	self.gui.Enabled = true
	self:_rerender()
end

function SettingsDock.close(self: SettingsDock)
	self.visible = false
	self.gui.Enabled = false
	self:_rerender()
end

function SettingsDock.toggle(self: SettingsDock)
	if self.visible then
		self:close()
	else
		self:open()
	end
end

function SettingsDock.destroy(self: SettingsDock)
	self.onSaved = nil
	if self.tree ~= nil then
		Roact.unmount(self.tree)
		self.tree = nil
	end
	self.gui:Destroy()
end

return SettingsDock
