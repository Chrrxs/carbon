--!strict

local InstanceIdentity = {}

local MCP_PLACE_ID_ATTRIBUTE = "__MCPPlaceId"

function InstanceIdentity.compute(placeId: number, serverStorage: any?, httpService: any): string
	if placeId ~= 0 then
		return `place:{placeId}`
	end

	local existing = if serverStorage == nil then nil else serverStorage:GetAttribute(MCP_PLACE_ID_ATTRIBUTE)
	if type(existing) == "string" and existing ~= "" then
		return `anon:{existing}`
	end

	return `anon:{httpService:GenerateGUID(false)}`
end

function InstanceIdentity.observeChanges(
	placeId: number,
	serverStorage: any?,
	initialIdentity: string,
	onChange: (string) -> ()
): any
	if placeId ~= 0 or serverStorage == nil then
		return {
			Disconnect = function() end,
		}
	end

	local currentIdentity = initialIdentity
	local function refresh()
		local stored = serverStorage:GetAttribute(MCP_PLACE_ID_ATTRIBUTE)
		if type(stored) ~= "string" or stored == "" then
			return
		end
		local nextIdentity = `anon:{stored}`
		if nextIdentity == currentIdentity then
			return
		end
		currentIdentity = nextIdentity
		onChange(nextIdentity)
	end
	local connection = serverStorage:GetAttributeChangedSignal(MCP_PLACE_ID_ATTRIBUTE):Connect(refresh)
	-- Close the race between the initial identity read and signal connection.
	refresh()
	return connection
end

return InstanceIdentity
