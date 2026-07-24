--!strict

local PrivilegedReferenceTransport = {}

local function sourceIdFromHex(value): buffer?
	if type(value) ~= "string" or #value ~= 32 or string.match(value, "^[0-9a-f]+$") == nil then
		return nil
	end
	local result = buffer.create(16)
	for offset = 0, 15 do
		buffer.writeu8(result, offset, tonumber(string.sub(value, offset * 2 + 1, offset * 2 + 2), 16) :: number)
	end
	return result
end

function PrivilegedReferenceTransport.supportsDescriptor(enabled: boolean, descriptor): boolean
	return enabled
		and descriptor.serializes == true
		and descriptor.dataType == "Ref"
		and descriptor.privilegedExcluded ~= true
end

function PrivilegedReferenceTransport.sourceReference(sourceId: buffer)
	return { __carbonSourceRef = sourceId }
end

function PrivilegedReferenceTransport.sourceId(value): buffer?
	return if type(value) == "table" and typeof(value.__carbonSourceRef) == "buffer"
		then value.__carbonSourceRef
		else nil
end

function PrivilegedReferenceTransport.contentObjectValue(targetDebugId: string?, sourceIdsByDebugId)
	if targetDebugId == nil then
		return nil
	end
	local sourceId = sourceIdsByDebugId[targetDebugId]
	if sourceId == nil then
		return nil
	end
	return { Content = { Object = sourceId } }
end

function PrivilegedReferenceTransport.resolveReadTarget(
	targetDebugId: string?,
	targetsByDebugId,
	sourceIdsByDebugId
): (boolean, any?)
	if targetDebugId == nil then
		return true, nil
	end
	local target = targetsByDebugId[targetDebugId]
	if target ~= nil then
		return true, target
	end
	local sourceId = sourceIdsByDebugId[targetDebugId]
	if sourceId ~= nil then
		return true, PrivilegedReferenceTransport.sourceReference(sourceId)
	end
	return false, `serialized reference target {targetDebugId} is outside Carbon's managed source tree`
end

function PrivilegedReferenceTransport.resolveTarget(targetDebugId: string?, targetsByDebugId): (boolean, any?)
	return PrivilegedReferenceTransport.resolveReadTarget(targetDebugId, targetsByDebugId, {})
end

function PrivilegedReferenceTransport.resolveResponse(response, targetsByDebugId, sourceIdsByDebugId): (boolean, any?)
	if type(response) ~= "table" then
		return false, "RML returned an invalid serialized-reference response"
	end
	if response.error ~= nil then
		return false, `RML could not read required serialized reference: {tostring(response.error)}`
	end
	if response.targetDebugId ~= nil and type(response.targetDebugId) ~= "string" then
		return false, "RML returned an invalid serialized-reference target"
	end
	if response.sourceId ~= nil then
		local sourceId = sourceIdFromHex(response.sourceId)
		if sourceId == nil or response.targetDebugId == nil then
			return false, "RML returned an invalid serialized-reference source identity"
		end
		return true, PrivilegedReferenceTransport.sourceReference(sourceId)
	end
	return PrivilegedReferenceTransport.resolveReadTarget(response.targetDebugId, targetsByDebugId, sourceIdsByDebugId)
end

function PrivilegedReferenceTransport.cacheResponses(
	cache: { [string]: any },
	requests: { any },
	values: any,
	keyFor: (any) -> string
): (boolean, string?)
	if type(values) ~= "table" or #values ~= #requests then
		return false, "RML returned an invalid serialized-reference batch"
	end
	for index, request in requests do
		local response = values[index]
		if type(response) ~= "table" then
			return false, `RML returned an invalid serialized-reference response at index {index}`
		end
		if response.error ~= nil then
			return false,
				`RML could not read required serialized reference at index {index}: {tostring(response.error)}`
		end
		if response.targetDebugId ~= nil and type(response.targetDebugId) ~= "string" then
			return false, `RML returned an invalid serialized-reference target at index {index}`
		end
		if
			response.sourceId ~= nil and (sourceIdFromHex(response.sourceId) == nil or response.targetDebugId == nil)
		then
			return false, `RML returned an invalid serialized-reference source identity at index {index}`
		end
		cache[keyFor(request)] = response
	end
	return true
end

function PrivilegedReferenceTransport.beginBatch(requests, chunks, readChunk, keyFor): (boolean, any)
	local cache = {}
	for _, chunk in chunks(requests) do
		local success, response = pcall(readChunk, chunk)
		if not success then
			return false, response
		end
		if type(response) ~= "table" then
			return false, "RML returned an invalid serialized-reference batch envelope"
		end
		local cached, cacheError = PrivilegedReferenceTransport.cacheResponses(cache, chunk, response.values, keyFor)
		if not cached then
			return false, cacheError
		end
	end
	return true, cache
end

return PrivilegedReferenceTransport
