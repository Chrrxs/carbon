--!strict

local fs = require("@lune/fs")
local process = require("@lune/process")
local serde = require("@lune/serde")

local MsgPack = require("../src/Lib/MsgPack")
local Update = require("./reflection-database-update")

local apiPath = process.args[1]
local versionText = process.args[2]
assert(
	apiPath ~= nil and versionText ~= nil,
	"usage: lune run scripts/update-reflection-database.luau <API-Dump.json> <four-component-version>"
)

local version = {}
for component in string.gmatch(versionText, "[^.]+") do
	local value = tonumber(component)
	assert(value ~= nil and value % 1 == 0, `invalid reflection version {versionText}`)
	table.insert(version, value)
end
assert(#version == 4, `reflection version must have four components: {versionText}`)

local databasePath = "src/Lib/Dom/database.msgpack"
local overlayPath = "src/Lib/Dom/reflection.json"
local database = MsgPack.decode(fs.readFile(databasePath))
local api = serde.decode("json", fs.readFile(apiPath))
local overlay, skippedTypes = Update.fromApi(api, version)
local result = Update.applyOverlay(database, overlay)

local encodedOverlay = serde.encode("json", overlay)
encodedOverlay = string.gsub(encodedOverlay, '"tags":{}', '"tags":[]')
fs.writeFile(overlayPath, encodedOverlay)
fs.writeFile(databasePath, MsgPack.encode(database))
print(
	`Updated reflection database to {versionText}: {result.addedClasses} classes and {result.addedProperties} properties added; {result.synchronizedProperties} properties synchronized`
)
for qualified in skippedTypes do
	print(`Skipped unsupported reflection type {qualified}`)
end
