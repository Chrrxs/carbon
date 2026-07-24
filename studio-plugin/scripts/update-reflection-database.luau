--!strict

local fs = require("@lune/fs")
local process = require("@lune/process")
local serde = require("@lune/serde")

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

local primitiveTypes = {
	bool = "Bool",
	double = "Float64",
	float = "Float32",
	int = "Int32",
	int64 = "Int64",
	string = "String",
}

local dataTypes = {
	Axes = true,
	BinaryString = true,
	BrickColor = true,
	CFrame = true,
	Color3 = true,
	ColorSequence = true,
	Content = true,
	ContentId = true,
	Faces = true,
	Font = true,
	NumberRange = true,
	NumberSequence = true,
	PhysicalProperties = true,
	Ray = true,
	Rect = true,
	Region3int16 = true,
	SecurityCapabilities = true,
	UDim = true,
	UDim2 = true,
	UniqueId = true,
	Vector2 = true,
	Vector3 = true,
}

local skippedTypes = {}

local function convertType(valueType)
	if valueType.Category == "Enum" then
		return { Enum = valueType.Name }
	elseif valueType.Category == "Class" then
		return { Value = "Ref" }
	elseif valueType.Category == "Primitive" then
		local name = primitiveTypes[valueType.Name]
		assert(name ~= nil, `unsupported API primitive property type {valueType.Name}`)
		return { Value = name }
	elseif valueType.Category == "DataType" and dataTypes[valueType.Name] then
		return { Value = valueType.Name }
	end
	local qualified = `{valueType.Category}.{valueType.Name}`
	skippedTypes[qualified] = true
	return nil
end

local function securityLevel(security, direction: string): string
	if type(security) == "string" then
		return security
	end
	return security[direction]
end

local function scriptability(member): string
	local canRead = securityLevel(member.Security, "Read") == "None"
	local canWrite = securityLevel(member.Security, "Write") == "None"
	if canRead and canWrite then
		return "ReadWrite"
	elseif canRead then
		return "Read"
	elseif canWrite then
		return "Write"
	end
	return "None"
end

local addedClasses = 0
local addedProperties = 0
for _, apiClass in api.Classes do
	local class = database.Classes[apiClass.Name]
	if class == nil then
		class = {
			Name = apiClass.Name,
			Superclass = if apiClass.Superclass == "<<<ROOT>>>" then nil else apiClass.Superclass,
			Tags = apiClass.Tags or {},
			DefaultProperties = {},
			Properties = {},
		}
		database.Classes[apiClass.Name] = class
		addedClasses += 1
	end

	for _, member in apiClass.Members do
		if member.MemberType ~= "Property" or class.Properties[member.Name] ~= nil then
			continue
		end
		local serialization = member.Serialization
		local serializes = serialization ~= nil and serialization.CanLoad == true and serialization.CanSave == true
		if not serializes then
			continue
		end
		local dataType = convertType(member.ValueType)
		if dataType == nil then
			continue
		end
		class.Properties[member.Name] = {
			Name = member.Name,
			DataType = dataType,
			Scriptability = scriptability(member),
			Tags = member.Tags or {},
			Kind = {
				Canonical = {
					Serialization = "Serializes",
				},
			},
		}
		addedProperties += 1
	end
end

for _, apiEnum in api.Enums do
	local items = {}
	for _, item in apiEnum.Items do
		items[item.Name] = item.Value
	end
	database.Enums[apiEnum.Name] = {
		name = apiEnum.Name,
		items = items,
	}
end

database.Version = version
fs.writeFile("src/Lib/Dom/database.msgpack", MsgPack.encode(database))
print(`Updated reflection database to {versionText}: {addedClasses} classes, {addedProperties} properties added`)
for qualified in skippedTypes do
	print(`Skipped unsupported non-place reflection type {qualified}`)
end
