--!strict

local Plan = require("../src/Lib/Dom/PrivilegedReadPlan")

local instances = {
	{ id = "part", defaults = { CFrame = true, Color = true, Name = true } },
	{ id = "number", defaults = { Value = true } },
}

local descriptors = {
	CFrame = { name = "CFrame", dataType = "CFrame" },
	Color = { name = "Color", dataType = "Color3" },
	Name = { name = "Name", dataType = "String" },
	Value = { name = "Value", dataType = "Float32" },
}
local rawTypes = { CFrame = true, Color3 = true, Float32 = true }

local requests = Plan.build(instances, {
	debugId = function(instance)
		return instance.id
	end,
	attributes = function()
		return "AttributesSerialize"
	end,
	defaults = function(instance)
		return instance.defaults
	end,
	descriptor = function(_, property)
		return descriptors[property]
	end,
	supportsRaw = function(descriptor)
		return rawTypes[descriptor.dataType] == true
	end,
	privileged = function(instance)
		-- CFrame intentionally overlaps the raw/default plan and must be deduped.
		return if instance.id == "part"
			then { descriptors.CFrame, { name = "Capabilities", dataType = "SecurityCapabilities" } }
			else {}
	end,
})

local keys = {}
for _, request in requests do
	local key = request.debugId .. "\0" .. request.property
	assert(not keys[key], `duplicate exact batch request {key}`)
	keys[key] = true
end

assert(keys["part\0CFrame"], "ordinary scriptable CFrame must be batch-cached")
assert(keys["part\0Color"], "ordinary scriptable Color3 must be batch-cached")
assert(keys["number\0Value"], "ordinary scriptable Float32 must be batch-cached")
assert(not keys["part\0Name"], "ordinary non-raw String should not be elevated")
assert(keys["part\0Capabilities"], "protected serialized values must remain batched")
assert(#requests == 6, "exact plan must include attributes, raw defaults, and protected values once")

print("PASS: exact raw scriptable properties are included in the deduplicated batch plan")
