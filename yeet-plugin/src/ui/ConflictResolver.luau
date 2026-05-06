--!strict

-- Three-pane conflict resolver, Roact edition.
--
-- Visual layout matches the shared design system (dark cards, pill
-- buttons, colour-coded badges). The resolver's logic — per-hunk choice
-- tracking, preview recomputation, queue advance, ScriptEditorService
-- scratch for manual edit — stays imperative because its lifecycle
-- outlasts any single render. Every hand-off between imperative and
-- Roact is via `_rerender()`.

local ScriptEditorService = game:GetService("ScriptEditorService")
local ServerStorage = game:GetService("ServerStorage")
local TextService = game:GetService("TextService")

local Roact = require(script.Parent.Parent.Packages.Roact)
local theme = require(script.Parent.theme)
local Card = require(script.Parent.components.Card)
local Button = require(script.Parent.components.Button)
local Badge = require(script.Parent.components.Badge)
local Heading = require(script.Parent.components.Heading)
local ScrollList = require(script.Parent.components.ScrollList)
local DiffView = require(script.Parent.components.DiffView)
local diffLines = require(script.Parent.components.diffLines)

export type ScriptKind = "module_script" | "script" | "local_script"

export type ConflictHunk = {
	id: string,
	base_range: { number },
	studio_range: { number },
	fs_range: { number },
	studio_text: string,
	fs_text: string,
	context_before: string,
	context_after: string,
}

export type ConflictKindTag = "edit" | "delete_vs_edit" | "create_vs_create"

export type ConflictKind = {
	kind: ConflictKindTag,
	deleted: ("studio" | "fs")?,
}

export type FileConflictView = {
	path: string,
	script_kind: ScriptKind,
	conflict_kind: ConflictKind,
	base_content: string?,
	studio_content: string?,
	fs_content: string?,
	hunks: { ConflictHunk },
}

export type HunkChoice = "keep_studio" | "keep_fs" | "keep_both" | "manual"

export type WireResolution = {
	choice: HunkChoice,
	text: string?,
}

export type FileResolution = {
	path: string,
	hunks: { [string]: WireResolution },
}

export type Handlers = {
	onResolve: (FileResolution) -> (),
	onCancel: () -> (),
	-- Fired when the user closes the dock (or `_onCancel` runs) with N
	-- conflicts still unresolved. Receives the list of paths the daemon
	-- must keep in `pending_conflicts` for re-surface on the next sync —
	-- without this, the daemon's view (only the resolved 3 of 5) and the
	-- plugin's view (queue cleared) silently drift apart.
	onAbandon: ({ string }) -> (),
	-- Fired when the user clicks "Line-by-line…" on a conflict. Receives
	-- the conflict (with both sides' content) so the host (Widget) can
	-- open MergePicker, build the resolution there, and emit
	-- `ConflictResolvedManual` over the wire. The conflict stays in the
	-- resolver queue until the host calls `removeConflict(path)` after
	-- a successful manual apply — cancelling MergePicker leaves it
	-- intact so the per-hunk picker still works as a fallback.
	onLineByLine: (FileConflictView) -> (),
}

type HunkState = {
	choice: HunkChoice,
	manualText: string,
}

type FileState = {
	conflict: FileConflictView,
	choices: { [string]: HunkState },
}

-- Per-VM unique WIDGET_ID. See BulkSyncPreview.luau for the rationale —
-- Studio caches dock layout per WIDGET_ID across plugin reloads (which
-- happen on every project switch); reusing the ID makes the new VM's
-- toggle silently no-op against a stale slot. A fresh GUID per load
-- sidesteps the slot-attachment bug at the cost of dock-position memory.
local WIDGET_ID = string.format(
	"YeetConflicts_%s",
	game:GetService("HttpService"):GenerateGUID(false)
)

-- ─── Line-splicing helpers (unchanged from the imperative version) ───────

local function splitLines(text: string): { string }
	local out: { string } = {}
	local buf = ""
	for i = 1, #text do
		local ch = text:sub(i, i)
		if ch == "\n" then
			table.insert(out, buf)
			buf = ""
		else
			buf = buf .. ch
		end
	end
	if buf ~= "" then
		table.insert(out, buf)
	end
	return out
end

local function joinLines(lines: { string }): string
	return table.concat(lines, "\n")
end

local function spliceLines(
	baseLines: { string },
	start: number,
	stop: number,
	replacement: { string }
): { string }
	local out: { string } = {}
	for i = 1, start do
		table.insert(out, baseLines[i])
	end
	for _, line in replacement do
		table.insert(out, line)
	end
	for i = stop + 1, #baseLines do
		table.insert(out, baseLines[i])
	end
	return out
end

-- True when (start, stop) name a slice that's actually inside baseLines.
-- A daemon-side bug or wire corruption could produce ranges past EOF
-- (`stop > #baseLines`) or negative starts; without this check the splice
-- silently drops or duplicates lines, then the user clicks Apply and the
-- mangled content lands in both Studio and disk. The contract we enforce:
-- 0 <= start <= stop <= #baseLines. start may equal 0 (insert-before) or
-- equal #baseLines (insert-after); stop may equal start (pure insertion).
local function isValidHunkRange(
	baseLineCount: number,
	start: number,
	stop: number
): boolean
	if typeof(start) ~= "number" or typeof(stop) ~= "number" then
		return false
	end
	if start < 0 or stop < start or stop > baseLineCount then
		return false
	end
	return true
end

local function widestLine(text: string): number
	local widest = 0
	for line in text:gmatch("[^\n]*") do
		local size = TextService:GetTextSize(
			line,
			theme.typography.code.size,
			theme.typography.code.font,
			Vector2.new(math.huge, math.huge)
		)
		if size.X > widest then
			widest = size.X
		end
	end
	return widest
end

-- ─── Choice-driven preview (same algorithm, new colours) ──────────────────

local function computePreview(state: FileState): string
	local conflict = state.conflict
	if conflict.conflict_kind.kind == "delete_vs_edit" then
		local hunk = conflict.hunks[1]
		if hunk == nil then
			return conflict.studio_content or conflict.fs_content or ""
		end
		local choice = state.choices[hunk.id]
		local text
		if choice.choice == "keep_studio" then
			text = hunk.studio_text
		elseif choice.choice == "keep_fs" then
			text = hunk.fs_text
		elseif choice.choice == "keep_both" then
			text = (conflict.studio_content or "") .. (conflict.fs_content or "")
		else
			text = choice.manualText
		end
		if text == "" then
			return "<file will be deleted>"
		end
		return text
	end

	local base = conflict.base_content or ""
	local baseLines = splitLines(base)
	local sorted: { ConflictHunk } = table.clone(conflict.hunks)
	table.sort(sorted, function(a, b)
		return a.base_range[1] > b.base_range[1]
	end)

	local current: { string } = table.clone(baseLines)
	for _, hunk in sorted do
		local choice = state.choices[hunk.id]
		local replacement: { string }
		if choice.choice == "keep_studio" then
			replacement = splitLines(hunk.studio_text)
		elseif choice.choice == "keep_fs" then
			replacement = splitLines(hunk.fs_text)
		elseif choice.choice == "keep_both" then
			replacement = {}
			for _, line in splitLines(hunk.studio_text) do
				table.insert(replacement, line)
			end
			for _, line in splitLines(hunk.fs_text) do
				table.insert(replacement, line)
			end
		else
			replacement = splitLines(choice.manualText)
		end
		-- Validate before splicing. A malformed range from the daemon (or a
		-- protocol regression) would otherwise silently drop / duplicate
		-- content and surface as the wrong text in the preview — which the
		-- user would then click Apply on, committing the corruption. Bail
		-- out by returning the unmodified base so the caller (and the
		-- modal) sees obviously-not-merged content.
		if not isValidHunkRange(#current, hunk.base_range[1], hunk.base_range[2]) then
			warn(string.format(
				"[Yeet] invalid hunk range %s..%s for base of %d lines (path=%s, hunk_id=%s); aborting preview",
				tostring(hunk.base_range[1]),
				tostring(hunk.base_range[2]),
				#current,
				tostring(conflict.path),
				tostring(hunk.id)
			))
			return base
		end
		current = spliceLines(current, hunk.base_range[1], hunk.base_range[2], replacement)
	end
	return joinLines(current)
end

-- ─── Roact components ────────────────────────────────────────────────────

local function choiceColor(choice: HunkChoice): Color3
	if choice == "keep_studio" then
		return theme.colors.accentStudio
	elseif choice == "keep_fs" then
		return theme.colors.accentIde
	elseif choice == "keep_both" then
		return theme.colors.accentPrimary
	end
	return theme.colors.accentAccept
end

local function hunkDisplayText(
	text: string,
	context_before: string,
	context_after: string
): string
	local out = context_before .. text .. context_after
	if out == "" then
		return "(empty)"
	end
	return out
end

type PaneProps = {
	label: string,
	labelColor: Color3,
	body: string,
	layoutOrder: number,
}

local function HunkPane(props: PaneProps)
	-- `UIFlexItem.Fill` lets the parent `UIListLayout` hand out equal width to
	-- each pane regardless of dock width. The old `0.3333 - spacing.sm` math
	-- was close but didn't re-flow cleanly when the user resized the widget
	-- or when three panes shared less than ideal horizontal room.
	-- A UISizeConstraint keeps short hunks from collapsing to label-only
	-- thumbnails — the body still AutomaticSize.Ys up for long hunks, but
	-- never drops below `80px` so the diff reads as three distinct columns.
	return Roact.createElement("Frame", {
		Size = UDim2.new(1, 0, 0, 0),
		AutomaticSize = Enum.AutomaticSize.Y,
		BackgroundColor3 = theme.colors.background,
		BorderSizePixel = 0,
		LayoutOrder = props.layoutOrder,
	}, {
		flex = Roact.createElement("UIFlexItem", {
			FlexMode = Enum.UIFlexMode.Fill,
		}),
		minSize = Roact.createElement("UISizeConstraint", {
			MinSize = Vector2.new(0, 80),
		}),
		corner = Roact.createElement("UICorner", {
			CornerRadius = UDim.new(0, theme.radius.md),
		}),
		-- 3px inset colored strip at the top doubles as a side-ID so the
		-- user can tell STUDIO from IDE even without reading the label.
		accent = Roact.createElement("Frame", {
			Size = UDim2.new(1, 0, 0, 3),
			BackgroundColor3 = props.labelColor,
			BorderSizePixel = 0,
			ZIndex = 2,
		}, {
			corner = Roact.createElement("UICorner", {
				CornerRadius = UDim.new(0, theme.radius.sm),
			}),
		}),
		layout = Roact.createElement("UIListLayout", {
			FillDirection = Enum.FillDirection.Vertical,
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, theme.spacing.sm),
		}),
		padding = Roact.createElement("UIPadding", {
			PaddingTop = UDim.new(0, theme.spacing.md),
			PaddingBottom = UDim.new(0, theme.spacing.md),
			PaddingLeft = UDim.new(0, theme.spacing.md),
			PaddingRight = UDim.new(0, theme.spacing.md),
		}),
		label = Roact.createElement("TextLabel", {
			Size = UDim2.new(1, 0, 0, 16),
			BackgroundTransparency = 1,
			Text = props.label,
			TextColor3 = props.labelColor,
			Font = theme.typography.label.font,
			TextSize = theme.typography.label.size,
			TextXAlignment = Enum.TextXAlignment.Left,
			LayoutOrder = 1,
		}),
		body = Roact.createElement("TextLabel", {
			Size = UDim2.new(1, 0, 0, 0),
			AutomaticSize = Enum.AutomaticSize.Y,
			BackgroundTransparency = 1,
			Text = props.body,
			TextColor3 = theme.colors.text,
			Font = theme.typography.code.font,
			TextSize = theme.typography.code.size + 1,
			TextXAlignment = Enum.TextXAlignment.Left,
			TextYAlignment = Enum.TextYAlignment.Top,
			TextWrapped = true,
			LayoutOrder = 2,
		}),
	})
end

type HunkRowProps = {
	hunk: ConflictHunk,
	baseSlice: string,
	conflict: FileConflictView,
	choice: HunkChoice,
	layoutOrder: number,
	onChoose: (HunkChoice) -> (),
}

local function HunkRow(props: HunkRowProps)
	local hunk = props.hunk
	local border = choiceColor(props.choice)
	local choiceButton = function(label: string, choice: HunkChoice, order: number)
		local isActive = props.choice == choice
		local variant: "primary" | "ghost" | "accept" | "danger"
		if isActive then
			if choice == "keep_studio" or choice == "keep_fs" then
				variant = "primary"
			elseif choice == "keep_both" then
				variant = "primary"
			else
				variant = "accept"
			end
		else
			variant = "ghost"
		end
		return Roact.createElement(Button, {
			text = label,
			variant = variant,
			layoutOrder = order,
			onClick = function()
				props.onChoose(choice)
			end,
		})
	end

	return Roact.createElement("Frame", {
		Size = UDim2.new(1, 0, 0, 0),
		AutomaticSize = Enum.AutomaticSize.Y,
		BackgroundColor3 = theme.colors.card,
		BorderSizePixel = 0,
		LayoutOrder = props.layoutOrder,
	}, {
		corner = Roact.createElement("UICorner", {
			CornerRadius = UDim.new(0, theme.radius.lg),
		}),
		stroke = Roact.createElement("UIStroke", {
			ApplyStrokeMode = Enum.ApplyStrokeMode.Border,
			Thickness = 2,
			Color = border,
		}),
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
		panes = Roact.createElement("Frame", {
			Size = UDim2.new(1, 0, 0, 0),
			AutomaticSize = Enum.AutomaticSize.Y,
			BackgroundTransparency = 1,
			LayoutOrder = 1,
		}, {
			layout = Roact.createElement("UIListLayout", {
				FillDirection = Enum.FillDirection.Horizontal,
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, theme.spacing.sm),
			}),
			studio = Roact.createElement(HunkPane, {
				label = "STUDIO",
				labelColor = theme.colors.accentStudio,
				body = hunkDisplayText(hunk.studio_text, hunk.context_before, hunk.context_after),
				layoutOrder = 1,
			}),
			base = Roact.createElement(HunkPane, {
				label = "BASE",
				labelColor = theme.colors.textMuted,
				body = hunkDisplayText(props.baseSlice, hunk.context_before, hunk.context_after),
				layoutOrder = 2,
			}),
			ide = Roact.createElement(HunkPane, {
				label = "IDE",
				labelColor = theme.colors.accentIde,
				body = hunkDisplayText(hunk.fs_text, hunk.context_before, hunk.context_after),
				layoutOrder = 3,
			}),
		}),
		buttons = Roact.createElement("Frame", {
			Size = UDim2.new(1, 0, 0, 32),
			BackgroundTransparency = 1,
			LayoutOrder = 2,
		}, {
			layout = Roact.createElement("UIListLayout", {
				FillDirection = Enum.FillDirection.Horizontal,
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, theme.spacing.sm),
				VerticalAlignment = Enum.VerticalAlignment.Center,
			}),
			studio = choiceButton("Keep Studio", "keep_studio", 1),
			ide = choiceButton("Keep IDE", "keep_fs", 2),
			both = choiceButton("Keep Both", "keep_both", 3),
			manual = choiceButton("Edit Manually", "manual", 4),
		}),
	})
end

type ViewProps = {
	visible: boolean,
	queue: { FileState },
	current: number,
	onChoose: (hunkId: string, choice: HunkChoice) -> (),
	onApply: () -> (),
	onCancel: () -> (),
	onResolveAll: (HunkChoice) -> (),
	onPrev: () -> (),
	onNext: () -> (),
	onLineByLine: () -> (),
}

local function ConflictView(props: ViewProps)
	if not props.visible or props.current < 1 or props.queue[props.current] == nil then
		return Roact.createElement("Frame", {
			Size = UDim2.fromScale(1, 1),
			BackgroundColor3 = theme.colors.background,
			BorderSizePixel = 0,
		})
	end

	local state = props.queue[props.current]
	local conflict = state.conflict
	local baseLines = splitLines(conflict.base_content or "")

	local kindLabel
	local kindTag = conflict.conflict_kind.kind
	if kindTag == "edit" then
		kindLabel = "edit conflict"
	elseif kindTag == "delete_vs_edit" then
		local deleted = conflict.conflict_kind.deleted
		kindLabel = if deleted == "studio"
			then "Studio deleted; IDE edited"
			else "IDE deleted; Studio edited"
	else
		kindLabel = "both sides created it with different content"
	end

	local hunkChildren: { [string]: any } = {}
	for index, hunk in conflict.hunks do
		local baseSliceLines: { string } = {}
		for i = hunk.base_range[1] + 1, hunk.base_range[2] do
			table.insert(baseSliceLines, baseLines[i] or "")
		end
		local baseSlice = joinLines(baseSliceLines)
		if baseSlice == "" then
			baseSlice = "(empty)"
		end
		hunkChildren["hunk_" .. index] = Roact.createElement(HunkRow, {
			hunk = hunk,
			baseSlice = baseSlice,
			conflict = conflict,
			choice = state.choices[hunk.id].choice,
			layoutOrder = index,
			onChoose = function(choice)
				props.onChoose(hunk.id, choice)
			end,
		})
	end

	local previewText = computePreview(state)
	local previewWidth = math.max(widestLine(previewText) + 12, 200)

	local headerCard = Roact.createElement(Card, {
		layoutOrder = 1,
		padding = theme.spacing.lg,
		children = {
			layout = Roact.createElement("UIListLayout", {
				FillDirection = Enum.FillDirection.Vertical,
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, theme.spacing.md),
			}),
			top = Roact.createElement("Frame", {
				Size = UDim2.new(1, 0, 0, 28),
				BackgroundTransparency = 1,
				LayoutOrder = 1,
			}, {
				layout = Roact.createElement("UIListLayout", {
					FillDirection = Enum.FillDirection.Horizontal,
					VerticalAlignment = Enum.VerticalAlignment.Center,
					Padding = UDim.new(0, theme.spacing.sm),
					SortOrder = Enum.SortOrder.LayoutOrder,
				}),
				prev = Roact.createElement(Button, {
					text = "‹",
					variant = "ghost",
					layoutOrder = 1,
					disabled = props.current <= 1,
					onClick = props.onPrev,
				}),
				next_ = Roact.createElement(Button, {
					text = "›",
					variant = "ghost",
					layoutOrder = 2,
					disabled = props.current >= #props.queue,
					onClick = props.onNext,
				}),
				title = Roact.createElement("TextLabel", {
					Size = UDim2.new(1, -130, 1, 0),
					BackgroundTransparency = 1,
					Text = string.format("File %d / %d", props.current, #props.queue),
					TextColor3 = theme.colors.textMuted,
					Font = theme.typography.caption.font,
					TextSize = theme.typography.caption.size,
					TextXAlignment = Enum.TextXAlignment.Left,
					LayoutOrder = 3,
				}),
			}),
			heading = Roact.createElement(Heading, {
				title = conflict.path,
				subtitle = string.format("%s · %d hunk(s)", kindLabel, #conflict.hunks),
				layoutOrder = 2,
			}),
			legend = Roact.createElement("Frame", {
				Size = UDim2.new(1, 0, 0, 22),
				BackgroundTransparency = 1,
				LayoutOrder = 3,
			}, {
				layout = Roact.createElement("UIListLayout", {
					FillDirection = Enum.FillDirection.Horizontal,
					Padding = UDim.new(0, theme.spacing.sm),
					SortOrder = Enum.SortOrder.LayoutOrder,
				}),
				studio = Roact.createElement(Badge, {
					text = "Studio",
					color = theme.colors.accentStudio,
					layoutOrder = 1,
				}),
				ide = Roact.createElement(Badge, {
					text = "IDE",
					color = theme.colors.accentIde,
					layoutOrder = 2,
				}),
				both = Roact.createElement(Badge, {
					text = "Both",
					color = theme.colors.accentPrimary,
					layoutOrder = 3,
				}),
				manual = Roact.createElement(Badge, {
					text = "Manual",
					color = theme.colors.accentAccept,
					layoutOrder = 4,
				}),
			}),
		},
	})

	-- Height = whatever is left after header, preview, and footer. Asking for
	-- UDim2.new(1, 0, 1, 0) would blow past the parent and push Apply /
	-- Cancel off-screen; UIFlexItem.Fill hands the leftover to this card.
	local hunksCard = Roact.createElement(Card, {
		layoutOrder = 2,
		size = UDim2.new(1, 0, 0, 0),
		automaticSize = Enum.AutomaticSize.None,
		padding = theme.spacing.md,
		children = {
			flex = Roact.createElement("UIFlexItem", {
				FlexMode = Enum.UIFlexMode.Fill,
			}),
			scroll = Roact.createElement(ScrollList, {
				size = UDim2.fromScale(1, 1),
				itemGap = theme.spacing.md,
				children = hunkChildren,
			}),
		},
	})

	local previewCard = Roact.createElement(Card, {
		layoutOrder = 3,
		size = UDim2.new(1, 0, 0, 180),
		automaticSize = Enum.AutomaticSize.None,
		padding = theme.spacing.md,
		children = {
			layout = Roact.createElement("UIListLayout", {
				FillDirection = Enum.FillDirection.Vertical,
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, theme.spacing.sm),
			}),
			label = Roact.createElement("TextLabel", {
				Size = UDim2.new(1, 0, 0, 14),
				BackgroundTransparency = 1,
				Text = "Preview (what will be written)",
				TextColor3 = theme.colors.textMuted,
				Font = theme.typography.label.font,
				TextSize = theme.typography.label.size,
				TextXAlignment = Enum.TextXAlignment.Left,
				LayoutOrder = 1,
			}),
			body = Roact.createElement("ScrollingFrame", {
				Size = UDim2.new(1, 0, 1, -24),
				BackgroundColor3 = theme.colors.background,
				BorderSizePixel = 0,
				ScrollBarThickness = 4,
				ScrollBarImageColor3 = theme.colors.textMuted,
				CanvasSize = UDim2.new(),
				AutomaticCanvasSize = Enum.AutomaticSize.XY,
				LayoutOrder = 2,
			}, {
				corner = Roact.createElement("UICorner", {
					CornerRadius = UDim.new(0, theme.radius.md),
				}),
				padding = Roact.createElement("UIPadding", {
					PaddingTop = UDim.new(0, theme.spacing.sm),
					PaddingBottom = UDim.new(0, theme.spacing.sm),
					PaddingLeft = UDim.new(0, theme.spacing.md),
					PaddingRight = UDim.new(0, theme.spacing.md),
				}),
				text = Roact.createElement("TextLabel", {
					Size = UDim2.fromOffset(previewWidth, 0),
					AutomaticSize = Enum.AutomaticSize.Y,
					BackgroundTransparency = 1,
					Text = previewText,
					TextColor3 = theme.colors.text,
					Font = theme.typography.code.font,
					TextSize = theme.typography.code.size,
					TextXAlignment = Enum.TextXAlignment.Left,
					TextYAlignment = Enum.TextYAlignment.Top,
				}),
			}),
		},
	})

	local footerCard = Roact.createElement(Card, {
		layoutOrder = 4,
		padding = theme.spacing.md,
		children = {
			layout = Roact.createElement("UIListLayout", {
				FillDirection = Enum.FillDirection.Vertical,
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, theme.spacing.sm),
			}),
			resolveAll = Roact.createElement("Frame", {
				Size = UDim2.new(1, 0, 0, 32),
				BackgroundTransparency = 1,
				LayoutOrder = 1,
			}, {
				layout = Roact.createElement("UIListLayout", {
					FillDirection = Enum.FillDirection.Horizontal,
					SortOrder = Enum.SortOrder.LayoutOrder,
					Padding = UDim.new(0, theme.spacing.sm),
					VerticalAlignment = Enum.VerticalAlignment.Center,
				}),
				hint = Roact.createElement("TextLabel", {
					Size = UDim2.new(0, 130, 1, 0),
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
						props.onResolveAll("keep_studio")
					end,
				}),
				ide = Roact.createElement(Button, {
					text = "IDE",
					variant = "ghost",
					layoutOrder = 3,
					onClick = function()
						props.onResolveAll("keep_fs")
					end,
				}),
				both = Roact.createElement(Button, {
					text = "Both",
					variant = "ghost",
					layoutOrder = 4,
					onClick = function()
						props.onResolveAll("keep_both")
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
					SortOrder = Enum.SortOrder.LayoutOrder,
					Padding = UDim.new(0, theme.spacing.sm),
				}),
				cancel = Roact.createElement(Button, {
					text = "Cancel",
					variant = "danger",
					layoutOrder = 1,
					onClick = props.onCancel,
				}),
				lineByLine = Roact.createElement(Button, {
					-- Opens the per-line merge picker for the current
					-- conflict. Per-hunk picks above are still the
					-- one-click default; line-by-line is for edits the
					-- per-hunk choices can't express.
					text = "Line-by-line…",
					variant = "ghost",
					layoutOrder = 2,
					onClick = props.onLineByLine,
				}),
				apply = Roact.createElement(Button, {
					text = "Apply Resolution",
					variant = "accept",
					layoutOrder = 3,
					onClick = props.onApply,
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
		hunks = hunksCard,
		preview = previewCard,
		footer = footerCard,
	})
end

-- ─── Imperative class ────────────────────────────────────────────────────

local ConflictResolver = {}
ConflictResolver.__index = ConflictResolver

type self = {
	plugin: Plugin,
	gui: DockWidgetPluginGui,
	tree: any,
	queue: { FileState },
	current: number,
	handlers: Handlers?,
	scratchScript: Script?,
	editingHunkId: string?,
	scratchSourceConn: RBXScriptConnection?,
	-- Generation counter for the idle-timeout watchdog. Each user action
	-- bumps it; the scheduled `task.delay` callback only fires the
	-- abandonment if the gen it captured matches the current gen, so
	-- earlier scheduled watchdogs become no-ops once the user does
	-- anything new.
	idleGen: number,
	-- Auxiliary read-only Studio↔IDE diff that pops alongside the
	-- scratch when the user picks "Manual" — gives them the comparison
	-- context that the scratch alone (one buffer) can't show.
	diffDock: DockWidgetPluginGui?,
	diffDockTree: any,
}

-- After this many seconds of no user interaction with the conflict dock,
-- abandon every queued conflict and close. Without this a user who walks
-- away from the keyboard with the dock open leaves the daemon waiting
-- for a resolution that never comes — meanwhile every new event echoing
-- back hits the same conflict path and gets queued behind the abandoned
-- ones. 30 minutes is generous enough to cover lunch breaks but short
-- enough that an overnight session doesn't keep the daemon hung
-- forever.
local IDLE_TIMEOUT_SEC = 30 * 60

export type ConflictResolver = typeof(setmetatable({} :: self, ConflictResolver))

-- ─── Scratch script management (unchanged behaviour) ─────────────────────

local function seedManualText(state: FileState, hunk: ConflictHunk): string
	local slot = state.choices[hunk.id]
	if slot.manualText ~= "" then
		return slot.manualText
	end
	if hunk.fs_text ~= "" then
		return hunk.fs_text
	end
	return hunk.studio_text
end

local function ensureScratch(self: ConflictResolver): Script
	local existing = self.scratchScript
	if existing ~= nil and existing.Parent ~= nil then
		return existing
	end
	local scratch = Instance.new("Script")
	scratch.Name = "__yeet_merge_scratch__"
	scratch.Archivable = false
	scratch.Parent = ServerStorage
	self.scratchScript = scratch
	return scratch
end

local function flushScratch(self: ConflictResolver)
	local scratch = self.scratchScript
	local editingId = self.editingHunkId
	if scratch == nil or editingId == nil then
		return
	end
	local state = self.queue[self.current]
	if state == nil then
		return
	end
	local slot = state.choices[editingId]
	if slot == nil then
		return
	end
	slot.manualText = scratch.Source
end

local function disconnectScratchWatcher(self: ConflictResolver)
	local conn = self.scratchSourceConn
	if conn == nil then
		return
	end
	conn:Disconnect()
	self.scratchSourceConn = nil
end

local function closeScratch(self: ConflictResolver)
	disconnectScratchWatcher(self)
	flushScratch(self)
	local scratch = self.scratchScript
	if scratch ~= nil then
		pcall(function()
			ScriptEditorService:CloseScriptDocumentAsync(scratch)
		end)
		scratch:Destroy()
	end
	self.scratchScript = nil
	self.editingHunkId = nil
end

-- Lazily creates the Studio↔IDE diff dock used as a read-only companion
-- to the manual-edit scratch. Reused across files so picking "Manual"
-- on a different conflict swaps the diff content rather than spawning
-- a new dock.
local function ensureDiffDock(self: ConflictResolver): DockWidgetPluginGui
	local existing = self.diffDock
	if existing ~= nil then
		return existing
	end
	local info = DockWidgetPluginGuiInfo.new(
		Enum.InitialDockState.Float,
		false, -- start disabled, openDiffDockForConflict Enables
		true,  -- override enabled-restore so the dock doesn't auto-pop on plugin reload
		720, 480,
		520, 320
	)
	-- Per-VM unique ID (see ConflictResolver's main WIDGET_ID rationale).
	local conflictDiffId = string.format(
		"YeetConflictDiff_%s",
		game:GetService("HttpService"):GenerateGUID(false)
	)
	local gui = self.plugin:CreateDockWidgetPluginGui(conflictDiffId, info)
	gui.Title = "Yeet — Conflict Diff v0.2.0"
	gui.Name = "YeetConflictDiff"
	gui.Enabled = false
	self.diffDock = gui
	return gui
end

local function closeDiffDock(self: ConflictResolver)
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

-- Opens / refreshes the auxiliary diff dock with `conflict`'s Studio
-- and IDE text. Always two-pane (Studio vs IDE) — base is usually
-- close to one of the sides and adds visual noise; the user has the
-- in-dock 3-pane HunkRow if they want the base view.
local function openDiffDockForConflict(self: ConflictResolver, conflict: FileConflictView)
	local gui = ensureDiffDock(self)
	gui.Title = string.format("Yeet — Conflict Diff: %s", conflict.path)
	local studioLines = diffLines.split(conflict.studio_content or "")
	local ideLines = diffLines.split(conflict.fs_content or "")
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

local function openManualEditor(self: ConflictResolver, hunkId: string)
	local state = self.queue[self.current]
	if state == nil then
		return
	end
	local hunk: ConflictHunk? = nil
	for _, candidate in state.conflict.hunks do
		if candidate.id == hunkId then
			hunk = candidate
			break
		end
	end
	if hunk == nil then
		return
	end

	if self.editingHunkId == hunkId and self.scratchScript ~= nil then
		local ok = pcall(function()
			ScriptEditorService:OpenScriptDocumentAsync(self.scratchScript)
		end)
		if ok then
			return
		end
	end

	flushScratch(self)
	disconnectScratchWatcher(self)

	local scratch = ensureScratch(self)
	scratch.Source = seedManualText(state, hunk)
	self.editingHunkId = hunkId

	self.scratchSourceConn = scratch:GetPropertyChangedSignal("Source"):Connect(function()
		self:_setChoice(hunkId, "manual", scratch.Source)
	end)

	self:_setChoice(hunkId, "manual", scratch.Source)

	-- Pop the read-only Studio↔IDE diff alongside the scratch so the
	-- user has the full comparison context while editing the merged
	-- text. Without this the manual editor was a single buffer with no
	-- reference for what to merge — exactly the UX gap the user
	-- reported in the round of audits.
	openDiffDockForConflict(self, state.conflict)

	task.spawn(function()
		local ok, err = pcall(function()
			ScriptEditorService:OpenScriptDocumentAsync(scratch)
		end)
		if not ok then
			warn(`[yeet] ScriptEditorService:OpenScriptDocumentAsync failed: {err}`)
		end
	end)
end

-- ─── State mutations ─────────────────────────────────────────────────────

-- Bumps the idle generation and arms a fresh watchdog. Each user
-- interaction (choose / apply / nav / resolveAll / enqueue) calls this so
-- the timer keeps resetting; only an actual idle window of
-- IDLE_TIMEOUT_SEC fires the abandonment. Captures `myGen` so an earlier
-- scheduled callback whose user-counterpart was already superseded
-- becomes a silent no-op when it eventually wakes.
function ConflictResolver._noteActivity(self: ConflictResolver)
	self.idleGen += 1
	local myGen = self.idleGen
	task.delay(IDLE_TIMEOUT_SEC, function()
		if self.idleGen ~= myGen then
			return -- user did something more recent; this watchdog is stale
		end
		if #self.queue == 0 then
			return -- nothing to abandon
		end
		-- Forward the queued paths to the same abandonment path the
		-- explicit cancel button uses so the daemon's pending_conflicts
		-- bookkeeping stays consistent. Keeps the audit trail unified
		-- under "abandoned" too.
		print(string.format(
			"[Yeet] conflict resolver idle for %ds; auto-abandoning %d queued conflict(s)",
			IDLE_TIMEOUT_SEC,
			#self.queue
		))
		self:_onCancel()
	end)
end

function ConflictResolver._setChoice(
	self: ConflictResolver,
	hunkId: string,
	choice: HunkChoice,
	manualText: string?
)
	local state = self.queue[self.current]
	if state == nil then
		return
	end
	local slot = state.choices[hunkId]
	if slot == nil then
		return
	end
	slot.choice = choice
	if manualText ~= nil then
		slot.manualText = manualText
	end
	self:_noteActivity()
	self:_rerender()
end

function ConflictResolver._onChoose(self: ConflictResolver, hunkId: string, choice: HunkChoice)
	if choice == "manual" then
		openManualEditor(self, hunkId)
	else
		self:_setChoice(hunkId, choice, nil)
	end
end

local function defaultChoiceFor(): HunkState
	return {
		choice = "keep_studio",
		manualText = "",
	}
end

local function makeFileState(conflict: FileConflictView): FileState
	local choices: { [string]: HunkState } = {}
	for _, hunk in conflict.hunks do
		choices[hunk.id] = defaultChoiceFor()
	end
	return {
		conflict = conflict,
		choices = choices,
	}
end

function ConflictResolver._emitCurrent(self: ConflictResolver)
	local state = self.queue[self.current]
	if state == nil then
		return
	end
	local handlers = self.handlers
	if handlers == nil then
		return
	end
	local wire: { [string]: WireResolution } = {}
	for hunkId, slot in state.choices do
		if slot.choice == "manual" then
			wire[hunkId] = { choice = "manual", text = slot.manualText }
		else
			wire[hunkId] = { choice = slot.choice }
		end
	end
	handlers.onResolve({
		path = state.conflict.path,
		hunks = wire,
	})
end

function ConflictResolver._advance(self: ConflictResolver)
	table.remove(self.queue, self.current)
	if self.current > #self.queue then
		self.current = #self.queue
	end
	if #self.queue == 0 then
		self.gui.Enabled = false
		closeScratch(self)
		closeDiffDock(self)
	elseif self.current < 1 then
		self.current = 1
	end
end

function ConflictResolver._onApply(self: ConflictResolver)
	flushScratch(self)
	self:_emitCurrent()
	self:_advance()
	self:_noteActivity()
	self:_rerender()
end

function ConflictResolver._onCancel(self: ConflictResolver)
	local handlers = self.handlers
	-- Snapshot the paths that were still queued. The daemon needs this list
	-- so its `pending_conflicts` map keeps these paths until the user comes
	-- back; otherwise we silently lose them (resolved 3, abandoned 2 ⇒
	-- daemon advances `tree_base` thinking all 5 were handled, divergence
	-- never resurfaces until another edit reintroduces it).
	local abandoned: { string } = {}
	for _, state in self.queue do
		table.insert(abandoned, state.conflict.path)
	end
	closeScratch(self)
	closeDiffDock(self)
	self.queue = {}
	self.current = 0
	self.gui.Enabled = false
	self:_rerender()
	if handlers ~= nil then
		if #abandoned > 0 then
			handlers.onAbandon(abandoned)
		end
		handlers.onCancel()
	end
end

function ConflictResolver._onResolveAll(self: ConflictResolver, choice: HunkChoice)
	flushScratch(self)
	while self.queue[self.current] ~= nil do
		local state = self.queue[self.current]
		for _, slot in state.choices do
			slot.choice = choice
		end
		self:_emitCurrent()
		table.remove(self.queue, self.current)
		if self.current > #self.queue then
			self.current = #self.queue
		end
	end
	self.current = 0
	self.gui.Enabled = false
	closeScratch(self)
	closeDiffDock(self)
	self:_noteActivity()
	self:_rerender()
end

function ConflictResolver._onPrev(self: ConflictResolver)
	if self.current > 1 then
		flushScratch(self)
		self.current -= 1
		self:_noteActivity()
		self:_rerender()
	end
end

function ConflictResolver._onNext(self: ConflictResolver)
	if self.current < #self.queue then
		flushScratch(self)
		self.current += 1
		self:_noteActivity()
		self:_rerender()
	end
end

-- Hands the current conflict to the host (typically Widget) so it can
-- open the line-by-line MergePicker. The conflict stays in our queue
-- until the host calls `removeConflict(path)` after a successful manual
-- apply — cancelling the picker leaves things untouched and the per-
-- hunk picker stays available as a fallback.
function ConflictResolver._onLineByLine(self: ConflictResolver)
	local state = self.queue[self.current]
	if state == nil then
		return
	end
	local handlers = self.handlers
	if handlers == nil then
		return
	end
	flushScratch(self)
	self:_noteActivity()
	handlers.onLineByLine(state.conflict)
end

-- Removes the queued conflict for `path`, if any, and advances the
-- viewport. Used by the host after a successful `ConflictResolvedManual`
-- so the now-resolved conflict disappears from the dock without the
-- user having to also click Apply on the per-hunk view.
function ConflictResolver.removeConflict(self: ConflictResolver, path: string)
	for i, state in self.queue do
		if state.conflict.path == path then
			table.remove(self.queue, i)
			-- Adjust the cursor so we don't index past the new end.
			if self.current > #self.queue then
				self.current = #self.queue
			end
			if #self.queue == 0 then
				self.current = 0
				self.gui.Enabled = false
				closeScratch(self)
				closeDiffDock(self)
			elseif self.current < 1 then
				self.current = 1
			end
			self:_rerender()
			return
		end
	end
end

-- ─── Render bridge ───────────────────────────────────────────────────────

function ConflictResolver._renderTree(self: ConflictResolver)
	return Roact.createElement(ConflictView, {
		visible = #self.queue > 0 and self.current >= 1,
		queue = self.queue,
		current = self.current,
		onChoose = function(hunkId, choice)
			self:_onChoose(hunkId, choice)
		end,
		onApply = function()
			self:_onApply()
		end,
		onCancel = function()
			self:_onCancel()
		end,
		onResolveAll = function(choice)
			self:_onResolveAll(choice)
		end,
		onPrev = function()
			self:_onPrev()
		end,
		onNext = function()
			self:_onNext()
		end,
		onLineByLine = function()
			self:_onLineByLine()
		end,
	})
end

function ConflictResolver._rerender(self: ConflictResolver)
	if self.tree == nil then
		return
	end
	Roact.update(self.tree, self:_renderTree())
end

-- ─── Public API ──────────────────────────────────────────────────────────

function ConflictResolver.new(plugin: Plugin): ConflictResolver
	local info = DockWidgetPluginGuiInfo.new(
		Enum.InitialDockState.Float,
		false,
		true,
		960,
		640,
		720,
		480
	)
	local gui = plugin:CreateDockWidgetPluginGui(WIDGET_ID, info)
	gui.Title = "Yeet — Conflicts v0.2.0"
	gui.Name = WIDGET_ID
	gui.Enabled = false

	local self = setmetatable({
		plugin = plugin,
		gui = gui,
		tree = nil,
		queue = {},
		current = 0,
		handlers = nil :: Handlers?,
		scratchScript = nil :: Script?,
		editingHunkId = nil :: string?,
		scratchSourceConn = nil :: RBXScriptConnection?,
		idleGen = 0,
		diffDock = nil :: DockWidgetPluginGui?,
		diffDockTree = nil,
	}, ConflictResolver)

	self.tree = Roact.mount(self:_renderTree(), gui, "Content")
	return self
end

function ConflictResolver.setHandlers(self: ConflictResolver, handlers: Handlers)
	self.handlers = handlers
end

function ConflictResolver.enqueue(self: ConflictResolver, conflicts: { FileConflictView })
	for _, conflict in conflicts do
		table.insert(self.queue, makeFileState(conflict))
	end
	if self.current < 1 and #self.queue > 0 then
		self.current = 1
	end
	self.gui.Enabled = #self.queue > 0
	if #self.queue > 0 then
		-- Arms the idle watchdog. Subsequent user actions reset it; if
		-- the dock is left untouched for IDLE_TIMEOUT_SEC the queued
		-- conflicts get auto-abandoned via the same path as the cancel
		-- button.
		self:_noteActivity()
	end
	self:_rerender()
end

function ConflictResolver.destroy(self: ConflictResolver)
	-- Same abandon-snapshot dance as `_onCancel`: a destroy on plugin reload
	-- or place close must not silently lose pending conflicts. Best-effort
	-- (handlers may already have been cleared by the caller in some teardown
	-- orderings, in which case the daemon misses the signal — but that is
	-- strictly better than today's "always lose").
	local handlers = self.handlers
	if handlers ~= nil and #self.queue > 0 then
		local abandoned: { string } = {}
		for _, state in self.queue do
			table.insert(abandoned, state.conflict.path)
		end
		handlers.onAbandon(abandoned)
	end
	closeScratch(self)
	closeDiffDock(self)
	-- closeDiffDock only Disables the dock; on full teardown we also
	-- need to Destroy the GUI so it doesn't leak across plugin reloads.
	if self.diffDock ~= nil then
		self.diffDock:Destroy()
		self.diffDock = nil
	end
	self.queue = {}
	self.current = 0
	self.handlers = nil
	if self.tree ~= nil then
		Roact.unmount(self.tree)
		self.tree = nil
	end
	self.gui:Destroy()
end

return ConflictResolver
