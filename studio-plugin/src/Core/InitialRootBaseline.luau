--!strict

local InitialRootBaseline = {}

-- The bridge's DataModel callback can run before Studio finishes parenting the
-- serialized place roots. The roots endpoint is the authoritative current
-- enumeration; the plugin's separately frozen hierarchy prevents roots created
-- later by reconciliation from being adopted.
function InitialRootBaseline.indexCurrent(roots): { [string]: boolean }
	local current = {}
	for _, root in roots or {} do
		current[root.debugId] = true
	end
	return current
end

function InitialRootBaseline.hiddenCurrent(roots, visibleDebugIds): { any }
	local hidden = {}
	for _, root in roots or {} do
		if not visibleDebugIds[root.debugId] then
			table.insert(hidden, root)
		end
	end
	return hidden
end

function InitialRootBaseline.managedFrozenVisible(frozenRoots, currentDebugIds, describe): { [string]: boolean }
	local managed = {}
	for _, root in frozenRoots or {} do
		local debugId, shouldManage = describe(root)
		if debugId ~= nil and currentDebugIds[debugId] and shouldManage then
			managed[debugId] = true
		end
	end
	return managed
end

function InitialRootBaseline.shouldAdoptFrozen(
	existedAtEndpoint: boolean,
	isAlreadyTracked: boolean,
	shouldAdoptRoot: boolean,
	isMaterializedByCarbon: boolean,
	isStudioTransient: boolean
): boolean
	return existedAtEndpoint
		and not isAlreadyTracked
		and shouldAdoptRoot
		and not isMaterializedByCarbon
		and not isStudioTransient
end

function InitialRootBaseline.eligibleCurrentChildren(children, isMaterialized): { any }
	local eligible = {}
	for _, child in children do
		if not isMaterialized(child) then
			table.insert(eligible, child)
		end
	end
	return eligible
end

function InitialRootBaseline.shouldPreserveUnavailableClass(classIsKnown: boolean): boolean
	return not classIsKnown
end

return InitialRootBaseline
