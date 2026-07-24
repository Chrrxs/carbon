--!strict

local AdditionBoundary = {}

function AdditionBoundary.captureKnownDescendants(root, getChildren: (any) -> { any }, isTracked: (any) -> boolean)
	local known = {}
	local stack = getChildren(root)
	while #stack > 0 do
		local instance = table.remove(stack)
		if isTracked(instance) then
			known[instance] = true
		end
		for _, child in getChildren(instance) do
			table.insert(stack, child)
		end
	end
	return known
end

function AdditionBoundary.shouldInclude(known, instance): boolean
	return known[instance] ~= true
end

return AdditionBoundary
