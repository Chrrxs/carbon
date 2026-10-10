--!strict

local QuickSave = require("../src/QuickSave")

local function immediate(callback: () -> ())
	callback()
end

local function newStudioTestService(editModeActive: boolean, executeError: string?)
	local service = {
		EditModeActive = editModeActive,
		executions = {},
		testArgs = nil :: any,
		ended = {},
	}
	function service:ExecuteMultiplayerTestAsync(numPlayers: number, args: any)
		table.insert(self.executions, { numPlayers = numPlayers, args = args })
		if executeError ~= nil then
			error(executeError, 0)
		end
		return nil
	end
	function service:GetTestArgs()
		return self.testArgs
	end
	function service:EndTest(value: any)
		table.insert(self.ended, value)
	end
	return service
end

local function options(overrides: { [string]: any }?): any
	local result: { [string]: any } = { spawn = immediate }
	for key, value in overrides or {} do
		result[key] = value
	end
	return result
end

-- An idle edit Studio starts one Server & Clients test whose Start Server
-- action writes the save, tagged with Carbon's request token.
local idle = newStudioTestService(true)
local reports = {}
local stopRequests = 0
assert(
	QuickSave.trigger(
		idle,
		"token-1",
		function(reason)
			table.insert(reports, reason)
		end,
		options({
			stopPlaytest = function()
				stopRequests += 1
				return true, nil
			end,
		})
	),
	"an idle edit Studio must start the quick-save test"
)
assert(#idle.executions == 1, "exactly one quick-save test must start")
assert(idle.executions[1].numPlayers == 1, "StudioTestService requires at least one test client")
assert(idle.executions[1].args == "carbon-quick-save:token-1", "the test must carry Carbon's request token")
assert(#reports == 0, "a started quick save is not a failure")
assert(stopRequests == 0, "an idle Studio has no playtest to stop")

-- A running playtest is stopped first, then the quick save starts once Studio
-- is back in edit mode.
local playing = newStudioTestService(false)
local now = 0
reports = {}
stopRequests = 0
assert(
	QuickSave.trigger(
		playing,
		"token-2",
		function(reason)
			table.insert(reports, reason)
		end,
		options({
			stopPlaytest = function()
				stopRequests += 1
				return true, nil
			end,
			clock = function()
				return now
			end,
			wait = function(seconds: number)
				now += seconds
				if now >= 0.5 then
					playing.EditModeActive = true
				end
			end,
		})
	),
	"a running playtest must not prevent the quick save"
)
assert(stopRequests == 1, "the running playtest must be stopped exactly once")
assert(#reports == 0, "a stopped playtest is not a failure: " .. tostring(reports[1]))
assert(#playing.executions == 1, "the quick save must start after the playtest ends")
assert(playing.executions[1].args == "carbon-quick-save:token-2")

-- A playtest that never ends falls back to auto-recovery instead of waiting forever.
local stuck = newStudioTestService(false)
now = 0
reports = {}
QuickSave.trigger(
	stuck,
	"token-3",
	function(reason)
		table.insert(reports, reason)
	end,
	options({
		stopPlaytest = function()
			return true, nil
		end,
		clock = function()
			return now
		end,
		wait = function(seconds: number)
			now += seconds
		end,
	})
)
assert(#stuck.executions == 0, "no quick save may start while the playtest still runs")
assert(#reports == 1 and string.find(reports[1], "did not stop", 1, true), "the stuck playtest must be reported")

-- Without a way to stop the playtest, or when the request fails, serve falls back at once.
local unmanaged = newStudioTestService(false)
reports = {}
QuickSave.trigger(unmanaged, "token-4", function(reason)
	table.insert(reports, reason)
end, options())
assert(#unmanaged.executions == 0)
assert(#reports == 1 and string.find(reports[1], "already running", 1, true), "the failure must be reported")

local refused = newStudioTestService(false)
reports = {}
QuickSave.trigger(
	refused,
	"token-5",
	function(reason)
		table.insert(reports, reason)
	end,
	options({
		stopPlaytest = function()
			return false, "serve is unreachable"
		end,
	})
)
assert(#refused.executions == 0)
assert(#reports == 1 and string.find(reports[1], "serve is unreachable", 1, true), "the stop failure must be reported")

-- Studio errors from the test API are reported instead of escaping the plugin.
local rejecting = newStudioTestService(true, "Only one multiplayer test session can run at a time")
reports = {}
assert(QuickSave.trigger(rejecting, "token-6", function(reason)
	table.insert(reports, reason)
end, options()))
assert(
	#reports == 1 and string.find(reports[1], "Only one multiplayer test", 1, true),
	"a Studio test API error must reach serve"
)

-- The test server ends only tests that Carbon started for a quick save.
local captureServer = newStudioTestService(false)
captureServer.testArgs = "carbon-quick-save:token-1"
assert(QuickSave.endCaptureTest(captureServer), "a Carbon quick-save test server must end itself")
assert(captureServer.ended[1] == QuickSave.END_RESULT, "the capture test must end with Carbon's result")

for _, args in { nil, "", "carbon-quick-save:", "LobbySpawnTest", { "carbon-quick-save:token" } } do
	local userServer = newStudioTestService(false)
	userServer.testArgs = args
	assert(not QuickSave.endCaptureTest(userServer), "a user-started test must keep running")
	assert(#userServer.ended == 0, "a user-started test must never be ended")
end

local unavailable = newStudioTestService(false)
function unavailable:GetTestArgs()
	error("GetTestArgs is unavailable outside a test session", 0)
end
assert(not QuickSave.endCaptureTest(unavailable), "missing test arguments are not a Carbon test")

print("PASS: Studio quick saves stop a running playtest first and end only Carbon's test server")
