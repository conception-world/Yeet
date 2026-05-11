--!strict

-- Top-level plugin entry. Runs once in the plugin VM when Yeet.rbxm is loaded.

if not plugin then
  error("Yeet must run as a Studio plugin (not as a game script)", 0)
end

-- Stamp the Studio Output so a reload can be verified without opening any
-- widget. Match the version tag on `Widget` / `ConflictResolver` titles.
print("[Yeet] plugin loaded v0.4.0")

-- Sweep orphan scratch instances from previous Yeet sessions out of
-- ServerStorage. ConflictResolver and BulkSyncPreview both stash a
-- temporary `Script` there for manual editing; their `closeScratch` calls
-- destroy them on clean shutdown, but a Studio crash, place close
-- mid-edit, or plugin reload while a scratch is open leaves the instance
-- behind. Without this sweep they accumulate in the user's place file
-- until they manually delete them.
local ServerStorage = game:GetService("ServerStorage")
local function sweepOrphanScratches()
  local removed = 0
  for _, child in ServerStorage:GetChildren() do
    if child.Name == "__yeet_merge_scratch__" or child.Name == "__yeet_sync_preview_scratch__" then
      pcall(function()
        child:Destroy()
      end)
      removed += 1
    end
  end
  if removed > 0 then
    print(string.format("[Yeet] swept %d orphan scratch instance(s) from ServerStorage", removed))
  end
end
sweepOrphanScratches()

local Widget = require(script.ui.Widget)
local SettingsDock = require(script.ui.SettingsDock)
local Settings = require(script.ui.Settings)

-- Read persisted settings BEFORE constructing the Widget so the
-- constructor can apply them at a known-safe point (after every
-- internal field is initialized). Earlier code did
-- `widget:applySettings(...)` after `Widget.new()` returned — which
-- worked, but only because of the line order in this file. Passing
-- the struct through the constructor makes the contract explicit
-- and proof against future re-orderings of this file.
local initialSettings = Settings.load(plugin)
local widget = Widget.new(plugin, initialSettings)
-- Settings dock is created eagerly so the inline "Settings" button
-- on the main Yeet panel has something to toggle. The dock itself
-- starts hidden (Enabled=false in the constructor) so it doesn't
-- pop on plugin load.
local settingsDock = SettingsDock.new(plugin)
-- When the user saves a new settings struct, push it back into
-- the Widget so consumers (Connection.connect, Activity logger,
-- etc.) re-read on the next call. The Widget owns the connection
-- lifecycle so it's the right consumer to notify.
settingsDock:setOnSaved(function(updated)
  widget:applySettings(updated)
end)
-- Wire the Settings button inside the Yeet panel to open/close the
-- settings dock. Lives in the panel (not the toolbar) so the user
-- finds it at the same spot as Connect / Pause — one fewer click
-- target on the global Studio toolbar.
widget:setOnOpenSettings(function()
  settingsDock:toggle()
end)

local toolbar = plugin:CreateToolbar("Yeet")
local openButton = toolbar:CreateButton("YeetOpen", "Open the Yeet panel", "rbxassetid://133454131974501", "Yeet")
openButton.ClickableWhenViewportHidden = true

openButton.Click:Connect(function()
  widget:toggle()
  openButton:SetActive(widget:isOpen())
end)

-- The persisted settings are applied INSIDE `Widget.new` (via the
-- second constructor argument above). Earlier this file made a
-- second `widget:applySettings(initialSettings)` call here as a
-- safety net; it's redundant now and would double-trigger the
-- auto-connect logic if `autoConnect=true`.

plugin.Unloading:Connect(function()
  widget:destroy()
  settingsDock:destroy()
end)
