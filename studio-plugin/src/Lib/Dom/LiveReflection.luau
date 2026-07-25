--!strict

local LiveReflection = {}

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
	Color3uint8 = true,
	ColorSequence = true,
	Content = true,
	ContentId = true,
	Faces = true,
	Font = true,
	NumberRange = true,
	MaterialColors = true,
	NetAssetRef = true,
	NumberSequence = true,
	PhysicalProperties = true,
	Ray = true,
	Rect = true,
	Region3 = true,
	Region3int16 = true,
	SharedString = true,
	SecurityCapabilities = true,
	UDim = true,
	UDim2 = true,
	UniqueId = true,
	Vector2 = true,
	Vector2int16 = true,
	Vector3 = true,
	Vector3int16 = true,
	OptionalCoordinateFrame = "OptionalCFrame",
}

local classTags = {
	Deprecated = true,
	NotBrowsable = true,
	NotCreatable = true,
	NotReplicated = true,
	PlayerReplicated = true,
	Service = true,
	Settings = true,
	UserSettings = true,
}

local propertyTags = {
	Deprecated = true,
	Hidden = true,
	NotBrowsable = true,
	NotReplicated = true,
	NotScriptable = true,
	ReadOnly = true,
	WriteOnly = true,
}

local function filterTags(tags, supported)
	local result = {}
	for _, tag in tags or {} do
		if supported[tag] then
			table.insert(result, tag)
		end
	end
	return result
end

local function convertType(valueType, skippedTypes)
	if valueType.Category == "Enum" then
		return { Enum = valueType.Name }
	elseif valueType.Category == "Class" then
		return { Value = "Ref" }
	elseif valueType.Category == "Primitive" then
		local name = primitiveTypes[valueType.Name]
		if name ~= nil then
			return { Value = name }
		end
	elseif valueType.Category == "DataType" then
		local mapped = dataTypes[valueType.Name]
		if mapped ~= nil then
			return { Value = if type(mapped) == "string" then mapped else valueType.Name }
		end
	end
	local qualified = `{valueType.Category}.{valueType.Name}`
	skippedTypes[qualified] = true
	return nil
end

local function hasTag(tags, expected: string): boolean
	for _, tag in tags or {} do
		if tag == expected then
			return true
		end
	end
	return false
end

local function securityLevel(security, direction: string): string
	if type(security) == "string" then
		return security
	end
	return security[direction]
end

local function accessible(level: string): boolean
	return level == "None" or level == "PluginSecurity"
end

local function scriptability(member): string
	local tags = member.Tags or {}
	if hasTag(tags, "NotScriptable") then
		return "None"
	end
	local canRead = accessible(securityLevel(member.Security, "Read"))
	local canWrite = not hasTag(tags, "ReadOnly") and accessible(securityLevel(member.Security, "Write"))
	if canRead and canWrite then
		return "ReadWrite"
	elseif canRead then
		return "Read"
	elseif canWrite then
		return "Write"
	end
	return "None"
end

local function roundTrips(member): boolean
	local serialization = member.Serialization
	return serialization ~= nil and serialization.CanLoad == true and serialization.CanSave == true
end

local function synchronizedKind(existing, serializes: boolean)
	local inferred = if serializes then "Serializes" else "DoesNotSerialize"
	if existing == nil then
		return { Canonical = { Serialization = inferred } }
	end
	local kind = existing.Kind
	if kind.Alias ~= nil then
		return kind
	end
	local canonical = kind.Canonical
	if canonical == nil or type(canonical.Serialization) == "string" then
		return { Canonical = { Serialization = inferred } }
	end
	return kind
end

function LiveReflection.fromApi(api, version)
	assert(type(api) == "table" and type(api.Classes) == "table" and type(api.Enums) == "table")
	assert(type(version) == "table" and #version == 4, "reflection version must have four components")

	local skippedTypes = {}
	local skippedProperties = {}
	local overlay = {
		version = version,
		classes = {},
		enums = {},
	}
	for _, apiClass in api.Classes do
		local class = {
			superclass = if apiClass.Superclass == "<<<ROOT>>>" then nil else apiClass.Superclass,
			tags = filterTags(apiClass.Tags, classTags),
			properties = {},
		}
		overlay.classes[apiClass.Name] = class
		for _, member in apiClass.Members do
			if member.MemberType ~= "Property" then
				continue
			end
			local dataType = convertType(member.ValueType, skippedTypes)
			if dataType == nil then
				local qualified = `{member.ValueType.Category}.{member.ValueType.Name}`
				skippedProperties[`{apiClass.Name}.{member.Name}`] = {
					className = apiClass.Name,
					propertyName = member.Name,
					dataType = qualified,
					serializes = roundTrips(member),
				}
			end
			if dataType ~= nil then
				class.properties[member.Name] = {
					dataType = dataType,
					scriptability = scriptability(member),
					tags = filterTags(member.Tags, propertyTags),
					serializes = roundTrips(member),
				}
			end
		end
	end
	for _, apiEnum in api.Enums do
		local items = {}
		for _, item in apiEnum.Items do
			items[item.Name] = item.Value
		end
		overlay.enums[apiEnum.Name] = { items = items }
	end
	return overlay, skippedTypes, skippedProperties
end

function LiveReflection.applyOverlay(database, overlay)
	assert(type(database) == "table" and type(database.Classes) == "table" and type(database.Enums) == "table")
	local addedClasses = 0
	local addedProperties = 0
	local synchronizedProperties = 0

	for className, current in overlay.classes do
		local class = database.Classes[className]
		if class == nil then
			class = {
				Name = className,
				Superclass = current.superclass,
				Tags = current.tags,
				DefaultProperties = {},
				Properties = {},
			}
			database.Classes[className] = class
			addedClasses += 1
		else
			class.Name = className
			class.Superclass = current.superclass
			class.Tags = current.tags
			class.DefaultProperties = class.DefaultProperties or {}
			class.Properties = class.Properties or {}
		end
		for propertyName, property in current.properties do
			local existing = class.Properties[propertyName]
			class.Properties[propertyName] = {
				Name = propertyName,
				DataType = if existing == nil then property.dataType else existing.DataType,
				Scriptability = if existing ~= nil and existing.Scriptability == "Custom"
					then "Custom"
					else property.scriptability,
				Tags = property.tags,
				Kind = synchronizedKind(existing, property.serializes),
			}
			if existing == nil then
				addedProperties += 1
			else
				synchronizedProperties += 1
			end
		end
	end

	local enums = {}
	for enumName, current in overlay.enums do
		enums[enumName] = { name = enumName, items = current.items }
	end
	database.Enums = enums
	database.Version = overlay.version
	return {
		addedClasses = addedClasses,
		addedProperties = addedProperties,
		synchronizedProperties = synchronizedProperties,
	}
end

function LiveReflection.apply(database, api, version)
	local overlay, skippedTypes, skippedProperties = LiveReflection.fromApi(api, version)
	local result = LiveReflection.applyOverlay(database, overlay)
	result.skippedTypes = skippedTypes
	result.skippedProperties = {}
	for path, skipped in skippedProperties do
		local class = database.Classes[skipped.className]
		if skipped.serializes and (class == nil or class.Properties[skipped.propertyName] == nil) then
			result.skippedProperties[path] = skipped.dataType
		end
	end
	return result
end

return LiveReflection
