--!strict

-- Builds one deduplicated read plan for every exact property that a full
-- snapshot/diff pass will consume. Keeping this separate makes it possible to
-- prove that ordinary scriptable raw properties do not fall through to one
-- privileged HTTP request per value.

local PrivilegedReadPlan = {}

function PrivilegedReadPlan.build(instances: { any }, callbacks): { { debugId: string, property: string } }
	local requests = {}
	local seen = {}

	local function add(debugId: string, property: string)
		local key = debugId .. "\0" .. property
		if not seen[key] then
			seen[key] = true
			table.insert(requests, { debugId = debugId, property = property })
		end
	end

	for _, instance in instances do
		local debugId = callbacks.debugId(instance)
		local attributes = callbacks.attributes(instance)
		if attributes ~= nil then
			add(debugId, attributes)
		end

		for property in callbacks.defaults(instance) do
			local descriptor = callbacks.descriptor(instance, property)
			if descriptor ~= nil and callbacks.supportsRaw(descriptor) then
				add(debugId, descriptor.name)
			end
		end

		for _, descriptor in callbacks.privileged(instance) do
			add(debugId, descriptor.name)
		end
	end

	return requests
end

return PrivilegedReadPlan
