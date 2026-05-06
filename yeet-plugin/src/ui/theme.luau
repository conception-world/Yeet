--!strict

-- Design tokens for the Yeet plugin UI. Pulled into one place so a future
-- theme pass (light mode, high-contrast, etc.) is a single-file edit instead
-- of a grep through every component. Values track the reference mockup the
-- user approved: deep dark surfaces, generous rounded corners, monochrome
-- with targeted colour accents for status signaling.

local theme = {}

theme.colors = {
	-- Plugin window bg (behind cards). Deliberately darker than cards so
	-- stacked surfaces have visible separation without needing strokes.
	background = Color3.fromRGB(18, 18, 22),
	-- Primary card surface — the "island" used for every major region.
	card = Color3.fromRGB(30, 30, 36),
	-- Next-level surface (pill buttons, chips, nested panels). About one
	-- step lighter than `card` so buttons inside a card read as raised.
	surface = Color3.fromRGB(44, 44, 52),
	-- Hover/active state; lighter still, keeps the pill legible while
	-- pressed without shifting the overall palette.
	surfaceActive = Color3.fromRGB(60, 60, 70),
	stroke = Color3.fromRGB(52, 52, 60),

	text = Color3.fromRGB(240, 240, 245),
	textMuted = Color3.fromRGB(150, 150, 162),
	textInverse = Color3.fromRGB(22, 22, 28),

	-- Status accents. Names match usage intent, not hue — if we swap
	-- palettes later, callers don't have to change.
	accentPrimary = Color3.fromRGB(100, 130, 240),
	accentAccept = Color3.fromRGB(60, 200, 130),
	accentDecline = Color3.fromRGB(230, 80, 100),
	accentWarning = Color3.fromRGB(235, 180, 70),
	accentInfo = Color3.fromRGB(100, 170, 230),
	-- "Studio blue" and "IDE orange" used by the conflict resolver to
	-- colour-code the three panes; kept in the theme so a future redesign
	-- keeps them in sync with everything else.
	accentStudio = Color3.fromRGB(60, 130, 220),
	accentIde = Color3.fromRGB(220, 130, 60),

	-- Backgrounds for per-line diff visualisation. Picked to be subtly
	-- different from `card`/`surface` so unchanged context lines stay
	-- visually quiet while added/removed lines pop without screaming.
	diffNeutral = Color3.fromRGB(30, 30, 36),
	diffRemoved = Color3.fromRGB(80, 35, 40),
	diffAdded = Color3.fromRGB(35, 70, 45),
	-- Tinted variants of `surface` used by the MergePicker source
	-- columns to reinforce STUDIO-vs-IDE identity at the per-line
	-- level. Sits roughly 10% accent over the surface base —
	-- visible enough to distinguish at a glance even with the
	-- panel's coloured top stripe scrolled out of view, dim enough
	-- to NOT read as red/green "remove/add" semantics (which would
	-- re-introduce the "click red to add" confusion that the plain
	-- neutral surface fix solved). Keep these in lockstep with
	-- `accentStudio` / `accentIde` if those ever change hue.
	diffStudio = Color3.fromRGB(48, 52, 64),  -- very slight blue shift
	diffIde = Color3.fromRGB(56, 50, 44),     -- very slight orange shift
	-- A separate gutter colour keeps line numbers visually parented to
	-- the line content but slightly less saturated.
	diffGutter = Color3.fromRGB(48, 48, 56),
}

theme.radius = {
	sm = 6,
	md = 10,
	lg = 16,
	pill = 999, -- UICorner clamps to half the min dim, so this just means "fully round"
}

theme.spacing = {
	tight = 4,
	sm = 8,
	md = 12,
	lg = 16,
	xl = 24,
}

theme.typography = {
	title = {
		font = Enum.Font.GothamBold,
		size = 16,
	},
	subtitle = {
		font = Enum.Font.GothamMedium,
		size = 13,
	},
	body = {
		font = Enum.Font.Gotham,
		size = 13,
	},
	label = {
		font = Enum.Font.GothamMedium,
		size = 12,
	},
	caption = {
		font = Enum.Font.Gotham,
		size = 11,
	},
	code = {
		font = Enum.Font.Code,
		size = 12,
	},
}

return theme
