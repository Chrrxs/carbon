local Carbon = script:FindFirstAncestor("Carbon")
local RuntimeServices = require(Carbon.RuntimeServices)
local ContentProvider = RuntimeServices.get("ContentProvider") :: ContentProvider

local Util = require(Carbon.Util)
local Log = require(Carbon.Log)

local Assets = require(script.Parent.Assets)

local Loader = {}

function Loader.assets(): { string }
	return Util.dictionaryToArray(Assets)
end

function Loader.load()
	local assets = Loader.assets()

	Log.trace(`Loading {#assets} assets..`)

	task.spawn(function()
		ContentProvider:PreloadAsync(assets)
	end)
end

return Loader
