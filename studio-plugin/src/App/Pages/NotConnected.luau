local Carbon = script:FindFirstAncestor("Carbon")
local App = Carbon.App
local Components = App.Components
local Util = Components.Util

local Fusion = require(Carbon.Packages.Fusion)

local Config = require(Carbon.Config)
local Assets = require(App.Assets)
local Theme = require(App.Theme)
local filterPort = require(Util.filterPort)

local TextButton = require(Components.TextButton)
local IconButton = require(Components.IconButton)
local Container = require(Components.Container)
local Padding = require(Components.Padding)
local Input = require(Components.Input)
local List = require(Components.List)
local Text = require(Components.Text)
local Box = require(Components.Box)

local New = Fusion.New
local Value = Fusion.Value
local Cleanup = Fusion.Cleanup
local Computed = Fusion.Computed
local Children = Fusion.Children
local Spring = Fusion.Spring

type Props = {
	App: { [string]: any },
	Port: number,
}

local function getValue(setting: Config.Setting, value: any, input: boolean?): string
	if input then
		return value ~= Config:getDefault(setting) and value or ""
	else
		return value ~= "" and value or Config:getDefault(setting)
	end
end

return function(props: Props): { Instance }
	local portInput = Value(getValue("Port", props.Port, true))

	local portFocused = Value(false)

	local connections = {
		Config:onChanged("Port", function(value)
			portInput:set(getValue("Port", value, true))
			props.App:setPort(getValue("Port", value))
		end),
	}

	return {
		List {},
		Box {
			Size = UDim2.new(1, 0, 0, Theme.CompSizeY.Large),
			AutomaticSize = Enum.AutomaticSize.None,
			BorderColor = Spring(
				Computed(function(use)
					if use(portFocused) then
						return use(Theme.Colors.Brand)
					else
						return use(Theme.Colors.Border)
					end
				end),
				Theme.SpringFastSpeed,
				Theme.SpringDamping
			),
			[Cleanup] = connections,
			[Children] = {
				List {
					Spacing = 8,
					FillDirection = Enum.FillDirection.Horizontal,
					VerticalAlignment = Enum.VerticalAlignment.Center,
				},
				Text {
					AutomaticSize = Enum.AutomaticSize.XY,
					Text = "Port",
					Font = Theme.Fonts.Bold,
					Color = Theme.Colors.TextDimmed,
				},
				Input {
					Size = UDim2.fromScale(1, 1),
					Font = Theme.Fonts.Mono,
					PlaceholderText = Config:getDefault("Port"),
					Text = portInput,
					Started = function()
						portFocused:set(true)
					end,
					Changed = function(text)
						portInput:set(filterPort(text))
					end,
					Finished = function(port)
						portFocused:set(false)
						props.App:setPort(getValue("Port", port))
					end,
					[Children] = {
						New "UIFlexItem" {
							FlexMode = Enum.UIFlexMode.Fill,
						},
					},
				},
				Padding {
					Horizontal = 10,
				},
			},
		},
		Container {
			Size = UDim2.fromScale(1, 0),
			[Children] = {
				List {
					FillDirection = Enum.FillDirection.Horizontal,
					HorizontalAlignment = Enum.HorizontalAlignment.Right,
				},
				TextButton {
					Solid = true,
					LayoutOrder = 2,
					Text = "Connect",
					Activated = function()
						props.App:connect()
					end,
				},
				IconButton {
					Icon = Assets.Icons.Settings,
					BackgroundActive = Computed(function(use)
						return use(props.App.settingsWidget) ~= nil
					end),
					Activated = function()
						props.App:settings()
					end,
					LayoutOrder = 1,
				},
				IconButton {
					Icon = Assets.Icons.Help,
					BackgroundActive = Computed(function(use)
						return use(props.App.helpWidget) ~= nil
					end),
					Activated = function()
						props.App:help()
					end,
				},
			},
		},
	}
end
