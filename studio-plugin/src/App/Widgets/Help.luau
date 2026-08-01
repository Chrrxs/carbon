local Carbon = script:FindFirstAncestor("Carbon")
local App = Carbon.App
local Components = App.Components

local Fusion = require(Carbon.Packages.Fusion)

local Theme = require(App.Theme)

local Widget = require(Components.Plugin.Widget)
local ScrollingContainer = require(Components.ScrollingContainer)
local Container = require(Components.Container)
local Padding = require(Components.Padding)
local Input = require(Components.Input)
local List = require(Components.List)
local Text = require(Components.Text)
local Box = require(Components.Box)

local Children = Fusion.Children

local function Entry(body: { any })
	return Box {
		Size = UDim2.fromScale(1, 0),
		[Children] = {
			Padding {},
			List {
				Spacing = 6,
			},
			body,
		},
	}
end

local function Header(text: string)
	return Text {
		Font = Theme.Fonts.Bold,
		Text = text,
	}
end

local function Paragraph(text: string)
	return Text {
		Text = text,
		RichText = true,
		TextWrapped = true,
		TextSize = Theme.TextSize.Small,
		Color = Theme.Colors.TextDimmed,
	}
end

local function Link(text: string, url: string)
	return Container {
		Size = UDim2.fromScale(1, 0),
		[Children] = {
			List {
				HorizontalAlignment = Enum.HorizontalAlignment.Center,
				Spacing = 8,
			},
			Text {
				Font = Theme.Fonts.Bold,
				Text = text,
			},
			Box {
				AutomaticSize = Enum.AutomaticSize.XY,
				[Children] = {
					Padding {},
					Input {
						AutomaticSize = Enum.AutomaticSize.XY,
						Text = url,
						TextEditable = false,
						TextSize = Theme.TextSize.Large,
						Color = Theme.Colors.Brand,
					},
				},
			},
		},
	}
end

type Props = {
	Closed: (() -> ())?,
}

return function(props: Props): DockWidgetPluginGui
	return Widget {
		Name = "Carbon - Help",
		OverrideEnabled = true,
		MinimumSize = Vector2.new(400, 350),
		Closed = props.Closed,
		[Children] = {
			Padding {
				Right = 4,
			},
			ScrollingContainer {
				ScrollBar = true,
				[Children] = {
					List {
						Spacing = 22,
					},
					Padding {
						Padding = Theme.WidgetPadding,
					},
					Link("Carbon documentation", "https://github.com/Chrrxs/carbon"),
					Entry {
						Header "1. Create a Carbon project",
						Paragraph "Run <b>carbon init</b> to create the project file, data directory, and mapped starter source.",
					},
					Entry {
						Header "2. Start the local session",
						Paragraph "Run <b>carbon serve game.carbon.json</b>. Carbon builds a disposable place, starts the loopback server, and launches Studio with the matching plugin and runtime.",
					},
					Entry {
						Header "3. Connect Studio",
						Paragraph "Managed launches connect automatically. To attach an already open place, enter the port used by <b>carbon serve</b>, select <b>Connect</b>, and confirm the target project before Carbon applies its mappings.",
					},
					Entry {
						Header "4. Edit mapped source",
						Paragraph "Mappings are filesystem-authoritative and sync one way into Studio. Studio edits beneath a mapping never write back and are replaced by the next valid filesystem update.",
					},
					Entry {
						Header "5. Save Studio-owned state",
						Paragraph "Carbon continuously saves the Studio-authored complement from Studio auto-recovery. <b>Wait for Auto-Capture</b> shows the current cycle; use <b>carbon capture game.carbon.json saved.rbxl</b> for a manual File > Save to File place. During stop, saving over the temporary served place completes the wait immediately. Capture never writes mapped files or changes project topology.",
					},
				},
			},
		},
	}
end
