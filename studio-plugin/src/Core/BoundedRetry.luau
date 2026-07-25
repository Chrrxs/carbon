--!strict

local MAX_ATTEMPTS = 10
local BASE_DELAY_SECONDS = 0.25
local MAX_DELAY_SECONDS = 2

local BoundedRetry = {}

local function delayForAttempt(attempt: number): number
	return math.min(BASE_DELAY_SECONDS * 2 ^ (attempt - 1), MAX_DELAY_SECONDS)
end

function BoundedRetry.run(fetch: () -> any, wait: (number) -> ()): (boolean, any)
	local lastError = nil
	for attempt = 1, MAX_ATTEMPTS do
		local success, result = pcall(fetch)
		if success then
			return true, result
		end
		lastError = result
		if attempt < MAX_ATTEMPTS then
			wait(delayForAttempt(attempt))
		end
	end
	return false, lastError
end

function BoundedRetry.runUntil(fetch: () -> any, wait: (number) -> (), shouldContinue: () -> boolean): (boolean, any)
	local lastError = nil
	local attempt = 1
	while shouldContinue() do
		local success, result = pcall(fetch)
		if success then
			return true, result
		end
		lastError = result
		wait(delayForAttempt(attempt))
		attempt += 1
	end
	return false, lastError
end

return BoundedRetry
