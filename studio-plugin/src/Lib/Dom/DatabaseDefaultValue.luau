--!strict

local DatabaseDefaultValue = {}

function DatabaseDefaultValue.decode(
	default: { [string]: any },
	decodeBase64: (string) -> string,
	uniqueIdFromHex: (string) -> buffer
): { [string]: any }
	if type(default.BinaryString) == "string" then
		return { BinaryString = buffer.fromstring(decodeBase64(default.BinaryString)) }
	elseif type(default.SharedString) == "string" then
		return { SharedString = decodeBase64(default.SharedString) }
	elseif type(default.UniqueId) == "string" then
		return { UniqueId = uniqueIdFromHex(default.UniqueId) }
	end
	return default
end

return DatabaseDefaultValue
