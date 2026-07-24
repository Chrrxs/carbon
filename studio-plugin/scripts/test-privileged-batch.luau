--!strict

local Batch = require("../src/Lib/Dom/PrivilegedBatch")

local function requests(count: number)
	local result = table.create(count)
	for index = 1, count do
		result[index] = { debugId = `id-{index}`, property = "AttributesSerialize" }
	end
	return result
end

assert(#Batch.chunks(requests(0)) == 0, "empty batches must not make an HTTP request")
assert(#Batch.chunks(requests(1)) == 1 and #Batch.chunks(requests(1))[1] == 1, "one item must remain one chunk")
assert(Batch.LIMIT == 1024, "serialized-property carrier batches must stay memory-bounded")
assert(#Batch.chunks(requests(1024)) == 1, "the carrier limit must fit one chunk")
local over = Batch.chunks(requests(1025))
assert(#over == 2 and #over[1] == 1024 and #over[2] == 1, "the carrier limit boundary must retain order")
assert(over[1][1024].debugId == "id-1024" and over[2][1].debugId == "id-1025", "chunk order must be exact")

local cache = {}
local request = { { debugId = "same", property = "Capabilities" } }
local ok = Batch.cacheResponses(cache, request, { { value = "first" } }, function(item)
	return item.debugId .. "\0" .. item.property
end)
assert(ok and cache["same\0Capabilities"].value == "first", "successful values must be cached by exact key")

ok = Batch.cacheResponses(cache, request, { { value = "second" } }, function(item)
	return item.debugId .. "\0" .. item.property
end)
assert(ok and cache["same\0Capabilities"].value == "second", "duplicate keys must update deterministically")

local itemError, reason = Batch.cacheResponses({}, request, { { error = "denied" } }, function()
	return "unused"
end)
assert(not itemError and string.find(reason or "", "denied", 1, true), "per-item read errors must fail closed")

local invalid = Batch.cacheResponses({}, request, {}, function()
	return "unused"
end)
assert(not invalid, "response cardinality mismatches must fail closed")

print("PASS: privileged batch chunking, ordering, caching, and errors")
