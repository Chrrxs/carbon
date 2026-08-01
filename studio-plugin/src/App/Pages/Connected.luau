local Carbon = script:FindFirstAncestor("Carbon")
local App = Carbon.App
local Widgets = App.Widgets
local Components = App.Components

local Fusion = require(Carbon.Packages.Fusion)

local Types = require(Carbon.Types)
local Assets = require(App.Assets)
local Theme = require(App.Theme)

local Project = require(Widgets.Project)
local TextButton = require(Components.TextButton)
local IconButton = require(Components.IconButton)
local Container = require(Components.Container)
local Padding = require(Components.Padding)
local Text = require(Components.Text)
local List = require(Components.List)
local Box = require(Components.Box)

local Value = Fusion.Value
local Observer = Fusion.Observer
local Computed = Fusion.Computed
local Children = Fusion.Children
local Cleanup = Fusion.Cleanup
local peek = Fusion.peek

type Props = {
	App: { [string]: any },
	Project: Fusion.Value<Types.Project>,
}

return function(props: Props): { Instance }
	local syncedText = Value("just now")
	local projectWidget = Value(nil)
	local captureState = Value("idle")
	local captureMessage = Value("")
	local compactActions = Computed(function(use)
		return use(props.App.rootSize).X < 380
	end)

	local function updateSyncedText()
		return task.spawn(function()
			while true do
				local elapsed = os.time() - peek(props.App.lastSync)
				local interval = 1
				local text

				if elapsed < 1 then
					text = "just now"
				elseif elapsed < 60 then
					text = math.floor(elapsed) .. "s ago"
				elseif elapsed < 3600 then
					interval = 60
					text = elapsed // interval .. "m ago"
				elseif elapsed < 86400 then
					interval = 3600
					text = elapsed // interval .. "h ago"
				else
					interval = 86400
					text = elapsed // interval .. "d ago"
				end

				syncedText:set(text)
				task.wait(interval)
			end
		end)
	end

	local thread = updateSyncedText()
	local disconnect = Observer(props.App.lastSync):onChange(function()
		task.cancel(thread)
		thread = updateSyncedText()
	end)

	return {
		List {},
		Box {
			Size = UDim2.new(1, 0, 0, Theme.CompSizeY.Large * 1.5),
			[Children] = {
				Padding {},
				Text {
					Size = UDim2.fromScale(0.8, 0.6),
					Text = Computed(function(use)
						return use(props.Project).name
					end),
					Font = Theme.Fonts.Bold,
					Scaled = true,
				},
				Text {
					Position = UDim2.fromScale(0, 0.6),
					Size = UDim2.fromScale(0.8, 0.4),
					Text = `{props.App.host}:{props.App.port}`,
					TextSize = Theme.TextSize.Small,
					Font = Theme.Fonts.Mono,
					Scaled = true,
				},
				Text {
					AnchorPoint = Vector2.new(1, 0.5),
					Position = UDim2.new(1, -2, 0.5, 0),
					Text = Computed(function(use)
						local lastSyncKind = use(props.App.lastSyncKind)
						local action = ""

						if lastSyncKind == "Server" then
							action = "↓"
						elseif lastSyncKind == "Client" then
							action = "↑"
						end

						return action .. "<i> Synced\n" .. use(syncedText) .. "</i>"
					end),
					RichText = true,
					Color = Theme.Colors.TextDimmed,
					TextSize = Theme.TextSize.Small,
					TextXAlignment = Enum.TextXAlignment.Right,
					LineHeight = 1.2,
				},
			},
		},
		Container {
			Size = UDim2.fromScale(1, 0),
			[Children] = {
				List {
					Spacing = 4,
					FillDirection = Enum.FillDirection.Horizontal,
					HorizontalAlignment = Enum.HorizontalAlignment.Right,
				},
				TextButton {
					Text = Computed(function(use)
						local compact = use(compactActions)
						if use(captureState) == "running" then
							return if compact then "Waiting..." else "Waiting for Auto-Capture..."
						end
						return if compact then "Wait" else "Wait for Auto-Capture"
					end),
					TextSize = Theme.TextSize.Small,
					Active = Computed(function(use)
						return use(captureState) ~= "running"
					end),
					TextTransparency = Computed(function(use)
						return if use(captureState) == "running" then 0.35 else 0
					end),
					LayoutOrder = 1,
					Activated = function()
						if peek(captureState) == "running" then
							return
						end
						captureState:set("running")
						captureMessage:set("Waiting for the current Studio auto-recovery capture...")
						props.App.core
							:captureManifest(function(status)
								captureState:set(status.state)
								captureMessage:set(status.message or "Capture status updated")
							end)
							:andThen(function()
								props.App.lastSync:set(os.time())
								props.App.lastSyncKind:set("Client")
							end)
							:catch(function(err)
								local message = err.message or tostring(err)
								captureState:set("failed")
								captureMessage:set(message)
								task.defer(function()
									error(`[Carbon] Capture Manifest request failed: {message}`, 0)
								end)
							end)
					end,
				},
				TextButton {
					Text = "Disconnect",
					TextSize = Theme.TextSize.Small,
					LayoutOrder = 4,
					Solid = true,
					Activated = function()
						props.App:disconnect()
						props.App:home()
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
					LayoutOrder = 2,
				},
				IconButton {
					Icon = Assets.Icons.Info,
					BackgroundActive = Computed(function(use)
						return use(projectWidget) ~= nil
					end),
					Activated = function()
						local widget = peek(projectWidget)

						if widget then
							widget:Destroy()
							widget = nil
						else
							widget = Project {
								App = props.App,
								Project = props.Project,

								Closed = function()
									peek(projectWidget):Destroy()
									projectWidget:set(nil)
								end,
							}
						end

						projectWidget:set(widget)
					end,
					LayoutOrder = 3,
					[Cleanup] = function()
						local widget = peek(projectWidget)

						if widget then
							widget:Destroy()
							widget = nil
						end

						disconnect()
						task.cancel(thread)
					end,
				},
			},
		},
		Text {
			Size = UDim2.new(1, 0, 0, Theme.CompSizeY.Small),
			AutomaticSize = Enum.AutomaticSize.Y,
			Visible = Computed(function(use)
				return use(captureState) ~= "idle"
			end),
			Text = Computed(function(use)
				local state = use(captureState)
				local message = use(captureMessage)
				if state == "running" then
					return `Capturing: {message}`
				elseif state == "complete" then
					return `Capture complete: {message}`
				else
					return `Capture failed: {message}`
				end
			end),
			Color = Computed(function(use)
				local state = use(captureState)
				if state == "complete" then
					return use(Theme.Colors.Diff.Add)
				elseif state == "failed" then
					return use(Theme.Colors.Diff.Remove)
				else
					return use(Theme.Colors.TextDimmed)
				end
			end),
			TextSize = Theme.TextSize.Small,
			TextWrapped = true,
		},
	}
end
