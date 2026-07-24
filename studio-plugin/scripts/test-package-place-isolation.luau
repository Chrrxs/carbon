--!strict

local fs = require("@lune/fs")
local Signal = require("../src/Signal")

local source = fs.readFile("src/Signal.luau")
local appSource = fs.readFile("src/App/init.luau")

assert(
	not string.find(source, "game.ReplicatedStorage", 1, true)
		and not string.find(source, "GetService('ReplicatedStorage')", 1, true)
		and not string.find(source, 'GetService("ReplicatedStorage")', 1, true),
	"bundled packages must not assume a canonical ReplicatedStorage name or materialize the service"
)
assert(
	not string.find(source, "require(game", 1, true),
	"bundled package behavior must not be controlled by an arbitrary user-place config ModuleScript"
)
assert(
	string.find(source, "then yieldable else true", 1, true)
		and string.find(source, "then keepOrder else false", 1, true),
	"Signal must retain deterministic plugin-local defaults"
)
assert(
	string.find(appSource, "require(Carbon.Signal)", 1, true) and not string.find(appSource, "Packages.Signal", 1, true),
	"production code must not route back through the place-coupled external package"
)

local signal = Signal.new(false)
local fired = 0
signal:Bind(function(value)
	fired += value
end)
signal:Once(function(value)
	fired += value * 10
end)
signal:Fire(1)
signal:Fire(1)
assert(fired == 12, "plugin-local Signal must preserve Bind and Once behavior")

print("PASS: bundled packages are isolated from renamed or adversarial place services")
