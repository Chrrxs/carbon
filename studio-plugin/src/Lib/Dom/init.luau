local Carbon = script:FindFirstAncestor("Carbon")

local RuntimeServices = require(Carbon.RuntimeServices)
local HttpService = RuntimeServices.get("HttpService") :: HttpService

local TransientMetadata = require(Carbon.TransientMetadata)
local database: { [string]: any } = {}
local reflectionInitialized = false
local defaultPropertiesCache = {}

local function getDatabase()
	assert(reflectionInitialized, "reflection database is not initialized")
	return database
end

local Error = require(script.Error)
local EncodedValue = require(script.EncodedValue)
local ExactSerializedAccess = require(script.ExactSerializedAccess)
local PropertyDescriptor = require(script.PropertyDescriptor)
local ProtectedProperties = require(script.ProtectedProperties)
local PrivilegedAccess = require(script.PrivilegedAccess)
local PrivilegedReadPlan = require(script.PrivilegedReadPlan)
local PrivilegedValueCodec = require(script.PrivilegedValueCodec)
local LiveReflection = require(script.LiveReflection)

local function reflectionVersion(): string
	return table.concat(getDatabase().Version, ".")
end

local function applyReflection(version: { number }, apiDump: string | { [string]: any }, studioVersion: string)
	assert(type(version) == "table" and #version == 4, "reflection version must be a four-component array")
	local expectedVersion = table.concat(version, ".")
	assert(
		studioVersion == expectedVersion,
		`Unsupported Studio reflection version {studioVersion}; this Carbon plugin requires exactly {expectedVersion}`
	)

	local api: { [string]: any }
	if type(apiDump) == "string" then
		local success, decoded = pcall(function()
			return HttpService:JSONDecode(apiDump)
		end)
		assert(success and type(decoded) == "table", `failed to JSON-decode reflection API dump: {tostring(decoded)}`)
		api = decoded
	elseif type(apiDump) == "table" then
		api = apiDump
	else
		error("reflection apiDump must be a string or decoded table", 2)
	end

	assert(
		type(api.Classes) == "table" and type(api.Enums) == "table",
		"invalid API dump: missing Classes or Enums table"
	)

	local nextDatabase = {}
	local result = LiveReflection.apply(nextDatabase, api, version)
	database = nextDatabase
	reflectionInitialized = true
	table.clear(defaultPropertiesCache)
	return result
end

local function assertReflectionVersion(studioVersion: string)
	local expected = reflectionVersion()
	assert(
		studioVersion == expected,
		`Unsupported Studio reflection version {studioVersion}; this Carbon plugin requires exactly {expected}`
	)
end

local function findCanonicalPropertyDescriptor(className, propertyName)
	local currentClassName = className

	repeat
		local currentClass = getDatabase().Classes[currentClassName]

		if currentClass == nil then
			return currentClass
		end

		local propertyData = currentClass.Properties[propertyName]
		if propertyData ~= nil then
			local canonicalData = propertyData.Kind.Canonical
			if canonicalData ~= nil then
				local descriptor = PropertyDescriptor.fromRaw(propertyData, currentClassName, propertyName)
				descriptor.privilegedExcluded = ProtectedProperties.exclusionReason(
					currentClassName,
					propertyName,
					propertyData
				) ~= nil
				return descriptor
			end

			local aliasData = propertyData.Kind.Alias
			if aliasData ~= nil then
				local canonicalProperty = currentClass.Properties[aliasData.AliasFor]
				local descriptor = PropertyDescriptor.fromRaw(canonicalProperty, currentClassName, aliasData.AliasFor)
				descriptor.privilegedExcluded = ProtectedProperties.exclusionReason(
					currentClassName,
					aliasData.AliasFor,
					canonicalProperty
				) ~= nil
				return descriptor
			end

			return nil
		end

		currentClassName = currentClass.Superclass
	until currentClassName == nil

	return nil
end

local function deriveRuntimeDefaults(className, properties)
	local createSuccess, defaultInstance = pcall(Instance.new, className)
	if not createSuccess then
		return
	end

	local seen = {}
	local currentClassName = className
	while currentClassName ~= nil do
		local class = getDatabase().Classes[currentClassName]
		if class == nil then
			break
		end
		for propertyName, propertyData in class.Properties do
			if seen[propertyName] then
				continue
			end
			seen[propertyName] = true
			if propertyName == "Name" or propertyName == "Parent" or properties[propertyName] ~= nil then
				continue
			end
			if propertyData.Kind.Canonical == nil then
				continue
			end
			local descriptor = findCanonicalPropertyDescriptor(className, propertyName)
			if
				descriptor == nil
				or not descriptor.serializes
				or not descriptor:canRead()
				or descriptor.dataType == "Ref"
				or descriptor.privilegedExcluded
			then
				continue
			end
			local readSuccess, value = descriptor:read(defaultInstance)
			if not readSuccess then
				continue
			end
			local encodeSuccess, encoded = EncodedValue.encode(value, descriptor.dataType)
			if encodeSuccess then
				properties[propertyName] = encoded
			end
		end
		currentClassName = class.Superclass
	end

	defaultInstance:Destroy()
end

local function getDefaultProperties(className)
	if defaultPropertiesCache[className] then
		return defaultPropertiesCache[className]
	end

	local class = getDatabase().Classes[className]
	local properties = {}

	if not class then
		return properties
	end
	local protected = {}
	for _, property in
		ProtectedProperties.forClass(getDatabase(), className, function(descriptor)
			return PrivilegedAccess.supportsDescriptor(descriptor)
				or PrivilegedAccess.supportsMaterializedDescriptor(descriptor)
				or PrivilegedAccess.supportsReferenceDescriptor(descriptor)
		end)
	do
		protected[property] = true
	end

	deriveRuntimeDefaults(className, properties)
	for property in protected do
		properties[property] = nil
	end
	local rawDescriptors = {}
	for property in properties do
		local descriptor = findCanonicalPropertyDescriptor(className, property)
		local rawType = descriptor and PrivilegedAccess.rawType(descriptor)
		if descriptor ~= nil and rawType ~= nil then
			local _, databaseDefault = next(properties[property])
			local localDefault = PrivilegedValueCodec.databaseDefaultToCanonical(rawType, databaseDefault)
			if localDefault ~= nil then
				properties[property] = { BinaryString = buffer.fromstring(localDefault) }
			else
				table.insert(rawDescriptors, {
					className = descriptor.className,
					name = descriptor.name,
					dataType = rawType,
				})
			end
		end
	end
	if #rawDescriptors > 0 then
		local success, exactDefaults = PrivilegedAccess.readDefaults(className, rawDescriptors)
		if not success then
			error(`Failed to read exact engine defaults for {className}: {tostring(exactDefaults)}`)
		end
		for property, raw in exactDefaults do
			properties[property] = { BinaryString = buffer.fromstring(raw) }
		end
	end
	defaultPropertiesCache[className] = properties

	return properties
end

local function canWriteProperty(className, propertyName)
	local descriptor = findCanonicalPropertyDescriptor(className, propertyName)
	return descriptor ~= nil
		and (
			descriptor:canWrite()
			or PrivilegedAccess.supportsDescriptor(descriptor)
			or PrivilegedAccess.supportsMaterializedDescriptor(descriptor)
			or PrivilegedAccess.supportsReferenceDescriptor(descriptor)
		)
end

local function canonicalPropertyName(className: string, propertyName: string): string
	local descriptor = findCanonicalPropertyDescriptor(className, propertyName)
	return if descriptor then descriptor.name else propertyName
end

local function getRefProperties(className)
	local properties = {}
	local seen = {}

	local currentClassName = className

	while currentClassName ~= nil do
		local currentClass = getDatabase().Classes[currentClassName]

		if currentClass == nil then
			break
		end

		for property, propertyData in pairs(currentClass.Properties) do
			if seen[property] or property == "Parent" then
				continue
			end

			seen[property] = true

			local descriptor = findCanonicalPropertyDescriptor(className, property)
			if
				descriptor ~= nil
				and descriptor.dataType == "Ref"
				and descriptor.serializes
				and not descriptor.privilegedExcluded
				and (
					descriptor.scriptability == "ReadWrite"
					or PrivilegedAccess.supportsReferenceDescriptor(descriptor)
				)
			then
				table.insert(properties, property)
			end
		end

		currentClassName = currentClass.Superclass
	end

	return properties
end

local function getProtectedSerializedProperties(className)
	return ProtectedProperties.forClass(getDatabase(), className, function(descriptor)
		return PrivilegedAccess.supportsDescriptor(descriptor)
			or PrivilegedAccess.supportsMaterializedDescriptor(descriptor)
			or PrivilegedAccess.supportsReferenceDescriptor(descriptor)
	end)
end

local function getPrivilegedSerializedProperties(className)
	return ProtectedProperties.supportedForClass(getDatabase(), className, function(descriptor)
		return PrivilegedAccess.supportsDescriptor(descriptor)
			or PrivilegedAccess.supportsMaterializedDescriptor(descriptor)
	end)
end

return {
	getDatabase = getDatabase,
	reflectionVersion = reflectionVersion,
	applyReflection = applyReflection,
	assertReflectionVersion = assertReflectionVersion,
	findCanonicalPropertyDescriptor = findCanonicalPropertyDescriptor,
	getDefaultProperties = getDefaultProperties,
	canWriteProperty = canWriteProperty,
	canonicalPropertyName = canonicalPropertyName,
	getRefProperties = getRefProperties,
	getProtectedSerializedProperties = getProtectedSerializedProperties,
	getPrivilegedSerializedProperties = getPrivilegedSerializedProperties,
}
