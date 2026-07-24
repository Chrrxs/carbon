--!strict

local DefaultComparison = {}

function DefaultComparison.matches(instanceValue, encodedDefault, decode, equals, tree): boolean
	local success, defaultValue = decode(encodedDefault, tree)
	return success and equals(instanceValue, defaultValue)
end

return DefaultComparison
