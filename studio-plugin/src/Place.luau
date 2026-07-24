--!strict
local Carbon = script:FindFirstAncestor("Carbon")
local RuntimeServices = require(Carbon.RuntimeServices)
local MarketPlaceService = RuntimeServices.get("MarketplaceService") :: MarketplaceService

local name = game.Name
local isFile = (name:find(".rbxl") or name:find(".rbxlx")) ~= nil

if not isFile then
	pcall(function()
		name = MarketPlaceService:GetProductInfo(game.PlaceId).Name
	end)
end

return {
	Name = name,
	IsFile = isFile,
	Version = game.PlaceVersion,
}
