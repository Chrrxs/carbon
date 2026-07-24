--!strict

local Carbon = script:FindFirstAncestor("Carbon")
local Base64 = require(Carbon.Lib.Base64)
local PrivilegedValueCodec = require(script.Parent.PrivilegedValueCodec)
local PrivilegedBatch = require(script.Parent.PrivilegedBatch)
local PrivilegedReferenceTransport = require(script.Parent.PrivilegedReferenceTransport)
local ProtectedProperties = require(script.Parent.ProtectedProperties)

local client = nil
local supportedTypes = {}
local rawTypes = {}
local batchCache: { [string]: any }? = nil
local referenceBatchCache: { [string]: any }? = nil
local referenceTargetsByDebugId = setmetatable({}, { __mode = "v" })
local referenceSourceIdsByDebugId = {}
local serializedReferences = false
local MATERIALIZED_RAW_TYPES = {
	ColorSequence = true,
	PhysicalProperties = true,
	Region3 = true,
}
local MATERIALIZED_READ_ONLY_TYPES = {
	Bool = true,
	Content = true,
	Enum = true,
	Int32 = true,
	String = true,
	Vector3 = true,
}

local PrivilegedAccess = {}

local function descriptorRawType(descriptor): string?
	return if rawTypes[descriptor.dataType] == true then descriptor.dataType else nil
end

function PrivilegedAccess.configure(newClient: any?, capabilities: any?)
	client = newClient
	batchCache = nil
	referenceBatchCache = nil
	referenceTargetsByDebugId = setmetatable({}, { __mode = "v" })
	referenceSourceIdsByDebugId = {}
	serializedReferences = capabilities ~= nil and capabilities.serializedReferences == true
	table.clear(supportedTypes)
	table.clear(rawTypes)
	if capabilities then
		for _, typeName in capabilities.binaryTypes or {} do
			supportedTypes[typeName] = true
		end
		for _, typeName in capabilities.scalarTypes or {} do
			supportedTypes[typeName] = true
		end
		for _, typeName in capabilities.blittableTypes or {} do
			supportedTypes[typeName] = true
		end
		for _, typeName in capabilities.rawTypes or {} do
			supportedTypes[typeName] = true
			rawTypes[typeName] = true
		end
	end
end

local function batchKey(debugId: string, propertyName: string): string
	return debugId .. "\0" .. propertyName
end

local function indexReferenceTarget(instance: Instance)
	local success, debugId = pcall(instance.GetDebugId, instance, 128)
	if success then
		referenceTargetsByDebugId[debugId] = instance
	end
end

local function indexReachableReferenceTargets()
	indexReferenceTarget(game)
	for _, instance in game:GetDescendants() do
		indexReferenceTarget(instance)
	end
end

local function clearBatchCaches()
	batchCache = nil
	referenceBatchCache = nil
end

function PrivilegedAccess.beginBatch(
	requests: { { debugId: string, property: string } },
	referenceRequests: { { debugId: string, property: string } }?,
	instances: { Instance }?
): (boolean, any?)
	batchCache = {}
	referenceBatchCache = {}
	for _, instance in instances or {} do
		indexReferenceTarget(instance)
	end
	if client == nil then
		return true
	end
	for _, chunk in PrivilegedBatch.chunks(requests) do
		local success, response = pcall(function()
			return client:readPrivilegedProperties(chunk):expect()
		end)
		if not success then
			clearBatchCaches()
			return false, response
		end
		local cached, cacheError = PrivilegedBatch.cacheResponses(batchCache, chunk, response.values, function(request)
			return batchKey(request.debugId, request.property)
		end)
		if not cached then
			clearBatchCaches()
			return false, cacheError
		end
	end
	local referencesOk, referenceResult = PrivilegedReferenceTransport.beginBatch(
		referenceRequests or {},
		PrivilegedBatch.chunks,
		function(chunk)
			return client:readPrivilegedReferences(chunk):expect()
		end,
		function(request)
			return batchKey(request.debugId, request.property)
		end
	)
	if not referencesOk then
		clearBatchCaches()
		return false, referenceResult
	end
	referenceBatchCache = referenceResult
	return true
end

function PrivilegedAccess.endBatch()
	clearBatchCaches()
end

local function readResponse(instance: Instance, propertyName: string): (boolean, any)
	local debugId = instance:GetDebugId(128)
	if batchCache ~= nil then
		local cached = batchCache[batchKey(debugId, propertyName)]
		if cached ~= nil then
			if cached.error ~= nil then
				return false, cached.error
			end
			return true, cached
		end
	end
	local success, response = pcall(function()
		return client:readPrivilegedProperty(debugId, propertyName):expect()
	end)
	return success, response
end

local function readReferenceResponse(instance: Instance, propertyName: string): (boolean, any)
	local debugId = instance:GetDebugId(128)
	if referenceBatchCache ~= nil then
		local cached = referenceBatchCache[batchKey(debugId, propertyName)]
		if cached ~= nil then
			if cached.error ~= nil then
				return false, cached.error
			end
			return true, cached
		end
	end
	local success, response = pcall(function()
		return client:readPrivilegedReferences({ { debugId = debugId, property = propertyName } }):expect()
	end)
	if not success then
		return false, response
	end
	local request = { { debugId = debugId, property = propertyName } }
	local cache = {}
	local cached, cacheError = PrivilegedReferenceTransport.cacheResponses(
		cache,
		request,
		response.values,
		function(item)
			return batchKey(item.debugId, item.property)
		end
	)
	if not cached then
		return false, cacheError
	end
	return true, cache[batchKey(debugId, propertyName)]
end

function PrivilegedAccess.isAvailable(): boolean
	return client ~= nil
end

function PrivilegedAccess.supportsDescriptor(descriptor): boolean
	return client ~= nil
		and descriptor.serializes == true
		and (descriptor.scriptability == "None" or descriptor.scriptability == "Write" or (descriptor.scriptability == "Custom" and descriptor.dataType == "OptionalCFrame"))
		and descriptor.privilegedExcluded ~= true
		and supportedTypes[descriptor.dataType] == true
end

function PrivilegedAccess.supportsMaterializedDescriptor(descriptor): boolean
	return client ~= nil
		and descriptor.serializes == true
		and descriptor.scriptability == "Read"
		and descriptor.privilegedExcluded ~= true
		and MATERIALIZED_READ_ONLY_TYPES[descriptor.dataType] == true
		and ProtectedProperties.isPersistentReadOnly(descriptor.className, descriptor.name)
end

function PrivilegedAccess.supportsReferenceDescriptor(descriptor): boolean
	return client ~= nil and PrivilegedReferenceTransport.supportsDescriptor(serializedReferences, descriptor)
end

function PrivilegedAccess.setReferenceSourceIds(sourceIdsByDebugId)
	referenceSourceIdsByDebugId = sourceIdsByDebugId or {}
end

function PrivilegedAccess.encodeContentObject(target: Instance)
	local success, debugId = pcall(target.GetDebugId, target, 128)
	if not success then
		return nil
	end
	return PrivilegedReferenceTransport.contentObjectValue(debugId, referenceSourceIdsByDebugId)
end

function PrivilegedAccess.supportsType(dataType: string): boolean
	return client ~= nil and supportedTypes[dataType] == true
end

function PrivilegedAccess.supportsRawDescriptor(descriptor): boolean
	return client ~= nil
		and descriptor.serializes == true
		and descriptor.privilegedExcluded ~= true
		and descriptorRawType(descriptor) ~= nil
end

function PrivilegedAccess.rawType(descriptor): string?
	return descriptorRawType(descriptor)
end

function PrivilegedAccess.isRawType(dataType: string): boolean
	return rawTypes[dataType] == true
end

function PrivilegedAccess.requiresRaw(descriptor, _value: any): boolean
	-- Every advertised exact type stays on the binary path. This prevents a
	-- future caller from accidentally reintroducing a Luau number/object
	-- conversion for a type that has canonical raw transport.
	return PrivilegedAccess.supportsRawDescriptor(descriptor)
end

function PrivilegedAccess.readRaw(instance: Instance, propertyName: string, dataType: string): (boolean, any?)
	if not PrivilegedAccess.supportsType(dataType) then
		return false
	end
	local success, response = readResponse(instance, propertyName)
	if not success then
		return false, response
	end
	local decodeSuccess, value = pcall(Base64.decode, response.value)
	if not decodeSuccess then
		return false, value
	end
	local canonicalSuccess, canonical = pcall(PrivilegedValueCodec.engineToCanonical, dataType, value)
	return canonicalSuccess, canonical
end

local function materializeProperty(
	instance: Instance,
	propertyName: string,
	dataType: string,
	raw: string
): (boolean, any?)
	local success, result = pcall(function()
		client
			:writeMaterializedPrivilegedProperty(
				instance:GetDebugId(128),
				instance.ClassName,
				propertyName,
				dataType,
				buffer.fromstring(raw)
			)
			:expect()
	end)
	return success, if success then nil else result
end

function PrivilegedAccess.writeRaw(
	instance: Instance,
	propertyName: string,
	dataType: string,
	value: string
): (boolean, any?)
	if not PrivilegedAccess.supportsType(dataType) then
		return false
	end
	if MATERIALIZED_RAW_TYPES[dataType] then
		return materializeProperty(instance, propertyName, dataType, value)
	end
	local success, result = pcall(function()
		client:writePrivilegedProperty(instance:GetDebugId(128), propertyName, Base64.encode(value)):expect()
	end)
	return success, if success then nil else result
end

function PrivilegedAccess.readDefaults(className: string, descriptors): (boolean, any?)
	if client == nil or #descriptors == 0 then
		return true, {}
	end
	local properties = {}
	for index, descriptor in descriptors do
		properties[index] = descriptor.name
	end
	local success, response = pcall(function()
		return client:readPrivilegedDefaults(className, properties):expect()
	end)
	if not success then
		return false, response
	end
	if type(response.values) ~= "table" or #response.values ~= #descriptors then
		return false, "Carbon server returned the wrong exact-default count"
	end
	local defaults = {}
	for index, descriptor in descriptors do
		local item = response.values[index]
		if item.error ~= nil then
			return false, item.error
		end
		local decodeSuccess, raw = pcall(Base64.decode, item.value)
		if not decodeSuccess then
			return false, raw
		end
		local canonicalSuccess, canonical = pcall(PrivilegedValueCodec.engineToCanonical, descriptor.dataType, raw)
		if not canonicalSuccess then
			return false, canonical
		end
		defaults[descriptor.name] = canonical
	end
	return true, defaults
end

function PrivilegedAccess.read(instance: Instance, descriptor)
	if not PrivilegedAccess.supportsDescriptor(descriptor) then
		return false
	end
	local success, response = readResponse(instance, descriptor.name)
	if not success then
		return false, response
	end
	local decodeSuccess, value = pcall(Base64.decode, response.value)
	if not decodeSuccess then
		return false, value
	end
	local coercionSuccess, coerced = pcall(PrivilegedValueCodec.decode, descriptor.dataType, value)
	return coercionSuccess, coerced
end

function PrivilegedAccess.readReference(instance: Instance, descriptor): (boolean, any?)
	if not PrivilegedAccess.supportsReferenceDescriptor(descriptor) then
		return false
	end
	local success, response = readReferenceResponse(instance, descriptor.name)
	if not success then
		return false, response
	end
	local resolved, target =
		PrivilegedReferenceTransport.resolveResponse(response, referenceTargetsByDebugId, referenceSourceIdsByDebugId)
	if resolved then
		return true, target
	end
	indexReachableReferenceTargets()
	return PrivilegedReferenceTransport.resolveResponse(
		response,
		referenceTargetsByDebugId,
		referenceSourceIdsByDebugId
	)
end

function PrivilegedAccess.write(instance: Instance, descriptor, value): (boolean, any?)
	if not PrivilegedAccess.supportsDescriptor(descriptor) then
		return false
	end
	local raw = PrivilegedValueCodec.encode(descriptor.dataType, value)
	if raw == nil then
		return false, `unsupported privileged value for {descriptor.dataType}`
	end
	if
		descriptor.dataType == "SharedString"
		or descriptor.dataType == "NetAssetRef"
		or descriptor.dataType == "UniqueId"
	then
		return materializeProperty(instance, descriptor.name, descriptor.dataType, raw)
	end
	local success, result = pcall(function()
		client:writePrivilegedProperty(instance:GetDebugId(128), descriptor.name, Base64.encode(raw)):expect()
	end)
	return success, if success then nil else result
end

function PrivilegedAccess.writeMaterialized(instance: Instance, descriptor, value): (boolean, any?)
	if not PrivilegedAccess.supportsMaterializedDescriptor(descriptor) then
		return false
	end
	local raw = PrivilegedValueCodec.encode(descriptor.dataType, value)
	if raw == nil then
		if descriptor.dataType == "Content" then
			return false, "Content.Object requires full connected-graph materialization"
		end
		return false, `unsupported materialized value for {descriptor.dataType}`
	end
	return materializeProperty(instance, descriptor.name, descriptor.dataType, raw)
end

function PrivilegedAccess.writeReference(instance: Instance, descriptor, value: Instance?): (boolean, any?)
	if not PrivilegedAccess.supportsReferenceDescriptor(descriptor) then
		return false
	end
	if value ~= nil and typeof(value) ~= "Instance" then
		return false, "serialized Ref value is not an Instance or nil"
	end
	local targetDebugId = if value == nil then nil else value:GetDebugId(128)
	local success, result = pcall(function()
		client:writePrivilegedReference(instance:GetDebugId(128), descriptor.name, targetDebugId):expect()
	end)
	return success, if success then nil else result
end

function PrivilegedAccess.create(className: string, parent: Instance, name: string): Instance?
	if client == nil then
		return nil
	end
	local success, response = pcall(function()
		return client:createPrivilegedInstance(className, parent:GetDebugId(128), name):expect()
	end)
	if not success then
		return nil
	end
	for _, child in parent:GetChildren() do
		local debugSuccess, debugId = pcall(child.GetDebugId, child, 128)
		if debugSuccess and debugId == response.debugId then
			return child
		end
	end
	return nil
end

return PrivilegedAccess
