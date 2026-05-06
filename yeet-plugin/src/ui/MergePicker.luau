--!strict

-- Line-level merge picker. The user picks individual lines from Studio
-- and IDE; the merged buffer assembles into the final file content
-- that's sent back via `ConflictResolvedManual`.
--
-- Earlier behaviour was "start with empty merged buffer; click each
-- line you want; reorder yourself" — technically functional but
-- impractical (200-line file = 200 clicks, no idea where each line
-- came from). The redesign:
--   1. Pre-populate the merged buffer with the LCS-aligned union of
--      both sides (every change from both sides included). For most
--      conflicts this is the right answer or one tweak away from it.
--   2. Tag each merged line with its source (Studio / IDE / both) and
--      show a colored badge so the user knows what they're keeping.
--   3. Source columns dim lines already present in merged, so the
--      user can tell what they've added without scrolling back.
--   4. Three bulk-operation buttons at the top: Reset to default,
--      Take all Studio, Take all IDE — covers the common shapes
--      without N clicks.
--
-- Lifecycle: free-standing dock owned by ConflictResolver, opened from
-- the "Line-by-line…" button. State lives only inside MergePicker.
-- Closing the dock discards in-progress merges.

local Roact = require(script.Parent.Parent.Packages.Roact)
local theme = require(script.Parent.theme)
local Card = require(script.Parent.components.Card)
local Button = require(script.Parent.components.Button)
local Heading = require(script.Parent.components.Heading)
local diffLines = require(script.Parent.components.diffLines)

export type Handlers = {
	-- Fired with the final merged content + sha when the user clicks
	-- Apply. Resolver wires this to send `ConflictResolvedManual` over
	-- the wire.
	onApply: (path: string, content: string) -> (),
	-- Fired when the user closes the dock without applying.
	onCancel: (path: string) -> (),
}

-- One row in the merged buffer. Carries provenance so the UI can
-- show which side the line came from, and so we can dim the
-- corresponding line in the source column.
type MergedLine = {
	text: string,
	source: "studio" | "ide" | "both",
	-- Original line numbers in the source columns; either may be nil
	-- depending on `source`. Used to compute the dim/included lookups.
	studioLineno: number?,
	ideLineno: number?,
}

type self = {
	plugin: Plugin,
	gui: DockWidgetPluginGui,
	tree: any,
	visible: boolean,
	-- Current conflict context. `path`, `studioLines`, `ideLines` are
	-- captured at open() time and stay constant for the picker session.
	path: string,
	studioLines: { string },
	ideLines: { string },
	-- LCS-aligned diff between studioLines and ideLines, computed once
	-- at open() and reused for every render. The hunk-visibility masks
	-- are derived from this; caching avoids re-running the O(N*M) LCS
	-- on every keystroke when the user clicks lines.
	diffRows: { DiffRow },
	-- The user's in-progress merged result. Each entry is a MergedLine
	-- with provenance. Apply concats `entry.text` for each entry with
	-- "\n" between them.
	merged: { MergedLine },
	-- True = render the full files in all three columns. False (default)
	-- = render only diff hunks + CONTEXT_LINES on either side; collapse
	-- the rest behind separators. Toggled via the header button.
	showFullFile: boolean,
	handlers: Handlers?,
}

export type MergePicker = typeof(setmetatable({} :: self, MergePicker))

local MergePicker = {}
MergePicker.__index = MergePicker

-- Per-VM unique WIDGET_ID. See BulkSyncPreview.luau for the rationale.
local WIDGET_ID = string.format(
	"YeetMergePicker_%s",
	game:GetService("HttpService"):GenerateGUID(false)
)
local LINE_HEIGHT = 18
-- Lines of unchanged context shown around each diff hunk (git's -U3
-- default). Anything outside the union of these windows is collapsed
-- behind a "── N unchanged lines hidden ──" separator. Picked 3 because
-- git/IDE muscle memory expects it; bump if users complain that they
-- need more peripheral vision before/after edits to orient themselves.
local CONTEXT_LINES = 3

-- pcall wrapper for any callback we hand to `Roact.Event.Activated`.
-- Studio swallows uncaught errors raised inside an Activated callback
-- with no visible feedback, so a buggy click handler can make a
-- button silently stop responding. Routing every Activated through
-- this helper turns those errors into Studio Output `warn` lines.
local function safeActivated(cb: () -> ()): () -> ()
	return function()
		local ok, err = pcall(cb)
		if not ok then
			warn(string.format(
				"[Yeet] MergePicker click handler threw: %s",
				tostring(err)
			))
		end
	end
end

-- ─── Default-merge helpers ───────────────────────────────────────────────

-- Produces the diff-aligned union of `studioLines` and `ideLines` as a
-- merged buffer: every change from both sides included, context lines
-- once, modified hunks emit Studio first then IDE so the user can
-- read both versions inline. This is the picker's starting state on
-- open() and the target of the "Reset to default" button.
local function defaultMergedFromAlignment(
	studioLines: { string },
	ideLines: { string }
): { MergedLine }
	local out: { MergedLine } = {}
	for _, row in diffLines.align(studioLines, ideLines) do
		if row.kind == "context" then
			-- Both sides have this line; include once tagged "both".
			-- left/right line numbers are by definition both present.
			table.insert(out, {
				text = row.left :: string,
				source = "both",
				studioLineno = row.left_lineno,
				ideLineno = row.right_lineno,
			})
		elseif row.kind == "removed" then
			-- Studio-only line — include from Studio.
			table.insert(out, {
				text = row.left :: string,
				source = "studio",
				studioLineno = row.left_lineno,
			})
		elseif row.kind == "added" then
			-- IDE-only line — include from IDE.
			table.insert(out, {
				text = row.right :: string,
				source = "ide",
				ideLineno = row.right_lineno,
			})
		else -- modified: both sides have a different line at this position
			table.insert(out, {
				text = row.left :: string,
				source = "studio",
				studioLineno = row.left_lineno,
			})
			table.insert(out, {
				text = row.right :: string,
				source = "ide",
				ideLineno = row.right_lineno,
			})
		end
	end
	return out
end

local function takeAllFromSide(
	lines: { string },
	source: "studio" | "ide"
): { MergedLine }
	local out: { MergedLine } = {}
	for i, text in lines do
		local entry: MergedLine = {
			text = text,
			source = source,
		}
		if source == "studio" then
			entry.studioLineno = i
		else
			entry.ideLineno = i
		end
		table.insert(out, entry)
	end
	return out
end

-- Returns lookup tables of which source-column line numbers currently
-- have at least one corresponding entry in the merged buffer. Used by
-- the source columns to dim lines the user has already picked.
local function buildIncludedLookups(merged: { MergedLine }): (
	{ [number]: boolean },
	{ [number]: boolean }
)
	local studioIncluded: { [number]: boolean } = {}
	local ideIncluded: { [number]: boolean } = {}
	for _, entry in merged do
		if entry.studioLineno ~= nil then
			studioIncluded[entry.studioLineno] = true
		end
		if entry.ideLineno ~= nil then
			ideIncluded[entry.ideLineno] = true
		end
	end
	return studioIncluded, ideIncluded
end

-- Local re-declaration of diffLines.DiffRow. Luau can't reach the
-- exported type through `require` (per-project convention; see memory
-- on luau_types_across_require), so we mirror the shape here. Keep
-- this in sync with components/diffLines.luau:DiffRow.
type DiffRow = {
	kind: "context" | "removed" | "added" | "modified",
	left: string?,
	right: string?,
	left_lineno: number?,
	right_lineno: number?,
}

-- ─── Hunk visibility helpers ─────────────────────────────────────────────
--
-- A "hunk" is a maximal run of non-context align() rows; the picker
-- shows each hunk plus CONTEXT_LINES of context on either side and
-- collapses everything else behind a separator. Without this, a 500-
-- line file with one 5-line change forces the user to scroll through
-- 495 unchanged lines to find the diff — making the line-by-line
-- picker effectively unusable for real-world files.
--
-- The collapse is purely a render-time filter: the merged buffer
-- still contains every line (so Apply produces a complete file). Only
-- which rows are MOUNTED into Roact changes.

-- True for any align()-row index within CONTEXT_LINES of a row whose
-- kind != "context". Used to partition rows into "show" vs "collapse".
local function computeHunkMask(
	rows: { DiffRow },
	contextLines: number
): { boolean }
	local mask: { boolean } = table.create(#rows, false)
	for i, row in rows do
		if row.kind ~= "context" then
			local lo = math.max(1, i - contextLines)
			local hi = math.min(#rows, i + contextLines)
			for j = lo, hi do
				mask[j] = true
			end
		end
	end
	return mask
end

-- Per-source-line lookups derived from the row mask: studioLineHunk[i]
-- == true iff Studio line i appears in some visible align() row. Used
-- by the source columns to decide which line buttons to mount.
local function buildLineHunkLookups(
	rows: { DiffRow },
	mask: { boolean }
): ({ [number]: boolean }, { [number]: boolean })
	local studioMask: { [number]: boolean } = {}
	local ideMask: { [number]: boolean } = {}
	for i, row in rows do
		if mask[i] then
			if row.left_lineno ~= nil then
				studioMask[row.left_lineno] = true
			end
			if row.right_lineno ~= nil then
				ideMask[row.right_lineno] = true
			end
		end
	end
	return studioMask, ideMask
end

-- A merged-buffer entry is in-hunk if it represents an edit (source =
-- studio/ide; the user explicitly added it from one side) OR it's a
-- context line whose source-line happens to fall in a hunk window.
-- Pure-context entries far from any edit get collapsed.
local function mergedEntryInHunk(
	entry: MergedLine,
	studioLineHunk: { [number]: boolean },
	ideLineHunk: { [number]: boolean }
): boolean
	if entry.source ~= "both" then
		return true
	end
	if entry.studioLineno ~= nil and studioLineHunk[entry.studioLineno] then
		return true
	end
	if entry.ideLineno ~= nil and ideLineHunk[entry.ideLineno] then
		return true
	end
	return false
end

-- A single-row "── N unchanged ──" placeholder. Sits in the same
-- ScrollingFrame's UIListLayout as the line buttons; LayoutOrder
-- positions it between the surrounding visible rows. Pure visual —
-- not clickable yet (could be expanded to "click to reveal" in the
-- future, but that's a separate roadmap item).
local function diffSeparator(
	layoutOrder: number,
	hiddenCount: number,
	fromLine: number,
	toLine: number
): any
	return Roact.createElement("Frame", {
		Size = UDim2.new(1, 0, 0, LINE_HEIGHT),
		BackgroundColor3 = theme.colors.diffGutter,
		BorderSizePixel = 0,
		LayoutOrder = layoutOrder,
	}, {
		label = Roact.createElement("TextLabel", {
			Size = UDim2.fromScale(1, 1),
			BackgroundTransparency = 1,
			Text = string.format(
				"── %d unchanged line(s) hidden (lines %d–%d) ──",
				hiddenCount, fromLine, toLine
			),
			TextColor3 = theme.colors.textMuted,
			Font = theme.typography.code.font,
			TextSize = theme.typography.code.size,
			TextXAlignment = Enum.TextXAlignment.Center,
			TextYAlignment = Enum.TextYAlignment.Center,
		}),
	})
end

-- ─── Pure view ───────────────────────────────────────────────────────────

type ViewProps = {
	visible: boolean,
	path: string,
	studioLines: { string },
	ideLines: { string },
	merged: { MergedLine },
	studioIncluded: { [number]: boolean },
	ideIncluded: { [number]: boolean },
	-- Per-source-line visibility from the hunk mask. Lines not in the
	-- lookup are collapsed behind a "── N unchanged ──" separator
	-- unless `showFullFile` is true.
	studioLineHunk: { [number]: boolean },
	ideLineHunk: { [number]: boolean },
	-- When true, every line renders regardless of the hunk mask. Toggled
	-- by the "Show all lines" button in the header. Default false:
	-- production conflicts almost always have a small change region in
	-- a much larger file, and showing 500 unchanged lines drowns the
	-- diff in noise.
	showFullFile: boolean,
	onPickStudio: (lineno: number) -> (),
	onPickIde: (lineno: number) -> (),
	onMoveMerged: (idx: number, delta: number) -> (),
	onRemoveMerged: (idx: number) -> (),
	onApply: () -> (),
	onCancel: () -> (),
	onResetDefault: () -> (),
	onTakeAllStudio: () -> (),
	onTakeAllIde: () -> (),
	onToggleShowFull: () -> (),
}

local function lineButton(
	text: string,
	bgColor: Color3,
	layoutOrder: number,
	gutterText: string,
	included: boolean,
	onClick: () -> ()
): any
	-- When the line is already included in merged, drop background to
	-- the dock's outermost shade (well below `surface`) so the line
	-- visibly recedes from the still-pickable ones. ✓ in the gutter
	-- and muted body text reinforce "this is in the result already;
	-- you don't need to act on it." Click is still wired so the user
	-- can re-add (extra copy) deliberately.
	local effectiveBg = if included
		then theme.colors.background  -- recedes; clearly distinguishable from `surface`
		else bgColor
	local gutterDisplay = if included
		then "✓ " .. gutterText
		else gutterText
	local textColor = if included then theme.colors.textMuted else theme.colors.text
	return Roact.createElement("TextButton", {
		Size = UDim2.new(1, 0, 0, LINE_HEIGHT),
		BackgroundColor3 = effectiveBg,
		BorderSizePixel = 0,
		AutoButtonColor = true,
		Text = "",
		LayoutOrder = layoutOrder,
		[Roact.Event.Activated] = safeActivated(onClick),
	}, {
		gutter = Roact.createElement("TextLabel", {
			Size = UDim2.new(0, 50, 1, 0),
			BackgroundColor3 = theme.colors.diffGutter,
			BorderSizePixel = 0,
			Text = gutterDisplay,
			TextColor3 = if included then theme.colors.accentAccept else theme.colors.textMuted,
			Font = theme.typography.code.font,
			TextSize = theme.typography.code.size,
			TextXAlignment = Enum.TextXAlignment.Right,
		}, {
			pad = Roact.createElement("UIPadding", {
				PaddingRight = UDim.new(0, 4),
			}),
		}),
		body = Roact.createElement("TextLabel", {
			Position = UDim2.new(0, 50, 0, 0),
			Size = UDim2.new(1, -50, 1, 0),
			BackgroundTransparency = 1,
			Text = text,
			TextColor3 = textColor,
			Font = theme.typography.code.font,
			TextSize = theme.typography.code.size,
			TextXAlignment = Enum.TextXAlignment.Left,
			TextYAlignment = Enum.TextYAlignment.Center,
			TextTruncate = Enum.TextTruncate.None,
		}, {
			pad = Roact.createElement("UIPadding", {
				PaddingLeft = UDim.new(0, 6),
				PaddingRight = UDim.new(0, 6),
			}),
		}),
	})
end

-- Pure scroll component for one of the three columns. Just the
-- ScrollingFrame with line buttons + collapsed-region separators —
-- the surrounding panel chrome (rounded card, accent stripe, label,
-- hint) lives in `sourcePanel` so it can be reused for the merged
-- column too. Sized to fill its parent (the wrapper inside
-- sourcePanel sets the actual dimensions via a UIFlexItem).
local function sourceColumnScroll(
	lines: { string },
	includedSet: { [number]: boolean },
	-- When non-nil AND showFullFile is false, only lines with
	-- lineInHunk[i] == true are mounted; runs of hidden lines collapse
	-- behind a single "── N unchanged ──" separator. nil falls back to
	-- the full-file render (used as a defensive default if the caller
	-- ever forgets to pass the lookup).
	lineInHunk: { [number]: boolean }?,
	showFullFile: boolean,
	bgColor: Color3,
	onClick: (lineno: number) -> ()
): any
	local children: { [string]: any } = {
		layout = Roact.createElement("UIListLayout", {
			FillDirection = Enum.FillDirection.Vertical,
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, 1),
		}),
	}
	local renderingFiltered = (lineInHunk ~= nil) and (not showFullFile)
	-- Track the current run of hidden lines so we can flush a single
	-- separator at the boundary instead of one per line. Resetting
	-- after each flush avoids accidental coalescing across two hunks.
	local hiddenStart: number? = nil
	local hiddenLast: number? = nil
	local function flushSeparator()
		if hiddenStart ~= nil and hiddenLast ~= nil then
			local count = hiddenLast - hiddenStart + 1
			children[`sep_{hiddenStart}`] = diffSeparator(
				hiddenStart,
				count,
				hiddenStart,
				hiddenLast
			)
			hiddenStart = nil
			hiddenLast = nil
		end
	end
	for i, line in lines do
		local lineno = i
		local visible = (not renderingFiltered)
			or (lineInHunk ~= nil and lineInHunk[lineno] == true)
		if not visible then
			if hiddenStart == nil then
				hiddenStart = lineno
			end
			hiddenLast = lineno
		else
			flushSeparator()
			local included = includedSet[lineno] == true
			children[`r{i}`] = lineButton(
				line,
				bgColor,
				i,
				tostring(lineno),
				included,
				function()
					onClick(lineno)
				end
			)
		end
	end
	flushSeparator()
	return Roact.createElement("ScrollingFrame", {
		-- Fills the wrapper's flex-allocated rectangle. Sizing decisions
		-- (column width, share of remaining vertical space) are owned by
		-- the parent layout, NOT this component — that's how the dock
		-- can resize without us recomputing pixel offsets here.
		Size = UDim2.fromScale(1, 1),
		BackgroundColor3 = theme.colors.diffNeutral,
		BorderSizePixel = 0,
		AutomaticCanvasSize = Enum.AutomaticSize.Y,
		CanvasSize = UDim2.new(),
		ScrollBarThickness = 6,
		ScrollBarImageColor3 = theme.colors.surface,
	}, children)
end

-- Visual chrome for a column: rounded card surface with a coloured
-- top stripe (accentStudio / accentIde / accentAccept), a small
-- heading row (label + hint), and a flex-fill scroll wrapper. Used
-- for all three columns (Studio / IDE / Merged) so they read as
-- visually parallel — same shape, different accent.
--
-- `width` is a UDim2 because the caller decides whether the panel
-- spans half the row (sources) or the full row (merged); height
-- always fills the parent via flex.
local function sourcePanel(
	label: string,
	accent: Color3,
	hint: string,
	scrollElement: any,
	width: UDim2,
	layoutOrder: number
): any
	return Roact.createElement("Frame", {
		Size = width,
		BackgroundColor3 = theme.colors.card,
		BorderSizePixel = 0,
		LayoutOrder = layoutOrder,
	}, {
		corner = Roact.createElement("UICorner", {
			CornerRadius = UDim.new(0, theme.radius.md),
		}),
		layout = Roact.createElement("UIListLayout", {
			FillDirection = Enum.FillDirection.Vertical,
			SortOrder = Enum.SortOrder.LayoutOrder,
		}),
		stripe = Roact.createElement("Frame", {
			-- 3px accent ribbon at the top — the visual cue that ties a
			-- panel to its source (Studio blue, IDE orange, accept
			-- green for the merged result). Matches the "Saelig" / Stock
			-- card stripe in the reference mockups.
			Size = UDim2.new(1, 0, 0, 3),
			BackgroundColor3 = accent,
			BorderSizePixel = 0,
			LayoutOrder = 1,
		}),
		labelRow = Roact.createElement("Frame", {
			Size = UDim2.new(1, 0, 0, 28),
			BackgroundTransparency = 1,
			LayoutOrder = 2,
		}, {
			pad = Roact.createElement("UIPadding", {
				PaddingLeft = UDim.new(0, theme.spacing.sm),
				PaddingRight = UDim.new(0, theme.spacing.sm),
			}),
			layout = Roact.createElement("UIListLayout", {
				FillDirection = Enum.FillDirection.Horizontal,
				VerticalAlignment = Enum.VerticalAlignment.Center,
				Padding = UDim.new(0, theme.spacing.sm),
				SortOrder = Enum.SortOrder.LayoutOrder,
			}),
			label = Roact.createElement("TextLabel", {
				-- AutomaticSize.X gives the label its intrinsic text
				-- width; the hint that follows takes the rest. Avoids
				-- the "fixed-pixel label width" antipattern that breaks
				-- when the dock is narrower or the font shifts.
				Size = UDim2.new(0, 0, 1, 0),
				AutomaticSize = Enum.AutomaticSize.X,
				BackgroundTransparency = 1,
				Text = label,
				TextColor3 = accent,
				Font = theme.typography.label.font,
				TextSize = theme.typography.label.size,
				TextXAlignment = Enum.TextXAlignment.Left,
				LayoutOrder = 1,
			}),
			hintLabel = Roact.createElement("TextLabel", {
				Size = UDim2.new(1, 0, 1, 0),
				BackgroundTransparency = 1,
				Text = hint,
				TextColor3 = theme.colors.textMuted,
				Font = theme.typography.caption.font,
				TextSize = theme.typography.caption.size,
				TextXAlignment = Enum.TextXAlignment.Left,
				TextTruncate = Enum.TextTruncate.AtEnd,
				LayoutOrder = 2,
			}, {
				flex = Roact.createElement("UIFlexItem", {
					FlexMode = Enum.UIFlexMode.Fill,
				}),
			}),
		}),
		scrollWrapper = Roact.createElement("Frame", {
			-- Flex-fill wrapper so the scroll grows to whatever vertical
			-- space the panel has after the stripe + label row. The
			-- scroll itself stays at fromScale(1,1) inside this wrapper.
			Size = UDim2.fromScale(1, 1),
			BackgroundTransparency = 1,
			LayoutOrder = 3,
		}, {
			flex = Roact.createElement("UIFlexItem", {
				FlexMode = Enum.UIFlexMode.Fill,
			}),
			content = scrollElement,
		}),
	})
end

local function provenanceBadge(source: "studio" | "ide" | "both"): any
	local letter, color
	if source == "studio" then
		letter = "S"
		color = theme.colors.accentStudio
	elseif source == "ide" then
		letter = "I"
		color = theme.colors.accentIde
	else
		letter = "="
		color = theme.colors.textMuted
	end
	return Roact.createElement("TextLabel", {
		Size = UDim2.new(0, 22, 1, 0),
		BackgroundColor3 = color,
		BorderSizePixel = 0,
		Text = letter,
		TextColor3 = theme.colors.text,
		Font = theme.typography.label.font,
		TextSize = theme.typography.label.size,
		TextXAlignment = Enum.TextXAlignment.Center,
		TextYAlignment = Enum.TextYAlignment.Center,
	})
end

local function mergedRow(
	entry: MergedLine,
	idx: number,
	total: number,
	onMove: (delta: number) -> (),
	onRemove: () -> ()
): any
	-- Layout: [provenance badge] [body text] [↑/↓/✕ controls]
	-- Body shifts right by 22 to leave room for the badge; controls
	-- now occupy the rightmost 120px (was 90 px) so the three
	-- buttons fit at 32 px each with 6 px gaps. Earlier 26×LINE_HEIGHT
	-- with 2 px gaps was a cramped tap target — uncomfortable for
	-- precision merging.
	return Roact.createElement("Frame", {
		Size = UDim2.new(1, 0, 0, LINE_HEIGHT + 4),
		BackgroundColor3 = theme.colors.surface,
		BorderSizePixel = 0,
		LayoutOrder = idx,
	}, {
		badge = provenanceBadge(entry.source),
		body = Roact.createElement("TextLabel", {
			Position = UDim2.new(0, 22, 0, 0),
			Size = UDim2.new(1, -142, 1, 0),
			BackgroundTransparency = 1,
			Text = entry.text,
			TextColor3 = theme.colors.text,
			Font = theme.typography.code.font,
			TextSize = theme.typography.code.size,
			TextXAlignment = Enum.TextXAlignment.Left,
			TextYAlignment = Enum.TextYAlignment.Center,
			TextTruncate = Enum.TextTruncate.None,
		}, {
			pad = Roact.createElement("UIPadding", {
				PaddingLeft = UDim.new(0, 8),
				PaddingRight = UDim.new(0, 4),
			}),
		}),
		controls = Roact.createElement("Frame", {
			Position = UDim2.new(1, -120, 0, 0),
			Size = UDim2.new(0, 120, 1, 0),
			BackgroundTransparency = 1,
		}, {
			layout = Roact.createElement("UIListLayout", {
				FillDirection = Enum.FillDirection.Horizontal,
				SortOrder = Enum.SortOrder.LayoutOrder,
				HorizontalAlignment = Enum.HorizontalAlignment.Right,
				VerticalAlignment = Enum.VerticalAlignment.Center,
				Padding = UDim.new(0, 6),
			}),
			up = Roact.createElement("TextButton", {
				Size = UDim2.new(0, 32, 0, LINE_HEIGHT + 2),
				BackgroundColor3 = if idx > 1 then theme.colors.surfaceActive else theme.colors.diffGutter,
				BorderSizePixel = 0,
				Text = "↑",
				TextColor3 = theme.colors.text,
				Font = theme.typography.label.font,
				TextSize = theme.typography.label.size,
				LayoutOrder = 1,
				Active = idx > 1,
				AutoButtonColor = idx > 1,
				[Roact.Event.Activated] = safeActivated(function()
					if idx > 1 then onMove(-1) end
				end),
			}, {
				corner = Roact.createElement("UICorner", {
					CornerRadius = UDim.new(0, theme.radius.sm),
				}),
			}),
			down = Roact.createElement("TextButton", {
				Size = UDim2.new(0, 32, 0, LINE_HEIGHT + 2),
				BackgroundColor3 = if idx < total then theme.colors.surfaceActive else theme.colors.diffGutter,
				BorderSizePixel = 0,
				Text = "↓",
				TextColor3 = theme.colors.text,
				Font = theme.typography.label.font,
				TextSize = theme.typography.label.size,
				LayoutOrder = 2,
				Active = idx < total,
				AutoButtonColor = idx < total,
				[Roact.Event.Activated] = safeActivated(function()
					if idx < total then onMove(1) end
				end),
			}, {
				corner = Roact.createElement("UICorner", {
					CornerRadius = UDim.new(0, theme.radius.sm),
				}),
			}),
			remove = Roact.createElement("TextButton", {
				Size = UDim2.new(0, 32, 0, LINE_HEIGHT + 2),
				BackgroundColor3 = theme.colors.accentDecline,
				BorderSizePixel = 0,
				Text = "✕",
				TextColor3 = theme.colors.text,
				Font = theme.typography.label.font,
				TextSize = theme.typography.label.size,
				LayoutOrder = 3,
				[Roact.Event.Activated] = safeActivated(onRemove),
			}, {
				corner = Roact.createElement("UICorner", {
					CornerRadius = UDim.new(0, theme.radius.sm),
				}),
			}),
		}),
	})
end

local function MergePickerView(props: ViewProps)
	if not props.visible then
		return Roact.createElement("Frame", {
			Size = UDim2.fromScale(1, 1),
			BackgroundColor3 = theme.colors.background,
			BorderSizePixel = 0,
		})
	end

	-- ── Sources area: two side-by-side panels (Studio | IDE). Each is
	-- a `sourcePanel` (rounded card, stripe-coloured per side, hint
	-- row, scroll). Width is 50% scale minus half the gap so the two
	-- panels share width with breathing room — fully responsive when
	-- the dock resizes.
	-- Each source column gets a faintly accent-tinted background —
	-- `diffStudio` (~10% blue over surface) for Studio,
	-- `diffIde` (~10% orange over surface) for IDE. The tints are
	-- subtle enough that the picker context "click to add"
	-- semantics aren't confused with the red/green "remove/add"
	-- diff semantics that the plain-surface bg replaced; loud
	-- enough that the column identity reads even with the panel's
	-- coloured top stripe scrolled out of view. The MERGED RESULT
	-- panel keeps `surface` since each merged row carries a
	-- coloured provenance badge that already conveys origin
	-- per-line.
	local studioScroll = sourceColumnScroll(
		props.studioLines,
		props.studioIncluded,
		props.studioLineHunk,
		props.showFullFile,
		theme.colors.diffStudio,
		props.onPickStudio
	)
	local ideScroll = sourceColumnScroll(
		props.ideLines,
		props.ideIncluded,
		props.ideLineHunk,
		props.showFullFile,
		theme.colors.diffIde,
		props.onPickIde
	)
	local panelGap = theme.spacing.sm
	local halfWidth = UDim2.new(0.5, -math.floor(panelGap / 2), 1, 0)
	local pickHint = "click a line to add to merged · ✓ = already in"
	local studioPanel = sourcePanel(
		"STUDIO",
		theme.colors.accentStudio,
		pickHint,
		studioScroll,
		halfWidth,
		1
	)
	local idePanel = sourcePanel(
		"IDE",
		theme.colors.accentIde,
		pickHint,
		ideScroll,
		halfWidth,
		2
	)
	-- Wrapper Frame for the two-panel row. Itself flex-fills so it
	-- shares vertical space with the merged area below (see the root
	-- UIListLayout further down).
	local sourcesArea = Roact.createElement("Frame", {
		Size = UDim2.fromScale(1, 1),
		BackgroundTransparency = 1,
		LayoutOrder = 2,
	}, {
		flex = Roact.createElement("UIFlexItem", {
			FlexMode = Enum.UIFlexMode.Fill,
		}),
		layout = Roact.createElement("UIListLayout", {
			FillDirection = Enum.FillDirection.Horizontal,
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, panelGap),
		}),
		studio = studioPanel,
		ide = idePanel,
	})

	-- ── Merged area: single full-width panel. Same `sourcePanel`
	-- shape as Studio/IDE so the three columns read as a family. The
	-- inner ScrollingFrame holds the merged buffer rows + collapsed
	-- separators for out-of-hunk runs.
	local mergedChildren: { [string]: any } = {
		layout = Roact.createElement("UIListLayout", {
			FillDirection = Enum.FillDirection.Vertical,
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, 2),
		}),
	}
	-- Collapse out-of-hunk "both" runs in the merged column too,
	-- mirroring what the source columns do. Entries with source !=
	-- "both" (Studio/IDE picks the user explicitly added) are always
	-- shown; pure-context "both" entries far from any edit collapse
	-- behind a separator. Tracking is keyed off the entry's source-
	-- linenos against the column hunk lookups so the visible window
	-- stays aligned across all three panels.
	local mHiddenStart: number? = nil
	local mHiddenLast: number? = nil
	local mHiddenCount = 0
	local function mFlushSeparator()
		if mHiddenStart ~= nil and mHiddenLast ~= nil then
			mergedChildren[`sep_m{mHiddenStart}`] = diffSeparator(
				mHiddenStart,
				mHiddenCount,
				mHiddenStart,
				mHiddenLast
			)
			mHiddenStart = nil
			mHiddenLast = nil
			mHiddenCount = 0
		end
	end
	for i, entry in props.merged do
		local idx = i
		local visible = props.showFullFile
			or mergedEntryInHunk(entry, props.studioLineHunk, props.ideLineHunk)
		if not visible then
			if mHiddenStart == nil then
				mHiddenStart = idx
			end
			mHiddenLast = idx
			mHiddenCount += 1
		else
			mFlushSeparator()
			mergedChildren[`m{i}`] = mergedRow(
				entry,
				idx,
				#props.merged,
				function(delta) props.onMoveMerged(idx, delta) end,
				function() props.onRemoveMerged(idx) end
			)
		end
	end
	mFlushSeparator()
	if #props.merged == 0 then
		mergedChildren.empty = Roact.createElement("TextLabel", {
			Size = UDim2.new(1, 0, 0, 28),
			BackgroundTransparency = 1,
			Text = "(merged buffer is empty — click 'Reset to default' or pick lines from above)",
			TextColor3 = theme.colors.textMuted,
			Font = theme.typography.body.font,
			TextSize = theme.typography.body.size,
			TextXAlignment = Enum.TextXAlignment.Center,
			LayoutOrder = 1,
		})
	end

	local mergedScroll = Roact.createElement("ScrollingFrame", {
		Size = UDim2.fromScale(1, 1),
		BackgroundColor3 = theme.colors.diffNeutral,
		BorderSizePixel = 0,
		AutomaticCanvasSize = Enum.AutomaticSize.Y,
		CanvasSize = UDim2.new(),
		ScrollBarThickness = 6,
		ScrollBarImageColor3 = theme.colors.surface,
	}, mergedChildren)
	local mergedHint = string.format(
		"%d line(s) · S=Studio · I=IDE · ==unchanged · ↑↓ reorder · ✕ drop",
		#props.merged
	)
	local mergedPanelEl = sourcePanel(
		"MERGED RESULT",
		theme.colors.accentAccept,
		mergedHint,
		mergedScroll,
		UDim2.fromScale(1, 1),
		1
	)
	-- Outer flex-fill wrapper so the merged panel shares vertical
	-- space with the sources area. UIFlexItem.Fill on both makes them
	-- split remaining height ~50/50; the user can resize the dock to
	-- bias either side.
	local mergedArea = Roact.createElement("Frame", {
		Size = UDim2.fromScale(1, 1),
		BackgroundTransparency = 1,
		LayoutOrder = 3,
	}, {
		flex = Roact.createElement("UIFlexItem", {
			FlexMode = Enum.UIFlexMode.Fill,
		}),
		content = mergedPanelEl,
	})

	-- ── Header card: title + path subtitle + bulk action buttons.
	-- The Heading component matches the "Saelig / Building Design
	-- systems Case Study" pattern from the reference mockups: bold
	-- title on top, muted subtitle (the file path) below.
	-- Bulk buttons stay visually uniform (ghost variant) except the
	-- show-all toggle, which switches to `accept` (green) when
	-- active — that gives its on/off state a visual cue independent
	-- of the label text, visible from across the dock.
	local pathHeaderCard = Roact.createElement(Card, {
		layoutOrder = 1,
		padding = theme.spacing.md,
		children = {
			layout = Roact.createElement("UIListLayout", {
				FillDirection = Enum.FillDirection.Vertical,
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, theme.spacing.md),
			}),
			heading = Roact.createElement(Heading, {
				title = "Line-by-line Merge",
				subtitle = props.path,
				layoutOrder = 1,
			}),
			bulkRow = Roact.createElement("Frame", {
				Size = UDim2.new(1, 0, 0, 32),
				BackgroundTransparency = 1,
				LayoutOrder = 2,
			}, {
				layout = Roact.createElement("UIListLayout", {
					FillDirection = Enum.FillDirection.Horizontal,
					SortOrder = Enum.SortOrder.LayoutOrder,
					VerticalAlignment = Enum.VerticalAlignment.Center,
					Padding = UDim.new(0, theme.spacing.sm),
				}),
				reset = Roact.createElement(Button, {
					text = "Reset to default",
					variant = "ghost",
					layoutOrder = 1,
					onClick = props.onResetDefault,
				}),
				takeStudio = Roact.createElement(Button, {
					text = "Take all Studio",
					variant = "ghost",
					layoutOrder = 2,
					onClick = props.onTakeAllStudio,
				}),
				takeIde = Roact.createElement(Button, {
					text = "Take all IDE",
					variant = "ghost",
					layoutOrder = 3,
					onClick = props.onTakeAllIde,
				}),
				-- Toggle between hunk-only view (default) and full-file
				-- view. Hunk-only is the right default for production
				-- conflicts (5-line change in a 500-line file), but the
				-- user occasionally needs to grab a line from outside
				-- the hunk window — that's what this is for. The
				-- variant flip ghost ↔ accept gives the on/off state a
				-- visual cue independent of the label text.
				toggleShowFull = Roact.createElement(Button, {
					text = if props.showFullFile then "Hide unchanged" else "Show all lines",
					variant = if props.showFullFile then "accept" else "ghost",
					layoutOrder = 4,
					onClick = props.onToggleShowFull,
				}),
			}),
		},
	})

	-- ── Footer card: Cancel + Apply, right-aligned. Lives in its
	-- own card so the action area has visible separation from the
	-- merged panel above (avoids the "everything blends" complaint).
	-- Padding bumped to `lg` (16 px) so the buttons breathe off the
	-- card edges, and inter-button gap to `md` (12 px) so Cancel
	-- and Apply don't read as a single segmented control.
	local footerCard = Roact.createElement(Card, {
		layoutOrder = 4,
		padding = theme.spacing.lg,
		children = {
			layout = Roact.createElement("UIListLayout", {
				FillDirection = Enum.FillDirection.Horizontal,
				HorizontalAlignment = Enum.HorizontalAlignment.Right,
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, theme.spacing.md),
			}),
			cancel = Roact.createElement(Button, {
				text = "Cancel",
				variant = "ghost",
				layoutOrder = 1,
				onClick = props.onCancel,
			}),
			apply = Roact.createElement(Button, {
				text = string.format("Apply (%d lines)", #props.merged),
				variant = "accept",
				layoutOrder = 2,
				onClick = props.onApply,
				disabled = #props.merged == 0,
			}),
		},
	})

	-- Root: vertical UIListLayout with 4 children. Header + footer
	-- size to their content (Card's default is AutomaticSize.Y);
	-- sources + merged each carry a UIFlexItem.Fill so they split
	-- whatever vertical space the dock has after the chrome.
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
		header = pathHeaderCard,
		sources = sourcesArea,
		merged = mergedArea,
		footer = footerCard,
	})
end

-- ─── Imperative class ────────────────────────────────────────────────────

function MergePicker.new(plugin: Plugin): MergePicker
	local info = DockWidgetPluginGuiInfo.new(
		Enum.InitialDockState.Float,
		false,  -- start disabled
		true,   -- override enabled-restore so plugin reload doesn't auto-pop
		860, 600,
		640, 400
	)
	local gui = plugin:CreateDockWidgetPluginGui(WIDGET_ID, info)
	gui.Title = "Yeet — Line-by-Line Merge v0.2.0"
	gui.Name = WIDGET_ID
	gui.Enabled = false
	local self = setmetatable({
		plugin = plugin,
		gui = gui,
		tree = nil,
		visible = false,
		path = "",
		studioLines = {},
		ideLines = {},
		diffRows = {} :: { DiffRow },
		merged = {} :: { MergedLine },
		showFullFile = false,
		handlers = nil :: Handlers?,
	}, MergePicker)
	self.tree = Roact.mount(self:_renderTree(), gui, "Content")
	return self
end

function MergePicker._rerender(self: MergePicker)
	if self.tree == nil then
		return
	end
	Roact.update(self.tree, self:_renderTree())
end

function MergePicker._renderTree(self: MergePicker)
	local studioIncluded, ideIncluded = buildIncludedLookups(self.merged)
	-- Hunk masks recomputed each render. They depend only on the
	-- (immutable) diffRows and CONTEXT_LINES, so this is cheap — but
	-- we don't memoize because the cost is dominated by Roact mount,
	-- not by these table walks. Future optimization candidate if a
	-- profile says otherwise.
	local hunkMask = computeHunkMask(self.diffRows, CONTEXT_LINES)
	local studioLineHunk, ideLineHunk = buildLineHunkLookups(self.diffRows, hunkMask)
	return Roact.createElement(MergePickerView, {
		visible = self.visible,
		path = self.path,
		studioLines = self.studioLines,
		ideLines = self.ideLines,
		merged = self.merged,
		studioIncluded = studioIncluded,
		ideIncluded = ideIncluded,
		studioLineHunk = studioLineHunk,
		ideLineHunk = ideLineHunk,
		showFullFile = self.showFullFile,
		onPickStudio = function(lineno)
			local line = self.studioLines[lineno]
			if line ~= nil then
				table.insert(self.merged, {
					text = line,
					source = "studio",
					studioLineno = lineno,
				})
				self:_rerender()
			end
		end,
		onPickIde = function(lineno)
			local line = self.ideLines[lineno]
			if line ~= nil then
				table.insert(self.merged, {
					text = line,
					source = "ide",
					ideLineno = lineno,
				})
				self:_rerender()
			end
		end,
		onMoveMerged = function(idx, delta)
			local target = idx + delta
			if target < 1 or target > #self.merged then
				return
			end
			local tmp = self.merged[idx]
			self.merged[idx] = self.merged[target]
			self.merged[target] = tmp
			self:_rerender()
		end,
		onRemoveMerged = function(idx)
			table.remove(self.merged, idx)
			self:_rerender()
		end,
		onResetDefault = function()
			self.merged = defaultMergedFromAlignment(self.studioLines, self.ideLines)
			self:_rerender()
		end,
		onTakeAllStudio = function()
			self.merged = takeAllFromSide(self.studioLines, "studio")
			self:_rerender()
		end,
		onTakeAllIde = function()
			self.merged = takeAllFromSide(self.ideLines, "ide")
			self:_rerender()
		end,
		onToggleShowFull = function()
			self.showFullFile = not self.showFullFile
			self:_rerender()
		end,
		onApply = function()
			local handlers = self.handlers
			if handlers == nil or #self.merged == 0 then
				return
			end
			-- Serialize MergedLine[] → plain text. Each entry's `text`
			-- joined with "\n". Provenance is dropped on the wire — the
			-- daemon only needs the final content.
			local plain: { string } = table.create(#self.merged)
			for _, entry in self.merged do
				table.insert(plain, entry.text)
			end
			local content = table.concat(plain, "\n")
			local path = self.path
			-- Close before firing the callback so any consequence (e.g.
			-- daemon broadcasts a FileChanged that flows back through
			-- Applier) doesn't re-render us mid-teardown.
			self.handlers = nil
			self.visible = false
			self.gui.Enabled = false
			self.merged = {}
			self:_rerender()
			handlers.onApply(path, content)
		end,
		onCancel = function()
			local handlers = self.handlers
			local path = self.path
			self.handlers = nil
			self.visible = false
			self.gui.Enabled = false
			self.merged = {}
			self:_rerender()
			if handlers ~= nil then
				handlers.onCancel(path)
			end
		end,
	})
end

-- Opens the picker for the given conflict. `studio_content` and
-- `fs_content` are split into lines via diffLines.split so empty
-- trailing lines are preserved (matches the daemon's notion of lines).
-- The merged buffer is pre-populated with the diff-aligned union of
-- the two sides — every change from both sides included as a starting
-- point. The user tweaks from there instead of starting from scratch.
function MergePicker.open(
	self: MergePicker,
	path: string,
	studio_content: string,
	fs_content: string,
	handlers: Handlers
)
	self.path = path
	self.studioLines = diffLines.split(studio_content)
	self.ideLines = diffLines.split(fs_content)
	-- Compute the LCS alignment once; defaultMergedFromAlignment
	-- and the per-render hunk masks both consume it. Recomputing on
	-- every render is wasteful for big files (LCS is O(N*M)).
	self.diffRows = diffLines.align(self.studioLines, self.ideLines)
	self.merged = defaultMergedFromAlignment(self.studioLines, self.ideLines)
	-- Default to hunk-only view on every open(). If the user toggled
	-- "Show all lines" in a previous session of the dock, that
	-- preference doesn't carry over — production conflicts almost
	-- always benefit from the focused view, and a stale "Show all"
	-- on a fresh conflict reintroduces the very scrolling problem
	-- this view was built to solve.
	self.showFullFile = false
	self.handlers = handlers
	self.visible = true
	self.gui.Enabled = true
	self.gui.Title = string.format("Yeet — Line-by-Line Merge: %s", path)
	self:_rerender()
end

function MergePicker.close(self: MergePicker)
	self.handlers = nil
	self.visible = false
	self.gui.Enabled = false
	self.merged = {}
	self:_rerender()
end

function MergePicker.destroy(self: MergePicker)
	self.handlers = nil
	if self.tree ~= nil then
		Roact.unmount(self.tree)
		self.tree = nil
	end
	self.gui:Destroy()
end

return MergePicker
