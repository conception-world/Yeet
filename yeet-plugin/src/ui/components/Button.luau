--!strict

-- Pill-shaped button matching the mockup's "Team Invite" / "accept" style.
-- Three variants cover everything the plugin needs today:
--   * primary — filled, call-to-action (Connect, Apply Sync)
--   * danger  — filled red, destructive (Disconnect, Cancel)
--   * ghost   — subtle surface fill, secondary actions (See Code, close)
-- Size defaults to content-hugging; callers can override `size` for rows
-- that need a fixed width.

local Roact = require(script.Parent.Parent.Parent.Packages.Roact)
local theme = require(script.Parent.Parent.theme)

export type Variant = "primary" | "danger" | "ghost" | "accept"

type Props = {
  text: string,
  onClick: () -> (),
  variant: Variant?,
  size: UDim2?,
  layoutOrder: number?,
  disabled: boolean?,
  icon: string?,
}

local function colorsFor(variant: Variant, disabled: boolean): (Color3, Color3)
  if disabled then
    return theme.colors.surface, theme.colors.textMuted
  end
  if variant == "danger" then
    return theme.colors.accentDecline, theme.colors.text
  elseif variant == "ghost" then
    return theme.colors.surface, theme.colors.text
  elseif variant == "accept" then
    return theme.colors.accentAccept, theme.colors.textInverse
  end
  return theme.colors.accentPrimary, theme.colors.text
end

local function Button(props: Props)
  local variant = props.variant or "primary"
  local disabled = props.disabled == true
  local bg, fg = colorsFor(variant, disabled)
  local label = if props.icon ~= nil then `{props.icon}  {props.text}` else props.text
  -- `ghost` was previously indistinguishable from a TextLabel in many
  -- contexts (its bg `surface` blends with the parent Card on hover,
  -- and there's no border). Add a 1px stroke so the button always
  -- has a visible outline — mirroring the "outlined button"
  -- convention in modern UI kits. Disabled buttons get a softer
  -- stroke; primary/danger/accept variants don't need one because
  -- their bg is already vivid.
  local children: { [string]: any } = {
    corner = Roact.createElement("UICorner", {
      CornerRadius = UDim.new(0, theme.radius.pill),
    }),
    padding = Roact.createElement("UIPadding", {
      PaddingLeft = UDim.new(0, theme.spacing.lg),
      PaddingRight = UDim.new(0, theme.spacing.lg),
      PaddingTop = UDim.new(0, theme.spacing.sm),
      PaddingBottom = UDim.new(0, theme.spacing.sm),
    }),
  }
  if variant == "ghost" then
    -- Stroke is what makes a `ghost` button read as a button rather
    -- than as a TextLabel — so the values are tuned a bit louder
    -- than a typical 1px border. `surfaceActive` (RGB 60,60,70) is
    -- 16 RGB lighter than the ghost bg `surface` (44,44,52), giving
    -- enough contrast against the dock's dark cards. 1.5 px is just
    -- visibly thicker than 1 px without crowding the corner radius.
    -- Disabled state stays softer (`diffGutter`) so it reads as
    -- inert rather than primary.
    children.stroke = Roact.createElement("UIStroke", {
      Color = if disabled then theme.colors.diffGutter else theme.colors.surfaceActive,
      Thickness = 1.5,
      ApplyStrokeMode = Enum.ApplyStrokeMode.Border,
    })
  end
  return Roact.createElement("TextButton", {
    Size = props.size or UDim2.new(0, 0, 0, 32),
    AutomaticSize = if props.size ~= nil then Enum.AutomaticSize.None else Enum.AutomaticSize.XY,
    LayoutOrder = props.layoutOrder,
    BackgroundColor3 = bg,
    BorderSizePixel = 0,
    AutoButtonColor = not disabled,
    Active = not disabled,
    Text = label,
    TextColor3 = fg,
    Font = theme.typography.label.font,
    TextSize = theme.typography.label.size,
    [Roact.Event.Activated] = function()
      if disabled then
        return
      end
      -- pcall the user-supplied callback so a throw inside doesn't
      -- silently kill the Roact event-handling thread. Studio
      -- swallows uncaught errors raised inside an Activated callback
      -- with no visible feedback — exactly the failure shape that
      -- hid the `plugin:GetSetting` bug for hours. Logging via warn
      -- gives a breadcrumb in the Studio Output window; the dock's
      -- Activity panel won't pick it up but the user can grep
      -- "[Yeet]" if a button stops responding.
      local ok, err = pcall(props.onClick)
      if not ok then
        warn(string.format("[Yeet] Button onClick threw: %s", tostring(err)))
      end
    end,
  }, children)
end

return Button
