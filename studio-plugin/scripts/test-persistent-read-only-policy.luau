--!strict

local ProtectedProperties = require("../src/Lib/Dom/ProtectedProperties")
local database = require("../src/Lib/Dom/database")

local materializedTypes = {
	Bool = true,
	Content = true,
	Enum = true,
	Int32 = true,
	String = true,
	Vector3 = true,
}

local persistent = {
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

for _, pair in persistent do
	assert(
		ProtectedProperties.isPersistentReadOnly(pair[1], pair[2]),
		`{pair[1]}.{pair[2]} must use native engine materialization`
	)
	local property = database.Classes[pair[1]].Properties[pair[2]]
	local dataTypeKey, dataTypeValue = next(property.DataType)
	local dataType = if dataTypeKey == "Enum" then "Enum" else dataTypeValue
	assert(property.Scriptability == "Read", `{pair[1]}.{pair[2]} must remain Scriptability=Read`)
	assert(materializedTypes[dataType], `{pair[1]}.{pair[2]} has unsupported materialized type {dataType}`)
	assert(
		table.find(ProtectedProperties.forClass(database, pair[1]), pair[2]) ~= nil,
		`{pair[1]}.{pair[2]} must remain a protected serialized-property candidate`
	)
end

for _, pair in
	{
		{ "AudioDeviceInput", "Active" },
		{ "AudioRecorder", "IsRecording" },
		{ "GamepadService", "GamepadCursorEnabled" },
		{ "PartOperation", "TriangleCount" },
		{ "NetworkSettings", "HttpProxyEnabled" },
		{ "UserGameSettings", "VignetteEnabled" },
		{ "Folder", "LoadDefaultChat" },
	}
do
	assert(
		not ProtectedProperties.isPersistentReadOnly(pair[1], pair[2]),
		`{pair[1]}.{pair[2]} must not silently enter the place-persistent allowlist`
	)
end

print("PASS: only audited place-persistent read-only properties use scalar materialization")
