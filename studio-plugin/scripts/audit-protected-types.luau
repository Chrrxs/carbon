--!strict

local fs = require("@lune/fs")
local process = require("@lune/process")

local MsgPack = require("../src/Lib/MsgPack")
local ProtectedProperties = require("../src/Lib/Dom/ProtectedProperties")

local database = MsgPack.decode(fs.readFile("src/Lib/Dom/database.msgpack"))
local counts = {}
local examples = {}
local properties = {}

for className in database.Classes do
	for _, descriptor in
		ProtectedProperties.supportedForClass(database, className, function()
			return true
		end)
	do
		counts[descriptor.dataType] = (counts[descriptor.dataType] or 0) + 1
		examples[descriptor.dataType] = examples[descriptor.dataType] or `{className}.{descriptor.name}`
		properties[descriptor.dataType] = properties[descriptor.dataType] or {}
		table.insert(properties[descriptor.dataType], `{className}.{descriptor.name}`)
	end
end

local types = {}
for dataType in counts do
	table.insert(types, dataType)
end
table.sort(types)

for _, dataType in types do
	if process.args[2] ~= nil and process.args[2] ~= dataType then
		continue
	end
	print(`{dataType}\t{counts[dataType]}\t{examples[dataType]}`)
	if process.args[1] == "--list" then
		table.sort(properties[dataType])
		for _, property in properties[dataType] do
			print(`  {property}`)
		end
	end
end
