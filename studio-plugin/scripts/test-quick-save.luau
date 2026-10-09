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

-- An idle edit Studio starts one Server & Clients test whose Start Server
-- action writes the save, tagged with Carbon's request token.
local idle = newStudioTestService(true)
local reports = {}
assert(
	QuickSave.trigger(idle, "token-1", function(reason)
		table.insert(reports, reason)
	end, immediate),
	"an idle edit Studio must start the quick-save test"
)
assert(#idle.executions == 1, "exactly one quick-save test must start")
assert(idle.executions[1].numPlayers == 1, "StudioTestService requires at least one test client")
assert(idle.executions[1].args == "carbon-quick-save:token-1", "the test must carry Carbon's request token")
assert(#reports == 0, "a started quick save is not a failure")

-- A running playtest cannot start another test; serve must fall back at once.
local playing = newStudioTestService(false)
reports = {}
assert(not QuickSave.trigger(playing, "token-2", function(reason)
	table.insert(reports, reason)
end, immediate), "a running test session cannot quick-save")
assert(#playing.executions == 0, "no test may start during another test session")
assert(#reports == 1 and string.find(reports[1], "already running", 1, true), "the failure must be reported")

-- Studio errors from the test API are reported instead of escaping the plugin.
local rejecting = newStudioTestService(true, "Only one multiplayer test session can run at a time")
reports = {}
assert(QuickSave.trigger(rejecting, "token-3", function(reason)
	table.insert(reports, reason)
end, immediate))
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

print("PASS: Studio quick saves start only from an idle edit Studio and end only Carbon's test server")
