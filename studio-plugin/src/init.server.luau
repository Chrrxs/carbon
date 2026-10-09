--!strict
local RuntimeServices = require(script.RuntimeServices)
local RunService = RuntimeServices.get("RunService") :: RunService

if RunService:IsRunning() then
	if RunService:IsServer() then
		-- A local test server started for a Carbon quick save has no further use;
		-- the edit Studio's save already exists.
		local QuickSave = require(script.QuickSave)
		local StudioTestService = RuntimeServices.get("StudioTestService")
		task.spawn(QuickSave.endCaptureTest, StudioTestService)
	end
	return
end

local Config = require(script.Config)
local Log = require(script.Log)
local App = require(script.App)

-- App and its bundled dependencies have now finished their synchronous module
-- initialization. Any new root they caused is runtime tool state, not place data.
RuntimeServices.sealBootstrap()

Config.load()
Log.__setLevel(Config:get("LogLevel"))
App.new()

Config:onChanged("LogLevel", function(level)
	Log.__setLevel(level)
end)
