--!strict

local fs = require("@lune/fs")

local app = fs.readFile("src/App/init.luau")
local assets = fs.readFile("src/App/Assets.json")
local client = fs.readFile("src/Client/init.luau")
local config = fs.readFile("src/Config.luau")
local core = fs.readFile("src/Core/init.luau")
local help = fs.readFile("src/App/Widgets/Help.luau")
local notConnected = fs.readFile("src/App/Pages/NotConnected.luau")
local scriptDiff = fs.readFile("src/App/Widgets/ScriptDiff.luau")
local settings = fs.readFile("src/App/Widgets/Settings.luau")

for _, assetPath in { "assets/carbon-wordmark.png", "assets/carbon-icon.png" } do
	assert(not fs.isFile(assetPath), `{assetPath} must not ship after image branding is removed`)
end

assert(
	not string.find(assets, '"Carbon"', 1, true)
		and not string.find(assets, "116137219167140", 1, true)
		and not string.find(assets, "121423847213303", 1, true),
	"the plugin asset manifest must contain no Carbon image branding"
)
assert(
	string.find(app, 'Name = "Carbon",', 1, true)
		and not string.find(app, 'Name = "Dervex Tools",', 1, true)
		and string.find(app, 'Image = ""', 1, true)
		and not string.find(app, "Assets.Carbon", 1, true)
		and not string.find(app, "local Image = require(Components.Image)", 1, true)
		and not string.find(app, "Image {", 1, true),
	"the toolbar and widget header must contain no Carbon image branding"
)

for _, obsoleteSetting in { "Host", "Https", "DiffLinesLimit", "DisplayPrompts", "ChangesThreshold" } do
	assert(
		not string.find(config, `"{obsoleteSetting}"`, 1, true)
			and not string.find(settings, `"{obsoleteSetting}"`, 1, true),
		`obsolete setting {obsoleteSetting} must not remain user-configurable`
	)
end
assert(
	string.find(config, '"Port"', 1, true)
		and string.find(config, '"AutoReconnect"', 1, true)
		and string.find(config, '"LogLevel"', 1, true),
	"Carbon must retain the port, reconnect, and logging controls"
)
assert(
	string.find(config, "hadObsoleteSetting", 1, true)
		and string.find(config, "plugin:SetSetting(configKey, if next(config) then config else nil)", 1, true),
	"loading settings must prune obsolete keys persisted by older plugin builds"
)
assert(
	string.find(core, 'Client.new(host or Config.HOST, port or Config:get("Port"))', 1, true)
		and string.find(client, 'return `http://{self.host}:{self.port}/{endpoint or ""}`', 1, true)
		and not string.find(client, "self.https", 1, true),
	"the Studio client must always use Carbon's loopback HTTP contract"
)
assert(
	not string.find(notConnected, "hostInput", 1, true)
		and not string.find(notConnected, "filterHost", 1, true)
		and not fs.isFile("src/App/Components/Util/filterHost.luau")
		and not fs.isFile("src/App/Components/Util/filterNumber.luau")
		and string.find(notConnected, 'PlaceholderText = Config:getDefault("Port")', 1, true),
	"manual attachment must expose only Carbon's configurable port"
)
assert(
	string.find(scriptDiff, "local DIFF_LINES_LIMIT = 3000", 1, true)
		and not string.find(scriptDiff, 'Config:get("DiffLinesLimit")', 1, true),
	"the internal diff safety limit must not remain a user-facing setting"
)
assert(
	string.find(help, "Wait for Auto-Capture", 1, true)
		and string.find(help, "filesystem-authoritative", 1, true)
		and not string.find(help, "Two-Way Sync", 1, true),
	"help must describe Carbon's current one-way mapping and automatic/manual capture model"
)

print("PASS: front-facing Studio plugin exposes Carbon's current settings without image branding")
