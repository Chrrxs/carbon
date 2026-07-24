--!strict

local function formatError(message: string, values: { any }, stringify: (any) -> string): string
	for i, value in values do
		local replacement = stringify(value)
		message = message:gsub("$" .. i, function()
			return replacement
		end)
	end

	return message
end

return formatError
