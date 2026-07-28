local plugin = script:FindFirstAncestorWhichIsA("Plugin")

local Carbon = script:FindFirstAncestor("Carbon")
local RuntimeServices = require(Carbon.RuntimeServices)
local RunService = RuntimeServices.get("RunService") :: RunService
local TweenService = RuntimeServices.get("TweenService") :: TweenService
local HttpService = RuntimeServices.get("HttpService") :: HttpService
local CoreGui = RuntimeServices.get("CoreGui") :: CoreGui
local Packages = Carbon.Packages
local Helpers = Carbon.Helpers

local Components = script.Components
local Widgets = script.Widgets
local Pages = script.Pages

local Fusion = require(Packages.Fusion)
local Signal = require(Carbon.Signal)

local manifest = require(Carbon.manifest)
local Config = require(Carbon.Config)
local Types = require(Carbon.Types)
local Util = require(Carbon.Util)
local Core = require(Carbon.Core)
local describeChanges = require(Helpers.describeChanges)
local ManagedWorktree = require(Carbon.ManagedWorktree)
local ManagedConnectionMarker = require(Carbon.ManagedConnectionMarker)
local LiveSessionMarker = require(Carbon.LiveSessionMarker)

local Loader = require(script.Loader)
local Theme = require(script.Theme)

local Toolbar = require(Components.Plugin.Toolbar)
local ToolbarButton = require(Components.Plugin.ToolbarButton)
local Widget = require(Components.Plugin.Widget)
local Container = require(Components.Container)
local Padding = require(Components.Padding)
local Text = require(Components.Text)
local List = require(Components.List)

local NotConnected = require(Pages.NotConnected)
local Connecting = require(Pages.Connecting)
local Connected = require(Pages.Connected)
local Unavailable = require(Pages.Unavailable)
local Prompt = require(Pages.Prompt)
local Error = require(Pages.Error)

local Settings = require(Widgets.Settings)
local Help = require(Widgets.Help)
local Diff = require(Widgets.Diff)

local New = Fusion.New
local Value = Fusion.Value
local Spring = Fusion.Spring
local OnEvent = Fusion.OnEvent
local OnChange = Fusion.OnChange
local Observer = Fusion.Observer
local Computed = Fusion.Computed
local Children = Fusion.Children
local cleanup = Fusion.cleanup
local peek = Fusion.peek

local RECONNECT_INTERVAL = 5
local ENDPOINT_ATTRIBUTE = "__StudioWorktree_CarbonEndpoint"
local PROJECT_ATTRIBUTE = "__StudioWorktree_CarbonProject"
local GENERATION_ATTRIBUTE = "__StudioWorktree_CarbonGeneration"
local IDENTITY_ATTRIBUTE = "__StudioWorktree_Identity"
local SESSION_ATTRIBUTE = "__StudioWorktree_Session"

local function getManagedWorktree()
	local workspace = RuntimeServices.get("Workspace") :: Workspace
	local endpoint = workspace:GetAttribute(ENDPOINT_ATTRIBUTE)
	local project = workspace:GetAttribute(PROJECT_ATTRIBUTE)
	local worktreeId = workspace:GetAttribute(IDENTITY_ATTRIBUTE)
	local sessionToken = workspace:GetAttribute(SESSION_ATTRIBUTE)
	local sourceGeneration = workspace:GetAttribute(GENERATION_ATTRIBUTE)
	return ManagedWorktree.resolve(endpoint, project, worktreeId, sessionToken, sourceGeneration)
end

local App = {}
App.__index = App

function App.new()
	Loader.load()

	local self = setmetatable({}, App)

	self.core = nil
	self.connectionMarker = nil
	self.liveSessionMarker = nil
	self.lastUpdate = os.clock()
	self.onCanceled = Signal.new()

	self.host = Config.HOST
	self.port = Config:get("Port")
	local liveSession = LiveSessionMarker.find(CoreGui, HttpService)
	self.managedWorktree = liveSession or getManagedWorktree()
	self.liveSessionMarker = liveSession and liveSession.marker or nil
	if self.managedWorktree then
		self.host = self.managedWorktree.host
		self.port = self.managedWorktree.port
	end

	self.lastSync = Value(os.time())
	self.lastSyncKind = Value("Unknown")

	self.rootSize = Value(Vector2.new(300, 200))
	self.pages = Value({})

	self.settingsWidget = Value(nil)
	self.helpWidget = Value(nil)
	self.diffWidget = Value(nil)

	local isOpen = Value(false)

	local toolbarButton = ToolbarButton {
		Toolbar = Toolbar {
			Name = "Carbon",
		},
		Name = "Carbon",
		ToolTip = "Open Carbon UI",
		Image = "",
		[OnEvent "Click"] = function()
			isOpen:set(not peek(isOpen))
		end,
	}

	Widget {
		Name = "Carbon",
		MinimumSize = peek(self.rootSize),
		InitialEnabled = false,
		OverrideEnabled = true,
		Enabled = isOpen,
		[OnChange "Enabled"] = function(isEnabled)
			isOpen:set(isEnabled)
		end,
		[OnChange "AbsoluteSize"] = function(size)
			self.rootSize:set(size)
		end,
		[Children] = self.pages,
	}

	plugin.Unloading:Connect(Observer(isOpen):onChange(function()
		toolbarButton:SetActive(peek(isOpen))
	end))

	toolbarButton:SetActive(peek(isOpen))

	if RunService:IsEdit() then
		self:home()

		if self.managedWorktree then
			self:connect()
		end

		self.liveSessionAdded = CoreGui.ChildAdded:Connect(function(child)
			local session = LiveSessionMarker.resolve(child, CoreGui, HttpService)
			if session == nil or child == self.liveSessionMarker then
				return
			end
			self:disconnect()
			self.liveSessionMarker = child
			self.managedWorktree = session
			self.host = session.host
			self.port = session.port
			self:home()
			self:connect()
		end)
		self.liveSessionRemoved = CoreGui.ChildRemoved:Connect(function(child)
			if child ~= self.liveSessionMarker then
				return
			end
			self.liveSessionMarker = nil
			self:disconnect()
			self.managedWorktree = getManagedWorktree()
			self.host = self.managedWorktree and self.managedWorktree.host or Config.HOST
			self.port = self.managedWorktree and self.managedWorktree.port or Config:get("Port")
			self:home()
		end)
	else
		self:setPage(Unavailable())
	end

	return self
end

function App:setPage(page)
	local pages = peek(self.pages)

	for id, child in pages do
		if child._finished then
			page[id] = nil
			continue
		end

		task.spawn(function()
			child._destructor(child._value)
			child._finished = true
		end)
	end

	local size = Value(UDim2.fromScale(1, 1))

	-- Extremely hacky way to smoothly transition between pages
	page = Computed(function()
		return New "Frame" {
			AnchorPoint = Vector2.new(0.5, 0.5),
			Position = UDim2.fromScale(0.5, 0.5),
			Size = Spring(size, 10),
			BackgroundColor3 = Theme.Colors.Background,
			BorderSizePixel = 0,
			[Children] = {
				List {
					VerticalAlignment = Enum.VerticalAlignment.Center,
					HorizontalAlignment = Enum.HorizontalAlignment.Center,
					Spacing = 8,
				},
				Padding {
					Padding = Theme.WidgetPadding,
				},
				Container {
					Size = UDim2.fromScale(1, 0),
					[Children] = {
						Text {
							Text = "Carbon",
							Font = Theme.Fonts.Bold,
							TextSize = Theme.TextSize.Large,
						},
						Text {
							AnchorPoint = Vector2.new(1, 1),
							Position = UDim2.new(1, -3, 1, 0),
							Text = `v{manifest.package.version}`,
							Color = Theme.Colors.TextDimmed,
							TextSize = Theme.TextSize.Small,
						},
					},
				},
				Container {
					Size = UDim2.fromScale(1, 0),
					[Children] = page,
				},
			},
		}
	end, function(page)
		if not page then
			return
		end

		local TWEEN_INFO = TweenInfo.new(0.2, Enum.EasingStyle.Sine, Enum.EasingDirection.Out)
		TweenService:Create(page, TWEEN_INFO, { BackgroundTransparency = 1 }):Play()

		for _, descendant in page:GetDescendants() do
			local properties = {}

			if descendant:IsA("GuiObject") then
				if descendant:IsA("GuiButton") then
					descendant.Active = false
				end

				properties.BackgroundTransparency = 1

				if descendant:IsA("TextLabel") or descendant:IsA("TextButton") or descendant:IsA("TextBox") then
					properties.TextTransparency = 1
				elseif descendant:IsA("ImageLabel") or descendant:IsA("ImageButton") then
					properties.ImageTransparency = 1
				end
			end

			if descendant:IsA("UIStroke") then
				properties.Transparency = 1
			end

			if next(properties) then
				TweenService:Create(descendant, TWEEN_INFO, properties):Play()
			end
		end

		page.ZIndex = 2

		size:set(UDim2.fromScale(1.2, 1))

		task.wait(0.2)
		cleanup(page)
	end)

	pages[Util.generateGUID()] = page
	self.pages:set(pages)
end

function App:home()
	self.lastUpdate = os.clock()

	self:setPage(NotConnected {
		App = self,
		Port = self.port,
	})
end

function App:settings()
	local widget = peek(self.settingsWidget)

	if widget then
		widget:Destroy()
		widget = nil
	else
		widget = Settings {
			Closed = function()
				peek(self.settingsWidget):Destroy()
				self.settingsWidget:set(nil)
			end,
		}
	end

	self.settingsWidget:set(widget)
end

function App:help()
	local widget = peek(self.helpWidget)

	if widget then
		widget:Destroy()
		widget = nil
	else
		widget = Help {
			Closed = function()
				peek(self.helpWidget):Destroy()
				self.helpWidget:set(nil)
			end,
		}
	end

	self.helpWidget:set(widget)
end

function App:diff(changes: Types.Changes?, tree: any?)
	local widget = peek(self.diffWidget)

	if widget then
		widget:Destroy()
		widget = nil
	elseif changes and tree then
		widget = Diff {
			Changes = changes,
			Tree = tree,
			Closed = function()
				peek(self.diffWidget):Destroy()
				self.diffWidget:set(nil)
			end,
		}
	end

	self.diffWidget:set(widget)
end

function App:connect()
	local project = Value({})
	local canceled = false
	local loading = true

	self.core = Core.new(self.host, self.port, self.managedWorktree and self.managedWorktree.project)

	task.spawn(function()
		task.wait(0.2)

		if not self:isConnected() and loading then
			self:setPage(Connecting {
				App = self,
			})
		end
	end)

	self.onCanceled:Once(function()
		canceled = true
	end)

	self.core:onPrompt(function(message, changes, tree)
		loading = false

		local signal = Signal.new()
		local result = false

		-- Prompt user to accept or reject incoming changes
		if changes then
			local options = { "Accept", "Diff", "Cancel" }
			message = describeChanges(changes)

			self:setPage(Prompt {
				App = self,
				Message = message,
				Options = options,
				Signal = signal,
			})

			local choice = signal:Wait()

			while choice == "Diff" do
				self:diff(changes, tree)
				choice = signal:Wait()
			end

			self:diff()

			result = choice == "Accept"
		-- Prompt user whether to continue when server ID differs
		else
			local options = { "Continue", "Cancel" }

			self:setPage(Prompt {
				App = self,
				Message = message,
				Options = options,
				Signal = signal,
			})

			result = signal:Wait() == "Continue"
		end

		if self.core.status == Core.Status.Connected then
			self:setPage(Connected {
				App = self,
				Project = project,
			})
		end

		return result
	end)

	self.core:onReady(function(projectDetails)
		loading = false
		project:set(projectDetails)

		self.lastSync:set(os.time())

		self.lastSyncKind:set("Server")

		self:setPage(Connected {
			App = self,
			Project = project,
		})

		if self.managedWorktree and self.managedWorktree.project.worktreeId ~= nil then
			ManagedConnectionMarker.destroy(self.connectionMarker)
			local serverStorage = game:FindFirstChildOfClass("ServerStorage")
			self.connectionMarker = if serverStorage
				then ManagedConnectionMarker.create(serverStorage, self.managedWorktree.project.sessionToken)
				else nil
		end
	end)

	self.core:onSync(function(kind, data)
		self.lastSync:set(os.time())
		self.lastSyncKind:set(
			(kind == "SyncChanges" or kind == "SyncDetails") and "Server"
				or kind == "SyncbackChanges" and "Client"
				or "None"
		)

		if kind == "SyncDetails" then
			project:set(data)
		elseif self.core.project then
			-- Source reconciliation mutates Core.project's generation in place.
			-- Give Fusion a new table identity so open project UI refreshes too.
			project:set(table.clone(self.core.project))
		end
	end)

	self.core:run():catch(function(err)
		loading = false
		local message = err.message or tostring(err)
		local hardRestartRequired = err.kind == "RestartRequired"
			or string.find(message, "hard restart", 1, true) ~= nil

		if Core.wasExitGraceful(err) or canceled then
			self:disconnect()
			self:home()
			return
		end

		if err.kind == "ManagedReload" then
			self:disconnect()
			task.wait(RECONNECT_INTERVAL)
			self:connect()
			return
		end

		if hardRestartRequired then
			task.defer(function()
				error(`[Carbon] {message}`, 0)
			end)
		else
			warn(`[Carbon] {message}`)
		end

		self:setPage(Error {
			App = self,
			Message = message,
		})

		self:disconnect()

		if Config:get("AutoReconnect") and not hardRestartRequired then
			task.wait(RECONNECT_INTERVAL)

			if self.lastUpdate + RECONNECT_INTERVAL <= os.clock() then
				self:connect()
			end
		end
	end)
end

function App:disconnect(cancel: boolean?)
	self.lastUpdate = os.clock()

	if self.core then
		if cancel then
			self.onCanceled:Fire()
		end

		self.core:stop()
		self.core = nil
	end
	ManagedConnectionMarker.destroy(self.connectionMarker)
	self.connectionMarker = nil
end

function App:isConnected()
	return self.core and self.core.status == Core.Status.Connected
end

function App:setPort(port: number)
	self.port = port
end

return App
