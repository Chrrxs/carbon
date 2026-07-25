--!strict

local fs = require("@lune/fs")

local MsgPack = require("../src/Lib/MsgPack")

local database = MsgPack.decode(fs.readFile("src/Lib/Dom/database.msgpack"))
assert(
	MsgPack.encodeCanonical(database) == fs.readFile("src/Lib/Dom/database.msgpack"),
	"bundled reflection MessagePack must use deterministic canonical map ordering"
)
local required = {
	{ className = "Instance", propertyName = "Capabilities", dataType = "SecurityCapabilities" },
	{ className = "Lighting", propertyName = "LightingStyle", dataType = "LightingStyle", enum = true },
	{ className = "Model", propertyName = "LevelOfDetail", dataType = "ModelLevelOfDetail", enum = true },
}

for _, expected in required do
	local class = assert(database.Classes[expected.className], `missing reflection class {expected.className}`)
	local descriptor = assert(
		class.Properties[expected.propertyName],
		`missing current reflection property {expected.className}.{expected.propertyName}`
	)
	local dataType = if expected.enum == true then descriptor.DataType.Enum else descriptor.DataType.Value
	assert(
		dataType == expected.dataType,
		`wrong data type for {expected.className}.{expected.propertyName}: {tostring(dataType)}`
	)
	assert(
		descriptor.Kind.Canonical.Serialization == "Serializes",
		`{expected.className}.{expected.propertyName} must round-trip`
	)
end

assert(table.concat(database.Version, ".") == "0.731.0.7310942", "bundled reflection version is stale")
print("PASS: bundled reflection contains the current Studio serialized surface")
