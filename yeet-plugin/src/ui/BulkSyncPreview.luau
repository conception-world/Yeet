--!strict

-- Bulk-sync preview dock, rewritten on top of Roact using the shared design
-- system (Card / Button / Badge / ScrollList). Scratch-script management
-- for "See Code" stays imperative (ScriptEditorService is a blocking API
-- that doesn't fit a declarative render cycle), but every pixel the user
-- sees now flows through `Roact.update` with the state below.
--
-- Bootstrap preview adds a per-row `Choice` picker (Studio / IDE / Ignore)
-- that maps each file to an explicit `BulkSyncAction` the daemon will apply.
-- The `Ignore` slot is asymmetric on purpose (spec from the user):
--   * Modified    → Skip (leave both sides different — user decides later)
--   * StudioOnly  → Skip and log a warn (losing Studio-only work needs a
--                    loud breadcrumb so it doesn't surprise us later)
--   * IdeOnly     → DeleteFromIde (file was only on disk; user said drop it)
-- The Studio / IDE buttons mean "this side wins"; disabled states hide
-- combinations that have no meaning (Studio on IdeOnly, IDE on StudioOnly).

local ScriptEditorService = game:GetService("ScriptEditorService")
local ServerStorage = game:GetService("ServerStorage")

local Roact = require(script.Parent.Parent.Packages.Roact)
local theme = require(script.Parent.theme)
local Card = require(script.Parent.components.Card)
local Button = require(script.Parent.components.Button)
local Badge = require(script.Parent.components.Badge)
local Heading = require(script.Parent.components.Heading)
local ScrollList = require(script.Parent.components.ScrollList)
local DiffView = require(script.Parent.components.DiffView)
local diffLines = require(script.Parent.components.diffLines)

export type Entry = {
  path: string,
  status: string,
  studio_content: string?,
  fs_content: string?,
}

-- One of the three picker modes the row exposes.
export type Choice = "studio" | "ide" | "ignore"

-- Wire-level action names, keep in sync with `BulkSyncAction` in
-- yeet-daemon/src/protocol.rs. The daemon deserializes with
-- `#[serde(rename_all = "snake_case")]` on the Rust enum.
export type Action =
  "skip"
  | "keep_studio"
  | "keep_ide"
  | "push_to_ide"
  | "delete_from_studio"
  | "pull_to_studio"
  | "delete_from_ide"

export type Resolution = { path: string, action: Action }

export type Handlers = {
  onSeeCode: (entry: Entry) -> (),
  -- Fired when the user clicks "Merge…" on a Modified entry. The host
  -- (Widget) opens MergePicker with the entry's two sides; on Apply
  -- the host sends `conflict_resolved_manual` over the wire AND
  -- removes the entry from this preview locally so the subsequent
  -- "Apply Sync" doesn't overwrite the manually-merged content.
  onLineByLine: (entry: Entry) -> (),
  onApply: (resolutions: { Resolution }) -> (),
  onCancel: () -> (),
}

-- BASE_WIDGET_ID + a per-VM unique suffix.
--
-- The user reported "interface não aparece no bootstrap" specifically
-- when SWITCHING PROJECTS in Studio. Every project switch reloads the
-- plugin VM (Plugin.Unloading on the old, fresh script run on the new),
-- but Studio caches dock layout per WIDGET_ID across VM reloads. With a
-- stable WIDGET_ID, the new VM's `CreateDockWidgetPluginGui` reuses the
-- old, now-stale slot — its WindowManager state references the destroyed
-- gui, and Enabled toggles silently no-op. `OverrideEnabledRestore=true`
-- handles the Enabled bit but NOT the slot-attachment bug.
--
-- Per-VM unique IDs sidesteps this entirely: each plugin load registers
-- a brand-new widget with its own clean slot. Trade-off: dock position
-- isn't remembered across project switches. Acceptable — losing memorised
-- position once per project switch beats "dock never appears".
local BASE_WIDGET_ID = "YeetBulkSyncPreview"
local function freshWidgetId(): string
  -- HttpService:GenerateGUID is the only Roblox-provided UUID source.
  -- false = no curly braces.
  local guid = game:GetService("HttpService"):GenerateGUID(false)
  return string.format("%s_%s", BASE_WIDGET_ID, guid)
end
local WIDGET_ID = freshWidgetId()

-- ─── Pure view helpers ────────────────────────────────────────────────────

local function colorForStatus(status: string): Color3
  if status == "modified" then
    return theme.colors.accentWarning
  elseif status == "studio_only" then
    return theme.colors.accentStudio
  elseif status == "ide_only" then
    return theme.colors.accentIde
  end
  return theme.colors.textMuted
end

local function labelForStatus(status: string): string
  if status == "modified" then
    return "modified"
  elseif status == "studio_only" then
    return "studio only"
  elseif status == "ide_only" then
    return "ide only"
  end
  return status
end

local function directionTitle(direction: string): string
  if direction == "from_studio" then
    return "Sync From Studio"
  elseif direction == "from_ide" then
    return "Sync From IDE"
  elseif direction == "bootstrap" then
    return "Initial Sync"
  end
  return direction
end

-- Encodes the semantic matrix described in the module header. Returning the
-- wire action directly keeps the view layer ignorant of the daemon protocol
-- beyond the strings — the Widget reads these and ships them as-is.
local function actionFor(status: string, choice: Choice): Action
  if status == "modified" then
    if choice == "studio" then
      return "keep_studio"
    elseif choice == "ide" then
      return "keep_ide"
    end
    return "skip"
  elseif status == "studio_only" then
    if choice == "studio" then
      return "push_to_ide"
    end
    -- `ide` is disabled for StudioOnly; `ignore` falls through to Skip.
    return "skip"
  elseif status == "ide_only" then
    if choice == "ide" then
      return "pull_to_studio"
    end
    -- `studio` is disabled for IdeOnly; `ignore` deletes the orphan IDE file.
    return "delete_from_ide"
  end
  return "skip"
end

-- Which choices are applicable for a given status. The view grays out the
-- others so the user can't pick an action that has no meaning.
local function choiceAvailable(status: string, choice: Choice): boolean
  if choice == "ignore" then
    return true
  end
  if status == "modified" then
    return true
  end
  if status == "studio_only" and choice == "studio" then
    return true
  end
  if status == "ide_only" and choice == "ide" then
    return true
  end
  return false
end

-- Safer default per status: "everything reasonable syncs". Users can override
-- each row or use the "Resolve all as" quick buttons. We avoid defaulting to
-- Ignore everywhere because the point of the preview is confirming a sync,
-- not silently skipping it.
local function defaultChoice(status: string): Choice
  if status == "modified" then
    return "ide"
  elseif status == "studio_only" then
    return "studio"
  elseif status == "ide_only" then
    return "ide"
  end
  return "ignore"
end

-- ─── Components ───────────────────────────────────────────────────────────

type ChoiceButtonProps = {
  label: string,
  choice: Choice,
  current: Choice,
  available: boolean,
  layoutOrder: number,
  onPick: (Choice) -> (),
}

local function ChoiceButton(props: ChoiceButtonProps)
  local isActive = props.current == props.choice
  -- `accept` variant reads as "selected + going to act" for Studio/IDE;
  -- the Ignore choice uses `danger` when active so the user's eye catches
  -- the "this row will skip or delete" rows at a glance.
  local variant: "accept" | "danger" | "ghost"
  if isActive then
    variant = if props.choice == "ignore" then "danger" else "accept"
  else
    variant = "ghost"
  end
  return Roact.createElement(Button, {
    text = props.label,
    variant = variant,
    layoutOrder = props.layoutOrder,
    disabled = not props.available,
    onClick = function()
      if props.available then
        props.onPick(props.choice)
      end
    end,
  })
end

type RowProps = {
  entry: Entry,
  choice: Choice,
  layoutOrder: number,
  onSeeCode: () -> (),
  onLineByLine: () -> (),
  onChoose: (Choice) -> (),
}

local function EntryRow(props: RowProps)
  local status = props.entry.status
  return Roact.createElement(Card, {
    layoutOrder = props.layoutOrder,
    color = theme.colors.surface,
    radius = theme.radius.md,
    padding = theme.spacing.md,
    children = {
      layout = Roact.createElement("UIListLayout", {
        FillDirection = Enum.FillDirection.Vertical,
        SortOrder = Enum.SortOrder.LayoutOrder,
        Padding = UDim.new(0, theme.spacing.sm),
      }),
      top = Roact.createElement("Frame", {
        Size = UDim2.new(1, 0, 0, 22),
        BackgroundTransparency = 1,
        LayoutOrder = 1,
      }, {
        layout = Roact.createElement("UIListLayout", {
          FillDirection = Enum.FillDirection.Horizontal,
          VerticalAlignment = Enum.VerticalAlignment.Center,
          HorizontalAlignment = Enum.HorizontalAlignment.Left,
          Padding = UDim.new(0, theme.spacing.sm),
          SortOrder = Enum.SortOrder.LayoutOrder,
        }),
        path = Roact.createElement("TextLabel", {
          -- Flex-fill so the path takes whatever width is left
          -- after the badge (which sizes to its text). Was a
          -- hardcoded `1, -140` that broke when the badge text
          -- changed length or the dock was narrower.
          Size = UDim2.fromScale(1, 1),
          BackgroundTransparency = 1,
          Text = props.entry.path,
          TextColor3 = theme.colors.text,
          Font = theme.typography.code.font,
          TextSize = theme.typography.code.size,
          TextXAlignment = Enum.TextXAlignment.Left,
          TextTruncate = Enum.TextTruncate.AtEnd,
          LayoutOrder = 1,
        }, {
          flex = Roact.createElement("UIFlexItem", {
            FlexMode = Enum.UIFlexMode.Fill,
          }),
        }),
        badge = Roact.createElement(Badge, {
          text = labelForStatus(status),
          color = colorForStatus(status),
          backgroundColor = theme.colors.background,
          layoutOrder = 2,
        }),
      }),
      actions = Roact.createElement("Frame", {
        Size = UDim2.new(1, 0, 0, 28),
        BackgroundTransparency = 1,
        LayoutOrder = 2,
      }, {
        layout = Roact.createElement("UIListLayout", {
          FillDirection = Enum.FillDirection.Horizontal,
          VerticalAlignment = Enum.VerticalAlignment.Center,
          Padding = UDim.new(0, theme.spacing.sm),
          SortOrder = Enum.SortOrder.LayoutOrder,
        }),
        studio = Roact.createElement(ChoiceButton, {
          label = "Studio",
          choice = "studio",
          current = props.choice,
          available = choiceAvailable(status, "studio"),
          layoutOrder = 1,
          onPick = props.onChoose,
        }),
        ide = Roact.createElement(ChoiceButton, {
          label = "IDE",
          choice = "ide",
          current = props.choice,
          available = choiceAvailable(status, "ide"),
          layoutOrder = 2,
          onPick = props.onChoose,
        }),
        ignore = Roact.createElement(ChoiceButton, {
          label = "Ignore",
          choice = "ignore",
          current = props.choice,
          available = choiceAvailable(status, "ignore"),
          layoutOrder = 3,
          onPick = props.onChoose,
        }),
        spacer = Roact.createElement("Frame", {
          -- Flex-fill spacer that pushes `Merge…` / `See Code`
          -- to the right edge regardless of how wide the
          -- choice buttons end up. Was a hardcoded `1, -460`
          -- that we had to bump every time the right-side
          -- button group changed shape — UIFlexItem.Fill
          -- replaces that pixel arithmetic with a layout-time
          -- "take whatever's left" rule.
          Size = UDim2.fromScale(0, 1),
          BackgroundTransparency = 1,
          LayoutOrder = 4,
        }, {
          flex = Roact.createElement("UIFlexItem", {
            FlexMode = Enum.UIFlexMode.Fill,
          }),
        }),
        -- Line-by-line merge — only meaningful for Modified
        -- entries (both sides have content to mix). For
        -- StudioOnly / IdeOnly there's only one side, so the
        -- merge picker would just be a "take the one side that
        -- exists" choice already covered by Studio / IDE
        -- buttons. Disabled in those cases so the user doesn't
        -- click and find an empty column.
        merge = Roact.createElement(Button, {
          text = "Merge…",
          variant = "ghost",
          icon = "⇆",
          layoutOrder = 5,
          disabled = status ~= "modified",
          onClick = props.onLineByLine,
        }),
        seeCode = Roact.createElement(Button, {
          text = "See Code",
          variant = "ghost",
          icon = "⌘",
          layoutOrder = 6,
          onClick = props.onSeeCode,
        }),
      }),
    },
  })
end

type ViewProps = {
  visible: boolean,
  request_id: string?,
  direction: string?,
  entries: { Entry },
  -- Number of entries actually rendered this tick. The full list lives
  -- in `entries`; the slice [1..visibleEntryCount] is what gets
  -- materialised. The bumping coroutine (see `_progressivelyExpand`)
  -- grows this every frame until it reaches #entries, so the dock
  -- opens instantly with the first chunk and the rest streams in.
  visibleEntryCount: number,
  choices: { [string]: Choice },
  -- When set, the named Migrate-all direction is "armed" — its button
  -- shows the file count + a confirm prompt. Click it again to commit;
  -- click anything else (cancel, the other direction, an entry) to reset.
  pendingMigrate: string?,
  onSeeCode: (entry: Entry) -> (),
  onLineByLine: (entry: Entry) -> (),
  onChoose: (path: string, choice: Choice) -> (),
  onResolveAll: (choice: Choice) -> (),
  onMigrateAll: (direction: "studio_to_ide" | "ide_to_studio") -> (),
  onApply: () -> (),
  onCancel: () -> (),
}

-- How many extra entries we render per frame as the progressive cap
-- bumps up. 100 keeps each tick under the 16ms budget on typical
-- hardware (an EntryRow mounts ~8 child instances, so ~800 instance
-- creations per chunk).
local PROGRESSIVE_RENDER_CHUNK = 100
-- Initial visible-row cap on `open()`. Sized so the first paint of the
-- dock happens within one frame even on a multi-thousand-file
-- bootstrap-preview. Anything beyond this count streams in via
-- `_progressivelyExpand` after the dock is visible.
local INITIAL_VISIBLE_COUNT = 100

local function PreviewView(props: ViewProps)
  if not props.visible then
    return Roact.createElement("Frame", {
      Size = UDim2.fromScale(1, 1),
      BackgroundColor3 = theme.colors.background,
      BorderSizePixel = 0,
    })
  end

  local isEmpty = #props.entries == 0
  local direction = props.direction or "from_studio"
  -- Slice the entries to the progressive cap. The full list still
  -- drives buttons like "Apply (N)" via #props.entries; we just don't
  -- materialise rows beyond the cap until the bumping coroutine has
  -- caught up.
  local totalEntries = #props.entries
  local visibleCap = math.min(props.visibleEntryCount, totalEntries)

  local rows: { [string]: any } = {}
  for index = 1, visibleCap do
    local entry = props.entries[index]
    local choice = props.choices[entry.path] or defaultChoice(entry.status)
    rows["row_" .. index] = Roact.createElement(EntryRow, {
      entry = entry,
      choice = choice,
      layoutOrder = index,
      onSeeCode = function()
        props.onSeeCode(entry)
      end,
      onLineByLine = function()
        props.onLineByLine(entry)
      end,
      onChoose = function(nextChoice)
        props.onChoose(entry.path, nextChoice)
      end,
    })
  end
  -- Footer "loading more…" hint while the progressive coroutine is
  -- still expanding the visible window. Plain text — the user sees
  -- the dock is alive and growing instead of "stuck at 100 of 848".
  if visibleCap < totalEntries then
    rows["__progressive_loading"] = Roact.createElement("TextLabel", {
      Size = UDim2.new(1, 0, 0, 22),
      BackgroundTransparency = 1,
      Text = string.format("loading… %d / %d entries rendered", visibleCap, totalEntries),
      TextColor3 = theme.colors.textMuted,
      Font = theme.typography.body.font,
      TextSize = theme.typography.body.size,
      TextXAlignment = Enum.TextXAlignment.Center,
      LayoutOrder = visibleCap + 1,
    })
  end

  local listContent
  if isEmpty then
    listContent = Roact.createElement(Card, {
      size = UDim2.new(1, 0, 0, 80),
      automaticSize = Enum.AutomaticSize.None,
      color = theme.colors.surface,
      radius = theme.radius.md,
      padding = theme.spacing.lg,
      children = {
        label = Roact.createElement("TextLabel", {
          Size = UDim2.fromScale(1, 1),
          BackgroundTransparency = 1,
          Text = "Everything is already in sync — nothing to apply.",
          TextColor3 = theme.colors.textMuted,
          Font = theme.typography.body.font,
          TextSize = theme.typography.body.size,
          TextWrapped = true,
        }),
      },
    })
  else
    listContent = Roact.createElement(ScrollList, {
      size = UDim2.fromScale(1, 1),
      itemGap = theme.spacing.sm,
      children = rows,
    })
  end

  -- Header = title + the one-click "migrate everything" shortcuts. When the
  -- user is switching between two Roblox places (the main reason this
  -- appeared) picking side-by-side per row is busywork — they just want
  -- "make the IDE match Studio" or the opposite. Buttons carry `danger`
  -- variants to make it clear both can delete on one side.
  local headerCard = Roact.createElement(Card, {
    layoutOrder = 1,
    padding = theme.spacing.lg,
    children = {
      layout = Roact.createElement("UIListLayout", {
        FillDirection = Enum.FillDirection.Vertical,
        SortOrder = Enum.SortOrder.LayoutOrder,
        Padding = UDim.new(0, theme.spacing.md),
      }),
      heading = Roact.createElement(Heading, {
        title = directionTitle(direction),
        subtitle = string.format("%d file(s) — pick a side per row, or migrate everything below", #props.entries),
        layoutOrder = 1,
      }),
      migrate = Roact.createElement("Frame", {
        Size = UDim2.new(1, 0, 0, 32),
        BackgroundTransparency = 1,
        LayoutOrder = 2,
      }, {
        layout = Roact.createElement("UIListLayout", {
          FillDirection = Enum.FillDirection.Horizontal,
          VerticalAlignment = Enum.VerticalAlignment.Center,
          -- `md` (12 px) gives the migrate buttons (which are
          -- destructive — `danger` variant) clear separation from
          -- the "Migrate all:" hint label. With `sm` (8 px) the
          -- hint and the first button read as a continuous chunk.
          Padding = UDim.new(0, theme.spacing.md),
          SortOrder = Enum.SortOrder.LayoutOrder,
        }),
        hint = Roact.createElement("TextLabel", {
          -- AutomaticSize.X sizes the label to its text width,
          -- so the row stays correct if the wording changes
          -- or the dock narrows. Was a fixed 110px that left
          -- dead space (or clipped on narrow docks).
          Size = UDim2.new(0, 0, 1, 0),
          AutomaticSize = Enum.AutomaticSize.X,
          BackgroundTransparency = 1,
          Text = "Migrate all:",
          TextColor3 = theme.colors.textMuted,
          Font = theme.typography.label.font,
          TextSize = theme.typography.label.size,
          TextXAlignment = Enum.TextXAlignment.Left,
          LayoutOrder = 1,
        }),
        -- Colour by source-side instead of by destructiveness: Studio
        -- → IDE paints in `accentStudio` (blue) because semantically
        -- "everything comes from Studio"; IDE → Studio paints in
        -- `accentIde` (orange). The earlier red-for-both variant made
        -- the two buttons look interchangeable and forced users to
        -- read the label carefully every time. The confirm step still
        -- flips to `accept` (green) so the second-click commit reads
        -- as a deliberate go-ahead distinct from the armed state.
        studioToIde = Roact.createElement(Button, {
          text = if props.pendingMigrate == "studio_to_ide"
            then `⚠ Confirm: {#props.entries} file(s) Studio → IDE`
            else "Studio → IDE",
          variant = if props.pendingMigrate == "studio_to_ide" then "accept" else "studio",
          layoutOrder = 2,
          onClick = function()
            props.onMigrateAll("studio_to_ide")
          end,
        }),
        ideToStudio = Roact.createElement(Button, {
          text = if props.pendingMigrate == "ide_to_studio"
            then `⚠ Confirm: {#props.entries} file(s) IDE → Studio`
            else "IDE → Studio",
          variant = if props.pendingMigrate == "ide_to_studio" then "accept" else "ide",
          layoutOrder = 3,
          onClick = function()
            props.onMigrateAll("ide_to_studio")
          end,
        }),
      }),
    },
  })

  -- Height = leftover after header and footer. Using UDim2.new(1, 0, 1, 0)
  -- here would request the full parent height and push the footer (Apply /
  -- Cancel) below the dock's visible area; UIFlexItem.Fill lets the sibling
  -- UIListLayout distribute remaining space to this card instead.
  local listCard = Roact.createElement(Card, {
    layoutOrder = 2,
    size = UDim2.new(1, 0, 0, 0),
    automaticSize = Enum.AutomaticSize.None,
    padding = theme.spacing.md,
    children = {
      flex = Roact.createElement("UIFlexItem", {
        FlexMode = Enum.UIFlexMode.Fill,
      }),
      content = listContent,
    },
  })

  -- Footer card carries two horizontal rows (resolve-all + Cancel/
  -- Apply). Earlier `spacing.md` outer padding + `spacing.sm` gaps
  -- made the buttons read as a tight pill group. Bumping outer to
  -- `lg` and the inner-row gaps to `md` gives each button room to
  -- breathe and reads as a deliberate action bar.
  local footerCard = Roact.createElement(Card, {
    layoutOrder = 3,
    padding = theme.spacing.lg,
    children = {
      layout = Roact.createElement("UIListLayout", {
        FillDirection = Enum.FillDirection.Vertical,
        SortOrder = Enum.SortOrder.LayoutOrder,
        Padding = UDim.new(0, theme.spacing.md),
      }),
      resolveAll = Roact.createElement("Frame", {
        Size = UDim2.new(1, 0, 0, 32),
        BackgroundTransparency = 1,
        LayoutOrder = 1,
      }, {
        layout = Roact.createElement("UIListLayout", {
          FillDirection = Enum.FillDirection.Horizontal,
          VerticalAlignment = Enum.VerticalAlignment.Center,
          Padding = UDim.new(0, theme.spacing.md),
          SortOrder = Enum.SortOrder.LayoutOrder,
        }),
        hint = Roact.createElement("TextLabel", {
          -- AutomaticSize.X — same rationale as the "Migrate
          -- all:" hint above. Pixel widths on inline labels
          -- are a brittle pattern that breaks under different
          -- DPI / theme / locale.
          Size = UDim2.new(0, 0, 1, 0),
          AutomaticSize = Enum.AutomaticSize.X,
          BackgroundTransparency = 1,
          Text = "Resolve all as:",
          TextColor3 = theme.colors.textMuted,
          Font = theme.typography.label.font,
          TextSize = theme.typography.label.size,
          TextXAlignment = Enum.TextXAlignment.Left,
          LayoutOrder = 1,
        }),
        studio = Roact.createElement(Button, {
          text = "Studio",
          variant = "ghost",
          layoutOrder = 2,
          onClick = function()
            props.onResolveAll("studio")
          end,
        }),
        ide = Roact.createElement(Button, {
          text = "IDE",
          variant = "ghost",
          layoutOrder = 3,
          onClick = function()
            props.onResolveAll("ide")
          end,
        }),
        ignore = Roact.createElement(Button, {
          text = "Ignore",
          variant = "ghost",
          layoutOrder = 4,
          onClick = function()
            props.onResolveAll("ignore")
          end,
        }),
      }),
      actions = Roact.createElement("Frame", {
        Size = UDim2.new(1, 0, 0, 36),
        BackgroundTransparency = 1,
        LayoutOrder = 2,
      }, {
        layout = Roact.createElement("UIListLayout", {
          FillDirection = Enum.FillDirection.Horizontal,
          HorizontalAlignment = Enum.HorizontalAlignment.Right,
          VerticalAlignment = Enum.VerticalAlignment.Center,
          Padding = UDim.new(0, theme.spacing.md),
          SortOrder = Enum.SortOrder.LayoutOrder,
        }),
        cancel = Roact.createElement(Button, {
          text = "Cancel",
          variant = "ghost",
          onClick = props.onCancel,
          layoutOrder = 1,
        }),
        apply = Roact.createElement(Button, {
          text = if isEmpty then "Close" else "Apply Sync",
          variant = if isEmpty then "ghost" else "accept",
          onClick = props.onApply,
          layoutOrder = 2,
        }),
      }),
    },
  })

  return Roact.createElement("Frame", {
    Size = UDim2.fromScale(1, 1),
    BackgroundColor3 = theme.colors.background,
    BorderSizePixel = 0,
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
    list = listCard,
    footer = footerCard,
  })
end

-- ─── Imperative class ─────────────────────────────────────────────────────

local BulkSyncPreview = {}
BulkSyncPreview.__index = BulkSyncPreview

type self = {
  plugin: Plugin,
  gui: DockWidgetPluginGui,
  tree: any,
  handlers: Handlers?,
  request_id: string?,
  direction: string?,
  entries: { Entry },
  -- Per-path chosen `Choice` — lazily populated from `defaultChoice` the
  -- first time the view asks. Writing here from `_pickChoice` triggers a
  -- re-render so the button group updates.
  choices: { [string]: Choice },
  visible: boolean,
  scratchScript: Script?,
  -- Two-click confirmation for the destructive "Migrate all" buttons.
  -- First click sets this to the candidate direction (`studio_to_ide` or
  -- `ide_to_studio`); the button re-renders as "Confirm: N files →".
  -- Second click within ~3s commits via onMigrateAll; clicking elsewhere
  -- resets to nil. Without this a fat-finger on Studio→IDE wipes every
  -- disk script in one accidental click — the production-corruption
  -- scenario T1.1 closes.
  pendingMigrate: string?,
  -- Auxiliary dock for the side-by-side diff viewer that "See Code"
  -- opens. Lazily created on first use; reused thereafter so opening
  -- multiple files in succession swaps content rather than spawning
  -- new windows.
  diffDock: DockWidgetPluginGui?,
  diffDockTree: any,
  -- Progressive-render cap. Roact mounts the entry list synchronously,
  -- so a 800-row preview creates ~8000 Instance.new() calls in one
  -- burst — Studio freezes for seconds and the dock never visually
  -- appears (the user sees the "interface não aparece" symptom).
  -- We cap the visible row count, open the dock instantly with the
  -- first chunk, and bump the cap every frame in a coroutine until
  -- everything is shown. Each bump triggers `_rerender` which lets
  -- Roact's reconciler add the new rows incrementally without
  -- redoing the ones already mounted.
  visibleEntryCount: number,
  -- Generation counter for the progressive-render coroutine. Bumped
  -- on every `open()` and `close()` so a stale task aborts itself
  -- when state moves on.
  renderGen: number,
}

export type BulkSyncPreview = typeof(setmetatable({} :: self, BulkSyncPreview))

-- ─── ScriptEditorService scratch (imperative) ────────────────────────────

local function ensureScratch(self: BulkSyncPreview): Script
  local existing = self.scratchScript
  if existing ~= nil and existing.Parent ~= nil then
    return existing
  end
  local scratch = Instance.new("Script")
  scratch.Name = "__yeet_sync_preview_scratch__"
  scratch.Archivable = false
  scratch.Parent = ServerStorage
  self.scratchScript = scratch
  return scratch
end

local function closeScratch(self: BulkSyncPreview)
  local scratch = self.scratchScript
  if scratch == nil then
    return
  end
  pcall(function()
    ScriptEditorService:CloseScriptDocumentAsync(scratch)
  end)
  scratch:Destroy()
  self.scratchScript = nil
end

-- Lazily creates the auxiliary diff dock. Two-pane DiffView lives inside,
-- one entry at a time. Re-used across "See Code" clicks so the user
-- doesn't accumulate stale dock windows.
local function ensureDiffDock(self: BulkSyncPreview): DockWidgetPluginGui
  local existing = self.diffDock
  if existing ~= nil then
    return existing
  end
  local info = DockWidgetPluginGuiInfo.new(
    Enum.InitialDockState.Float,
    false, -- start disabled, we Enable on first openDiffDock
    true, -- override enabled-restore so the dock doesn't auto-pop on plugin reload
    720,
    480,
    520,
    320
  )
  -- Per-VM unique ID (see top-of-file rationale on the main WIDGET_ID).
  local diffWidgetId = string.format("YeetSyncPreviewDiff_%s", game:GetService("HttpService"):GenerateGUID(false))
  local gui = self.plugin:CreateDockWidgetPluginGui(diffWidgetId, info)
  gui.Title = "Yeet — Diff v0.2.0"
  gui.Name = "YeetSyncPreviewDiff"
  gui.Enabled = false
  self.diffDock = gui
  return gui
end

local function closeDiffDock(self: BulkSyncPreview)
  local tree = self.diffDockTree
  if tree ~= nil then
    Roact.unmount(tree)
    self.diffDockTree = nil
  end
  local gui = self.diffDock
  if gui ~= nil then
    gui.Enabled = false
  end
end

-- Opens (or refreshes) the side-by-side diff dock for `entry`. For
-- Modified entries we render Studio-vs-IDE. For Studio-only or IDE-only
-- entries one column is empty (matching the "added" / "removed" colour
-- in DiffView), which still tells the user which side has the file.
local function openDiffDock(self: BulkSyncPreview, entry: Entry)
  local gui = ensureDiffDock(self)
  gui.Title = string.format("Yeet — Diff: %s", entry.path)
  local studioLines = diffLines.split(entry.studio_content or "")
  local ideLines = diffLines.split(entry.fs_content or "")
  local element = Roact.createElement(DiffView, {
    left = studioLines,
    right = ideLines,
    leftLabel = "STUDIO",
    rightLabel = "IDE",
  })
  if self.diffDockTree == nil then
    self.diffDockTree = Roact.mount(element, gui, "Content")
  else
    Roact.update(self.diffDockTree, element)
  end
  gui.Enabled = true
end

-- Resolves the current choice for a path, falling back to the default keyed
-- off the entry's status. Using the entries list as the source of truth for
-- status means we never have to re-sync `choices` when the daemon sends a
-- new preview list.
local function choiceFor(self: BulkSyncPreview, entry: Entry): Choice
  return self.choices[entry.path] or defaultChoice(entry.status)
end

local function pickChoice(self: BulkSyncPreview, path: string, choice: Choice)
  self.choices[path] = choice
  self:_rerender()
end

local function pickAll(self: BulkSyncPreview, choice: Choice)
  for _, entry in self.entries do
    -- Don't set choices that are unavailable for the row's status —
    -- that would leave the button group with no selected variant.
    if choiceAvailable(entry.status, choice) then
      self.choices[entry.path] = choice
    else
      -- Fall back to the safe default so the row still has a valid
      -- selection after "Resolve all" (avoids an all-grey state).
      self.choices[entry.path] = defaultChoice(entry.status)
    end
  end
  self:_rerender()
end

-- Collapses choices into the wire-level resolutions and emits a warn line
-- for any Studio-only row marked Ignore. The warn goes to the Widget log
-- (piped to Studio output via `print` in tests) because losing Studio
-- work should be loud enough to catch in daily use.
local function collectResolutions(self: BulkSyncPreview): { Resolution }
  local out: { Resolution } = {}
  for _, entry in self.entries do
    local choice = choiceFor(self, entry)
    if entry.status == "studio_only" and choice == "ignore" then
      warn(`[yeet] bootstrap: ignoring studio-only file '{entry.path}' — it will not be mirrored to IDE`)
    end
    local action = actionFor(entry.status, choice)
    table.insert(out, { path = entry.path, action = action })
  end
  return out
end

-- "Full migration" resolutions — one side wins for EVERY row, including the
-- asymmetric `studio_only` / `ide_only` cases the per-row picker leaves
-- disabled. Used by the "Migrate all" buttons in the preview header so the
-- user can flip a whole game's state with one click when switching places.
-- Direction strings match the button labels in the header.
local function computeMigration(self: BulkSyncPreview, direction: "studio_to_ide" | "ide_to_studio"): { Resolution }
  local out: { Resolution } = {}
  for _, entry in self.entries do
    local action: Action
    if direction == "studio_to_ide" then
      -- Studio is the source of truth end-to-end:
      -- modified → Studio overwrites IDE;
      -- studio_only → create on disk;
      -- ide_only → IDE has a file Studio doesn't want → drop it.
      if entry.status == "modified" then
        action = "keep_studio"
      elseif entry.status == "studio_only" then
        action = "push_to_ide"
      elseif entry.status == "ide_only" then
        action = "delete_from_ide"
      else
        action = "skip"
      end
    else
      -- IDE wins: modified → disk overwrites Studio;
      -- studio_only → Studio has an orphan instance → delete it;
      -- ide_only → materialize in Studio.
      if entry.status == "modified" then
        action = "keep_ide"
      elseif entry.status == "studio_only" then
        action = "delete_from_studio"
      elseif entry.status == "ide_only" then
        action = "pull_to_studio"
      else
        action = "skip"
      end
    end
    table.insert(out, { path = entry.path, action = action })
  end
  return out
end

-- ─── Render bridge ────────────────────────────────────────────────────────

function BulkSyncPreview._renderTree(self: BulkSyncPreview)
  return Roact.createElement(PreviewView, {
    visible = self.visible,
    request_id = self.request_id,
    direction = self.direction,
    entries = self.entries,
    visibleEntryCount = self.visibleEntryCount,
    choices = self.choices,
    pendingMigrate = self.pendingMigrate,
    onSeeCode = function(entry)
      -- Any interaction other than the matching Migrate-all click
      -- must reset the pending arm — otherwise users could "arm" a
      -- direction, then accidentally double-click a row label and
      -- still be primed to nuke the wrong side. See pendingMigrate
      -- doc on the self struct.
      if self.pendingMigrate ~= nil then
        self.pendingMigrate = nil
        self:_rerender()
      end
      openDiffDock(self, entry)
      local handlers = self.handlers
      if handlers ~= nil then
        handlers.onSeeCode(entry)
      end
    end,
    onLineByLine = function(entry)
      -- Same pendingMigrate guard as onSeeCode — clicking Merge
      -- on a row mid-migrate-confirm should disarm.
      if self.pendingMigrate ~= nil then
        self.pendingMigrate = nil
        self:_rerender()
      end
      local handlers = self.handlers
      if handlers ~= nil then
        handlers.onLineByLine(entry)
      end
    end,
    onChoose = function(path, choice)
      if self.pendingMigrate ~= nil then
        self.pendingMigrate = nil
      end
      pickChoice(self, path, choice)
    end,
    onResolveAll = function(choice)
      if self.pendingMigrate ~= nil then
        self.pendingMigrate = nil
      end
      pickAll(self, choice)
    end,
    onMigrateAll = function(direction)
      -- Two-click confirmation. First click: arm the direction.
      -- Second click on the same direction: commit. Click on the
      -- other direction (or any cancellation surface above) flips
      -- the arming. This trades a single fat-finger nuking the
      -- entire project for a brief extra click — well worth it
      -- for production game sync.
      local handlers = self.handlers
      if handlers == nil then
        return
      end
      if self.pendingMigrate ~= direction then
        self.pendingMigrate = direction
        self:_rerender()
        return
      end
      -- Confirmed. Closing the dock here mirrors onApply so the
      -- user isn't looking at stale state while the daemon
      -- processes the wire batch.
      local resolutions = computeMigration(self, direction)
      self.handlers = nil
      self.visible = false
      self.pendingMigrate = nil
      self.gui.Enabled = false
      closeScratch(self)
      closeDiffDock(self)
      self:_rerender()
      handlers.onApply(resolutions)
    end,
    onApply = function()
      local handlers = self.handlers
      if handlers == nil then
        return
      end
      local resolutions = collectResolutions(self)
      self.handlers = nil
      self.visible = false
      self.pendingMigrate = nil
      self.gui.Enabled = false
      closeScratch(self)
      closeDiffDock(self)
      self:_rerender()
      handlers.onApply(resolutions)
    end,
    onCancel = function()
      local handlers = self.handlers
      self.handlers = nil
      self.visible = false
      self.pendingMigrate = nil
      self.gui.Enabled = false
      closeScratch(self)
      closeDiffDock(self)
      self:_rerender()
      if handlers ~= nil then
        handlers.onCancel()
      end
    end,
  })
end

function BulkSyncPreview._rerender(self: BulkSyncPreview)
  if self.tree == nil then
    return
  end
  Roact.update(self.tree, self:_renderTree())
end

-- ─── Public API ───────────────────────────────────────────────────────────

function BulkSyncPreview.new(plugin: Plugin): BulkSyncPreview
  local info = DockWidgetPluginGuiInfo.new(Enum.InitialDockState.Float, false, true, 640, 480, 480, 320)
  local gui = plugin:CreateDockWidgetPluginGui(WIDGET_ID, info)
  gui.Title = "Yeet — Sync Preview v0.2.0"
  gui.Name = WIDGET_ID
  -- Warm-up cycle: briefly enable the dock so Studio's WindowManager
  -- registers it on the active layout, then disable. Without this
  -- warm-up the FIRST `gui.Enabled = true` after construction
  -- sometimes leaves the dock invisible (Studio appears to lazily
  -- allocate the widget's window slot only on the first observed
  -- enabled-state change). Symptom: "interface não aparece no
  -- bootstrap; aparece depois de disconnect+reconnect" — the second
  -- attempt was just doing the same toggle, but by then Studio had
  -- already allocated the slot from the first toggle.
  gui.Enabled = true
  gui.Enabled = false

  local self = setmetatable({
    plugin = plugin,
    gui = gui,
    tree = nil,
    handlers = nil :: Handlers?,
    request_id = nil :: string?,
    direction = nil :: string?,
    entries = {},
    choices = {} :: { [string]: Choice },
    visible = false,
    scratchScript = nil :: Script?,
    pendingMigrate = nil :: string?,
    diffDock = nil :: DockWidgetPluginGui?,
    diffDockTree = nil,
    visibleEntryCount = 0,
    renderGen = 0,
  }, BulkSyncPreview)

  self.tree = Roact.mount(self:_renderTree(), gui, "Content")
  return self
end

function BulkSyncPreview.open(
  self: BulkSyncPreview,
  request_id: string,
  direction: string,
  entries: { Entry },
  handlers: Handlers
)
  self.request_id = request_id
  self.direction = direction
  self.entries = entries
  self.handlers = handlers
  -- Seed `choices` with the default per-row picks so the wire payload is
  -- well-formed even if the user clicks Apply without touching a row.
  local seeded: { [string]: Choice } = {}
  for _, entry in entries do
    seeded[entry.path] = defaultChoice(entry.status)
  end
  self.choices = seeded
  self.visible = true
  self.pendingMigrate = nil
  -- Cap the initial render. Without this a 800-row preview creates
  -- ~8000 Instance.new() calls inside Roact.update synchronously,
  -- which freezes Studio for several seconds and drops the WebSocket
  -- (the user reported "interface não aparece no bootstrap"). The
  -- coroutine below bumps the cap progressively so the dock paints
  -- the first 100 rows immediately and the rest stream in.
  self.visibleEntryCount = math.min(#entries, INITIAL_VISIBLE_COUNT)
  -- Render content BEFORE flipping Enabled. If the user's previous
  -- session left Enabled = true, the dock is already painted from a
  -- stale tree (probably "empty / disconnected" placeholder); if we
  -- toggle Enabled here without first updating the tree, the user
  -- briefly sees the stale content. Update the tree first so the
  -- enabled-state transition reveals the right thing.
  self:_rerender()
  -- Hard yield: force a real frame gap between off and on so Studio's
  -- WindowManager registers the state transition. `task.defer`
  -- worked in some cases but a sub-frame gap; `task.wait(0)` yields
  -- one full frame, which is what we observed actually kicks the
  -- dock visible after a stale-position cache. Spawn it so `open()`
  -- doesn't yield its caller (Applier dispatch).
  task.spawn(function()
    self.gui.Enabled = false
    task.wait()
    self.gui.Enabled = true
    print(
      string.format(
        "[Yeet] BulkSyncPreview.open: gui.Enabled=%s gui.Title=%s entries=%d",
        tostring(self.gui.Enabled),
        tostring(self.gui.Title),
        #entries
      )
    )
    -- Recheck after 1s — if Studio reverted the Enabled (cached state
    -- override, focus stealing, etc.) we surface that in Output so the
    -- user knows it's a Studio-side reject, not a plugin bug.
    task.wait(1)
    if not self.gui.Enabled then
      warn(
        string.format(
          "[Yeet] BulkSyncPreview.open: gui.Enabled REVERTED to false within 1s — "
            .. "Studio rejected the visibility request. Try the 'Show Sync' "
            .. "toolbar button or Reset Plugin Layout."
        )
      )
    end
  end)
  if #entries > self.visibleEntryCount then
    self:_progressivelyExpand()
  end
end

-- Spawned from `open` when the entry list exceeds the initial cap.
-- Bumps `visibleEntryCount` by `PROGRESSIVE_RENDER_CHUNK` every frame
-- and re-renders, letting Roact's reconciler add rows incrementally
-- without re-mounting the ones already visible. The generation
-- counter aborts the loop if `open` is called again or `close` runs.
function BulkSyncPreview._progressivelyExpand(self: BulkSyncPreview)
  self.renderGen += 1
  local myGen = self.renderGen
  task.spawn(function()
    while true do
      if self.renderGen ~= myGen then
        return
      end
      local total = #self.entries
      if self.visibleEntryCount >= total then
        return
      end
      self.visibleEntryCount = math.min(total, self.visibleEntryCount + PROGRESSIVE_RENDER_CHUNK)
      self:_rerender()
      -- One frame between bumps. Keeps the main thread responsive
      -- (mouse, scroll, the WebSocket pump) between Roact mounts.
      task.wait()
    end
  end)
end

function BulkSyncPreview.close(self: BulkSyncPreview)
  self.handlers = nil
  self.visible = false
  self.gui.Enabled = false
  -- Bump renderGen so any in-flight progressive-expand coroutine
  -- exits on its next iteration instead of resurrecting visibility.
  self.renderGen += 1
  self.visibleEntryCount = 0
  closeScratch(self)
  closeDiffDock(self)
  self:_rerender()
end

-- Drops a single entry from the preview without closing the dock.
-- Used after the host (Widget) successfully resolves an entry via the
-- line-by-line MergePicker: the manual content is already on its way to
-- the daemon via `conflict_resolved_manual`, so the subsequent "Apply
-- Sync" must NOT also send a keep_studio/keep_ide for this path (which
-- would overwrite the user's merged content). The simplest safe answer
-- is to remove the row from this preview entirely; the user sees the
-- count tick down and the merged file disappears from the list.
function BulkSyncPreview.removeEntry(self: BulkSyncPreview, path: string)
  local kept: { Entry } = {}
  for _, entry in self.entries do
    if entry.path ~= path then
      table.insert(kept, entry)
    end
  end
  self.entries = kept
  self.choices[path] = nil
  -- Trim visibleEntryCount to the new total so the progressive cap
  -- doesn't outlive the entries it was sized for.
  self.visibleEntryCount = math.min(self.visibleEntryCount, #kept)
  self:_rerender()
end

function BulkSyncPreview.destroy(self: BulkSyncPreview)
  closeScratch(self)
  closeDiffDock(self)
  -- closeDiffDock only Disables the dock; on full teardown we also need
  -- to Destroy the GUI so it doesn't leak across plugin reloads.
  if self.diffDock ~= nil then
    self.diffDock:Destroy()
    self.diffDock = nil
  end
  self.handlers = nil
  if self.tree ~= nil then
    Roact.unmount(self.tree)
    self.tree = nil
  end
  self.gui:Destroy()
end

return BulkSyncPreview
