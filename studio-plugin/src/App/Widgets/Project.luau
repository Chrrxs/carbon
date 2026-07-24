local Carbon = script:FindFirstAncestor("Carbon")
local App = Carbon.App
local Components = App.Components

local Fusion = require(Carbon.Packages.Fusion)

local Types = require(Carbon.Types)
local Theme = require(App.Theme)

local Widget = require(Components.Plugin.Widget)
local ScrollingContainer = require(Components.ScrollingContainer)
local Padding = require(Components.Padding)
local Text = require(Components.Text)
local List = require(Components.List)
local Box = require(Components.Box)

local Children = Fusion.Children
local Computed = Fusion.Computed
local peek = Fusion.peek

type EntryProps = {
	Name: string,
	Value: string,
}

local function Entry(props: EntryProps): Frame
	return Box {
		Size = UDim2.fromScale(1, 0),
		[Children] = {
			Padding {},
			Text {
				AnchorPoint = Vector2.new(0, 0.5),
				Position = UDim2.fromScale(0, 0.5),
				Size = UDim2.new(0, 160, 1, 0),
				Text = props.Name,
			},
			Text {
				AnchorPoint = Vector2.new(1, 0),
				Position = UDim2.fromScale(1, 0),
				Size = UDim2.new(1, -160, 1, 0),
				AutomaticSize = Enum.AutomaticSize.Y,
				TextColor3 = Theme.Colors.TextDimmed,
				TextSize = Theme.TextSize.Medium,
				TextWrapped = true,
				Text = props.Value,
			},
		},
	}
end

type Props = {
	App: { [string]: any },
	Project: Fusion.Value<Types.Project>,
	Closed: (() -> ())?,
}

return function(props: Props): DockWidgetPluginGui
	return Widget {
		Name = "Carbon - Project",
		OverrideEnabled = true,
		MinimumSize = Vector2.new(400, 300),
		Closed = props.Closed,
		[Children] = {
			Padding {
				Right = 4,
			},
			ScrollingContainer {
				ScrollBar = true,
				[Children] = {
					List {
						HorizontalAlignment = Enum.HorizontalAlignment.Center,
					},
					Padding {
						Padding = Theme.WidgetPadding,
					},
					Entry {
						Name = "Name",
						Value = Computed(function(use)
							return use(props.Project).name
						end),
					},
					Entry {
						Name = "Root Instances",
						Value = Computed(function(use)
							local rootRefs = use(props.Project).rootRefs
							return tostring(if type(rootRefs) == "table" then #rootRefs else 0)
						end),
					},
					Entry {
						Name = "Mapped Roots",
						Value = Computed(function(use)
							local mappedRootRefs = use(props.Project).mappedRootRefs
							return tostring(if type(mappedRootRefs) == "table" then #mappedRootRefs else 0)
						end),
					},
					Entry {
						Name = "Source Generation",
						Value = Computed(function(use)
							return use(props.Project).sourceGeneration or "Unknown"
						end),
					},
					Entry {
						Name = "Server Version",
						Value = Computed(function(use)
							return use(props.Project).version
						end),
					},
					Entry {
						Name = "Host",
						Value = peek(props.App.host),
					},
					Entry {
						Name = "Port",
						Value = peek(props.App.port),
					},
				},
			},
		},
	}
end
