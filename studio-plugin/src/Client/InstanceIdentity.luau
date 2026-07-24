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

return InstanceIdentity
