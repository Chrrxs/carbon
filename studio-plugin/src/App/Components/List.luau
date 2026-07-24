local Carbon = script:FindFirstAncestor("Carbon")
local App = Carbon.App
local Components = App.Components
local Util = Components.Util

local Fusion = require(Carbon.Packages.Fusion)

local Theme = require(App.Theme)
local stripProps = require(Util.stripProps)

local New = Fusion.New
local Hydrate = Fusion.Hydrate

local COMPONENT_ONLY_PROPS = {
	"Spacing",
}

type Props = {
	Spacing: number?,
	[any]: any,
}

return function(props: Props): UIListLayout
	return Hydrate(New "UIListLayout" {
		SortOrder = Enum.SortOrder.LayoutOrder,
		Padding = UDim.new(0, props.Spacing or Theme.ListSpacing),
	})(stripProps(props, COMPONENT_ONLY_PROPS))
end
