--!strict

-- Modal shown when the user clicks "Yeet to IDE". Collects the target path,
-- mode, and inclusion flags, then hands the validated options back to the
-- caller via `onConfirm`. The caller drives the sending state externally
-- through the returned handle — we deliberately keep this file ignorant of
-- the ChunkedSender so the UI can be exercised without a live daemon.
--
-- The modal renders as a sibling of the DockWidget's root frame with a
-- high ZIndex so it overlays whatever the main widget is showing. There is
-- no ScreenGui involved: inside a PluginGui, Frame children compose with
-- ZIndexBehavior.Sibling just fine for a one-layer overlay.

local ACCENT = Color3.fromRGB(50, 110, 200)
local ACCENT_HOVER = Color3.fromRGB(70, 130, 220)
local DANGER = Color3.fromRGB(200, 70, 70)
local SUCCESS = Color3.fromRGB(70, 170, 90)
local BG = Color3.fromRGB(28, 28, 28)
local PANEL = Color3.fromRGB(40, 40, 40)
local INPUT_BG = Color3.fromRGB(22, 22, 22)
local BORDER = Color3.fromRGB(60, 60, 60)
local TEXT = Color3.fromRGB(230, 230, 230)
local TEXT_DIM = Color3.fromRGB(160, 160, 160)
local TEXT_DISABLED = Color3.fromRGB(110, 110, 110)

type Mode = "new_project" | { kind: "merge_existing", overwrite: boolean }

export type Options = {
	target_path: string,
	mode: Mode,
	include_non_script: boolean,
	include_binary: boolean,
	template: "minimal",
	project_name: string?,
}

-- Keep in sync with `SyncbackStats` in protocol.rs; only fields we display
-- are typed here, the rest pass through as any.
type SyncbackStats = {
	scripts_written: number,
	non_script_instances_written: number,
	meta_files_written: number,
	total_bytes: number,
	services_included: { string },
	warnings: { string },
}

export type OpenParams = {
	parent: Instance,
	defaultTargetPath: string,
	onBrowseFolder: () -> string?,
	onConfirm: (opts: Options) -> (),
	onOpenInIde: ((path: string) -> ())?,
	onClose: () -> (),
}

export type Handle = {
	close: (self: Handle) -> (),
	setSending: (self: Handle) -> (),
	setProgress: (self: Handle, sent: number, total: number) -> (),
	setComplete: (self: Handle, stats: SyncbackStats) -> (),
	setError: (self: Handle, message: string) -> (),
}

type State = "idle" | "sending" | "complete" | "error"

local SyncbackModal = {}

local function makePadding(parent: Instance, px: number)
	local padding = Instance.new("UIPadding")
	padding.PaddingTop = UDim.new(0, px)
	padding.PaddingBottom = UDim.new(0, px)
	padding.PaddingLeft = UDim.new(0, px)
	padding.PaddingRight = UDim.new(0, px)
	padding.Parent = parent
end

local function makeLabel(parent: Instance, text: string, textSize: number): TextLabel
	local label = Instance.new("TextLabel")
	label.BackgroundTransparency = 1
	label.Size = UDim2.new(1, 0, 0, textSize + 4)
	label.Font = Enum.Font.Gotham
	label.TextSize = textSize
	label.TextColor3 = TEXT
	label.TextXAlignment = Enum.TextXAlignment.Left
	label.Text = text
	label.Parent = parent
	return label
end

local function makeButton(
	parent: Instance,
	text: string,
	background: Color3,
	hover: Color3
): TextButton
	local btn = Instance.new("TextButton")
	btn.AutoButtonColor = false
	btn.BackgroundColor3 = background
	btn.BorderSizePixel = 0
	btn.Size = UDim2.new(0, 96, 0, 28)
	btn.Font = Enum.Font.GothamMedium
	btn.TextSize = 13
	btn.TextColor3 = TEXT
	btn.Text = text
	btn.Parent = parent
	btn.MouseEnter:Connect(function()
		if btn.Active then
			btn.BackgroundColor3 = hover
		end
	end)
	btn.MouseLeave:Connect(function()
		btn.BackgroundColor3 = background
	end)
	return btn
end

local function makeTextBox(parent: Instance, placeholder: string, initial: string): TextBox
	local box = Instance.new("TextBox")
	box.BackgroundColor3 = INPUT_BG
	box.BorderColor3 = BORDER
	box.BorderSizePixel = 1
	box.Font = Enum.Font.Code
	box.TextSize = 13
	box.TextColor3 = TEXT
	box.PlaceholderText = placeholder
	box.PlaceholderColor3 = TEXT_DIM
	box.Text = initial
	box.TextXAlignment = Enum.TextXAlignment.Left
	box.ClearTextOnFocus = false
	box.Size = UDim2.new(1, 0, 0, 26)
	box.Parent = parent
	local pad = Instance.new("UIPadding")
	pad.PaddingLeft = UDim.new(0, 6)
	pad.PaddingRight = UDim.new(0, 6)
	pad.Parent = box
	return box
end

type ToggleState = {
	value: boolean,
	button: TextButton,
	setValue: (boolean) -> (),
	setEnabled: (boolean) -> (),
}

-- Roblox has no native checkbox; we fake it with a 16x16 button that draws
-- "✓" or empty. The caller only cares about `value`/`setValue`; the
-- hand-drawn look keeps us away from Decal asset IDs that expire.
local function makeCheckbox(
	parent: Instance,
	text: string,
	initial: boolean,
	onChange: (boolean) -> ()
): ToggleState
	local row = Instance.new("Frame")
	row.BackgroundTransparency = 1
	row.Size = UDim2.new(1, 0, 0, 20)
	row.Parent = parent

	local box = Instance.new("TextButton")
	box.AutoButtonColor = false
	box.BackgroundColor3 = INPUT_BG
	box.BorderColor3 = BORDER
	box.BorderSizePixel = 1
	box.Size = UDim2.fromOffset(16, 16)
	box.Position = UDim2.fromOffset(0, 2)
	box.Font = Enum.Font.GothamBold
	box.TextSize = 12
	box.TextColor3 = ACCENT
	box.Text = initial and "✓" or ""
	box.Parent = row

	local label = Instance.new("TextLabel")
	label.BackgroundTransparency = 1
	label.Position = UDim2.fromOffset(24, 0)
	label.Size = UDim2.new(1, -24, 1, 0)
	label.Font = Enum.Font.Gotham
	label.TextSize = 13
	label.TextColor3 = TEXT
	label.TextXAlignment = Enum.TextXAlignment.Left
	label.Text = text
	label.Parent = row

	local state: ToggleState
	state = {
		value = initial,
		button = box,
		setValue = function(next: boolean)
			state.value = next
			box.Text = next and "✓" or ""
		end,
		setEnabled = function(enabled: boolean)
			box.Active = enabled
			box.AutoButtonColor = false
			if enabled then
				label.TextColor3 = TEXT
				box.TextColor3 = ACCENT
			else
				label.TextColor3 = TEXT_DISABLED
				box.TextColor3 = TEXT_DISABLED
			end
		end,
	}

	box.MouseButton1Click:Connect(function()
		if not box.Active then
			return
		end
		state.setValue(not state.value)
		onChange(state.value)
	end)

	return state
end

type RadioChoice<T> = {
	key: T,
	label: string,
}

type RadioState<T> = {
	value: T,
	setValue: (T) -> (),
	setEnabled: (boolean) -> (),
}

local function makeRadioGroup<T>(
	parent: Instance,
	choices: { RadioChoice<T> },
	initial: T,
	onChange: (T) -> ()
): RadioState<T>
	local container = Instance.new("Frame")
	container.BackgroundTransparency = 1
	container.Size = UDim2.new(1, 0, 0, #choices * 22)
	container.Parent = parent

	local buttons: { TextButton } = {}
	local labels: { TextLabel } = {}
	local keys: { T } = {}

	for idx, choice in choices do
		local row = Instance.new("Frame")
		row.BackgroundTransparency = 1
		row.Size = UDim2.new(1, 0, 0, 20)
		row.Position = UDim2.fromOffset(0, (idx - 1) * 22)
		row.Parent = container

		local dot = Instance.new("TextButton")
		dot.AutoButtonColor = false
		dot.BackgroundColor3 = INPUT_BG
		dot.BorderColor3 = BORDER
		dot.BorderSizePixel = 1
		dot.Size = UDim2.fromOffset(16, 16)
		dot.Position = UDim2.fromOffset(0, 2)
		dot.Font = Enum.Font.GothamBold
		dot.TextSize = 14
		dot.TextColor3 = ACCENT
		dot.Text = choice.key == initial and "●" or ""
		dot.Parent = row

		local label = Instance.new("TextLabel")
		label.BackgroundTransparency = 1
		label.Position = UDim2.fromOffset(24, 0)
		label.Size = UDim2.new(1, -24, 1, 0)
		label.Font = Enum.Font.Gotham
		label.TextSize = 13
		label.TextColor3 = TEXT
		label.TextXAlignment = Enum.TextXAlignment.Left
		label.Text = choice.label
		label.Parent = row

		buttons[idx] = dot
		labels[idx] = label
		keys[idx] = choice.key
	end

	local state: RadioState<T>
	state = {
		value = initial,
		setValue = function(next: T)
			state.value = next
			for idx, key in keys do
				buttons[idx].Text = key == next and "●" or ""
			end
		end,
		setEnabled = function(enabled: boolean)
			for idx, btn in buttons do
				btn.Active = enabled
				btn.TextColor3 = enabled and ACCENT or TEXT_DISABLED
				labels[idx].TextColor3 = enabled and TEXT or TEXT_DISABLED
			end
		end,
	}

	for idx, btn in buttons do
		local myKey = keys[idx]
		btn.MouseButton1Click:Connect(function()
			if not btn.Active then
				return
			end
			if state.value == myKey then
				return
			end
			state.setValue(myKey)
			onChange(myKey)
		end)
	end

	return state
end

function SyncbackModal.open(params: OpenParams): Handle
	local backdrop = Instance.new("Frame")
	backdrop.Name = "YeetSyncbackBackdrop"
	backdrop.Size = UDim2.fromScale(1, 1)
	backdrop.BackgroundColor3 = Color3.new(0, 0, 0)
	backdrop.BackgroundTransparency = 0.5
	backdrop.BorderSizePixel = 0
	backdrop.ZIndex = 100
	backdrop.Parent = params.parent

	local panel = Instance.new("Frame")
	panel.Name = "Panel"
	panel.AnchorPoint = Vector2.new(0.5, 0.5)
	panel.Position = UDim2.fromScale(0.5, 0.5)
	panel.Size = UDim2.fromScale(0.95, 0.95)
	panel.BackgroundColor3 = BG
	panel.BorderSizePixel = 0
	panel.ZIndex = 101
	panel.Parent = backdrop

	local panelConstraint = Instance.new("UISizeConstraint")
	panelConstraint.MinSize = Vector2.new(260, 320)
	panelConstraint.MaxSize = Vector2.new(420, 560)
	panelConstraint.Parent = panel

	-- Header
	local header = Instance.new("Frame")
	header.BackgroundColor3 = PANEL
	header.BorderSizePixel = 0
	header.Size = UDim2.new(1, 0, 0, 32)
	header.ZIndex = 102
	header.Parent = panel

	local title = makeLabel(header, "Yeet to IDE", 14)
	title.Font = Enum.Font.GothamBold
	title.Position = UDim2.fromOffset(12, 0)
	title.Size = UDim2.new(1, -44, 1, 0)
	title.TextYAlignment = Enum.TextYAlignment.Center
	title.ZIndex = 103

	local closeBtn = Instance.new("TextButton")
	closeBtn.AutoButtonColor = false
	closeBtn.BackgroundTransparency = 1
	closeBtn.Size = UDim2.fromOffset(28, 28)
	closeBtn.Position = UDim2.new(1, -30, 0, 2)
	closeBtn.Font = Enum.Font.GothamBold
	closeBtn.TextSize = 18
	closeBtn.TextColor3 = TEXT_DIM
	closeBtn.Text = "×"
	closeBtn.ZIndex = 103
	closeBtn.Parent = header

	-- Body
	local body = Instance.new("ScrollingFrame")
	body.BackgroundTransparency = 1
	body.BorderSizePixel = 0
	body.Position = UDim2.fromOffset(0, 32)
	body.Size = UDim2.new(1, 0, 1, -80)
	body.ScrollBarThickness = 4
	body.AutomaticCanvasSize = Enum.AutomaticSize.Y
	body.CanvasSize = UDim2.new()
	body.ZIndex = 102
	body.Parent = panel

	makePadding(body, 12)
	local bodyLayout = Instance.new("UIListLayout")
	bodyLayout.Padding = UDim.new(0, 8)
	bodyLayout.SortOrder = Enum.SortOrder.LayoutOrder
	bodyLayout.Parent = body

	-- Target path row
	makeLabel(body, "Target path", 12).LayoutOrder = 1

	local pathRow = Instance.new("Frame")
	pathRow.BackgroundTransparency = 1
	pathRow.Size = UDim2.new(1, 0, 0, 26)
	pathRow.LayoutOrder = 2
	pathRow.Parent = body

	local targetBox = makeTextBox(pathRow, "C:\\path\\to\\folder", params.defaultTargetPath)
	targetBox.Size = UDim2.new(1, -88, 1, 0)
	targetBox.ZIndex = 103

	local browseBtn = Instance.new("TextButton")
	browseBtn.AutoButtonColor = false
	browseBtn.BackgroundColor3 = PANEL
	browseBtn.BorderColor3 = BORDER
	browseBtn.BorderSizePixel = 1
	browseBtn.Position = UDim2.new(1, -82, 0, 0)
	browseBtn.Size = UDim2.fromOffset(82, 26)
	browseBtn.Font = Enum.Font.Gotham
	browseBtn.TextSize = 12
	browseBtn.TextColor3 = TEXT
	browseBtn.Text = "Browse…"
	browseBtn.ZIndex = 103
	browseBtn.Parent = pathRow

	-- Mode
	makeLabel(body, "Mode", 12).LayoutOrder = 3
	local modeRadio = makeRadioGroup(body, {
		{ key = "new_project" :: string, label = "New Project" },
		{ key = "merge_existing" :: string, label = "Merge Existing" },
	}, "new_project", function(_key)
		-- Overwrite row visibility is recomputed below via a refresh closure.
	end)
	;(modeRadio :: any).container_layout_order = 4

	local overwriteCheckbox = makeCheckbox(body, "Overwrite existing files", false, function(_v) end)
	overwriteCheckbox.button.Parent.Parent = body
	overwriteCheckbox.button.Parent.LayoutOrder = 5
	overwriteCheckbox.button.Parent.Visible = false

	-- Re-evaluate overwrite visibility whenever mode changes.
	local function refreshOverwriteVisibility()
		overwriteCheckbox.button.Parent.Visible = modeRadio.value == "merge_existing"
	end
	local prevSet = modeRadio.setValue
	modeRadio.setValue = function(next: string)
		prevSet(next)
		refreshOverwriteVisibility()
	end

	-- Inclusion flags
	makeLabel(body, "Inclusion", 12).LayoutOrder = 6
	local includeNonScript = makeCheckbox(body, "Include non-script instances", true, function(_v) end)
	includeNonScript.button.Parent.LayoutOrder = 7

	local includeBinary = makeCheckbox(
		body,
		"Include binary assets  (BinaryStringValue, MeshPart, Unions)",
		false,
		function(_v) end
	)
	includeBinary.button.Parent.LayoutOrder = 8

	-- Template — single option for now, treated as display-only.
	makeLabel(body, "Template", 12).LayoutOrder = 9
	local templateBtn = Instance.new("TextButton")
	templateBtn.AutoButtonColor = false
	templateBtn.BackgroundColor3 = INPUT_BG
	templateBtn.BorderColor3 = BORDER
	templateBtn.BorderSizePixel = 1
	templateBtn.Size = UDim2.new(1, 0, 0, 26)
	templateBtn.Font = Enum.Font.Gotham
	templateBtn.TextSize = 13
	templateBtn.TextColor3 = TEXT
	templateBtn.TextXAlignment = Enum.TextXAlignment.Left
	templateBtn.Text = "  Minimal  ▾"
	templateBtn.AutoLocalize = false
	templateBtn.LayoutOrder = 10
	templateBtn.Active = false
	templateBtn.Parent = body

	-- Project name
	makeLabel(body, "Project name (optional)", 12).LayoutOrder = 11
	local nameBox = makeTextBox(body, "defaults to folder name", "")
	nameBox.LayoutOrder = 12

	-- Progress region (hidden until sending)
	local progressRow = Instance.new("Frame")
	progressRow.BackgroundTransparency = 1
	progressRow.Size = UDim2.new(1, 0, 0, 42)
	progressRow.LayoutOrder = 13
	progressRow.Visible = false
	progressRow.Parent = body

	local progressOuter = Instance.new("Frame")
	progressOuter.BackgroundColor3 = INPUT_BG
	progressOuter.BorderColor3 = BORDER
	progressOuter.BorderSizePixel = 1
	progressOuter.Size = UDim2.new(1, 0, 0, 14)
	progressOuter.Parent = progressRow

	local progressInner = Instance.new("Frame")
	progressInner.BackgroundColor3 = ACCENT
	progressInner.BorderSizePixel = 0
	progressInner.Size = UDim2.fromScale(0, 1)
	progressInner.Parent = progressOuter

	local progressLabel = Instance.new("TextLabel")
	progressLabel.BackgroundTransparency = 1
	progressLabel.Position = UDim2.fromOffset(0, 18)
	progressLabel.Size = UDim2.new(1, 0, 0, 16)
	progressLabel.Font = Enum.Font.Code
	progressLabel.TextSize = 12
	progressLabel.TextColor3 = TEXT_DIM
	progressLabel.TextXAlignment = Enum.TextXAlignment.Left
	progressLabel.Text = "0 / 0 instances"
	progressLabel.Parent = progressRow

	-- Status line
	local status = Instance.new("TextLabel")
	status.BackgroundTransparency = 1
	status.Size = UDim2.new(1, 0, 0, 18)
	status.Font = Enum.Font.Gotham
	status.TextSize = 12
	status.TextColor3 = TEXT_DIM
	status.TextXAlignment = Enum.TextXAlignment.Left
	status.Text = "Ready"
	status.TextWrapped = true
	status.LayoutOrder = 14
	status.Parent = body

	-- Footer
	local footer = Instance.new("Frame")
	footer.BackgroundColor3 = PANEL
	footer.BorderSizePixel = 0
	footer.Position = UDim2.new(0, 0, 1, -48)
	footer.Size = UDim2.new(1, 0, 0, 48)
	footer.ZIndex = 102
	footer.Parent = panel

	local cancelBtn = makeButton(footer, "Cancel", PANEL, Color3.fromRGB(60, 60, 60))
	cancelBtn.BorderColor3 = BORDER
	cancelBtn.BorderSizePixel = 1
	cancelBtn.Position = UDim2.new(1, -208, 0.5, -14)
	cancelBtn.Size = UDim2.fromOffset(96, 28)
	cancelBtn.ZIndex = 103

	local confirmBtn = makeButton(footer, "Confirm", ACCENT, ACCENT_HOVER)
	confirmBtn.Position = UDim2.new(1, -104, 0.5, -14)
	confirmBtn.Size = UDim2.fromOffset(96, 28)
	confirmBtn.ZIndex = 103

	-- State management
	local state: State = "idle"
	local destroyed = false
	-- Captured at confirm time so the post-completion "Open in IDE" button
	-- still works after the user clears or edits the path field.
	local lastTargetPath: string? = nil

	local function setInputsEnabled(enabled: boolean)
		targetBox.TextEditable = enabled
		browseBtn.Active = enabled
		modeRadio.setEnabled(enabled)
		overwriteCheckbox.setEnabled(enabled)
		includeNonScript.setEnabled(enabled)
		includeBinary.setEnabled(enabled)
		nameBox.TextEditable = enabled
	end

	local function setStatus(text: string, color: Color3)
		status.Text = text
		status.TextColor3 = color
	end

	local function close()
		if destroyed then
			return
		end
		destroyed = true
		backdrop:Destroy()
		params.onClose()
	end

	local handle: Handle
	handle = {
		close = function(_self)
			close()
		end,
		setSending = function(_self)
			state = "sending"
			setInputsEnabled(false)
			progressRow.Visible = true
			progressInner.Size = UDim2.fromScale(0, 1)
			progressLabel.Text = "0 / 0 instances"
			confirmBtn.Text = "Sending…"
			confirmBtn.Active = false
			confirmBtn.BackgroundColor3 = Color3.fromRGB(35, 75, 140)
			cancelBtn.Text = "Abort"
			setStatus("Streaming place to daemon…", TEXT_DIM)
		end,
		setProgress = function(_self, sent, total)
			if state ~= "sending" then
				return
			end
			local ratio = total > 0 and math.clamp(sent / total, 0, 1) or 0
			progressInner.Size = UDim2.fromScale(ratio, 1)
			progressLabel.Text = string.format("%d / %d instances", sent, total)
		end,
		setComplete = function(_self, stats)
			state = "complete"
			progressInner.Size = UDim2.fromScale(1, 1)
			progressLabel.Text = string.format(
				"%d / %d instances",
				stats.scripts_written + stats.non_script_instances_written,
				stats.scripts_written + stats.non_script_instances_written
			)
			local warnings = #stats.warnings
			setStatus(
				string.format(
					"Done — %d scripts, %d meta files, %d warnings",
					stats.scripts_written,
					stats.meta_files_written,
					warnings
				),
				SUCCESS
			)
			confirmBtn.Text = "Open in IDE"
			confirmBtn.Active = true
			confirmBtn.BackgroundColor3 = ACCENT
			cancelBtn.Text = "Close"
		end,
		setError = function(_self, message)
			state = "error"
			setStatus("Error: " .. message, DANGER)
			confirmBtn.Text = "Retry"
			confirmBtn.Active = true
			confirmBtn.BackgroundColor3 = ACCENT
			cancelBtn.Text = "Close"
			setInputsEnabled(true)
		end,
	}

	closeBtn.MouseButton1Click:Connect(function()
		close()
	end)

	cancelBtn.MouseButton1Click:Connect(function()
		close()
	end)

	-- `onBrowseFolder` yields until the extension replies, so the button has
	-- to self-disable during the round-trip — otherwise a rage-click opens
	-- multiple dialogs and confuses `showOpenDialog` downstream.
	local pickInFlight = false
	browseBtn.MouseButton1Click:Connect(function()
		if state ~= "idle" or pickInFlight then
			return
		end
		pickInFlight = true
		browseBtn.Active = false
		local picked = params.onBrowseFolder()
		pickInFlight = false
		browseBtn.Active = state == "idle"
		if picked ~= nil and picked ~= "" then
			targetBox.Text = picked
		end
	end)

	confirmBtn.MouseButton1Click:Connect(function()
		if not confirmBtn.Active then
			return
		end
		if state == "complete" then
			if params.onOpenInIde ~= nil and lastTargetPath ~= nil then
				params.onOpenInIde(lastTargetPath)
			end
			return
		end
		if state == "error" then
			-- Retry resets state to idle so the user can tweak and confirm again.
			state = "idle"
			setStatus("Ready", TEXT_DIM)
			confirmBtn.Text = "Confirm"
			cancelBtn.Text = "Cancel"
			progressRow.Visible = false
			return
		end
		local target = targetBox.Text
		if target == "" then
			setStatus("Target path is required", DANGER)
			return
		end
		local modeValue: Mode
		if modeRadio.value == "new_project" then
			modeValue = "new_project"
		else
			modeValue = { kind = "merge_existing", overwrite = overwriteCheckbox.value }
		end
		local projectName: string? = nil
		if nameBox.Text ~= "" then
			projectName = nameBox.Text
		end
		local opts: Options = {
			target_path = target,
			mode = modeValue,
			include_non_script = includeNonScript.value,
			include_binary = includeBinary.value,
			template = "minimal",
			project_name = projectName,
		}
		lastTargetPath = target
		params.onConfirm(opts)
	end)

	return handle
end

return SyncbackModal
