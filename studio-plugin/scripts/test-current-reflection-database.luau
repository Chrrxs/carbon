--!strict

local fs = require("@lune/fs")
local process = require("@lune/process")
local serde = require("@lune/serde")

local LiveReflection = require("../src/Lib/Dom/LiveReflection")
local ProtectedProperties = require("../src/Lib/Dom/ProtectedProperties")

local apiPath = assert(process.args[1], "usage: test-current-reflection-database.luau <API-Dump.json> <version>")
local versionText = assert(process.args[2], "missing Studio version")
local major, minor, patch, build = string.match(versionText, "^(%d+)%.(%d+)%.(%d+)%.(%d+)$")
assert(major ~= nil and minor ~= nil and patch ~= nil and build ~= nil, `invalid Studio version {versionText}`)
local version = {
	assert(tonumber(major)),
	assert(tonumber(minor)),
	assert(tonumber(patch)),
	assert(tonumber(build)),
}

local api = serde.decode("json", fs.readFile(apiPath))
local database = {}
local result = LiveReflection.apply(database, api, version)
assert(result.addedClasses > 0 and result.addedProperties > 0, "live reflection did not construct a database")
assert(table.concat(database.Version, ".") == versionText, "live reflection version drifted")

local required = {
	{ className = "Instance", propertyName = "Capabilities", dataType = "SecurityCapabilities" },
	{ className = "Lighting", propertyName = "LightingStyle", dataType = "LightingStyle", enum = true },
	{ className = "Model", propertyName = "LevelOfDetail", dataType = "ModelLevelOfDetail", enum = true },
}
for _, expected in required do
	local class = assert(database.Classes[expected.className], `missing reflection class {expected.className}`)
	local descriptor =
		assert(class.Properties[expected.propertyName], `missing {expected.className}.{expected.propertyName}`)
	local dataType = if expected.enum then descriptor.DataType.Enum else descriptor.DataType.Value
	assert(dataType == expected.dataType, `wrong data type for {expected.className}.{expected.propertyName}`)
	assert(
		descriptor.Kind.Canonical.Serialization == "Serializes",
		`{expected.className}.{expected.propertyName} must round-trip`
	)
end

local attributes = database.Classes.Instance.Properties.Attributes
assert(
	attributes.DataType.Value == "Attributes" and attributes.Scriptability == "Custom",
	"Attributes policy was not applied"
)
assert(attributes.Kind.Canonical.Serialization.SerializesAs == "AttributesSerialize", "Attributes wire alias was lost")
assert(database.Classes.BasePart.Properties.Color3uint8.Kind.Alias.AliasFor == "Color", "legacy aliases were lost")

local materializedTypes = {
	Bool = true,
	Content = true,
	Enum = true,
	Int32 = true,
	String = true,
	Vector3 = true,
}
for _, pair in
	{
		{ "Chat", "LoadDefaultChat" },
		{ "HttpService", "HttpEnabled" },
		{ "Lighting", "LightingStyle" },
		{ "Lighting", "PrioritizeLightingQuality" },
		{ "MeshPart", "HasJointOffset" },
		{ "MeshPart", "HasSkinnedMesh" },
		{ "MeshPart", "JointOffset" },
		{ "MeshPart", "MeshContent" },
		{ "PackageLink", "DefaultName" },
		{ "PackageLink", "PackageContent" },
		{ "Players", "MaxPlayers" },
		{ "Players", "PreferredPlayers" },
		{ "StarterPlayer", "AllowCustomAnimations" },
		{ "TextChatService", "ChatVersion" },
	}
do
	local property = database.Classes[pair[1]].Properties[pair[2]]
	local dataTypeKey, dataTypeValue = next(property.DataType)
	local dataType = if dataTypeKey == "Enum" then "Enum" else dataTypeValue
	assert(
		property.Scriptability == "Read" or property.Scriptability == "Custom",
		`{pair[1]}.{pair[2]} must remain readable`
	)
	assert(materializedTypes[dataType], `{pair[1]}.{pair[2]} has unsupported materialized type {dataType}`)
	assert(
		table.find(ProtectedProperties.forClass(database, pair[1]), pair[2]) ~= nil,
		`{pair[1]}.{pair[2]} must remain a protected serialized-property candidate`
	)
end

print(`PASS: built live reflection {versionText} from the current API dump`)
