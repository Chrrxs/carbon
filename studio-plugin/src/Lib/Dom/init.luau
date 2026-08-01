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
local PropertyDescriptor = require(script.PropertyDescriptor)
local ProtectedProperties = require(script.ProtectedProperties)
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
				descriptor.protectedExcluded = ProtectedProperties.exclusionReason(
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
				descriptor.protectedExcluded = ProtectedProperties.exclusionReason(
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
				or descriptor.protectedExcluded
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
	for _, property in ProtectedProperties.forClass(getDatabase(), className) do
		protected[property] = true
	end

	deriveRuntimeDefaults(className, properties)
	for property in protected do
		properties[property] = nil
	end
	defaultPropertiesCache[className] = properties

	return properties
end

local function canWriteProperty(className, propertyName)
	local descriptor = findCanonicalPropertyDescriptor(className, propertyName)
	return descriptor ~= nil and descriptor:canWrite()
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
				and not descriptor.protectedExcluded
				and descriptor.scriptability == "ReadWrite"
			then
				table.insert(properties, property)
			end
		end

		currentClassName = currentClass.Superclass
	end

	return properties
end

local function getProtectedSerializedProperties(className)
	return ProtectedProperties.forClass(getDatabase(), className)
end

local function isCreatable(className)
	local class = getDatabase().Classes[className]

	if not class then
		return false
	end

	return (table.find(class.Tags, "NotCreatable") or table.find(class.Tags, "Service")) == nil
end

local function isService(className: string): boolean
	local class = getDatabase().Classes[className]
	return class ~= nil and table.find(class.Tags, "Service") ~= nil
end

local function hasClass(className: string): boolean
	return getDatabase().Classes[className] ~= nil
end

local function isRemovable(className: string): boolean
	return not isService(className)
end

local function isStudioTransient(instance: Instance): boolean
	-- Workspace.CurrentCamera is Studio's edit camera, not authored place state.
	-- It is deliberately absent from Carbon's source tree, so it must also be
	-- absent from sibling coordinates reported for authored Workspace children.
	-- This is identity-based: an ordinary authored Camera beside it remains
	-- serializable regardless of its name.
	if instance.Parent == workspace and instance == workspace.CurrentCamera then
		return true
	end
	local current = instance
	while current.Parent ~= nil and current.Parent ~= game do
		local parent = current.Parent
		if TransientMetadata.isInstance(current.ClassName, current.Name, parent.ClassName, current.Archivable) then
			return true
		end
		current = parent
	end
	return current.Parent ~= nil
		and TransientMetadata.isInstance(current.ClassName, current.Name, current.Parent.ClassName, current.Archivable)
end

local function readProperty(instance, propertyName)
	-- Workspace.CurrentCamera in edit mode points at Studio's transient camera.
	-- It is not place-authored state and its target is deliberately absent from
	-- the capture tree, so represent it as the serialized nil/default reference.
	if instance == workspace and propertyName == "CurrentCamera" then
		local currentCamera = workspace.CurrentCamera
		if currentCamera ~= nil and isStudioTransient(currentCamera) then
			return true, nil
		end
	end

	local descriptor = findCanonicalPropertyDescriptor(instance.ClassName, propertyName)

	if descriptor == nil then
		local fullName = ("%s.%s"):format(instance.ClassName, propertyName)

		return false, Error.new(Error.Kind.UnknownProperty, fullName)
	end

	return descriptor:read(instance)
end

local function writeProperty(instance, propertyName, value)
	local descriptor = findCanonicalPropertyDescriptor(instance.ClassName, propertyName)

	if descriptor == nil then
		local fullName = ("%s.%s"):format(instance.ClassName, propertyName)

		return false, Error.new(Error.Kind.UnknownProperty, fullName)
	end

	return descriptor:write(instance, value)
end

return {
	getDatabase = getDatabase,
	hasClass = hasClass,
	isCreatable = isCreatable,
	isService = isService,
	isRemovable = isRemovable,
	isStudioTransient = isStudioTransient,
	readProperty = readProperty,
	writeProperty = writeProperty,
	canWriteProperty = canWriteProperty,
	canonicalPropertyName = canonicalPropertyName,
	findCanonicalPropertyDescriptor = findCanonicalPropertyDescriptor,
	getDefaultProperties = getDefaultProperties,
	getRefProperties = getRefProperties,
	getProtectedSerializedProperties = getProtectedSerializedProperties,
	reflectionVersion = reflectionVersion,
	assertReflectionVersion = assertReflectionVersion,
	applyReflection = applyReflection,
	isRawType = function(_dataType)
		return false
	end,
	Error = Error,
	EncodedValue = EncodedValue,
}
