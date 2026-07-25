--!strict

local fs = require("@lune/fs")
local process = require("@lune/process")
local serde = require("@lune/serde")

local LiveReflection = require("../src/Lib/Dom/LiveReflection")
local MsgPack = require("../src/Lib/MsgPack")

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

local database = MsgPack.decode(fs.readFile("src/Lib/Dom/database.msgpack"))
local api = serde.decode("json", fs.readFile(apiPath))
local result = LiveReflection.apply(database, api, version)

fs.writeFile("src/Lib/Dom/database.msgpack", MsgPack.encodeCanonical(database))

local skippedTypes = {}
for dataType in result.skippedTypes do
	table.insert(skippedTypes, dataType)
end
table.sort(skippedTypes)
for _, dataType in skippedTypes do
	print(`Skipped unsupported non-place reflection type {dataType}`)
end

print(
	`Updated reflection database to {versionText}: {result.addedClasses} classes, {result.addedProperties} properties added`
)
