--!strict

local MAX_ATTEMPTS = 4
local BASE_DELAY_SECONDS = 0.25

local PrivilegedNegotiation = {}

function PrivilegedNegotiation.run(fetch: () -> any, wait: (number) -> ()): (boolean, any)
	local lastError = nil
	for attempt = 1, MAX_ATTEMPTS do
		local success, result = pcall(fetch)
		if success then
			return true, result
		end
		lastError = result
		if attempt < MAX_ATTEMPTS then
			wait(BASE_DELAY_SECONDS * 2 ^ (attempt - 1))
		end
	end
	return false, lastError
end

return PrivilegedNegotiation
