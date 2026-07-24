--!strict

local fs = require("@lune/fs")
local process = require("@lune/process")
local serde = require("@lune/serde")

local MsgPack = require("../src/Lib/MsgPack")

assert(string.byte(MsgPack.encode(MsgPack.Map.new())) == 0x80, "empty property maps must use MessagePack map wire type")
assert(string.byte(MsgPack.encode({})) == 0x90, "ordinary empty tables remain MessagePack arrays")

local rootSnapshotsRequest = MsgPack.encode({
	rootSourceIds = MsgPack.Map.new(),
	sourceInstanceIds = MsgPack.Map.new(),
})
for _, field in { "rootSourceIds", "sourceInstanceIds" } do
	local keyStart, keyEnd = string.find(rootSnapshotsRequest, field, 1, true)
	assert(keyStart ~= nil and keyEnd ~= nil, `root snapshot request omitted {field}`)
	assert(
		string.byte(rootSnapshotsRequest, keyEnd + 1) == 0x80,
		`empty root snapshot {field} must use MessagePack map wire type`
	)
end

for _, fixture in
	{
		{ decimal = "9223372036854775684", signed = true, marker = 0xD3 },
		{ decimal = "-9223372036854775808", signed = true, marker = 0xD3 },
		{ decimal = "17996771963439872", signed = false, marker = 0xCF },
		{ decimal = "18446744073709551615", signed = false, marker = 0xCF },
	}
do
	local exact = MsgPack.Int64.fromDecimal(fixture.decimal, fixture.signed)
	assert(MsgPack.Int64.toDecimal(exact) == fixture.decimal, `Int64 decimal drifted: {fixture.decimal}`)
	local encoded = MsgPack.encode(exact)
	assert(string.byte(encoded) == fixture.marker, "Int64 wrapper used the wrong MessagePack marker")
	local decoded = MsgPack.decode(encoded)
	assert(MsgPack.Int64.toDecimal(decoded) == fixture.decimal, `Int64 wire drifted: {fixture.decimal}`)
end

local databasePath = process.args[1] or "src/Lib/Dom/database.msgpack"
local database = MsgPack.decode(fs.readFile(databasePath))
local allValues = serde.decode("json", fs.readFile("src/Lib/Dom/allValues.json"))

local function findProperty(className: string, propertyName: string): any
	local currentClassName: string? = className
	while currentClassName ~= nil do
		local class = database.Classes[currentClassName]
		assert(class ~= nil, `missing reflection class {currentClassName}`)
		if class.Properties[propertyName] ~= nil then
			return class.Properties[propertyName]
		end
		currentClassName = class.Superclass
	end
	return nil
end

local function assertCanonicalContent(className: string, propertyName: string)
	local property = findProperty(className, propertyName)
	assert(property ~= nil, `{className}.{propertyName} is missing from the plugin reflection database`)
	assert(
		property.DataType.Value == "Content",
		`{className}.{propertyName} must use Content, got {property.DataType.Value}`
	)
	local canonical = property.Kind.Canonical
	assert(canonical ~= nil, `{className}.{propertyName} must be canonical`)
	assert(canonical.Serialization == "Serializes", `{className}.{propertyName} must serialize`)
end

for _, className in { "ParticleEmitter", "Beam", "Trail" } do
	assertCanonicalContent(className, "TextureContent")
end

assertCanonicalContent("SpecialMesh", "MeshContent")
assertCanonicalContent("SpecialMesh", "TextureContent")

local physical = allValues["PhysicalProperties-Custom"].value.PhysicalProperties
assert(physical.Custom ~= nil, "PhysicalProperties custom values must use the canonical { Custom = ... } wire shape")
assert(physical.density == nil, "legacy unwrapped PhysicalProperties wire values are forbidden")

print("PASS: plugin reflection and value fixtures use Carbon's canonical wire schema")
