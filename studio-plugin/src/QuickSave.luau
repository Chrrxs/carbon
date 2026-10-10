--!strict

-- Studio's Start Server action writes the complete edit DataModel with Studio's
-- own place saver before it launches a local test server. Carbon starts that
-- test to capture the place on demand. The serve process stops the test server
-- once the save exists; a capture test that still starts ends itself here.

local QuickSave = {}

QuickSave.TEST_ARGS_PREFIX = "carbon-quick-save:"
QuickSave.END_RESULT = "carbon-quick-save-complete"

function QuickSave.testArgs(token: string): string
	return QuickSave.TEST_ARGS_PREFIX .. token
end

function QuickSave.isCaptureTest(args: any): boolean
	return type(args) == "string"
		and #args > #QuickSave.TEST_ARGS_PREFIX
		and string.sub(args, 1, #QuickSave.TEST_ARGS_PREFIX) == QuickSave.TEST_ARGS_PREFIX
end

-- How long the edit Studio waits for a running playtest to end before Carbon
-- falls back to auto-recovery.
QuickSave.PLAYTEST_STOP_TIMEOUT = 15
local EDIT_MODE_POLL = 0.05

export type TriggerOptions = {
	-- Asks the running playtest's server to end it. Yields until serve has the
	-- request and returns whether it does, with the reason when it does not.
	stopPlaytest: (() -> (boolean, string?))?,
	spawn: ((() -> ()) -> ())?,
	wait: ((seconds: number) -> ())?,
	clock: (() -> number)?,
}

local function editModeActive(studioTestService: any): boolean
	local readable, active = pcall(function()
		return studioTestService.EditModeActive
	end)
	return not readable or active ~= false
end

-- End a running playtest so Studio can quick-save. Only the playtest's server
-- can call EndTest, so this asks it through serve and waits for edit mode.
local function endRunningPlaytest(studioTestService: any, options: TriggerOptions): string?
	if options.stopPlaytest == nil then
		return "Studio is already running a test session"
	end
	local requested, reason = options.stopPlaytest()
	if not requested then
		return `could not stop the running playtest: {reason or "unknown error"}`
	end
	local wait = options.wait or task.wait
	local clock = options.clock or os.clock
	local deadline = clock() + QuickSave.PLAYTEST_STOP_TIMEOUT
	while not editModeActive(studioTestService) do
		if clock() >= deadline then
			return `the running playtest did not stop within {QuickSave.PLAYTEST_STOP_TIMEOUT} seconds`
		end
		wait(EDIT_MODE_POLL)
	end
	return nil
end

-- Start the quick-save test without yielding the caller; the test call itself
-- yields until the test ends. A running playtest is stopped first. `report`
-- receives the reason Studio could not save, so Carbon falls back to
-- auto-recovery without waiting.
function QuickSave.trigger(
	studioTestService: any,
	token: string,
	report: (reason: string) -> (),
	options: TriggerOptions?
): boolean
	local resolved: TriggerOptions = options or {}
	local run = resolved.spawn or task.spawn
	run(function()
		if not editModeActive(studioTestService) then
			local failure = endRunningPlaytest(studioTestService, resolved)
			if failure ~= nil then
				report(failure)
				return
			end
		end
		local started, err = pcall(function()
			return studioTestService:ExecuteMultiplayerTestAsync(1, QuickSave.testArgs(token))
		end)
		if not started then
			report(tostring(err))
		end
	end)
	return true
end

-- Called in a local test server: end the test when Carbon started it only for
-- its save. Returns whether this was a Carbon quick-save test.
function QuickSave.endCaptureTest(studioTestService: any): boolean
	local readable, args = pcall(function()
		return studioTestService:GetTestArgs()
	end)
	if not readable or not QuickSave.isCaptureTest(args) then
		return false
	end
	pcall(function()
		studioTestService:EndTest(QuickSave.END_RESULT)
	end)
	return true
end

return QuickSave
