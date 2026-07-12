--!strict

-- Side-by-side line-aligned diff viewer. Renders two columns (with an
-- optional middle BASE column) where each row shows the corresponding
-- line on each side, coloured by its diff classification:
--
--   * context  — both sides match, neutral background
--   * removed  — left only, red background on the left, blank on right
--   * added    — right only, blank on left, green on the right
--   * modified — both sides have a line at this aligned position but
--                they differ; left red, right green
--
-- A small gutter on each side carries the line number (or a `·` when
-- the side has no line for this row) so a reader can correlate the diff
-- back to the original files.
--
-- This component renders one TextLabel per line per column, so file
-- sizes >~5K lines start to feel sluggish — the dock's ScrollingFrame
-- still works but the initial Roact mount cost is O(N). For the typical
-- Roblox script this is fine. Above diffLines' MAX_DIFF_LINES cap,
-- `diffLines.align` returns a sentinel (see REGRESSION(A15) in
-- diffLines.luau) and this component renders a plain-text fallback
-- (one TextLabel per side, not per line) instead of the per-line
-- render below. True virtualization of the normal per-line render is
-- still out of scope.

local Roact = require(script.Parent.Parent.Parent.Packages.Roact)
local theme = require(script.Parent.Parent.theme)
local diffLines = require(script.Parent.diffLines)

type DiffRow = diffLines.DiffRow

export type Props = {
	-- Required: the two sides to align. Pre-split into lines via
	-- `diffLines.split` so the caller controls how the source is broken
	-- (the daemon's hunks already know about newlines, so `split` matches
	-- their semantics).
	left: { string },
	right: { string },
	-- Optional column labels — defaults are "STUDIO" / "IDE" because
	-- those are the dominant callers; merge picker can override.
	leftLabel: string?,
	rightLabel: string?,
	-- Optional accent colours for the column headers. Default to the
	-- theme's accentStudio / accentIde so the diff dock visually pairs
	-- with the rest of the conflict UI.
	leftAccent: Color3?,
	rightAccent: Color3?,
	-- Optional fixed size; otherwise the view fills its parent.
	size: UDim2?,
	layoutOrder: number?,
}

-- Width of the line-number gutter inside each column. Sized for 5
-- digits + padding; bumps up to 6-digit if the file is longer than
-- 99999 lines (which never happens in Roblox scripts, but cheap to be
-- correct).
local function gutterWidth(maxLineNo: number): number
	if maxLineNo > 99999 then
		return 56
	end
	return 44
end

-- Background colour for a row given its diff kind and whether we're on
-- the left or right column. Modified rows colour both sides (red on
-- left, green on right) so the eye sees the swap; added/removed only
-- colour the side that has content.
local function rowBackground(kind: string, side: "left" | "right"): Color3
	if kind == "modified" then
		return if side == "left" then theme.colors.diffRemoved else theme.colors.diffAdded
	end
	if kind == "removed" then
		return if side == "left" then theme.colors.diffRemoved else theme.colors.diffNeutral
	end
	if kind == "added" then
		return if side == "left" then theme.colors.diffNeutral else theme.colors.diffAdded
	end
	return theme.colors.diffNeutral
end

local LINE_HEIGHT = 18

local function lineRow(
	row: DiffRow,
	side: "left" | "right",
	gutterPx: number,
	layoutOrder: number
): any
	local lineno = if side == "left" then row.left_lineno else row.right_lineno
	local text = if side == "left" then row.left else row.right
	-- For "blank" cells (removed-row right pane, added-row left pane)
	-- show an empty body so vertical alignment matches the other column.
	local body = text or ""
	local gutterText = if lineno ~= nil then tostring(lineno) else "·"
	return Roact.createElement("Frame", {
		Size = UDim2.new(1, 0, 0, LINE_HEIGHT),
		BackgroundColor3 = rowBackground(row.kind, side),
		BorderSizePixel = 0,
		LayoutOrder = layoutOrder,
	}, {
		gutter = Roact.createElement("TextLabel", {
			Size = UDim2.new(0, gutterPx, 1, 0),
			BackgroundColor3 = theme.colors.diffGutter,
			BorderSizePixel = 0,
			Text = gutterText,
			TextColor3 = theme.colors.textMuted,
			Font = theme.typography.code.font,
			TextSize = theme.typography.code.size,
			TextXAlignment = Enum.TextXAlignment.Right,
		}, {
			pad = Roact.createElement("UIPadding", {
				PaddingRight = UDim.new(0, 4),
			}),
		}),
		body = Roact.createElement("TextLabel", {
			Position = UDim2.new(0, gutterPx, 0, 0),
			Size = UDim2.new(1, -gutterPx, 1, 0),
			BackgroundTransparency = 1,
			Text = body,
			TextColor3 = theme.colors.text,
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

local function columnHeader(label: string, accent: Color3): any
	return Roact.createElement("Frame", {
		Size = UDim2.new(1, 0, 0, 22),
		BackgroundColor3 = theme.colors.surface,
		BorderSizePixel = 0,
		LayoutOrder = 0,
	}, {
		accent = Roact.createElement("Frame", {
			Size = UDim2.new(1, 0, 0, 3),
			BackgroundColor3 = accent,
			BorderSizePixel = 0,
		}),
		text = Roact.createElement("TextLabel", {
			Size = UDim2.new(1, 0, 1, 0),
			BackgroundTransparency = 1,
			Text = label,
			TextColor3 = theme.colors.text,
			Font = theme.typography.label.font,
			TextSize = theme.typography.label.size,
			TextXAlignment = Enum.TextXAlignment.Left,
		}, {
			pad = Roact.createElement("UIPadding", {
				PaddingLeft = UDim.new(0, 8),
			}),
		}),
	})
end

local function buildColumn(
	rows: { DiffRow },
	side: "left" | "right",
	label: string,
	accent: Color3,
	gutterPx: number,
	layoutOrder: number
): any
	local children: { [string]: any } = {
		layout = Roact.createElement("UIListLayout", {
			FillDirection = Enum.FillDirection.Vertical,
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, 0),
		}),
		header = columnHeader(label, accent),
	}
	for i, row in rows do
		children[`r{i}`] = lineRow(row, side, gutterPx, i)
	end
	-- Each column is a ScrollingFrame so very long files don't blow up
	-- the dock height. Synchronizing scroll between columns is a
	-- nice-to-have; today each scrolls independently.
	return Roact.createElement("ScrollingFrame", {
		Size = UDim2.new(0.5, -1, 1, 0),
		BackgroundColor3 = theme.colors.diffNeutral,
		BorderSizePixel = 0,
		LayoutOrder = layoutOrder,
		AutomaticCanvasSize = Enum.AutomaticSize.Y,
		CanvasSize = UDim2.new(),
		ScrollBarThickness = 6,
		ScrollBarImageColor3 = theme.colors.surface,
	}, children)
end

-- REGRESSION(A15) fallback column: the whole side rendered as a single
-- TextLabel (one Instance, `AutomaticSize` to fit the text) instead of
-- `buildColumn`'s one-Frame-plus-two-TextLabels-per-line. Instance
-- count is O(1) in the number of lines, so this can't reproduce the
-- freeze/OOM `buildColumn` would hit on a huge file. No diff
-- colouring or gutter — the lines aren't aligned, just shown as-is.
local function buildPlainColumn(
	lines: { string },
	label: string,
	accent: Color3,
	layoutOrder: number
): any
	return Roact.createElement("ScrollingFrame", {
		Size = UDim2.new(0.5, -1, 1, 0),
		BackgroundColor3 = theme.colors.diffNeutral,
		BorderSizePixel = 0,
		LayoutOrder = layoutOrder,
		AutomaticCanvasSize = Enum.AutomaticSize.Y,
		CanvasSize = UDim2.new(),
		ScrollBarThickness = 6,
		ScrollBarImageColor3 = theme.colors.surface,
	}, {
		layout = Roact.createElement("UIListLayout", {
			FillDirection = Enum.FillDirection.Vertical,
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, 0),
		}),
		header = columnHeader(label, accent),
		body = Roact.createElement("TextLabel", {
			Size = UDim2.new(1, 0, 0, 0),
			AutomaticSize = Enum.AutomaticSize.Y,
			BackgroundTransparency = 1,
			Text = table.concat(lines, "\n"),
			TextColor3 = theme.colors.text,
			Font = theme.typography.code.font,
			TextSize = theme.typography.code.size,
			TextXAlignment = Enum.TextXAlignment.Left,
			TextYAlignment = Enum.TextYAlignment.Top,
			TextWrapped = false,
			LayoutOrder = 1,
		}, {
			pad = Roact.createElement("UIPadding", {
				PaddingLeft = UDim.new(0, 6),
				PaddingRight = UDim.new(0, 6),
				PaddingTop = UDim.new(0, 2),
			}),
		}),
	})
end

-- Banner shown above the plain-text columns explaining why the
-- per-line diff was skipped.
local function tooLargeBanner(): any
	return Roact.createElement("TextLabel", {
		Size = UDim2.new(1, 0, 0, 22),
		BackgroundColor3 = theme.colors.surface,
		BorderSizePixel = 0,
		Text = "File too large for diff — showing plain text.",
		TextColor3 = theme.colors.textMuted,
		Font = theme.typography.label.font,
		TextSize = theme.typography.label.size,
		TextXAlignment = Enum.TextXAlignment.Left,
		LayoutOrder = 0,
	}, {
		pad = Roact.createElement("UIPadding", {
			PaddingLeft = UDim.new(0, 8),
		}),
	})
end

local function DiffView(props: Props)
	local left = props.left
	local right = props.right
	local rows = diffLines.align(left, right)
	local leftLabel = props.leftLabel or "STUDIO"
	local rightLabel = props.rightLabel or "IDE"
	local leftAccent = props.leftAccent or theme.colors.accentStudio
	local rightAccent = props.rightAccent or theme.colors.accentIde

	-- REGRESSION(A15): `align` refused to compute a real alignment for
	-- input this large (see MAX_DIFF_LINES in diffLines.luau). Render
	-- plain, un-diffed text instead of falling through to `buildColumn`
	-- below, which would mount a Frame + 2 TextLabels per line — the
	-- freeze/OOM this guard exists to prevent.
	if diffLines.isTooLarge(rows) then
		return Roact.createElement("Frame", {
			Size = props.size or UDim2.new(1, 0, 1, 0),
			BackgroundTransparency = 1,
			LayoutOrder = props.layoutOrder,
		}, {
			layout = Roact.createElement("UIListLayout", {
				FillDirection = Enum.FillDirection.Vertical,
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, 2),
			}),
			banner = tooLargeBanner(),
			columns = Roact.createElement("Frame", {
				Size = UDim2.new(1, 0, 1, -24),
				BackgroundTransparency = 1,
				LayoutOrder = 1,
			}, {
				layout = Roact.createElement("UIListLayout", {
					FillDirection = Enum.FillDirection.Horizontal,
					SortOrder = Enum.SortOrder.LayoutOrder,
					Padding = UDim.new(0, 2),
				}),
				leftCol = buildPlainColumn(left, leftLabel, leftAccent, 1),
				rightCol = buildPlainColumn(right, rightLabel, rightAccent, 2),
			}),
		})
	end

	local maxLine = math.max(#left, #right)
	local gutterPx = gutterWidth(maxLine)

	return Roact.createElement("Frame", {
		Size = props.size or UDim2.new(1, 0, 1, 0),
		BackgroundTransparency = 1,
		LayoutOrder = props.layoutOrder,
	}, {
		layout = Roact.createElement("UIListLayout", {
			FillDirection = Enum.FillDirection.Horizontal,
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, 2),
		}),
		leftCol = buildColumn(rows, "left", leftLabel, leftAccent, gutterPx, 1),
		rightCol = buildColumn(rows, "right", rightLabel, rightAccent, gutterPx, 2),
	})
end

return DiffView
