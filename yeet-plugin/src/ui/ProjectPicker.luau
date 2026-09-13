--!strict

-- The list of daemons found by net/Discovery, one row each, shown inside the
-- Widget's connection card.
--
-- Purely presentational: scanning, selection persistence and connecting are all
-- the Widget's job. This module renders state and reports clicks.
--
-- ## Visual choices, and why
--
-- Rows sit on `theme.colors.surface`, the same step the theme reserves for
-- "interactive element on a panel" (it is what a Button's pill uses inside a
-- Card). They are deliberately NOT wrapped in `Card`: this list already lives
-- inside the connection Card, and nesting one Card in another doubles an
-- elevation the theme defines precisely so stacked surfaces separate without
-- needing strokes.
--
-- Selection reads as a filled dot in `accentPrimary`, mirroring the status dot
-- directly above it in the same panel, so "which one is live" is expressed the
-- same way twice rather than in two vocabularies.
--
-- The root path sits under the name in `textMuted` at caption size. It is the
-- only thing distinguishing two projects that share a `name`, so it cannot be
-- dropped — but it is reference material, not the label, so it recedes.

local Roact = require(script.Parent.Parent.Packages.Roact)
local theme = require(script.Parent.theme)
local Badge = require(script.Parent.components.Badge)
local Button = require(script.Parent.components.Button)
local ScrollList = require(script.Parent.components.ScrollList)

-- Locally redeclared: types do not reliably cross `require` under --!strict.
export type DaemonEntry = {
	daemonId: string,
	projectName: string,
	projectRoot: string,
	port: number,
	daemonVersion: string,
	pluginConnected: boolean,
	url: string,
}

export type Props = {
	entries: { DaemonEntry },
	selectedDaemonId: string?,
	scanning: boolean,
	-- True while a connection is live. The picker locks down rather than
	-- disappearing: hiding it would make the panel jump on every connect, and
	-- the user still wants to see which project they are attached to.
	connected: boolean,
	-- Set when every probe failed the HttpEnabled pre-flight. That is a
	-- completely different problem from "no daemons running" and needs its own
	-- instruction, so it gets its own empty state.
	httpDisabled: boolean,
	-- A manual Daemon URL from Settings, when the user has overridden it. Shown
	-- as a pinned row so the escape hatch stays visible rather than being
	-- silently shadowed by the scan.
	manualUrl: string?,
	onSelect: (daemonId: string) -> (),
	onSelectManual: (() -> ())?,
	onRefresh: () -> (),
	layoutOrder: number?,
}

-- Rows are fixed-height so the list does not reflow as names of different
-- lengths arrive during an incremental scan.
local ROW_HEIGHT = 46
-- Gap between rows. Tighter than the card's own `md` rhythm because these rows
-- are one group — spacing them like separate sections would read as four
-- unrelated things rather than one list.
local ROW_GAP = theme.spacing.tight
-- Three rows visible before scrolling: enough for the common case of two or
-- three projects without the picker dominating a 480px-tall dock.
--
-- The arithmetic has to match what the ScrollList actually lays out (N rows and
-- N-1 gaps, same `ROW_GAP`), or the visible height and the content height
-- disagree and the last row is clipped mid-way.
local VISIBLE_ROWS = 3
local LIST_MAX_HEIGHT = ROW_HEIGHT * VISIBLE_ROWS + ROW_GAP * (VISIBLE_ROWS - 1)
-- Must match `ScrollBarThickness` in components/ScrollList.
local SCROLLBAR_WIDTH = 4

local function Row(props: {
	entry: DaemonEntry,
	selected: boolean,
	enabled: boolean,
	onClick: () -> (),
	layoutOrder: number,
})
	local entry = props.entry
	local dotColor = if props.selected then theme.colors.accentPrimary else theme.colors.textMuted

	local badges: { [any]: any } = {
		__layout = Roact.createElement("UIListLayout", {
			FillDirection = Enum.FillDirection.Horizontal,
			HorizontalAlignment = Enum.HorizontalAlignment.Right,
			VerticalAlignment = Enum.VerticalAlignment.Center,
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, theme.spacing.tight),
		}),
	}
	if entry.pluginConnected then
		badges.inUse = Roact.createElement(Badge, {
			text = "in use",
			color = theme.colors.accentWarning,
			layoutOrder = 1,
		})
	end

	return Roact.createElement("TextButton", {
		Size = UDim2.new(1, 0, 0, ROW_HEIGHT),
		LayoutOrder = props.layoutOrder,
		BackgroundColor3 = if props.selected
			then theme.colors.surfaceActive
			else theme.colors.surface,
		BorderSizePixel = 0,
		AutoButtonColor = props.enabled,
		Active = props.enabled,
		Text = "",
		[Roact.Event.Activated] = function()
			if props.enabled then
				props.onClick()
			end
		end,
	}, {
		corner = Roact.createElement("UICorner", {
			CornerRadius = UDim.new(0, theme.radius.sm),
		}),
		padding = Roact.createElement("UIPadding", {
			PaddingLeft = UDim.new(0, theme.spacing.sm),
			PaddingRight = UDim.new(0, theme.spacing.sm),
			PaddingTop = UDim.new(0, theme.spacing.tight),
			PaddingBottom = UDim.new(0, theme.spacing.tight),
		}),
		layout = Roact.createElement("UIListLayout", {
			FillDirection = Enum.FillDirection.Horizontal,
			VerticalAlignment = Enum.VerticalAlignment.Center,
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, theme.spacing.sm),
		}),
		dot = Roact.createElement("Frame", {
			Size = UDim2.fromOffset(8, 8),
			BackgroundColor3 = dotColor,
			BorderSizePixel = 0,
			LayoutOrder = 1,
		}, {
			corner = Roact.createElement("UICorner", {
				CornerRadius = UDim.new(1, 0),
			}),
		}),
		-- Name over path, stacked. Takes the remaining width so the badges stay
		-- pinned right regardless of how long the project name is.
		text = Roact.createElement("Frame", {
			Size = UDim2.new(1, -110, 1, 0),
			BackgroundTransparency = 1,
			LayoutOrder = 2,
		}, {
			layout = Roact.createElement("UIListLayout", {
				FillDirection = Enum.FillDirection.Vertical,
				VerticalAlignment = Enum.VerticalAlignment.Center,
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, 1),
			}),
			name = Roact.createElement("TextLabel", {
				Size = UDim2.new(1, 0, 0, 15),
				BackgroundTransparency = 1,
				Text = entry.projectName,
				TextColor3 = if props.enabled then theme.colors.text else theme.colors.textMuted,
				Font = theme.typography.label.font,
				TextSize = theme.typography.label.size,
				TextXAlignment = Enum.TextXAlignment.Left,
				TextTruncate = Enum.TextTruncate.AtEnd,
				LayoutOrder = 1,
			}),
			root = Roact.createElement("TextLabel", {
				Size = UDim2.new(1, 0, 0, 13),
				BackgroundTransparency = 1,
				Text = entry.projectRoot,
				TextColor3 = theme.colors.textMuted,
				Font = theme.typography.caption.font,
				TextSize = theme.typography.caption.size,
				-- Right-aligned so that when a long path overflows, the visible
				-- end is the tail — the project folder, which is what tells two
				-- rows apart. The head of the path is the same boilerplate on
				-- every row of a given machine.
				TextXAlignment = Enum.TextXAlignment.Right,
				TextTruncate = Enum.TextTruncate.AtEnd,
				LayoutOrder = 2,
			}),
		}),
		badges = Roact.createElement("Frame", {
			Size = UDim2.new(0, 0, 1, 0),
			AutomaticSize = Enum.AutomaticSize.X,
			BackgroundTransparency = 1,
			LayoutOrder = 3,
		}, badges),
		port = Roact.createElement("TextLabel", {
			Size = UDim2.fromOffset(44, 0),
			BackgroundTransparency = 1,
			Text = ":" .. tostring(entry.port),
			TextColor3 = theme.colors.textMuted,
			Font = theme.typography.code.font,
			TextSize = theme.typography.code.size,
			TextXAlignment = Enum.TextXAlignment.Right,
			LayoutOrder = 4,
		}),
	})
end

local function EmptyState(props: { scanning: boolean, httpDisabled: boolean })
	local message: string
	if props.scanning then
		message = "Scanning for daemons…"
	elseif props.httpDisabled then
		-- A different problem entirely from "none running", and it has a
		-- concrete fix the user can act on, so it gets its own text.
		message = "HTTP requests are disabled in this place.\n"
			.. "File → Game Settings → Security → Allow HTTP Requests."
	else
		message = "No Yeet daemons found.\n"
			.. "Open your project in your editor and run Yeet: Start, then Refresh.\n"
			.. "Using a custom port? Set Daemon URL in Settings."
	end
	return Roact.createElement("TextLabel", {
		Size = UDim2.new(1, 0, 0, 0),
		AutomaticSize = Enum.AutomaticSize.Y,
		BackgroundTransparency = 1,
		Text = message,
		TextColor3 = theme.colors.textMuted,
		Font = theme.typography.caption.font,
		TextSize = theme.typography.caption.size,
		TextXAlignment = Enum.TextXAlignment.Left,
		TextYAlignment = Enum.TextYAlignment.Top,
		TextWrapped = true,
		LayoutOrder = 2,
	})
end

local function ProjectPicker(props: Props)
	local entries = props.entries
	-- Rows are inert while connected: switching project mid-session would mean
	-- tearing down sync under the user without them asking.
	local enabled = not props.connected

	local children: { [any]: any } = {
		header = Roact.createElement("Frame", {
			Size = UDim2.new(1, 0, 0, 20),
			BackgroundTransparency = 1,
			LayoutOrder = 1,
		}, {
			layout = Roact.createElement("UIListLayout", {
				FillDirection = Enum.FillDirection.Horizontal,
				VerticalAlignment = Enum.VerticalAlignment.Center,
				SortOrder = Enum.SortOrder.LayoutOrder,
			}),
			label = Roact.createElement("TextLabel", {
				Size = UDim2.new(1, -80, 1, 0),
				BackgroundTransparency = 1,
				Text = if props.scanning then "Project (scanning…)" else "Project",
				TextColor3 = theme.colors.textMuted,
				Font = theme.typography.label.font,
				TextSize = theme.typography.label.size,
				TextXAlignment = Enum.TextXAlignment.Left,
				LayoutOrder = 1,
			}),
			refresh = Roact.createElement(Button, {
				text = if props.scanning then "Scanning" else "Refresh",
				variant = "ghost",
				size = UDim2.fromOffset(80, 20),
				disabled = props.scanning or props.connected,
				onClick = props.onRefresh,
				layoutOrder = 2,
			}),
		}),
	}

	local rows: { [any]: any } = {}
	local order = 0

	-- The manual override is pinned above the scan results. A user who set a
	-- Daemon URL by hand did so for a reason — most likely a daemon outside the
	-- scanned window — and the scan must not bury it.
	if props.manualUrl ~= nil and props.manualUrl ~= "" then
		order += 1
		rows.manual = Roact.createElement(Row, {
			entry = {
				daemonId = "",
				projectName = "Manual (from Settings)",
				projectRoot = props.manualUrl,
				port = 0,
				daemonVersion = "",
				pluginConnected = false,
				url = props.manualUrl,
			},
			selected = props.selectedDaemonId == nil or props.selectedDaemonId == "",
			enabled = enabled,
			onClick = function()
				local cb = props.onSelectManual
				if cb ~= nil then
					cb()
				end
			end,
			layoutOrder = order,
		})
	end

	for _, entry in entries do
		order += 1
		rows["d" .. entry.daemonId] = Roact.createElement(Row, {
			entry = entry,
			selected = props.selectedDaemonId == entry.daemonId,
			enabled = enabled,
			onClick = function()
				props.onSelect(entry.daemonId)
			end,
			layoutOrder = order,
		})
	end

	if order == 0 then
		children.empty = Roact.createElement(EmptyState, {
			scanning = props.scanning,
			httpDisabled = props.httpDisabled,
		})
	else
		-- N rows and N-1 gaps — the trailing gap the naive `order * (row + gap)`
		-- adds would leave a dead strip under the last row, and with a scrolling
		-- list that strip reads as "there is more below" when there is not.
		local contentHeight = order * ROW_HEIGHT + math.max(0, order - 1) * ROW_GAP
		local scrolls = contentHeight > LIST_MAX_HEIGHT
		children.list = Roact.createElement(ScrollList, {
			-- Give back the scrollbar's width only when the list actually
			-- scrolls. ScrollList's 4px bar is drawn over the content, so
			-- without this the port number on the right sits under it — and
			-- reserving the gutter unconditionally would leave every short list
			-- visibly misaligned against the header above it.
			size = UDim2.new(1, if scrolls then -(SCROLLBAR_WIDTH + 2) else 0, 0,
				math.min(LIST_MAX_HEIGHT, contentHeight)),
			itemGap = ROW_GAP,
			layoutOrder = 2,
			children = rows,
		})
	end

	-- The layout goes alongside the header/list/empty children, not around a
	-- nested wrapper: `children` already carries them all keyed, so one frame
	-- with one layout is the whole structure.
	children.__layout = Roact.createElement("UIListLayout", {
		FillDirection = Enum.FillDirection.Vertical,
		SortOrder = Enum.SortOrder.LayoutOrder,
		Padding = UDim.new(0, theme.spacing.sm),
	})

	-- `AutomaticSize.Y` on a container whose child is a fixed-height
	-- ScrollingFrame is the combination that works: the list states its height
	-- in pixels, this frame measures it through the UIListLayout, and the
	-- enclosing Card (itself AutomaticSize.Y) measures this. What would break is
	-- a SCALE height here — a percentage of a parent that is itself sizing to
	-- its content is circular, and Roblox resolves it inconsistently. That is
	-- also why the list's own height is computed in pixels above rather than
	-- left to AutomaticSize: a ScrollingFrame that auto-sizes to its canvas
	-- never scrolls.
	return Roact.createElement("Frame", {
		Size = UDim2.new(1, 0, 0, 0),
		AutomaticSize = Enum.AutomaticSize.Y,
		BackgroundTransparency = 1,
		LayoutOrder = props.layoutOrder,
	}, children)
end

return ProjectPicker
