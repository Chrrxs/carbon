--!strict

local StudioChangeTracker = require("../src/StudioChangeTracker")

local function newSignal()
	local listeners = {}
	local signal = {}
	function signal:Connect(callback)
		table.insert(listeners, callback)
		local connected = true
		return {
			Disconnect = function()
				if not connected then
					return
				end
				connected = false
				local index = table.find(listeners, callback)
				if index ~= nil then
					table.remove(listeners, index)
				end
			end,
		}
	end
	function signal:fire()
		for _, listener in table.clone(listeners) do
			listener()
		end
	end
	return signal
end

local recordingStarted = newSignal()
local undo = newSignal()
local redo = newSignal()
local changeHistoryService = {
	OnRecordingStarted = recordingStarted,
	OnUndo = undo,
	OnRedo = redo,
}

local sequence = 0
local httpService = {}
function httpService:GenerateGUID(_wrap)
	sequence += 1
	return `studio-generation-{sequence}`
end

local writes = {}
local workspaceInstance = {}
local tracker = StudioChangeTracker.new(
	changeHistoryService,
	httpService,
	workspaceInstance,
	function(instance, name, value)
		assert(instance == workspaceInstance, "the generation marker must be written to Workspace")
		assert(name == "__StudioWorktree_StudioChangeGeneration", "the generation marker name changed")
		table.insert(writes, value)
	end
)

tracker:start()
tracker:start()
assert(tracker:current() == "studio-generation-1", "startup must establish one clean-state generation")
assert(#writes == 1, "starting an active tracker twice must not rotate the generation")

recordingStarted:fire()
assert(tracker:current() == "studio-generation-2", "a new ChangeHistory recording must mark Studio dirty")
undo:fire()
assert(tracker:current() == "studio-generation-3", "undo must mark Studio dirty")
redo:fire()
assert(tracker:current() == "studio-generation-4", "redo must mark Studio dirty")
assert(#writes == 4 and writes[4] == tracker:current(), "every generation must be embedded in recovery metadata")

tracker:stop()
recordingStarted:fire()
undo:fire()
redo:fire()
assert(tracker:current() == "studio-generation-4", "a stopped tracker must ignore later Studio events")

print("PASS: Studio change generations distinguish idle shutdown from uncaptured live edits")
