--!strict

-- Roblox applies a shared HttpService request budget to Studio plugins. Keep
-- exact-build metadata reconciliation well below that budget and recover when
-- another plugin temporarily consumes it.

local RequestPolicy = {}

RequestPolicy.FULL_SOURCE_PAGE_INSTANCES = 4096
RequestPolicy.METADATA_SOURCE_PAGE_INSTANCES = 32768

function RequestPolicy.sourcePageInstances(metadataOnly: boolean?): number
	return if metadataOnly == true
		then RequestPolicy.METADATA_SOURCE_PAGE_INSTANCES
		else RequestPolicy.FULL_SOURCE_PAGE_INSTANCES
end

function RequestPolicy.retryDelay(message: any, attempt: number): number?
	if string.find(tostring(message), "Number of requests exceeded limit", 1, true) == nil then
		return nil
	end
	return math.min(2 ^ math.min(attempt, 5), 30)
end

return RequestPolicy
