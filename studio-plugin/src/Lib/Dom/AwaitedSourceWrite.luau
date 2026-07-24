--!strict

local AwaitedSourceWrite = {}

function AwaitedSourceWrite.run(updateSourceAsync, value: string): (boolean, any?)
	local success, reason = pcall(updateSourceAsync, function()
		return value
	end)
	return success, if success then nil else reason
end

return AwaitedSourceWrite
