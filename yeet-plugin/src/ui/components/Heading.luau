--!strict

-- Two-line header: bold title on top, muted subtitle below. Matches the
-- "Saelig / Building Design systems Case Study" pattern from the mockup's
-- Saelig card. Omit `subtitle` for a single-line title.

local Roact = require(script.Parent.Parent.Parent.Packages.Roact)
local theme = require(script.Parent.Parent.theme)

type Props = {
	title: string,
	subtitle: string?,
	layoutOrder: number?,
}

local function Heading(props: Props)
	local children: { [any]: any } = {
		layout = Roact.createElement("UIListLayout", {
			FillDirection = Enum.FillDirection.Vertical,
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, theme.spacing.tight),
		}),
		title = Roact.createElement("TextLabel", {
			Size = UDim2.new(1, 0, 0, theme.typography.title.size + 4),
			BackgroundTransparency = 1,
			Text = props.title,
			TextColor3 = theme.colors.text,
			Font = theme.typography.title.font,
			TextSize = theme.typography.title.size,
			TextXAlignment = Enum.TextXAlignment.Left,
			LayoutOrder = 1,
		}),
	}
	if props.subtitle ~= nil then
		children.subtitle = Roact.createElement("TextLabel", {
			Size = UDim2.new(1, 0, 0, theme.typography.caption.size + 4),
			BackgroundTransparency = 1,
			Text = props.subtitle,
			TextColor3 = theme.colors.textMuted,
			Font = theme.typography.caption.font,
			TextSize = theme.typography.caption.size,
			TextXAlignment = Enum.TextXAlignment.Left,
			LayoutOrder = 2,
		})
	end
	return Roact.createElement("Frame", {
		Size = UDim2.new(1, 0, 0, 0),
		AutomaticSize = Enum.AutomaticSize.Y,
		BackgroundTransparency = 1,
		LayoutOrder = props.layoutOrder,
	}, children)
end

return Heading
