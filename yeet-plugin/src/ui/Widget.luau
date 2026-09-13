--!strict

-- Plugin's main dock, rewritten on top of Roact. The dock itself is still
-- `CreateDockWidgetPluginGui` (no Roact wrapper for that — it's a Studio
-- host, not a regular Instance), but everything inside is a Roact tree.
--
-- State ownership: the Widget class keeps the imperative handles
-- (connection, applier, sourceWatcher) because their lifecycles outlast any
-- single render. Every call that would have reached into self.gui to
-- mutate a TextLabel now bumps the two props the view actually needs —
-- `connected` and `log` — and calls `_rerender()`. Roact.update diffs,
-- applies.

local Roact = require(script.Parent.Parent.Packages.Roact)
local Reconnect = require(script.Parent.Parent.net.Reconnect)
local Discovery = require(script.Parent.Parent.net.Discovery)
local ProjectPicker = require(script.Parent.ProjectPicker)
local Applier = require(script.Parent.Parent.sync.Applier)
local SourceWatcher = require(script.Parent.Parent.sync.SourceWatcher)
local ConflictResolver = require(script.Parent.ConflictResolver)
local BulkSyncPreview = require(script.Parent.BulkSyncPreview)
local MergePicker = require(script.Parent.MergePicker)
local theme = require(script.Parent.theme)
-- Used for its wrapped read/write helpers: every plugin setting goes through
-- the same JSON wrapper and same-tick readback check, so a key that fails to
-- persist is reported instead of silently degrading.
local Settings = require(script.Parent.Settings)
local Card = require(script.Parent.components.Card)
local Button = require(script.Parent.components.Button)
local Heading = require(script.Parent.components.Heading)
local ScrollList = require(script.Parent.components.ScrollList)

type Payload = { [string]: any }
type ConnectionHandle = {
  send: (self: ConnectionHandle, payload: Payload) -> boolean,
  close: (self: ConnectionHandle) -> (),
}
type FileResolution = {
  path: string,
  hunks: { [string]: { choice: string, text: string? } },
}
type ConflictResolverHandle = {
  setHandlers: (
    self: ConflictResolverHandle,
    handlers: {
      onResolve: (FileResolution) -> (),
      onCancel: () -> (),
      onAbandon: ({ string }) -> (),
      onLineByLine: (any) -> (),
    }
  ) -> (),
  enqueue: (self: ConflictResolverHandle, conflicts: { any }) -> (),
  removeConflict: (self: ConflictResolverHandle, path: string) -> (),
  destroy: (self: ConflictResolverHandle) -> (),
}
type MergePickerHandle = {
  open: (
    self: MergePickerHandle,
    path: string,
    studio_content: string,
    fs_content: string,
    handlers: {
      onApply: (path: string, content: string) -> (),
      onCancel: (path: string) -> (),
    }
  ) -> (),
  close: (self: MergePickerHandle) -> (),
  destroy: (self: MergePickerHandle) -> (),
}
type BulkSyncPreviewEntry = {
  path: string,
  status: string,
  studio_content: string?,
  fs_content: string?,
}
type BulkSyncPreviewResolution = {
  path: string,
  -- Wire-level action name, kept in sync with `BulkSyncAction` in
  -- yeet-daemon/src/protocol.rs and the Choice → Action mapping inside
  -- ui/BulkSyncPreview.luau.
  action: string,
}
type BulkSyncPreviewHandlers = {
  onSeeCode: (entry: BulkSyncPreviewEntry) -> (),
  -- Triggered by the "Merge…" button on each Modified entry. Host opens
  -- MergePicker on the entry's two sides; on Apply the host both sends
  -- `conflict_resolved_manual` and removes the entry from the preview
  -- so the subsequent "Apply Sync" doesn't overwrite the merged content.
  onLineByLine: (entry: BulkSyncPreviewEntry) -> (),
  onApply: (resolutions: { BulkSyncPreviewResolution }) -> (),
  onCancel: () -> (),
}
type BulkSyncPreviewHandle = {
  open: (
    self: BulkSyncPreviewHandle,
    request_id: string,
    direction: string,
    entries: { BulkSyncPreviewEntry },
    handlers: BulkSyncPreviewHandlers
  ) -> (),
  -- Drops a single row from the open preview (no daemon round-trip).
  -- Used after the host applies a manual merge for that path: the merged
  -- content is on its way to the daemon via `conflict_resolved_manual`,
  -- so the subsequent bulk Apply must not also send a side-pick for it.
  removeEntry: (self: BulkSyncPreviewHandle, path: string) -> (),
  close: (self: BulkSyncPreviewHandle) -> (),
  destroy: (self: BulkSyncPreviewHandle) -> (),
}
type ApplierHandle = {
  dispatch: (self: ApplierHandle, payload: Payload) -> (),
  hash: (self: ApplierHandle, content: string) -> string,
  isEcho: (self: ApplierHandle, path: string, hash: string) -> boolean,
  recordSelfEdit: (self: ApplierHandle, path: string, hash: string) -> (),
  forgetLocalDelete: (self: ApplierHandle, path: string) -> (),
  forgetLocalRename: (
    self: ApplierHandle,
    oldPath: string,
    newPath: string,
    hash: string
  ) -> (),
  resolvePath: (self: ApplierHandle, instance: Instance) -> (string?, string?),
  setTrackHooks: (
    self: ApplierHandle,
    onTracked: (path: string, instance: Instance) -> (),
    onUntracked: (path: string) -> ()
  ) -> (),
  setConflictDispatcher: (self: ApplierHandle, dispatcher: (conflicts: { any }) -> ()) -> (),
  setBootstrapDivergeHook: (
    self: ApplierHandle,
    hook: ((path: string, kind: string, studioContent: string, studioSha256: string) -> ())?
  ) -> (),
  setBulkSyncPreviewDispatcher: (self: ApplierHandle, dispatcher: ((payload: Payload) -> ())?) -> (),
  setStudioCreatedHook: (
    self: ApplierHandle,
    hook: ((path: string, kind: string, studioContent: string, studioSha256: string) -> ())?
  ) -> (),
  setStudioRenamedHook: (
    self: ApplierHandle,
    hook: ((
      oldPath: string,
      newPath: string,
      kind: string,
      studioContent: string,
      studioSha256: string
    ) -> ())?
  ) -> (),
  setStudioCollisionHook: (
    self: ApplierHandle,
    hook: ((path: string, source: string, sha256: string, colliders: { Instance }) -> ())?
  ) -> (),
  setBootstrapPreviewMode: (self: ApplierHandle, enabled: boolean) -> (),
  setBootstrapSnapshotHook: (
    self: ApplierHandle,
    hook: ((snapshot: { { path: string, kind: string, content: string, sha256: string } }) -> ())?
  ) -> (),
  setProjectRootMismatchHook: (
    self: ApplierHandle,
    hook: ((expected: string, got: string) -> ())?
  ) -> (),
  setPendingEditProbe: (
    self: ApplierHandle,
    probe: ((path: string) -> (string?, string?))?
  ) -> (),
  setApplyHooks: (
    self: ApplierHandle,
    onApplied: ((path: string, sha256: string) -> ())?,
    onApplyFailed: ((path: string, reason: string) -> ())?
  ) -> (),
  reset: (self: ApplierHandle) -> (),
}
type SourceWatcherHandle = {
  track: (self: SourceWatcherHandle, path: string, instance: Instance) -> (),
  untrack: (self: SourceWatcherHandle, path: string) -> (),
  peekPending: (self: SourceWatcherHandle, path: string) -> (string?, string?),
  destroy: (self: SourceWatcherHandle) -> (),
}

type LogEntry = { time: string, text: string }

-- Locally redeclared (types don't reliably cross `require` in strict mode).
type DaemonEntry = {
  daemonId: string,
  projectName: string,
  projectRoot: string,
  port: number,
  daemonVersion: string,
  pluginConnected: boolean,
  url: string,
}

type DiscoveryHandle = {
  cancel: (self: DiscoveryHandle) -> (),
}

type ViewProps = {
  connected: boolean,
  -- True between the moment the user clicks Connect (or a server-side
  -- close triggers a reconnect attempt) and the next successful socket
  -- open. Surfaced in the view as a yellow "Connecting…" pill with a
  -- "Click to stop" hint so the user can bail out of a stalled
  -- handshake instead of staring at a frozen button.
  connecting: boolean,
  paused: boolean,
  log: { LogEntry },
  onToggle: () -> (),
  onPauseToggle: () -> (),
  -- Fires when the user clicks the inline Settings button. The
  -- Widget doesn't own the SettingsDock — the host (init.server.luau)
  -- wires this to `settingsDock:toggle()`. Optional so the view
  -- doesn't render the row in tests/dev contexts where no settings
  -- dock has been registered.
  onOpenSettings: (() -> ())?,
  -- ── Project picker ──────────────────────────────────────────────────
  -- Daemons found by the last discovery scan, sorted by port.
  daemons: { DaemonEntry },
  -- Which row is highlighted. Nil (or "") means the manual Daemon URL row,
  -- when one is shown.
  selectedDaemonId: string?,
  scanning: boolean,
  -- Set when every probe failed the HttpEnabled pre-flight — a different
  -- problem from "nothing running", with a different fix.
  httpDisabled: boolean,
  -- The user's Daemon URL override from Settings, when it is not the default.
  manualUrl: string?,
  onSelectDaemon: (daemonId: string) -> (),
  onSelectManual: (() -> ())?,
  onRefreshDaemons: () -> (),
}

local DAEMON_URL = "ws://127.0.0.1:34872"
local WIDGET_ID = "YeetMain"
local LOG_LIMIT = 200

-- The pre-multi-place auth-token key. Kept only to delete it once: see
-- `forgetLegacyAuthToken`.
--
-- It was global (one token for every project) AND dotted. Both are bugs. The
-- global part made two daemons clear each other's token in a
-- reject -> re-pair -> reject loop, since each daemon generates its own. The
-- dot is worse: ui/Settings.luau documents that Studio does not reliably
-- persist setting keys containing `.`, which is why every settings key was
-- migrated to underscores — this one was missed, so the token most likely
-- never survived a reload at all.
local LEGACY_AUTH_TOKEN_SETTING = "yeet.authToken"

-- Per-project auth-token key. `daemonId` is the daemon's
-- `sha256(project_root)[..16]`, reported in both `daemon_info` (discovery) and
-- `project_opened` (so a plugin that connected via a manual Daemon URL, never
-- having scanned, still learns its key).
--
-- Underscore-separated and hex-only by construction: no dots, for the Studio
-- persistence bug above.
local function authTokenKey(daemonId: string): string
	return "yeet_auth_" .. daemonId
end

-- Per-place key remembering which daemon this place last connected to.
--
-- This is the ONLY place Yeet reads a Roblox identity value, and it is used
-- purely as an opaque namespacing integer: never sent to the daemon, never
-- logged, never part of any authorization decision.
--
-- It has to be per-place because `plugin:SetSetting` is scoped to the user's
-- Studio profile, not the place file. A single shared key would have two open
-- places overwrite each other's choice on every connect — the same class of bug
-- as the global auth token above.
--
-- `PlaceId` is 0 for a place that has never been saved; those share one bucket
-- rather than not remembering at all, since an unsaved place still benefits
-- from its choice surviving a plugin reload.
local function lastDaemonKey(): string
	local placeId = 0
	pcall(function()
		placeId = game.PlaceId
	end)
	if placeId == 0 then
		return "yeet_lastDaemon_unsaved"
	end
	return "yeet_lastDaemon_" .. tostring(placeId)
end

-- Drops the pre-multi-place global token once, on plugin load.
--
-- Not migrated to a per-project key, because there is no way to know which
-- project it belonged to — and, given the dotted-key persistence bug, it very
-- likely holds nothing anyway. The cost of dropping it is a single tokenless
-- re-pair through the extension's breadcrumb, which the user never sees.
local function forgetLegacyAuthToken(p: Plugin)
	pcall(function()
		p:SetSetting(LEGACY_AUTH_TOKEN_SETTING, nil)
	end)
end

-- ─── Pure view ─────────────────────────────────────────────────────────────

local function LogLine(props: { entry: LogEntry, layoutOrder: number })
  return Roact.createElement("Frame", {
    Size = UDim2.new(1, 0, 0, 0),
    AutomaticSize = Enum.AutomaticSize.Y,
    BackgroundTransparency = 1,
    LayoutOrder = props.layoutOrder,
  }, {
    layout = Roact.createElement("UIListLayout", {
      FillDirection = Enum.FillDirection.Horizontal,
      VerticalAlignment = Enum.VerticalAlignment.Top,
      SortOrder = Enum.SortOrder.LayoutOrder,
      Padding = UDim.new(0, 4),
    }),
    time = Roact.createElement("TextLabel", {
      Size = UDim2.fromOffset(60, 0),
      AutomaticSize = Enum.AutomaticSize.Y,
      BackgroundTransparency = 1,
      Text = props.entry.time,
      TextColor3 = theme.colors.textMuted,
      Font = theme.typography.code.font,
      TextSize = theme.typography.code.size,
      TextXAlignment = Enum.TextXAlignment.Left,
      LayoutOrder = 1,
    }),
    text = Roact.createElement("TextLabel", {
      Size = UDim2.new(1, -64, 0, 0),
      AutomaticSize = Enum.AutomaticSize.Y,
      BackgroundTransparency = 1,
      Text = props.entry.text,
      TextColor3 = theme.colors.text,
      Font = theme.typography.code.font,
      TextSize = theme.typography.code.size,
      TextXAlignment = Enum.TextXAlignment.Left,
      TextWrapped = true,
      LayoutOrder = 2,
    }),
  })
end

local function buildLogChildren(log: { LogEntry }): { [string]: any }
  local kids: { [string]: any } = {}
  for index, entry in log do
    kids["line_" .. index] = Roact.createElement(LogLine, {
      entry = entry,
      layoutOrder = index,
    })
  end
  return kids
end

local function WidgetView(props: ViewProps)
  local dotColor: Color3
  if props.connected then
    dotColor = theme.colors.accentAccept
  elseif props.connecting then
    dotColor = theme.colors.accentWarning
  else
    dotColor = theme.colors.textMuted
  end
  local statusDot = Roact.createElement("Frame", {
    Size = UDim2.fromOffset(10, 10),
    BackgroundColor3 = dotColor,
    BorderSizePixel = 0,
    LayoutOrder = 1,
  }, {
    corner = Roact.createElement("UICorner", { CornerRadius = UDim.new(1, 0) }),
  })

  local statusText: string
  if props.connected then
    statusText = "Connected"
  elseif props.connecting then
    statusText = "Connecting…"
  else
    statusText = "Disconnected"
  end
  local statusRow = Roact.createElement("Frame", {
    Size = UDim2.new(1, 0, 0.2, 0),
    BackgroundTransparency = 1,
    LayoutOrder = 1,
  }, {
    layout = Roact.createElement("UIListLayout", {
      FillDirection = Enum.FillDirection.Horizontal,
      VerticalAlignment = Enum.VerticalAlignment.Center,
      Padding = UDim.new(0, theme.spacing.sm),
      SortOrder = Enum.SortOrder.LayoutOrder,
    }),
    dot = statusDot,
    label = Roact.createElement("TextLabel", {
      Size = UDim2.new(1, -20, 1, 0),
      BackgroundTransparency = 1,
      Text = statusText,
      TextColor3 = theme.colors.textMuted,
      Font = theme.typography.caption.font,
      TextSize = theme.typography.caption.size,
      TextXAlignment = Enum.TextXAlignment.Left,
      LayoutOrder = 2,
    }),
  })

  local heading = Roact.createElement(Heading, {
    title = "Yeet",
    subtitle = "Roblox Studio ↔ IDE sync",
    layoutOrder = 2,
  })

  -- Fixed-pixel height shared by every button in this card. Earlier
  -- the buttons used `0.125` (12.5% of parent) for height, but the
  -- parent Card has `AutomaticSize.Y` and Roblox's layout pipeline
  -- handles "scale-of-auto-sized-parent" inconsistently — the first
  -- visible button (Connect/Disconnect) ended up noticeably taller
  -- than its siblings. Pinning every button to the same offset
  -- height eliminates the ambiguity: all three are exactly the same
  -- size regardless of how many are present.
  local BUTTON_HEIGHT = UDim2.new(1, 0, 0, 40)
  -- "Connecting…" gets a taller pill so the secondary "Click to stop"
  -- hint fits below the main label without crowding the glyphs.
  local CONNECTING_BUTTON_HEIGHT = UDim2.new(1, 0, 0, 56)

  local connectButton
  if props.connecting and not props.connected then
    -- Custom yellow pill with two stacked labels. The Button component
    -- is single-line by design, so this branch hand-rolls a TextButton
    -- that mirrors its pill shape (UICorner + label typography) while
    -- adding a muted "Click to stop" subtitle. Same `onToggle` handler
    -- as the regular Connect / Disconnect button: a click while
    -- "Connecting…" tears the in-flight reconnect down.
    connectButton = Roact.createElement("TextButton", {
      Size = CONNECTING_BUTTON_HEIGHT,
      AutomaticSize = Enum.AutomaticSize.None,
      LayoutOrder = 4,
      BackgroundColor3 = theme.colors.accentWarning,
      BorderSizePixel = 0,
      AutoButtonColor = true,
      Text = "",
      [Roact.Event.Activated] = function()
        local ok, err = pcall(props.onToggle)
        if not ok then
          warn(string.format("[Yeet] Connect (connecting) onClick threw: %s", tostring(err)))
        end
      end,
    }, {
      corner = Roact.createElement("UICorner", {
        CornerRadius = UDim.new(0, theme.radius.pill),
      }),
      layout = Roact.createElement("UIListLayout", {
        FillDirection = Enum.FillDirection.Vertical,
        VerticalAlignment = Enum.VerticalAlignment.Center,
        HorizontalAlignment = Enum.HorizontalAlignment.Center,
        SortOrder = Enum.SortOrder.LayoutOrder,
        Padding = UDim.new(0, 2),
      }),
      padding = Roact.createElement("UIPadding", {
        PaddingTop = UDim.new(0, theme.spacing.sm),
        PaddingBottom = UDim.new(0, theme.spacing.sm),
        PaddingLeft = UDim.new(0, theme.spacing.lg),
        PaddingRight = UDim.new(0, theme.spacing.lg),
      }),
      title = Roact.createElement("TextLabel", {
        Size = UDim2.new(1, 0, 0, theme.typography.label.size + 2),
        BackgroundTransparency = 1,
        Text = "Connecting…",
        TextColor3 = theme.colors.textInverse,
        Font = theme.typography.label.font,
        TextSize = theme.typography.label.size,
        TextXAlignment = Enum.TextXAlignment.Center,
        LayoutOrder = 1,
      }),
      hint = Roact.createElement("TextLabel", {
        Size = UDim2.new(1, 0, 0, theme.typography.caption.size + 2),
        BackgroundTransparency = 1,
        Text = "Click to stop",
        -- Slightly translucent dark on top of the warning yellow so
        -- the hint reads as secondary without losing contrast.
        TextColor3 = theme.colors.textInverse,
        TextTransparency = 0.35,
        Font = theme.typography.caption.font,
        TextSize = theme.typography.caption.size,
        TextXAlignment = Enum.TextXAlignment.Center,
        LayoutOrder = 2,
      }),
    })
  else
    connectButton = Roact.createElement(Button, {
      text = if props.connected then "Disconnect" else "Connect",
      variant = if props.connected then "danger" else "primary",
      size = BUTTON_HEIGHT,
      onClick = props.onToggle,
      layoutOrder = 4,
    })
  end

  -- Pause/Resume only makes sense when connected. Suppresses outgoing edits
  -- and incoming applies without tearing down the WebSocket — handy when
  -- you want to scratch on a Studio script without triggering a sync, or
  -- to bulk-edit on disk and only let the daemon catch up afterwards.
  -- Resuming forces a reconnect so the daemon's pending_deltas (or a
  -- fresh handshake if those overflowed) brings both sides back in sync.
  local pauseButton = if props.connected
    then Roact.createElement(Button, {
      text = if props.paused then "Resume sync" else "Pause sync",
      variant = if props.paused then "accept" else "ghost",
      size = BUTTON_HEIGHT,
      onClick = props.onPauseToggle,
      layoutOrder = 5,
    })
    else nil

  -- Settings button — always visible (connection state independent
  -- since users may want to tweak daemon URL / auto-connect BEFORE
  -- connecting). Uses ghost variant so it stays visually
  -- secondary to the primary Connect / Disconnect call-to-action.
  -- Lives at layoutOrder=6 so it sits below pauseButton when
  -- present and below connectButton when not.
  local settingsButton = if props.onOpenSettings ~= nil
    then Roact.createElement(Button, {
      text = "Settings",
      variant = "ghost",
      size = BUTTON_HEIGHT,
      onClick = props.onOpenSettings,
      layoutOrder = 6,
    })
    else nil

  local connectionCard = Roact.createElement(Card, {
    layoutOrder = 1,
    padding = theme.spacing.lg,
    children = {
      layout = Roact.createElement("UIListLayout", {
        FillDirection = Enum.FillDirection.Vertical,
        SortOrder = Enum.SortOrder.LayoutOrder,
        Padding = UDim.new(0, theme.spacing.md),
      }),
      status = statusRow,
      heading = heading,
      -- Above Connect: choosing the target precedes acting on it, so it reads
      -- in the order the user works.
      picker = Roact.createElement(ProjectPicker, {
        entries = props.daemons,
        selectedDaemonId = props.selectedDaemonId,
        scanning = props.scanning,
        connected = props.connected,
        httpDisabled = props.httpDisabled,
        manualUrl = props.manualUrl,
        onSelect = props.onSelectDaemon,
        onSelectManual = props.onSelectManual,
        onRefresh = props.onRefreshDaemons,
        layoutOrder = 3,
      }),
      button = connectButton,
      pauseButton = pauseButton,
      settingsButton = settingsButton,
    },
  })

  -- Height = leftover after connection card. UIFlexItem.Fill on the card
  -- absorbs all remaining space from the root UIListLayout. The scroll is
  -- sized explicitly: card.height - cardPad(12+12) - title(16) - gap(8) = -48.
  -- This ensures the scroll ends exactly at the card's bottom padding edge
  -- with no overflow into the root's bottom margin.
  local logCard = Roact.createElement(Card, {
    layoutOrder = 2,
    size = UDim2.new(1, 0, 0, 0),
    padding = theme.spacing.md,
    automaticSize = Enum.AutomaticSize.None,
    children = {
      flex = Roact.createElement("UIFlexItem", {
        FlexMode = Enum.UIFlexMode.Fill,
      }),
      layout = Roact.createElement("UIListLayout", {
        FillDirection = Enum.FillDirection.Vertical,
        SortOrder = Enum.SortOrder.LayoutOrder,
        Padding = UDim.new(0, theme.spacing.sm),
      }),
      title = Roact.createElement("TextLabel", {
        Size = UDim2.new(1, 0, 0, 16),
        BackgroundTransparency = 1,
        Text = "Activity",
        TextColor3 = theme.colors.textMuted,
        Font = theme.typography.label.font,
        TextSize = theme.typography.label.size,
        TextXAlignment = Enum.TextXAlignment.Left,
        LayoutOrder = 1,
      }),
      scroll = Roact.createElement(ScrollList, {
        size = UDim2.new(1, 0, 1, -48),
        padding = theme.spacing.sm,
        itemGap = 0,
        layoutOrder = 2,
        children = buildLogChildren(props.log),
      }),
    },
  })

  local rootChildren: { [string]: any } = {
    padding = Roact.createElement("UIPadding", {
      PaddingTop = UDim.new(0, theme.spacing.md),
      PaddingBottom = UDim.new(0, theme.spacing.lg),
      PaddingLeft = UDim.new(0, theme.spacing.md),
      PaddingRight = UDim.new(0, theme.spacing.md),
    }),
    layout = Roact.createElement("UIListLayout", {
      FillDirection = Enum.FillDirection.Vertical,
      SortOrder = Enum.SortOrder.LayoutOrder,
      Padding = UDim.new(0, theme.spacing.md),
    }),
    connection = connectionCard,
    log = logCard,
  }
  return Roact.createElement("Frame", {
    Size = UDim2.fromScale(1, 1),
    BackgroundColor3 = theme.colors.background,
    BorderSizePixel = 0,
  }, rootChildren)
end

-- ─── Imperative class (connection + daemon lifecycle) ──────────────────────

local Widget = {}
Widget.__index = Widget

type self = {
  plugin: Plugin,
  gui: DockWidgetPluginGui,
  connection: ConnectionHandle?,
  applier: ApplierHandle?,
  sourceWatcher: SourceWatcherHandle?,
  resolver: ConflictResolverHandle,
  syncPreview: BulkSyncPreviewHandle,
  -- Lazy: created on first use to avoid the docked widget popping at
  -- plugin load. Re-used across conflicts so the user doesn't accumulate
  -- merge-picker windows.
  mergePicker: MergePickerHandle?,
  tree: any,
  log: { LogEntry },
  connected: boolean,
  -- True from the moment `_onConnectPressed` runs until the next
  -- successful `onOpen` (or until `_teardownConnection` cancels the
  -- attempt). `onReconnecting` flips it back to true between automated
  -- retries so the dock shows the same "Connecting…" pill during
  -- handshake stalls as during the initial connect.
  connecting: boolean,
  -- True when a deferred log-batch flush is already scheduled. Set by
  -- `_appendLog`, cleared by `_rerender`. Lets us coalesce a burst of
  -- log lines into a single Roact diff instead of one per entry.
  logRerenderPending: boolean,
  -- When set, suppress outgoing SourceWatcher edits and inbound Applier
  -- mutations. The WebSocket stays open so reconnect-on-resume can use
  -- the daemon's pending_deltas to catch up cleanly. Toggled via the
  -- Pause/Resume button surfaced when connected.
  paused: boolean,
  -- User-configurable settings (see ui/Settings.luau). Pushed in via
  -- `applySettings` from init.server.luau on plugin load AND from
  -- the Settings dock's `onSaved` callback. The default value here
  -- mirrors `Settings.defaults()` so a `nil` settings struct never
  -- crashes — the field is read on every connect to honour the
  -- latest values without re-plumbing.
  settings: {
    autoConnect: boolean,
    skipHttpEnabledCheck: boolean,
    verboseLogging: boolean,
    autoConfirmBulkSync: boolean,
    daemonUrl: string,
  },
  -- Wired by init.server.luau to `settingsDock:toggle()`. Stored
  -- here so the Widget's view can render an inline Settings button
  -- without taking a hard dependency on the SettingsDock module.
  onOpenSettings: (() -> ())?,
  -- Stable id of the daemon/project this Widget is pointed at. Seeds the
  -- per-connection id in `_onConnectPressed`; the connection's own copy is what
  -- the auth handlers use, so a late callback from a previous daemon cannot
  -- touch this project's token.
  activeDaemonId: string?,
  -- ── Project picker ──────────────────────────────────────────────────
  daemons: { DaemonEntry },
  selectedDaemonId: string?,
  scanning: boolean,
  httpDisabled: boolean,
  activeScan: DiscoveryHandle?,
  scanGeneration: number,
  didInitialScan: boolean,
}

export type Widget = typeof(setmetatable({} :: self, Widget))

type WidgetSettings = {
  autoConnect: boolean,
  skipHttpEnabledCheck: boolean,
  verboseLogging: boolean,
  autoConfirmBulkSync: boolean,
  daemonUrl: string,
}

-- `initialSettings` is optional for testing / minimal-deps callers,
-- but the production caller in `init.server.luau` always passes the
-- persisted struct. Threading it through the constructor (instead of
-- calling `widget:applySettings(...)` after `Widget.new` returns)
-- closes a real footgun: applySettings is allowed to fire
-- `_onConnectPressed` if `autoConnect == true`, and that handler
-- reaches into `self.resolver`, `self.syncPreview`, `self.tree`, and
-- the plugin-toolbar wiring set up by the host. Earlier the host did
-- `widget = Widget.new(...) ; ... widget:applySettings(...)`, which
-- worked only because of the LINE ORDER in init.server.luau. A
-- future refactor that moved applySettings up would have nil-deref'd
-- against an unconstructed widget. Now the auto-connect kick-off
-- happens AT THE END of `Widget.new`, after every internal field is
-- guaranteed initialized; nothing in the host's call site can
-- reorder around it.
function Widget.new(plugin: Plugin, initialSettings: WidgetSettings?): Widget
  -- One-shot cleanup of the pre-multi-place global auth token.
  forgetLegacyAuthToken(plugin)

  local info = DockWidgetPluginGuiInfo.new(Enum.InitialDockState.Right, false, false, 320, 480, 280, 360)
  local gui = plugin:CreateDockWidgetPluginGui(WIDGET_ID, info)
  gui.Title = "Yeet v0.5.0"
  gui.Name = WIDGET_ID

  local resolver = ConflictResolver.new(plugin) :: any
  local syncPreview = BulkSyncPreview.new(plugin) :: any

  local self = setmetatable({
    plugin = plugin,
    gui = gui,
    connection = nil :: ConnectionHandle?,
    -- Stable id of the daemon/project we are talking to, from `daemon_info`
    -- (discovery) or `project_opened`. Namespaces this project's stored auth
    -- token. Nil before the first connect through a manual Daemon URL.
    activeDaemonId = nil :: string?,
    -- ── Project picker state ────────────────────────────────────────────
    daemons = {} :: { DaemonEntry },
    selectedDaemonId = nil :: string?,
    scanning = false,
    -- Set when every probe in a scan failed the HttpEnabled pre-flight, so the
    -- empty state can give the right instruction instead of "no daemons found".
    httpDisabled = false,
    -- Live scan, so a Refresh (or a teardown) can cancel the previous one
    -- instead of letting two scans race into the same state.
    activeScan = nil :: DiscoveryHandle?,
    -- Bumped per scan so a superseded scan's callbacks can identify themselves
    -- as stale and bow out. Necessary because cancelling a scan still fires its
    -- completion callback.
    scanGeneration = 0,
    -- Ensures the automatic "fill the picker on load" scan happens exactly
    -- once, rather than on every Settings save.
    didInitialScan = false,
    applier = nil :: ApplierHandle?,
    sourceWatcher = nil :: SourceWatcherHandle?,
    resolver = resolver :: ConflictResolverHandle,
    syncPreview = syncPreview :: BulkSyncPreviewHandle,
    mergePicker = nil :: MergePickerHandle?,
    tree = nil,
    log = {},
    connected = false,
    connecting = false,
    logRerenderPending = false,
    paused = false,
    -- Defaults match Settings.defaults() in ui/Settings.luau. If
    -- `initialSettings` was provided, it overrides these via
    -- `applySettings` below. The fallback only kicks in when no
    -- caller supplies a settings struct (test harness, minimal
    -- bootstrap).
    settings = {
      autoConnect = false,
      skipHttpEnabledCheck = false,
      verboseLogging = false,
      autoConfirmBulkSync = false,
      daemonUrl = DAEMON_URL,
    },
    onOpenSettings = nil :: (() -> ())?,
  }, Widget)

  self.tree = Roact.mount(self:_renderTree(), gui, "Content")
  -- All internal fields are now initialized. We defer applying the
  -- persisted settings via `task.defer` so the host (init.server.luau)
  -- has a chance to wire callbacks like `setOnOpenSettings` in the
  -- same frame BEFORE auto-connect fires. Without this defer, an
  -- `autoConnect=true` setting starts the connection while
  -- `onOpenSettings` is still nil; the dock renders without the
  -- Settings button until the next re-render. The defer keeps the
  -- visible UX clean: Studio's next frame already has the button.
  if initialSettings ~= nil then
    task.defer(function()
      -- Re-check `tree` so a fast destroy (Plugin.Unloading right
      -- after construction, e.g. test harness teardown) doesn't
      -- apply settings to a half-torn-down widget.
      if self.tree == nil then return end
      self:applySettings(initialSettings)
    end)
  else
    -- No settings to apply means no auto-connect path to run a scan for, so
    -- populate the picker here instead. With settings present, `applySettings`
    -- owns the first scan when auto-connect is on; when it is off the user's
    -- first Refresh (or opening the panel) fills the list.
    task.defer(function()
      if self.tree == nil then return end
      self:_startScan()
    end)
  end
  return self
end

function Widget.isOpen(self: Widget): boolean
  return self.gui.Enabled
end

-- Pushes a fresh settings struct in. Called from `init.server.luau`
-- once at plugin load and from the SettingsDock's `onSaved` callback
-- whenever the user clicks Save. Hot-apply is mostly "store the
-- values; the next time a consumer asks (next connect, next log
-- line, next BulkSyncPreview), it reads the fresh field". The one
-- thing that fires immediately is `autoConnect` on initial load.
function Widget.applySettings(self: Widget, s: {
  autoConnect: boolean,
  skipHttpEnabledCheck: boolean,
  verboseLogging: boolean,
  autoConfirmBulkSync: boolean,
  daemonUrl: string,
})
  local prev = self.settings
  self.settings = s
  -- Auto-connect: only honour on a fresh load (no live connection).
  -- A re-save while connected does NOT reconnect; the user can do
  -- that manually if they want the new daemonUrl to take effect.
  if s.autoConnect and self.connection == nil then
    -- Only fire on the FIRST apply (prev.autoConnect was the
    -- placeholder default `false`). Subsequent saves where the user
    -- toggled it on while disconnected also work, which is what we
    -- want — toggle on, see immediate connect.
    --
    -- Deferred behind a scan: with one daemon per project, connecting before
    -- discovery has run would use the Settings URL — a single fixed address —
    -- and could attach this place to a different project than the one it was
    -- last paired with. Waiting lets the remembered choice be restored first.
    self:_appendLog("auto-connect enabled — looking for this place's project")
    self:_startScanThen(function()
      if self.connection ~= nil then
        -- The user got there first while the scan ran.
        return
      end
      if self.selectedDaemonId == nil and #self.daemons > 0 then
        -- Daemons are running but none is the one this place remembers.
        -- Connecting to an arbitrary one is exactly the cross-project mistake
        -- the picker exists to prevent, so stop and let the user choose.
        self:_appendLog("auto-connect: no remembered project among those running — pick one")
        return
      end
      self:_onConnectPressed()
    end)
  elseif not self.didInitialScan then
    -- Auto-connect is off, so nothing else would populate the picker. Scan once
    -- on the first apply (plugin load) so the list is already filled when the
    -- user opens the panel, instead of showing an empty state until they find
    -- Refresh. Guarded so that merely saving Settings later does not kick off
    -- another scan behind the user's back.
    self:_startScan()
  end
  -- Settings-changed log line. Useful when the user is debugging
  -- "did my toggle take effect" — they see a confirmation in the
  -- Activity log right after Save.
  if prev.daemonUrl ~= s.daemonUrl
    or prev.skipHttpEnabledCheck ~= s.skipHttpEnabledCheck
    or prev.verboseLogging ~= s.verboseLogging
    or prev.autoConfirmBulkSync ~= s.autoConfirmBulkSync
  then
    self:_appendLog("settings updated")
  end
end

function Widget.toggle(self: Widget)
  self.gui.Enabled = not self.gui.Enabled
end

-- Manual escape hatch — toolbar wires this to "Show Sync" button. Forces
-- the BulkSyncPreview dock visible (with off→defer→on toggle). For when
-- Studio's cached dock layout has the widget parked off-screen or
-- minimised in a tab group, and `BulkSyncPreview.open()`'s automatic
-- toggle didn't get the dock back. Idempotent — calling it without an
-- active preview just enables an empty dock with the "all in sync"
-- placeholder.
function Widget.forceShowSyncPreview(self: Widget)
  local preview = self.syncPreview
  if preview == nil then
    return
  end
  -- Reach into the dock and force-toggle Enabled. We can't call
  -- preview:open(...) here without a fresh request_id, but bouncing
  -- Enabled is enough to make Studio re-evaluate window placement.
  local gui = (preview :: any).gui
  if typeof(gui) == "Instance" then
    gui.Enabled = false
    task.defer(function()
      gui.Enabled = true
      print(string.format(
        "[Yeet] forceShowSyncPreview: gui.Enabled = %s, gui.Title = %s",
        tostring(gui.Enabled),
        tostring(gui.Title)
      ))
    end)
  end
end

function Widget.destroy(self: Widget)
  -- Invalidate any in-flight scan before cancelling it. Cancelling still fires
  -- the completion callback, which touches `self` and calls `_rerender` — and
  -- by then the tree is being unmounted. Bumping the generation makes that
  -- callback recognise itself as stale and return immediately.
  self.scanGeneration += 1
  local scan = self.activeScan
  if scan ~= nil then
    pcall(function()
      scan:cancel()
    end)
    self.activeScan = nil
  end
  self:_teardownConnection()
  if self.tree ~= nil then
    Roact.unmount(self.tree)
    self.tree = nil
  end
  self.resolver:destroy()
  self.syncPreview:destroy()
  if self.mergePicker ~= nil then
    self.mergePicker:destroy()
    self.mergePicker = nil
  end
  self.gui:Destroy()
end

function Widget._renderTree(self: Widget)
  return Roact.createElement(WidgetView, {
    connected = self.connected,
    connecting = self.connecting,
    paused = self.paused,
    log = self.log,
    onToggle = function()
      self:_onToggleConnection()
    end,
    onPauseToggle = function()
      self:_onPauseToggle()
    end,
    onOpenSettings = self.onOpenSettings,
    daemons = self.daemons,
    selectedDaemonId = self.selectedDaemonId,
    scanning = self.scanning,
    httpDisabled = self.httpDisabled,
    -- Only surfaced when the user actually overrode it; the default URL is
    -- what the scan already covers, so pinning a row for it would be noise.
    manualUrl = if self.settings.daemonUrl ~= DAEMON_URL and self.settings.daemonUrl ~= ""
      then self.settings.daemonUrl
      else nil,
    onSelectDaemon = function(daemonId: string)
      self:_onSelectDaemon(daemonId)
    end,
    onSelectManual = function()
      self:_onSelectManual()
    end,
    onRefreshDaemons = function()
      self:_startScan()
    end,
  })
end

-- Runs a discovery scan and folds the result into the picker.
--
-- Cancels any scan already running: a user clicking Refresh twice, or a
-- failure-triggered rescan landing on top of a manual one, would otherwise have
-- two scans writing `self.daemons` in an unpredictable order.
function Widget._startScan(self: Widget)
  self:_startScanThen(nil)
end

-- `_startScan` with a callback that runs once the scan has completed and the
-- remembered selection has been restored. Used by auto-connect, which must not
-- pick a daemon before discovery has had its say.
function Widget._startScanThen(self: Widget, afterScan: (() -> ())?)
  self.didInitialScan = true

  -- Generation counter. `Discovery.cancel` deliberately still fires
  -- `onComplete` (so a caller never leaks a pending state), which means a
  -- cancelled scan's callbacks arrive AFTER the replacement scan has started.
  -- Without this guard the old scan's completion would overwrite the new
  -- scan's results and reset `scanning`/`activeScan` out from under it —
  -- clicking Refresh twice would leave the picker showing the first scan's
  -- findings and stuck in a bad state.
  self.scanGeneration += 1
  local generation = self.scanGeneration

  local previous = self.activeScan
  if previous ~= nil then
    pcall(function()
      previous:cancel()
    end)
    self.activeScan = nil
  end

  self.scanning = true
  self.httpDisabled = false
  -- Cleared BEFORE the scan starts, not after: the progress callback below
  -- appends to this list as answers arrive, so clearing it afterwards would
  -- throw away every daemon found in the first moments of the scan.
  self.daemons = {}
  self:_rerender()

  -- Counts probes that failed the HttpEnabled pre-flight. If EVERY port fails
  -- that way the problem is the place's HTTP setting, not the absence of
  -- daemons, and the empty state should say so.
  local httpFailures = 0
  local verbose = self.settings.verboseLogging

  local handle = Discovery.scan({
    skipHttpEnabledCheck = self.settings.skipHttpEnabledCheck,
    log = function(message: string)
      if message:find("HTTP requests are disabled", 1, true) ~= nil then
        httpFailures += 1
      end
      -- Per-port tracing is the only way to diagnose "the picker is empty" on
      -- a user's machine, since there is no console we can inspect — but it is
      -- noisy, so it rides behind the existing verboseLogging setting.
      if verbose then
        self:_appendLog(message)
      end
    end,
  }, function(entries: { DaemonEntry })
    if generation ~= self.scanGeneration then
      -- A newer scan superseded this one, so these results are stale. Returning
      -- here also leaves the newer scan's `activeScan`/`scanning` untouched.
      return
    end
    self.activeScan = nil
    self.scanning = false
    self.daemons = entries
    self.httpDisabled = #entries == 0 and httpFailures > 0

    -- Restore the remembered choice when it is still on the list. A remembered
    -- daemon that is NOT present leaves nothing selected on purpose: silently
    -- falling back to the first result is how a user ends up syncing the wrong
    -- project, which is the exact failure this picker exists to prevent.
    if self.selectedDaemonId == nil then
      local remembered = Settings.readWrapped(self.plugin, lastDaemonKey())
      if typeof(remembered) == "string" and remembered ~= "" then
        for _, entry in entries do
          if entry.daemonId == remembered then
            self.selectedDaemonId = remembered
            break
          end
        end
      end
    end

    if #entries == 0 then
      self:_appendLog(`no daemons found on {Discovery.windowDescription()}`)
    else
      self:_appendLog(`found {#entries} daemon(s)`)
    end
    self:_rerender()
    if afterScan ~= nil then
      -- Isolated: a throwing continuation must not leave `scanning` stuck on
      -- or the picker wedged mid-state.
      local ok, err = pcall(afterScan)
      if not ok then
        self:_appendLog(`!! post-scan action failed: {tostring(err)}`)
      end
    end
  end, function(entry: DaemonEntry)
    if generation ~= self.scanGeneration then
      -- Late answer from a superseded scan — appending it would mix two scans'
      -- findings into one list.
      return
    end
    -- Incremental: show each daemon the moment it answers rather than making
    -- the user wait for the slowest dead port in the window.
    table.insert(self.daemons, entry)
    self:_rerender()
  end)

  -- Cleared as soon as the scan completes; held meanwhile so it can be
  -- cancelled.
  self.activeScan = handle
end

-- Picks a discovered daemon. Remembered per place so the next session
-- reconnects to the same project without asking.
function Widget._onSelectDaemon(self: Widget, daemonId: string)
  if self.connected then
    -- Rows are inert while connected; this is belt and braces.
    return
  end
  self.selectedDaemonId = daemonId
  local failure = Settings.writeWrapped(self.plugin, lastDaemonKey(), daemonId)
  if failure ~= nil then
    self:_appendLog(`!! could not remember this project: {failure}`)
  end
  for _, entry in self.daemons do
    if entry.daemonId == daemonId then
      self:_appendLog(`selected {entry.projectName} (:{entry.port})`)
      break
    end
  end
  self:_rerender()
end

-- Picks the manual Daemon URL row, clearing the remembered daemon so the
-- Settings override wins on the next connect.
function Widget._onSelectManual(self: Widget)
  if self.connected then
    return
  end
  self.selectedDaemonId = nil
  local failure = Settings.clearWrapped(self.plugin, lastDaemonKey())
  if failure ~= nil then
    self:_appendLog(`!! could not clear the remembered project: {failure}`)
  end
  self:_appendLog("using the Daemon URL from Settings")
  self:_rerender()
end

-- Wires the Settings button's onClick. Host (init.server.luau)
-- calls this once at startup with `function() settingsDock:toggle()
-- end`. Setting a non-nil callback also makes the inline Settings
-- button render — view treats `nil` as "no button at all".
function Widget.setOnOpenSettings(self: Widget, cb: (() -> ())?)
  self.onOpenSettings = cb
  self:_rerender()
end

function Widget._rerender(self: Widget)
  if self.tree == nil then
    return
  end
  -- A scheduled batch flush would otherwise re-render redundantly right
  -- after this synchronous one. Clearing here keeps the timer's wake-up a
  -- no-op when there's nothing pending.
  self.logRerenderPending = false
  Roact.update(self.tree, self:_renderTree())
end

function Widget._appendLog(self: Widget, line: string)
  table.insert(self.log, {
    time = os.date("%H:%M:%S") :: string,
    text = line,
  })
  -- Trim from the front so the list can't grow unbounded across a long
  -- session. LogLine renders are cheap but unbounded Roact trees aren't.
  while #self.log > LOG_LIMIT do
    table.remove(self.log, 1)
  end
  -- Batch log-only re-renders. A chatty session (bulk sync emitting one
  -- log line per file) used to trigger one Roact diff per line, which
  -- visibly stutters the Studio UI on places with thousands of files.
  -- 500ms is well under human-perceivable lag for a log feed and bounds
  -- the worst-case render rate at 2/sec regardless of incoming volume.
  -- State-changing call sites (`_setConnected`, etc.) still rerender
  -- synchronously — only this hot path is deferred.
  if self.logRerenderPending then
    return
  end
  self.logRerenderPending = true
  task.delay(0.5, function()
    if self.logRerenderPending then
      self:_rerender()
    end
  end)
end

function Widget._setConnected(self: Widget, connected: boolean)
  if self.connected == connected then
    return
  end
  self.connected = connected
  self:_rerender()
end

function Widget._teardownConnection(self: Widget)
  local existing = self.connection
  if existing then
    -- Best-effort graceful shutdown: tells the daemon to rotate
    -- session_id and drop pending_deltas for this session immediately.
    -- Without this, a Studio reload (common during plugin development)
    -- leaves the daemon buffering events for a session whose plugin
    -- instance is gone — the next reconnect's resume path then replays
    -- those deltas at the fresh plugin's `fileToInstance` map, where
    -- the cached references are stale, producing silent corruption.
    -- pcall because the socket may already be half-dead.
    pcall(function()
      existing:send({ type = "session_end" })
    end)
    existing:close()
  end
  self.connection = nil
  local existingApplier = self.applier
  if existingApplier then
    existingApplier:reset()
  end
  self.applier = nil
  local existingWatcher = self.sourceWatcher
  if existingWatcher then
    existingWatcher:destroy()
  end
  self.sourceWatcher = nil
  self.syncPreview:close()
  -- `_setConnected` short-circuits when the bool doesn't change; if
  -- we tore down a connection that never reached `onOpen` (still in
  -- the connecting state, connected already false), we still need a
  -- render to drop the yellow pill. Force one when only the
  -- connecting flag flipped.
  local wasConnecting = self.connecting
  self.connecting = false
  self:_setConnected(false)
  if wasConnecting and not self.connected then
    self:_rerender()
  end
end

function Widget._onToggleConnection(self: Widget)
  if self.connection ~= nil then
    self:_appendLog("disconnecting")
    self:_teardownConnection()
    return
  end
  self:_onConnectPressed()
end

function Widget._onPauseToggle(self: Widget)
  if self.connection == nil then
    -- Pause is meaningless without a live connection; the button is
    -- hidden in the view in that state but defend in case of a race.
    return
  end
  if self.paused then
    -- Resume = force a reconnect so the daemon's pending_deltas (or a
    -- fresh handshake if those overflowed) brings both sides back in
    -- sync. Without the reconnect the plugin would have no record of
    -- whatever changed during the pause window.
    self.paused = false
    self:_appendLog("resuming sync (reconnecting to catch up)")
    self:_teardownConnection()
    self:_onConnectPressed()
    self:_rerender()
    return
  end
  self.paused = true
  self:_appendLog("paused — outgoing edits and incoming applies suppressed until resume")
  self:_rerender()
end

-- Read-only accessor for the pause flag. Used by SourceWatcher's onEdit
-- and the Applier dispatch hook to short-circuit the sync pipeline while
-- the user has the dock paused. Defined as a method so future callers
-- can't accidentally read the field directly and miss a future
-- refactor.
function Widget.isPaused(self: Widget): boolean
  return self.paused
end

function Widget._onConnectPressed(self: Widget)
  -- Flip into the "Connecting…" visual state up front, before any of
  -- the async machinery (Reconnect.run, Applier setup) runs. This way
  -- the button paints the yellow pill on the same frame as the click;
  -- the matching `connecting = false` lives in the `onOpen` callback
  -- below (success path) and in `_teardownConnection` (user bails out
  -- or socket dies before handshake).
  if not self.connecting then
    self.connecting = true
    self:_rerender()
  end
  local logger = function(line: string)
    self:_appendLog(line)
  end
  local applier: ApplierHandle = Applier.new(logger) :: any
  self.applier = applier
  local sourceWatcher = SourceWatcher.new({
    log = logger,
    hash = function(content)
      return applier:hash(content)
    end,
    isEcho = function(path, hash)
      return applier:isEcho(path, hash)
    end,
    resolvePath = function(instance)
      return applier:resolvePath(instance)
    end,
    onEdit = function(path, content, hash): boolean
      if self.paused then
        -- Pause swallows outgoing edits, but still record the self-edit so
        -- isEcho keeps the local mirror consistent if a stale daemon
        -- broadcast arrives mid-pause. Report false so the watcher skips its
        -- "-> file_changed" log (nothing went on the wire).
        applier:recordSelfEdit(path, hash)
        return false
      end
      local current = self.connection
      if current == nil then
        -- B11: mirror the other hooks — a fully torn-down connection (nil
        -- Handle) should not swallow the edit silently. (A transient
        -- disconnect keeps the Handle non-nil and is buffered by Reconnect.)
        self:_appendLog(`edit ignored (disconnected): {path}`)
        return false
      end
      local sent = current:send({
        type = "file_changed",
        path = path,
        content = content,
        sha256 = hash,
      })
      if sent then
        -- B10: seat the echo only after a successful (or buffered) send, so a
        -- frame that never left keeps recentlyApplied honest.
        applier:recordSelfEdit(path, hash)
      else
        -- B10: the socket looked usable but the frame did not go out. Surface
        -- a distinct warning instead of only the generic "error:" line, which
        -- left the status dot green and the "-> file_changed" log misleading.
        self:_appendLog(`!! file_changed send FAILED (still connected?): {path}`)
      end
      return sent
    end,
    onDelete = function(path)
      applier:forgetLocalDelete(path)
      if self.paused then
        return
      end
      local current = self.connection
      if current ~= nil then
        current:send({
          type = "file_deleted",
          path = path,
        })
      else
        -- B11: symmetric with onEdit — surface the dropped delete instead of
        -- swallowing it.
        self:_appendLog(`delete ignored (disconnected): {path}`)
      end
    end,
    onRename = function(oldPath, newPath, kind, content, hash)
      -- The Applier's local mirror is the source of truth for echo
      -- detection — flip it BEFORE the wire frame goes out so a
      -- mid-flight daemon broadcast for the old path doesn't trip up
      -- subsequent reconciliations.
      local okForget, forgetErr = pcall(function()
        applier:forgetLocalRename(oldPath, newPath, hash)
      end)
      if not okForget then
        self:_appendLog(`rename: forgetLocalRename failed for {oldPath} -> {newPath}: {tostring(forgetErr)}`)
        return
      end
      if self.paused then
        return
      end
      local current = self.connection
      if current == nil then
        self:_appendLog(`rename dropped (disconnected): {oldPath} -> {newPath}`)
        return
      end
      local okSend, sendErr = pcall(function()
        current:send({
          type = "file_renamed",
          old_path = oldPath,
          new_path = newPath,
          kind = kind,
          content = content,
          sha256 = hash,
        })
      end)
      if not okSend then
        self:_appendLog(`rename send failed for {oldPath} -> {newPath}: {tostring(sendErr)}`)
        return
      end
    end,
  })
  self.sourceWatcher = sourceWatcher :: any
  applier:setTrackHooks(function(path, instance)
    sourceWatcher:track(path, instance)
  end, function(path)
    sourceWatcher:untrack(path)
  end)
  -- A3: let the Applier consult the watcher for a pending, not-yet-sent
  -- Studio edit before it overwrites Source with daemon content.
  applier:setPendingEditProbe(function(path: string)
    return sourceWatcher:peekPending(path)
  end)
  -- A4/M14 apply-ACK: confirm (or report the failure of) each daemon-driven
  -- apply so the daemon only advances tree_base once Studio actually holds the
  -- change. These fire once per applied frame; on a torn-down connection we skip
  -- quietly (the daemon's apply-ACK timeout covers a genuinely lost
  -- confirmation) rather than spamming the log.
  applier:setApplyHooks(function(path: string, sha256: string)
    local current = self.connection
    if current == nil then
      return
    end
    current:send({
      type = "file_applied",
      path = path,
      sha256 = sha256,
    })
  end, function(path: string, reason: string)
    local current = self.connection
    if current == nil then
      return
    end
    current:send({
      type = "file_apply_failed",
      path = path,
      reason = reason,
    })
    self:_appendLog(`!! apply failed: {path} ({reason})`)
  end)
  applier:setConflictDispatcher(function(conflicts)
    self.resolver:enqueue(conflicts)
  end)
  applier:setBootstrapDivergeHook(function(path: string, kind: string, studioContent: string, studioSha256: string)
    local current = self.connection
    if current == nil then
      self:_appendLog(`bootstrap diverge ignored (disconnected): {path}`)
      return
    end
    current:send({
      type = "file_changed",
      path = path,
      content = studioContent,
      sha256 = studioSha256,
      kind = kind,
      bootstrap_diverge = true,
    })
    self:_appendLog(`bootstrap diverge echoed: {path}`)
  end)
  applier:setBulkSyncPreviewDispatcher(function(payload: Payload)
    self:_onBulkSyncPreview(payload)
  end)
  applier:setStudioCreatedHook(function(path: string, kind: string, studioContent: string, studioSha256: string)
    local current = self.connection
    if current == nil then
      self:_appendLog(`studio create ignored (disconnected): {path}`)
      return
    end
    current:send({
      type = "file_created",
      path = path,
      kind = kind,
      content = studioContent,
      sha256 = studioSha256,
    })
    self:_appendLog(`-> file_created {path}`)
  end)
  applier:setStudioRenamedHook(function(
    oldPath: string,
    newPath: string,
    kind: string,
    studioContent: string,
    studioSha256: string
  )
    if self.paused then
      return
    end
    local current = self.connection
    if current == nil then
      self:_appendLog(`studio rename ignored (disconnected): {oldPath} -> {newPath}`)
      return
    end
    current:send({
      type = "file_renamed",
      old_path = oldPath,
      new_path = newPath,
      kind = kind,
      content = studioContent,
      sha256 = studioSha256,
    })
    self:_appendLog(`-> file_renamed {oldPath} -> {newPath}`)
  end)
  applier:setStudioCollisionHook(function(
    path: string,
    source: string,
    sha256: string,
    _colliders: { Instance }
  )
    local current = self.connection
    if current == nil then
      self:_appendLog(`name_collision ignored (disconnected): {path}`)
      return
    end
    current:send({
      type = "name_collision",
      path = path,
      sha256 = sha256,
    })
    local _ = source -- reserved for future "preview the conflicting source" UI
    self:_appendLog(`!! name_collision reported to daemon: {path}`)
  end)
  -- Mirror the `request_bootstrap_preview` flag we set in the Hello: the
  -- Applier needs to know whether to run its preview-mode handler for
  -- `ProjectOpened` (which skips auto-materialize and hands a snapshot back
  -- to this Widget for forwarding).
  applier:setBootstrapPreviewMode(true)
  -- M26: refuse to materialize if the daemon starts advertising a different
  -- project_root than the one we first bootstrapped against (cross-project
  -- corruption guard). The Applier caches the root and fires this on a
  -- mismatch; we surface it and tear the connection down.
  -- Identity of the daemon THIS connection is talking to.
  --
  -- Deliberately a local captured by the auth closures below, not a field read
  -- from `self` when they fire. The two are not equivalent: a teardown does not
  -- silence an Applier's already-queued callbacks, so if the user disconnects
  -- and immediately connects to a different project, a late `auth_granted` or
  -- `auth_rejected` from the OLD daemon would read the NEW `self.activeDaemonId`
  -- and write — or clear — the wrong project's token. Binding the id to the
  -- connection makes each handler act on the daemon it actually belongs to.
  local connectionDaemonId: string? = self.activeDaemonId

  applier:setDaemonIdHook(function(daemonId: string)
    -- Learned here for a plugin that connected via a manual Daemon URL and so
    -- never ran a discovery scan; the picker path sets it earlier.
    connectionDaemonId = daemonId
    if self.activeDaemonId ~= daemonId then
      self.activeDaemonId = daemonId
    end
  end)
  applier:setProjectRootMismatchHook(function(expected: string, got: string)
    self:_appendLog(
      `!! daemon switched project_root ({expected} -> {got}); disconnecting to avoid cross-project corruption`
    )
    -- Defer the teardown: we're inside applier:dispatch, itself inside the
    -- socket MessageReceived handler; tearing the connection down mid-stack
    -- would disconnect the very signal we are running in.
    task.defer(function()
      self:_teardownConnection()
    end)
  end)
  -- Auth handlers. The daemon issues a token via `auth_granted` when the
  -- extension's pairing breadcrumb is fresh; we cache it and replay it in
  -- `Hello.auth_token` on future reconnects. The daemon regenerates its token
  -- every boot, so after a daemon restart our cached token is stale and the
  -- daemon replies `auth_rejected` (it no longer echoes a token on mismatch —
  -- that was a credential leak). On rejection we MUST clear the cached token so
  -- the automatic reconnect re-pairs tokenless via a fresh breadcrumb; keeping
  -- the stale token would loop reject→reconnect→reject forever.

  applier:setAuthGrantedHandler(function(payload: any)
    local token = tostring(payload.auth_token or "")
    if token == "" then
      return
    end
    local daemonId = connectionDaemonId
    if daemonId == nil or daemonId == "" then
      -- No id yet means `project_opened` has not arrived on THIS connection, so
      -- we do not know which project this token belongs to. Storing it under a
      -- guessed key would be worse than not storing it: the next connect
      -- re-pairs via the breadcrumb, which is invisible to the user.
      self:_appendLog("auth token received before project id; will re-pair next time")
      return
    end
    local failure = Settings.writeWrapped(self.plugin, authTokenKey(daemonId), token)
    if failure ~= nil then
      -- Surfaced rather than swallowed: a key that silently fails to persist is
      -- exactly how the legacy dotted-key bug hid for so long.
      self:_appendLog(`!! failed to persist auth token: {failure}`)
    else
      self:_appendLog("auth token saved for this project")
    end
  end)
  applier:setAuthRejectedHandler(function(payload: any)
    -- The daemon rejected our token (typically stale after a daemon restart —
    -- it regenerates the token every boot). Clear the cached token so the
    -- automatic reconnect sends none and re-pairs via a fresh breadcrumb;
    -- otherwise we would replay the same rejected token on every retry.
    -- Clears ONLY this project's token. Clearing a shared key — as the legacy
    -- global setting forced — meant a rejection from one daemon wiped every
    -- other project's credential too, which is what produced the
    -- reject -> re-pair -> reject thrash when two daemons were running.
    local reason = tostring((payload and payload.reason) or "unknown")
    -- This connection's daemon, not whatever the Widget is pointed at now — a
    -- late rejection must never clear a different project's credential.
    local daemonId = connectionDaemonId
    local failure: string? = nil
    if daemonId ~= nil and daemonId ~= "" then
      failure = Settings.clearWrapped(self.plugin, authTokenKey(daemonId))
    end
    if failure ~= nil then
      self:_appendLog(`!! auth rejected ({reason}); failed to clear token: {failure}`)
    else
      self:_appendLog(`auth rejected ({reason}); cleared stored token, will re-pair`)
    end
  end)
  applier:setBootstrapSnapshotHook(function(snapshot)
    local current = self.connection
    if current == nil then
      self:_appendLog(`bootstrap snapshot dropped (disconnected): {#snapshot} file(s)`)
      return
    end
    current:send({
      type = "studio_snapshot_report",
      snapshot = snapshot,
    })
    self:_appendLog(`-> studio_snapshot_report ({#snapshot} file(s))`)
  end)
  self.resolver:setHandlers({
    onResolve = function(resolution)
      local current = self.connection
      if current == nil then
        self:_appendLog(`resolution dropped (disconnected): {resolution.path}`)
        return
      end
      current:send({
        type = "conflict_resolved",
        resolutions = { resolution },
      })
    end,
    onCancel = function()
      self:_appendLog("conflict UI cancelled")
    end,
    onAbandon = function(paths)
      local current = self.connection
      if current == nil then
        self:_appendLog(`abandon dropped (disconnected): {#paths} path(s)`)
        return
      end
      current:send({
        type = "conflict_abandoned",
        paths = paths,
      })
      self:_appendLog(`-> conflict_abandoned ({#paths} path(s))`)
    end,
    onLineByLine = function(conflict)
      -- Lazy-init the picker so unused plugin sessions never spawn the
      -- extra dock widget. Reuse across conflicts.
      if self.mergePicker == nil then
        self.mergePicker = MergePicker.new(plugin) :: any
      end
      local picker = self.mergePicker
      if picker == nil then
        return
      end
      local path: string = conflict.path
      picker:open(
        path,
        conflict.studio_content or "",
        conflict.fs_content or "",
        {
          onApply = function(applyPath, content)
            local current = self.connection
            if current == nil then
              self:_appendLog(
                `manual resolve dropped (disconnected): {applyPath}`
              )
              return
            end
            local applierRef = self.applier
            if applierRef == nil then
              self:_appendLog(
                `manual resolve dropped (no applier): {applyPath}`
              )
              return
            end
            local sha = applierRef:hash(content)
            current:send({
              type = "conflict_resolved_manual",
              path = applyPath,
              content = content,
              sha256 = sha,
            })
            self:_appendLog(
              `-> conflict_resolved_manual {applyPath} ({#content} bytes)`
            )
            -- Drop the conflict from the resolver queue so the dock
            -- doesn't keep showing a now-resolved entry. The daemon
            -- removes its pending_conflicts entry on receive.
            self.resolver:removeConflict(applyPath)
          end,
          onCancel = function(cancelPath)
            self:_appendLog(`line-by-line cancelled: {cancelPath}`)
          end,
        }
      )
    end,
  })

  -- Resolve the URL at connect time so a Settings save while
  -- disconnected takes effect on the next Connect. The default
  -- (`DAEMON_URL`) matches the bundled daemon's bind address.
  local url = self.settings.daemonUrl
  if typeof(url) ~= "string" or url == "" then
    url = DAEMON_URL
  end

  -- A daemon picked in the project picker wins over the Settings URL. With one
  -- daemon per project the settings value is a single fixed address, so honouring
  -- it over an explicit pick would quietly connect the user to a different
  -- project than the one they just clicked.
  --
  -- Selecting the pinned "Manual (from Settings)" row clears `selectedDaemonId`,
  -- which falls through to the URL above — that is how the override stays
  -- reachable.
  local selectedId = self.selectedDaemonId
  if selectedId ~= nil and selectedId ~= "" then
    for _, entry in self.daemons do
      if entry.daemonId == selectedId then
        url = entry.url
        -- Known before the socket opens, so the Hello can carry this project's
        -- stored token on the very first attempt instead of re-pairing once.
        connectionDaemonId = selectedId
        self:_appendLog(`connecting to {entry.projectName}`)
        break
      end
    end
  end

  self:_appendLog("connecting to " .. url)

  local handle = Reconnect.run(url, {
    -- Forward Connection-level options (today: skip the
    -- HttpService.HttpEnabled pre-flight check). Read from the
    -- live settings struct so a Save during a live session takes
    -- effect on the next reconnect.
    connectionOptions = {
      skipHttpEnabledCheck = self.settings.skipHttpEnabledCheck,
    },
    buildHelloPayload = function(sessionId: string?): Payload
      -- `request_bootstrap_preview` opts into the two-stage cold-boot
      -- flow: after `ProjectOpened` the plugin ships a
      -- `studio_snapshot_report` and waits for the daemon's
      -- `BulkSyncPreview` before materializing anything. Without the
      -- flag the daemon falls back to the legacy auto-apply bootstrap,
      -- which keeps pre-E-this plugins working.
      local payload: Payload = {
        type = "hello",
        -- Plugin reports its own wire-protocol version. The daemon
        -- parses with semver and enforces
        -- `>= MIN_COMPATIBLE_PLUGIN_VERSION` (currently "0.2.0"); bump
        -- in lockstep with breaking protocol changes so the daemon can
        -- refuse old plugins instead of silently misinterpreting their
        -- frames.
        version = "0.5.0",
        studio_snapshot = {},
        request_bootstrap_preview = true,
      }
      if sessionId ~= nil then
        payload.session_id = sessionId
      end
      -- Auth token from the previous successful pairing, if any.
      -- Empty/nil sends no `auth_token` field; the daemon's auth
      -- gate is best-effort and accepts the connection regardless,
      -- opportunistically issuing a token via `auth_granted` when
      -- the extension's pairing breadcrumb is fresh.
      --
      -- IMPORTANT: `plugin` is NOT a global inside this module —
      -- the Studio global of that name exists only at the
      -- `init.server.luau` scope. We must reach for `self.plugin`
      -- (captured at `Widget.new`). Using the bare `plugin`
      -- silently raised "attempt to index nil" inside this Roact
      -- callback, which Roblox swallowed without surfacing — the
      -- net effect was that `buildHelloPayload` errored, no Hello
      -- was ever sent, and the daemon closed the socket on its
      -- read-Hello timeout. Symptom: Connect button does nothing,
      -- Activity log empty.
      --
      -- The token is stored per project (keyed by the daemon's `daemon_id`),
      -- so connecting to a second project no longer invalidates the first's
      -- token. It is nil on a first connect through a manual Daemon URL — we
      -- then send no token, the daemon pairs us via the extension's
      -- breadcrumb, and `project_opened` tells us the id to store it under
      -- next time.
      local daemonId = connectionDaemonId
      if daemonId ~= nil and daemonId ~= "" then
        local stored = Settings.readWrapped(self.plugin, authTokenKey(daemonId))
        if typeof(stored) == "string" and stored ~= "" then
          payload.auth_token = stored
        end
      end
      return payload
    end,
    onOpen = function(attempt: number, sessionId: string?)
      if attempt == 0 then
        self:_appendLog("socket open; sending hello")
      elseif sessionId ~= nil then
        self:_appendLog(`reconnected (attempt {attempt}); resuming session`)
      else
        self:_appendLog(`reconnected (attempt {attempt})`)
      end
      -- Handshake completed — leave the "Connecting…" visual state.
      -- `_setConnected(true)` already rerenders, so we just clear the
      -- flag inline; the next render reads `connecting = false`.
      self.connecting = false
      self:_setConnected(true)
    end,
    onMessage = function(payload)
      -- Pause suppresses incoming DataModel mutations. Other message
      -- kinds (project_opened/resumed/conflict_detected/bulk_sync_*)
      -- still flow through so the dock UI keeps working — only the
      -- "apply this content to a Studio script" frames are dropped.
      -- The user clicking Resume forces a reconnect so the daemon's
      -- pending_deltas replays whatever we missed.
      if self.paused then
        local kind = payload and payload.type
        if kind == "file_changed" or kind == "file_created" or kind == "file_deleted" then
          return
        end
      end
      applier:dispatch(payload)
    end,
    onError = function(err)
      self:_appendLog("error: " .. err)
    end,
    onReconnecting = function(attempt: number, delaySec: number)
      self:_appendLog(`reconnecting (attempt {attempt} in {delaySec}s)`)
      -- A reconnect cycle counts as "connecting" from the user's
      -- perspective: the dock should show the yellow pill with
      -- "Click to stop" so the user can break out instead of waiting
      -- through an open-ended retry loop.
      local wasConnecting = self.connecting
      self.connecting = true
      self:_setConnected(false)
      -- `_setConnected` already rerendered if `connected` flipped to
      -- false; if `onClose` ran first (most common path), connected
      -- was already false and `_setConnected` short-circuits — so we
      -- force one render here to make the yellow pill appear.
      if not wasConnecting then
        self:_rerender()
      end
    end,
    onResumed = function(_sessionId: string, replayed: number)
      self:_appendLog(`resumed: {replayed} buffered frame(s)`)
    end,
    onOutgoingFlush = function(count: number)
      self:_appendLog(`flushed {count} buffered edit(s) after reconnect`)
    end,
    onClose = function()
      self:_appendLog("socket closed")
      self.syncPreview:close()
      self:_setConnected(false)
    end,
  })

  self.connection = handle :: any
end

function Widget._onBulkSyncPreview(self: Widget, payload: Payload)
  local requestId = payload["request_id"]
  local direction = payload["direction"]
  local entries = payload["entries"]
  if typeof(requestId) ~= "string" or typeof(direction) ~= "string" or typeof(entries) ~= "table" then
    self:_appendLog("bulk sync preview: malformed payload")
    return
  end
  local typedEntries: { BulkSyncPreviewEntry } = entries :: any
  -- Loud, dedicated log line for the "interface não aparece no bootstrap"
  -- triage. If the user sees this and the dock is still invisible, the
  -- problem is the dock's WidgetGui state in Studio (off-screen position,
  -- minimised tab, etc.) — not a missing message. If they DON'T see this,
  -- the message never made it through Applier.dispatch.
  self:_appendLog(
    `>> opening BulkSyncPreview dock: {#typedEntries} file(s) direction={direction} request_id={requestId}`
  )
  self.syncPreview:open(requestId, direction, typedEntries, {
    onSeeCode = function(_entry: BulkSyncPreviewEntry)
      -- Preview-in-editor handled inside BulkSyncPreview via
      -- ScriptEditorService; no daemon round-trip needed.
    end,
    onLineByLine = function(entry: BulkSyncPreviewEntry)
      -- Reachable from the BulkSync preview without going through
      -- ConflictResolver (true conflicts are rare; most users hit the
      -- BulkSync flow first). Same wire frame either way:
      -- `conflict_resolved_manual`. The daemon's handler treats
      -- "no pending_conflict for this path" as a plain content
      -- overwrite and writes both sides, which is exactly what we
      -- want for a BulkSync-time manual merge.
      if self.mergePicker == nil then
        self.mergePicker = MergePicker.new(self.plugin) :: any
      end
      local picker = self.mergePicker
      if picker == nil then
        return
      end
      picker:open(
        entry.path,
        entry.studio_content or "",
        entry.fs_content or "",
        {
          onApply = function(applyPath, content)
            local current = self.connection
            if current == nil then
              self:_appendLog(
                `bulk merge dropped (disconnected): {applyPath}`
              )
              return
            end
            local applierRef = self.applier
            if applierRef == nil then
              self:_appendLog(
                `bulk merge dropped (no applier): {applyPath}`
              )
              return
            end
            local sha = applierRef:hash(content)
            current:send({
              type = "conflict_resolved_manual",
              path = applyPath,
              content = content,
              sha256 = sha,
            })
            self:_appendLog(
              `-> conflict_resolved_manual {applyPath} from BulkSync ({#content} bytes)`
            )
            -- Drop the row so the user's subsequent "Apply Sync"
            -- doesn't ALSO send a keep_studio/keep_ide for this path
            -- and overwrite the merged content we just shipped.
            self.syncPreview:removeEntry(applyPath)
          end,
          onCancel = function(cancelPath)
            self:_appendLog(`bulk merge cancelled: {cancelPath}`)
          end,
        }
      )
    end,
    onApply = function(resolutions)
      local current = self.connection
      if current == nil then
        self:_appendLog(`bulk sync confirm dropped (disconnected): {requestId}`)
        return
      end
      current:send({
        type = "bulk_sync_confirm",
        request_id = requestId,
        resolutions = resolutions,
      })
      self:_appendLog(`bulk sync applied: {requestId} ({#resolutions} resolution(s))`)
    end,
    onCancel = function()
      local current = self.connection
      if current == nil then
        return
      end
      current:send({
        type = "bulk_sync_cancel",
        request_id = requestId,
      })
      self:_appendLog(`bulk sync cancelled: {requestId}`)
    end,
  })
end

return Widget
