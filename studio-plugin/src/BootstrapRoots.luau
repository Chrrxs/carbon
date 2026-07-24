--!strict

local BootstrapRoots = {}

function BootstrapRoots.index(instances, getDebugId): { [string]: boolean }
	local indexed = {}
	for _, instance in instances do
		local debugId = getDebugId(instance)
		if debugId ~= nil then
			indexed[debugId] = true
		end
	end
	return indexed
end

function BootstrapRoots.materialized(initialDebugIds, instances, getDebugId, makeTransient): { [string]: boolean }
	local materialized = {}
	for _, instance in instances do
		local debugId = getDebugId(instance)
		if debugId ~= nil and not initialDebugIds[debugId] then
			materialized[debugId] = true
			makeTransient(instance)
		end
	end
	return materialized
end

function BootstrapRoots.promotedArchivable(wasMaterializedByCarbon: boolean, currentValue: boolean): boolean
	return if wasMaterializedByCarbon then true else currentValue
end

return BootstrapRoots
