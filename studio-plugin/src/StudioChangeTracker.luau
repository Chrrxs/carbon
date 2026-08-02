--!strict

local StudioChangeTracker = {}
StudioChangeTracker.__index = StudioChangeTracker

function StudioChangeTracker.new(changeHistoryService: any, httpService: any, workspaceInstance: Instance, writeToken)
	return setmetatable({
		changeHistoryService = changeHistoryService,
		httpService = httpService,
		workspace = workspaceInstance,
		writeToken = writeToken,
		generation = "",
		connections = {},
		started = false,
	}, StudioChangeTracker)
end

function StudioChangeTracker:advance()
	self.generation = self.httpService:GenerateGUID(false)
	self.writeToken(self.workspace, "__StudioWorktree_StudioChangeGeneration", self.generation)
end

function StudioChangeTracker:start()
	if self.started then
		return
	end
	self.started = true
	table.insert(
		self.connections,
		self.changeHistoryService.OnRecordingStarted:Connect(function()
			self:advance()
		end)
	)
	table.insert(
		self.connections,
		self.changeHistoryService.OnUndo:Connect(function()
			self:advance()
		end)
	)
	table.insert(
		self.connections,
		self.changeHistoryService.OnRedo:Connect(function()
			self:advance()
		end)
	)
	self:advance()
end

function StudioChangeTracker:current(): string
	return self.generation
end

function StudioChangeTracker:stop()
	for _, connection in self.connections do
		connection:Disconnect()
	end
	table.clear(self.connections)
	self.started = false
end

return StudioChangeTracker
