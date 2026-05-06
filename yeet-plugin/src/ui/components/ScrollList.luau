--!strict

-- Vertical scrolling container with theme-matched scrollbar and
-- automatic canvas sizing. Wraps the slightly awkward Roblox UIListLayout
-- pattern so callers just pass children with LayoutOrder and forget about
-- AbsoluteContentSize bindings.

local Roact = require(script.Parent.Parent.Parent.Packages.Roact)
local theme = require(script.Parent.Parent.theme)

type Props = {
	size: UDim2?,
	layoutOrder: number?,
	padding: number?,
	itemGap: number?,
	children: { [any]: any }?,
}

local function ScrollList(props: Props)
	local padding = props.padding or 0
	local gap = props.itemGap or theme.spacing.sm
	local children = {
		__layout = Roact.createElement("UIListLayout", {
			FillDirection = Enum.FillDirection.Vertical,
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, gap),
		}),
		__padding = Roact.createElement("UIPadding", {
			PaddingTop = UDim.new(0, padding),
			PaddingBottom = UDim.new(0, padding),
			PaddingLeft = UDim.new(0, padding),
			PaddingRight = UDim.new(0, padding),
		}),
	}
	if props.children ~= nil then
		for key, child in props.children do
			children[key] = child
		end
	end
	return Roact.createElement("ScrollingFrame", {
		Size = props.size or UDim2.fromScale(1, 1),
		LayoutOrder = props.layoutOrder,
		BackgroundTransparency = 1,
		BorderSizePixel = 0,
		ScrollBarThickness = 4,
		ScrollBarImageColor3 = theme.colors.textMuted,
		CanvasSize = UDim2.new(),
		AutomaticCanvasSize = Enum.AutomaticSize.Y,
		ScrollingDirection = Enum.ScrollingDirection.Y,
	}, children)
end

return ScrollList
