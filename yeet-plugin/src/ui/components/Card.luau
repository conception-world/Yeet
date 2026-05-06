--!strict

-- Rounded dark container — the building block everything else sits inside.
-- Matches the "island" aesthetic in the approved mockup: no stroke, heavy
-- corner radius, generous internal padding. Accepts either a fixed `size`
-- or leaves `AutomaticSize.Y` so content decides the height.

local Roact = require(script.Parent.Parent.Parent.Packages.Roact)
local theme = require(script.Parent.Parent.theme)

type Props = {
	size: UDim2?,
	position: UDim2?,
	anchorPoint: Vector2?,
	layoutOrder: number?,
	padding: number?,
	color: Color3?,
	radius: number?,
	automaticSize: Enum.AutomaticSize?,
	children: { [any]: any }?,
}

local function Card(props: Props)
	local padding = props.padding or theme.spacing.lg
	local radius = props.radius or theme.radius.lg
	local children = {
		__corner = Roact.createElement("UICorner", {
			CornerRadius = UDim.new(0, radius),
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
	return Roact.createElement("Frame", {
		Size = props.size or UDim2.new(1, 0, 0, 0),
		Position = props.position,
		AnchorPoint = props.anchorPoint,
		LayoutOrder = props.layoutOrder,
		BackgroundColor3 = props.color or theme.colors.card,
		BorderSizePixel = 0,
		AutomaticSize = props.automaticSize or Enum.AutomaticSize.Y,
	}, children)
end

return Card
