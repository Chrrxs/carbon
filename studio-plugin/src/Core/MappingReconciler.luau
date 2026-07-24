--!strict

-- Filesystem mappings use one deep, one-way seam:
--
--     source pages -> authoritative snapshot -> Studio reconciliation
--
-- Studio does not send edits through this module and no Instance signals are
-- installed. Drift is overwritten only when a new filesystem snapshot arrives.

local Carbon = script:FindFirstAncestor("Carbon")

local Log = require(Carbon.Log)
local Types = require(Carbon.Types)

local Changes = require(script.Parent.Changes)
local Processor = require(script.Parent.Processor)
local Tree = require(script.Parent.Tree)

local ZERO_REF = string.rep("\0", 16)

local MappingReconciler = {}
MappingReconciler.__index = MappingReconciler

local function key(id: buffer): string
	return buffer.tostring(id)
end

local function snapshotName(snapshot): string
	return if snapshot.rawName then buffer.tostring(snapshot.rawName) else snapshot.name
end

local function isLive(instance: Instance?): boolean
	if instance == nil then
		return false
	end
	if instance == game then
		return true
	end
	local success, descendant = pcall(instance.IsDescendantOf, instance, game)
	return success and descendant
end

local function studioPath(instance: Instance): string
	local success, fullName = pcall(instance.GetFullName, instance)
	return if success then `game.{fullName}` else `{instance.ClassName} {instance.Name}`
end

local function childrenByIdentity(parent: Instance, className: string, name: string, claimed): { Instance }
	local matches = {}
	for _, child in parent:GetChildren() do
		if child.ClassName == className and child.Name == name and claimed[child] ~= true then
			table.insert(matches, child)
		end
	end
	return matches
end

function MappingReconciler.new()
	local tree = Tree.new()
	return setmetatable({
		tree = tree,
		processor = Processor.new(tree),
		bindings = {} :: { [string]: Instance },
		mapped = {} :: { [string]: boolean },
	}, MappingReconciler)
end

function MappingReconciler:__readHierarchy(client): ({ Types.HierarchySnapshot }, { [string]: Types.HierarchySnapshot })
	local ordered = {}
	local byId = {}
	local cursor = nil
	repeat
		local page = client:getSnapshotPage(cursor):expect()
		for _, snapshot in page.instances do
			table.insert(ordered, snapshot)
			byId[key(snapshot.id)] = snapshot
		end
		cursor = page.cursor
	until page.done
	return ordered, byId
end

local function mappedClosure(ordered, byId, mappedRootRefs)
	local mapped = {}
	local roots = {}
	local selected = {}
	local visiting = {}
	for _, id in mappedRootRefs do
		mapped[key(id)] = true
		roots[key(id)] = true
	end
	local function resolve(idKey)
		if mapped[idKey] ~= nil then
			return mapped[idKey]
		end
		if visiting[idKey] then
			error("authoritative mapping hierarchy contains a cycle", 0)
		end
		visiting[idKey] = true
		local snapshot = byId[idKey]
		local result = snapshot ~= nil and resolve(key(snapshot.parent)) or false
		visiting[idKey] = nil
		mapped[idKey] = result
		return result
	end
	for _, snapshot in ordered do
		resolve(key(snapshot.id))
	end
	for idKey, isMapped in mapped do
		if isMapped then
			local cursor = idKey
			while cursor ~= nil and selected[cursor] ~= true do
				selected[cursor] = true
				local snapshot = byId[cursor]
				if snapshot == nil or key(snapshot.parent) == ZERO_REF then
					break
				end
				cursor = key(snapshot.parent)
			end
		end
	end
	return mapped, roots, selected
end

function MappingReconciler:__resetTree()
	self.tree = Tree.new()
	self.processor = Processor.new(self.tree)
end

-- A generation-matched serve build already contains the authoritative mapping
-- realization. Bind its source identities without applying changes or reading
-- properties so a later filesystem update can recover the exact objects after
-- Studio-side rename/reparent drift. This deliberately is not verification:
-- unmatched nodes are left unbound and startup never edits the place.
function MappingReconciler:prime(client, project): any
	local ordered, byId = self:__readHierarchy(client)
	local mapped, _, selected = mappedClosure(ordered, byId, project.mappedRootRefs)
	local managed = {}
	for _, snapshot in ordered do
		if selected[key(snapshot.id)] then
			table.insert(managed, snapshot)
		end
	end

	local current = {}
	local claimed = {}
	local unresolved = table.clone(managed)
	self:__resetTree()

	while #unresolved > 0 do
		local remaining = {}
		local progressed = false
		for _, snapshot in unresolved do
			local idKey = key(snapshot.id)
			local parent = if key(snapshot.parent) == ZERO_REF then game else self.tree:getInstance(snapshot.parent)
			if parent == nil then
				table.insert(remaining, snapshot)
				continue
			end

			local instance = if snapshot.class == "DataModel" then game else nil
			if instance == nil then
				local matches = childrenByIdentity(parent, snapshot.class, snapshotName(snapshot), claimed)
				-- The disposable build is expected to be exact. Refuse to guess if it
				-- is not; the first filesystem update will authoritatively reconcile it.
				instance = if #matches == 1 then matches[1] else nil
			end
			if instance == nil then
				table.insert(remaining, snapshot)
				continue
			end

			self.tree:insertInstance(instance, snapshot.id)
			claimed[instance] = true
			current[idKey] = instance
			progressed = true
		end
		if not progressed then
			break
		end
		unresolved = remaining
	end

	self.bindings = current
	self.mapped = mapped
	return {
		instances = #managed - #unresolved,
		expected = #managed,
	}
end

function MappingReconciler:__bindHierarchy(ordered, mapped)
	local previous = self.bindings
	local claimed = {}
	local current = {}
	local structural = Changes.new()
	local unresolved = table.clone(ordered)

	self:__resetTree()

	while #unresolved > 0 do
		local remaining = {}
		local progressed = false
		for _, snapshot in unresolved do
			local idKey = key(snapshot.id)
			local parent = if key(snapshot.parent) == ZERO_REF then game else self.tree:getInstance(snapshot.parent)
			if parent == nil then
				table.insert(remaining, snapshot)
				continue
			end

			local instance = if snapshot.class == "DataModel" then game else nil
			local prior = previous[idKey]
			if instance == nil and mapped[idKey] and isLive(prior) and claimed[prior] ~= true then
				-- Stable source IDs let a later source update recover rename, reparent,
				-- class, property, and Source drift without observing the edit itself.
				instance = prior
			end
			if instance == nil then
				local matches = childrenByIdentity(parent, snapshot.class, snapshotName(snapshot), claimed)
				instance = matches[1]
			end
			if instance == nil then
				instance = self.processor.write:createInstance(snapshot, true)
			else
				self.tree:insertInstance(instance, snapshot.id)
			end
			assert(instance ~= nil, `could not realize {snapshot.class} {snapshotName(snapshot)}`)
			claimed[instance] = true
			current[idKey] = instance

			if instance ~= game then
				local update = { id = snapshot.id }
				local changed = false
				if instance.ClassName ~= snapshot.class then
					update.class = snapshot.class
					changed = true
				end
				if instance.Name ~= snapshotName(snapshot) then
					update.name = snapshot.name
					update.rawName = snapshot.rawName
					changed = true
				end
				if instance.Parent ~= parent then
					update.parent = snapshot.parent
					changed = true
				end
				if changed then
					structural:update(update)
				end
			end
			progressed = true
		end
		if not progressed then
			error("authoritative mapping hierarchy is disconnected or out of order", 0)
		end
		unresolved = remaining
	end

	if not structural:isEmpty() then
		self.processor.write:applyChanges(structural, false)
	end
	for idKey in current do
		current[idKey] = self.tree.instanceMap[idKey]
	end
	return previous, current, claimed
end

function MappingReconciler:__applyProperties(client, mapped)
	local cursor = nil
	repeat
		local page = client:getSourcePage(cursor, false, true):expect()
		local instances = {}
		for _, snapshot in page.instances do
			-- Engine route anchors exist only so mapped roots can be located. Their
			-- properties remain Studio/engine-owned and must never be hydrated from
			-- the projected source snapshot.
			if mapped[key(snapshot.id)] then
				table.insert(instances, snapshot)
			end
		end
		local changes = self.processor:diffPropertyPage(instances)
		if not changes:isEmpty() then
			self.processor.write:applyChanges(changes, false)
		end
		cursor = page.cursor
	until page.done
end

local function destroy(instance: Instance, reason: string)
	local path = studioPath(instance)
	local className = instance.ClassName
	local success, failure = pcall(instance.Destroy, instance)
	if not success then
		error(`could not remove {reason} {className} {path}: {tostring(failure)}`, 0)
	end
	return className, path
end

function MappingReconciler:__removeObsolete(previous, current, claimed, mapped, roots)
	-- Filesystem removals are expected and are not warnings. Reuse by a new
	-- source identity (for example a rename) wins over deleting the object.
	for idKey, instance in previous do
		if self.mapped[idKey] and not mapped[idKey] and claimed[instance] ~= true and isLive(instance) then
			destroy(instance, "filesystem-removed mapped instance")
		end
	end

	local invalid = {}
	local invalidSet = {}
	local function mark(instance)
		if claimed[instance] ~= true and invalidSet[instance] ~= true then
			invalidSet[instance] = true
			table.insert(invalid, instance)
		end
	end

	for idKey in roots do
		local root = current[idKey]
		if root ~= nil then
			for _, descendant in root:GetDescendants() do
				mark(descendant)
			end
			local rootId = self.tree:getId(root)
			local snapshot = rootId and self.__hierarchyById[key(rootId)]
			local parent = root.Parent
			if snapshot ~= nil and parent ~= nil then
				-- A same-name sibling collides with the barrier itself even though it
				-- is not a descendant of the chosen realization.
				for _, sibling in parent:GetChildren() do
					if sibling ~= root and sibling.Name == snapshotName(snapshot) then
						mark(sibling)
					end
				end
			end
		end
	end

	-- Remove deepest nodes first so every warning retains its complete Studio
	-- path and an unknown parent is not detached before its children are logged.
	table.sort(invalid, function(left, right)
		return #left:GetFullName() > #right:GetFullName()
	end)
	local warnings = {}
	for _, instance in invalid do
		if isLive(instance) then
			local className, path = destroy(instance, "Studio-only mapped instance")
			local message =
				`Removed Studio-only instance from filesystem mapping during reconciliation: {className} {path}`
			table.insert(warnings, message)
			Log.warn(message)
		end
	end
	return warnings
end

function MappingReconciler:reconcile(client, project): any
	local ordered, byId = self:__readHierarchy(client)
	local mapped, roots, selected = mappedClosure(ordered, byId, project.mappedRootRefs)
	local managed = {}
	for _, snapshot in ordered do
		if selected[key(snapshot.id)] then
			table.insert(managed, snapshot)
		end
	end
	self.__hierarchyById = byId
	local previous, current, claimed = self:__bindHierarchy(managed, mapped)
	self:__applyProperties(client, mapped)
	local warnings = self:__removeObsolete(previous, current, claimed, mapped, roots)
	self.bindings = current
	self.mapped = mapped
	self.__hierarchyById = nil
	return {
		instances = #managed,
		removedInvalid = #warnings,
		warnings = warnings,
	}
end

return MappingReconciler
