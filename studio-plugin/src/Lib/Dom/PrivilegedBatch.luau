--!strict

-- Serialized-property reads may return a binary carrier model. Keeping these
-- chunks smaller than the HTTP protocol maximum bounds Studio serialization,
-- server decode memory, and the time before a failed launch reaches its logs.
local LIMIT = 1024

local PrivilegedBatch = {}

function PrivilegedBatch.chunks(requests: { any }): { { any } }
	local chunks = {}
	for first = 1, #requests, LIMIT do
		local last = math.min(first + LIMIT - 1, #requests)
		local chunk = table.create(last - first + 1)
		table.move(requests, first, last, 1, chunk)
		table.insert(chunks, chunk)
	end
	return chunks
end

function PrivilegedBatch.cacheResponses(
	cache: { [string]: any },
	requests: { any },
	values: any,
	keyFor: (any) -> string
): (boolean, string?)
	if type(values) ~= "table" or #values ~= #requests then
		return false, "RML returned an invalid privileged property batch"
	end
	for index, request in requests do
		local response = values[index]
		if type(response) ~= "table" then
			return false, `RML returned an invalid privileged property response at index {index}`
		end
		if response.error ~= nil then
			return false,
				`RML could not read required serialized property {tostring(request.property)} on {tostring(
					request.debugId
				)} at index {index}: {tostring(response.error)}`
		end
		if type(response.value) ~= "string" then
			return false, `RML omitted required serialized property bytes at index {index}`
		end
		cache[keyFor(request)] = response
	end
	return true
end

PrivilegedBatch.LIMIT = LIMIT

return PrivilegedBatch
