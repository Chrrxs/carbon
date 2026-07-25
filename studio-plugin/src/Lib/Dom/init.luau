local Carbon = script:FindFirstAncestor("Carbon")

local RuntimeServices = require(Carbon.RuntimeServices)
local HttpService = RuntimeServices.get("HttpService") :: HttpService

local TransientMetadata = require(Carbon.TransientMetadata)
local Base64 = require(Carbon.Lib.Base64)
local database = require(script.database)
local DatabaseDefaultValue = require(script.DatabaseDefaultValue)
local Error = require(script.Error)
local EncodedValue = require(script.EncodedValue)
local ExactSerializedAccess = require(script.ExactSerializedAccess)
local PropertyDescriptor = require(script.PropertyDescriptor)
local ProtectedProperties = require(script.ProtectedProperties)
local PrivilegedAccess = require(script.PrivilegedAccess)
local PrivilegedReadPlan = require(script.PrivilegedReadPlan)
local PrivilegedValueCodec = require(script.PrivilegedValueCodec)
local LiveReflection = require(script.LiveReflection)

local defaultPropertiesCache = {}

local function reflectionVersion(): string
	return table.concat(database.Version, ".")
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

	local result = LiveReflection.apply(database, api, version)
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
		local currentClass = database.Classes[currentClassName]

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
		local class = database.Classes[currentClassName]
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

	local class = database.Classes[className]
	local properties = {}

	if not class then
		return properties
	end
	local protected = {}
	for _, property in
		ProtectedProperties.forClass(database, className, function(descriptor)
			return PrivilegedAccess.supportsDescriptor(descriptor)
				or PrivilegedAccess.supportsMaterializedDescriptor(descriptor)
				or PrivilegedAccess.supportsReferenceDescriptor(descriptor)
		end)
	do
		protected[property] = true
	end

	for property, default in pairs(class.DefaultProperties) do
		local descriptor = findCanonicalPropertyDescriptor(className, property)

		if
			descriptor
			and descriptor.serializes
			and not protected[property]
			and (descriptor:canRead() or PrivilegedAccess.supportsDescriptor(descriptor) or PrivilegedAccess.supportsReferenceDescriptor(
				descriptor
			))
			and not descriptor.privilegedExcluded
			and next(default)
		then
			properties[property] = DatabaseDefaultValue.decode(default, Base64.decode, EncodedValue.uniqueIdFromHex)
		end
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
		local currentClass = database.Classes[currentClassName]

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
	return ProtectedProperties.forClass(database, className, function(descriptor)
		return PrivilegedAccess.supportsDescriptor(descriptor)
			or PrivilegedAccess.supportsMaterializedDescriptor(descriptor)
			or PrivilegedAccess.supportsReferenceDescriptor(descriptor)
	end)
end

local function getPrivilegedSerializedProperties(className)
	return ProtectedProperties.supportedForClass(database, className, function(descriptor)
		return PrivilegedAccess.supportsDescriptor(descriptor)
			or PrivilegedAccess.supportsMaterializedDescriptor(descriptor)
	end)
end

local function getBatchPrivilegedSerializedProperties(className)
	return ProtectedProperties.supportedForClass(database, className, PrivilegedAccess.supportsDescriptor)
end

local function isCreatable(className)
	local class = database.Classes[className]

	if not class then
		return false
	end

	return (table.find(class.Tags, "NotCreatable") or table.find(class.Tags, "Service")) == nil
end

local function isService(className: string): boolean
	local class = database.Classes[className]
	return class ~= nil and table.find(class.Tags, "Service") ~= nil
end

local function hasClass(className: string): boolean
	return database.Classes[className] ~= nil
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

	if descriptor.dataType == "Attributes" and descriptor.serializedName ~= nil then
		if propertyName == "Attributes" and TransientMetadata.hasAttributeOverlay(instance) then
			return descriptor:read(instance)
		end
		local success, value =
			ExactSerializedAccess.read(PrivilegedAccess, instance, descriptor.serializedName, "BinaryString")
		if success then
			return true, value
		end
		if success == false then
			return false, Error.new(Error.Kind.Roblox, value)
		end
	end

	if PrivilegedAccess.supportsReferenceDescriptor(descriptor) then
		local success, value = PrivilegedAccess.readReference(instance, descriptor)
		if success then
			return true, value
		end
		return false, Error.new(Error.Kind.Roblox, value)
	end

	if PrivilegedAccess.supportsRawDescriptor(descriptor) then
		local success, value =
			PrivilegedAccess.readRaw(instance, descriptor.name, PrivilegedAccess.rawType(descriptor) :: string)
		if success then
			return true, value
		end
		return false, Error.new(Error.Kind.Roblox, value)
	end

	if PrivilegedAccess.supportsDescriptor(descriptor) then
		local success, value = PrivilegedAccess.read(instance, descriptor)
		if success then
			return true, value
		end
		return false, Error.new(Error.Kind.Roblox, value)
	end

	return descriptor:read(instance)
end

local function writeProperty(instance, propertyName, value)
	local descriptor = findCanonicalPropertyDescriptor(instance.ClassName, propertyName)

	if
		descriptor ~= nil
		and descriptor.dataType == "Attributes"
		and descriptor.serializedName ~= nil
		and type(value) == "string"
	then
		local transientAttributes = {}
		if propertyName == "Attributes" then
			for name, attributeValue in instance:GetAttributes() do
				if TransientMetadata.isAttribute(instance, name) then
					transientAttributes[name] = attributeValue
				end
			end
		end
		local success, reason =
			ExactSerializedAccess.write(PrivilegedAccess, instance, descriptor.serializedName, "BinaryString", value)
		if success then
			if propertyName == "Attributes" and TransientMetadata.hasAttributeOverlay(instance) then
				TransientMetadata.rememberAuthoredAttributes(instance, instance:GetAttributes())
			end
			for name, attributeValue in transientAttributes do
				TransientMetadata.setTransportAttribute(instance, name, attributeValue)
			end
			return true
		end
		return false, Error.new(Error.Kind.Roblox, reason)
	end

	if descriptor ~= nil and PrivilegedAccess.supportsRawDescriptor(descriptor) and type(value) == "string" then
		local success, reason =
			PrivilegedAccess.writeRaw(instance, descriptor.name, PrivilegedAccess.rawType(descriptor) :: string, value)
		if success then
			return true
		end
		return false, Error.new(Error.Kind.Roblox, reason)
	end

	if descriptor == nil then
		local fullName = ("%s.%s"):format(instance.ClassName, propertyName)

		return false, Error.new(Error.Kind.UnknownProperty, fullName)
	end

	if PrivilegedAccess.supportsReferenceDescriptor(descriptor) then
		local success, reason = PrivilegedAccess.writeReference(instance, descriptor, value)
		if success then
			return true
		end
		return false, Error.new(Error.Kind.Roblox, reason)
	end

	if PrivilegedAccess.supportsMaterializedDescriptor(descriptor) then
		local success, reason = PrivilegedAccess.writeMaterialized(instance, descriptor, value)
		if success then
			return true
		end
		return false, Error.new(Error.Kind.Roblox, reason)
	end

	if PrivilegedAccess.supportsDescriptor(descriptor) then
		local success, reason = PrivilegedAccess.write(instance, descriptor, value)
		if success then
			return true
		end
		return false, Error.new(Error.Kind.Roblox, reason)
	end

	return descriptor:write(instance, value)
end

local function configurePrivileged(client, capabilities)
	PrivilegedAccess.configure(client, capabilities)
	table.clear(defaultPropertiesCache)
end

local function beginPrivilegedBatch(instances: { Instance }): (boolean, any?)
	local requests = PrivilegedReadPlan.build(instances, {
		debugId = function(instance)
			return instance:GetDebugId(128)
		end,
		attributes = function(instance)
			local descriptor = findCanonicalPropertyDescriptor(instance.ClassName, "Attributes")
			return if descriptor ~= nil
					and descriptor.serializedName ~= nil
					and PrivilegedAccess.supportsType("BinaryString")
				then descriptor.serializedName
				else nil
		end,
		defaults = function(instance)
			return getDefaultProperties(instance.ClassName)
		end,
		descriptor = function(instance, property)
			return findCanonicalPropertyDescriptor(instance.ClassName, property)
		end,
		supportsRaw = PrivilegedAccess.supportsRawDescriptor,
		privileged = function(instance)
			return getBatchPrivilegedSerializedProperties(instance.ClassName)
		end,
	})
	local referenceRequests = {}
	for _, instance in instances do
		local debugId = instance:GetDebugId(128)
		for _, property in getRefProperties(instance.ClassName) do
			local descriptor = findCanonicalPropertyDescriptor(instance.ClassName, property)
			if descriptor ~= nil and PrivilegedAccess.supportsReferenceDescriptor(descriptor) then
				table.insert(referenceRequests, { debugId = debugId, property = descriptor.name })
			end
		end
	end
	return PrivilegedAccess.beginBatch(requests, referenceRequests, instances)
end

local function hasUnknownPrivilegedDefault(className: string, properties: { [string]: any }): boolean
	local defaults = getDefaultProperties(className)
	for _, descriptor in getPrivilegedSerializedProperties(className) do
		if properties[descriptor.name] == nil and defaults[descriptor.name] == nil then
			return true
		end
	end
	return false
end

local function createPrivileged(className: string, parent: Instance, name: string): Instance?
	return PrivilegedAccess.create(className, parent, name)
end

return {
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
	getPrivilegedSerializedProperties = getPrivilegedSerializedProperties,
	configurePrivileged = configurePrivileged,
	setPrivilegedReferenceSourceIds = PrivilegedAccess.setReferenceSourceIds,
	encodePrivilegedContentObject = PrivilegedAccess.encodeContentObject,
	beginPrivilegedBatch = beginPrivilegedBatch,
	endPrivilegedBatch = PrivilegedAccess.endBatch,
	hasUnknownPrivilegedDefault = hasUnknownPrivilegedDefault,
	createPrivileged = createPrivileged,
	hasPrivilegedAccess = PrivilegedAccess.isAvailable,
	reflectionVersion = reflectionVersion,
	assertReflectionVersion = assertReflectionVersion,
	applyReflection = applyReflection,
	isRawType = PrivilegedAccess.isRawType,
	Error = Error,
	EncodedValue = EncodedValue,
}
