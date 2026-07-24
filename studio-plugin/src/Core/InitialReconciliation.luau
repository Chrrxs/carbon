--!strict

-- Startup maps source identities onto an already-built place while Studio can
-- keep editing it. Preserve the hierarchy snapshot's identity names so queued
-- edits still resolve after paged hydration completes.

local InitialReconciliation = {}

function InitialReconciliation.isTrustedBuiltPlace(project, expectedWorktree): boolean
	return project.isPlace == true
		and expectedWorktree ~= nil
		and type(expectedWorktree.sourceGeneration) == "string"
		and expectedWorktree.sourceGeneration ~= ""
		and project.sourceGeneration == expectedWorktree.sourceGeneration
end

function InitialReconciliation.identityName(initialNames, instance): string
	return if initialNames and initialNames[instance] ~= nil then initialNames[instance] else instance.Name
end

function InitialReconciliation.appendIdentityCandidate(bucket, initialNames, instance)
	local frozen = initialNames ~= nil and initialNames[instance] ~= nil
	if not frozen then
		table.insert(bucket, instance)
		return
	end
	for index, candidate in bucket do
		if initialNames[candidate] == nil then
			table.insert(bucket, index, instance)
			return
		end
	end
	table.insert(bucket, instance)
end

function InitialReconciliation.beginStableBatch(instances, isLive, beginBatch): (boolean, any?)
	local remaining = {}
	for _, instance in instances do
		if isLive(instance) then
			table.insert(remaining, instance)
		end
	end

	while true do
		local success, reason = beginBatch(remaining)
		if success then
			return true
		end

		local nextRemaining = {}
		for _, instance in remaining do
			if isLive(instance) then
				table.insert(nextRemaining, instance)
			end
		end
		if #nextRemaining == #remaining then
			return false, reason
		end
		remaining = nextRemaining
	end
end

return InitialReconciliation
