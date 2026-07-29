--!strict

-- Reflection-driven policy for serialized engine state. The database is the
-- source of truth: new canonical non-scriptable serialized properties become
-- protected automatically until the active privileged adapter reports support.

local BINARY_TYPES = {
	BinaryString = true,
	SharedString = true,
	ContentId = true,
	NetAssetRef = true,
	OptionalCFrame = true,
	UniqueId = true,
}

local ALWAYS_MATERIALIZED_TYPES = {
	-- CapabilityControl can reject writes even when API security reports None.
	-- Native serialized access preserves the persisted value consistently.
	SecurityCapabilities = true,
}

local RUNTIME_CACHE_PROPERTIES = {
	-- Recomputed from CSG geometry on load; Studio does not retain the serialized
	-- authoring-time count and exposes zero while the union remains unchanged.
	["PartOperation.TriangleCount"] = true,
}

local NON_GAMEPLAY_IDENTITY_PROPERTIES = {
	-- Studio assigns this opaque identifier when a LuaSourceContainer is
	-- materialized. It is not user-authored gameplay state, and feeding the
	-- generated value back after a filesystem script addition creates a false
	-- mapped-domain mutation.
	["LuaSourceContainer.ScriptGuid"] = true,
}

local SYNTHETIC_ALIASES = {
	-- The writable binary CollisionGroupData field is the persisted source of
	-- truth. CollisionGroups is an engine-only write alias for the same state.
	["Workspace.CollisionGroups"] = true,
}

local PERSISTENT_READ_ONLY_PROPERTIES = {
	["Chat.LoadDefaultChat"] = true,
	["HttpService.HttpEnabled"] = true,
	["Lighting.LightingStyle"] = true,
	["Lighting.PrioritizeLightingQuality"] = true,
	["MeshPart.HasJointOffset"] = true,
	["MeshPart.HasSkinnedMesh"] = true,
	["MeshPart.JointOffset"] = true,
	["MeshPart.MeshContent"] = true,
	["PackageLink.DefaultName"] = true,
	["PackageLink.PackageContent"] = true,
	["Players.MaxPlayers"] = true,
	["Players.PreferredPlayers"] = true,
	["StarterPlayer.AllowCustomAnimations"] = true,
	["TextChatService.ChatVersion"] = true,
}

local function serializes(propertyData): boolean
	local canonical = propertyData.Kind.Canonical
	if canonical == nil then
		return false
	end
	local serialization = canonical.Serialization
	return serialization == "Serializes" or (typeof(serialization) == "table" and serialization.SerializesAs ~= nil)
end

local ProtectedProperties = {}

function ProtectedProperties.isPersistentReadOnly(className: string, propertyName: string): boolean
	return PERSISTENT_READ_ONLY_PROPERTIES[`{className}.{propertyName}`] == true
end

function ProtectedProperties.requiresMaterialization(dataType: string): boolean
	return ALWAYS_MATERIALIZED_TYPES[dataType] == true
end

function ProtectedProperties.exclusionReason(className: string, propertyName: string, propertyData): string?
	local dataTypeKey, dataTypeValue = next(propertyData.DataType)
	local dataType = if dataTypeKey == "Enum" then "Enum" else dataTypeValue
	if dataType == "UniqueId" then
		return "non-gameplay identity"
	elseif NON_GAMEPLAY_IDENTITY_PROPERTIES[`{className}.{propertyName}`] then
		return "non-gameplay identity"
	end
	if RUNTIME_CACHE_PROPERTIES[`{className}.{propertyName}`] then
		return "runtime cache"
	elseif SYNTHETIC_ALIASES[`{className}.{propertyName}`] then
		return "synthetic alias"
	end
	return nil
end

local function candidatesForClass(database, className: string): { any }
	local descriptors = {}
	local seen = {}
	local currentClassName: string? = className

	while currentClassName ~= nil do
		local class = database.Classes[currentClassName]
		if class == nil then
			break
		end
		for propertyName, propertyData in class.Properties do
			if seen[propertyName] then
				continue
			end
			seen[propertyName] = true
			local dataTypeKey, dataTypeValue = next(propertyData.DataType)
			local dataType = if dataTypeKey == "Enum" then "Enum" else dataTypeValue
			local protectedScriptability = ProtectedProperties.requiresMaterialization(dataType)
				or propertyData.Scriptability == "None"
				or propertyData.Scriptability == "Write"
				or (propertyData.Scriptability == "Custom" and dataType == "OptionalCFrame")
				or (
					(propertyData.Scriptability == "Read" or propertyData.Scriptability == "Custom")
					and ProtectedProperties.isPersistentReadOnly(currentClassName, propertyName)
				)
			local candidate = propertyData.Kind.Canonical ~= nil
				and protectedScriptability
				and serializes(propertyData)
				and ProtectedProperties.exclusionReason(currentClassName, propertyName, propertyData) == nil
			if candidate then
				table.insert(descriptors, {
					className = currentClassName,
					name = propertyName,
					dataType = dataType,
					scriptability = propertyData.Scriptability,
					serializes = true,
				})
			end
		end
		currentClassName = class.Superclass
	end

	table.sort(descriptors, function(left, right)
		return left.name < right.name
	end)
	return descriptors
end

function ProtectedProperties.forClass(database, className: string, supports: ((any) -> boolean)?): { string }
	local properties = {}
	for _, descriptor in candidatesForClass(database, className) do
		if supports == nil or not supports(descriptor) then
			table.insert(properties, descriptor.name)
		end
	end
	return properties
end

function ProtectedProperties.supportedForClass(database, className: string, supports: (any) -> boolean): { any }
	local properties = {}
	for _, descriptor in candidatesForClass(database, className) do
		if supports(descriptor) then
			table.insert(properties, descriptor)
		end
	end
	return properties
end

ProtectedProperties.binaryTypes = BINARY_TYPES
ProtectedProperties.excludedNames = {}
return ProtectedProperties
