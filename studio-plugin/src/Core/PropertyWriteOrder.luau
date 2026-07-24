--!strict

local PropertyWriteOrder = {}

local function rank(property: string): number
	-- Sandboxed serializes through the same engine capability word. Applying it
	-- first and the exact SecurityCapabilities value last preserves both views.
	-- MeshContent can cause Roblox to recompute its skin/joint metadata. Copy the
	-- content first, then restore the three independently serialized values.
	if property == "Sandboxed" or property == "MeshContent" then
		return 1
	elseif
		property == "Capabilities"
		or property == "HasJointOffset"
		or property == "HasSkinnedMesh"
		or property == "JointOffset"
	then
		return 3
	end
	return 2
end

local function sort(properties: { string }): { string }
	table.sort(properties, function(left, right)
		local leftRank = rank(left)
		local rightRank = rank(right)
		return leftRank < rightRank or (leftRank == rightRank and left < right)
	end)
	return properties
end

function PropertyWriteOrder.keys(properties): { string }
	local keys = {}
	for property in properties do
		table.insert(keys, property)
	end
	return sort(keys)
end

function PropertyWriteOrder.names(properties: { string }): { string }
	return sort(table.clone(properties))
end

return PropertyWriteOrder
