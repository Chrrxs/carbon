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

-- Start the quick-save test without yielding the caller; the test call itself
-- yields until the test ends. `report` receives the reason Studio could not
-- start it, so Carbon falls back to auto-recovery without waiting.
function QuickSave.trigger(
	studioTestService: any,
	token: string,
	report: (reason: string) -> (),
	spawn: ((() -> ()) -> ())?
): boolean
	local readable, editModeActive = pcall(function()
		return studioTestService.EditModeActive
	end)
	if readable and editModeActive == false then
		report("Studio is already running a test session")
		return false
	end
	local run = spawn or task.spawn
	run(function()
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
