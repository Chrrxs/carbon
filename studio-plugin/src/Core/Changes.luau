--!strict
local Carbon = if script then script:FindFirstAncestor("Carbon") else nil

local Types = if script then require(Carbon.Types) else nil
-- selene: allow(incorrect_standard_library_use)
local PropertyDelta = if script then require(script.Parent.PropertyDelta) else require("./PropertyDelta")

local Changes = {}
Changes.__index = Changes

function Changes.new()
	local self = {
		additions = {} :: { Types.AddedSnapshot },
		updates = {} :: { Types.UpdatedSnapshot },
		removals = {} :: { Types.Ref | Instance },
		additionById = {} :: { [string]: any },
		additionIsRoot = {} :: { [string]: boolean },
		updateById = {} :: { [string]: Types.UpdatedSnapshot },
	}

	return setmetatable(self, Changes)
end

local function indexAddition(self, snapshot, isRoot: boolean)
	local id = buffer.tostring(snapshot.id)
	self.additionById[id] = snapshot
	self.additionIsRoot[id] = isRoot
	for _, child in snapshot.children do
		indexAddition(self, child, false)
	end
end

local function unindexAddition(self, snapshot)
	local id = buffer.tostring(snapshot.id)
	self.additionById[id] = nil
	self.additionIsRoot[id] = nil
	for _, child in snapshot.children do
		unindexAddition(self, child)
	end
end

function Changes:add(snapshot: Types.Snapshot, parent: Types.Ref?)
	local addedSnapshot = snapshot
	addedSnapshot.parent = parent or addedSnapshot.parent

	table.insert(self.additions, addedSnapshot)
	indexAddition(self, addedSnapshot, true)
end

function Changes:update(snapshot: Types.UpdatedSnapshot)
	table.insert(self.updates, snapshot)
	self.updateById[buffer.tostring(snapshot.id)] = snapshot
end

function Changes:mergeAdditionUpdate(snapshot: Types.UpdatedSnapshot): boolean
	local changeId = buffer.tostring(snapshot.id)
	local addition = self.additionById[changeId]
	local isRoot = self.additionIsRoot[changeId] == true
	if addition == nil or (not isRoot and snapshot.parent ~= nil) then
		return false
	end

	if snapshot.parent ~= nil then
		addition.parent = snapshot.parent
	end
	if snapshot.name ~= nil then
		addition.name = snapshot.name
		addition.rawName = snapshot.rawName
	end
	if snapshot.class ~= nil then
		addition.class = snapshot.class
	end
	local properties = PropertyDelta.merge(addition.properties, nil, snapshot.properties, snapshot.removedProperties)
	addition.properties = properties or {}
	return true
end

function Changes:mergeUpdate(snapshot: Types.UpdatedSnapshot): boolean
	local changeId = buffer.tostring(snapshot.id)
	local previous = self.updateById[changeId]
	if previous == nil then
		return false
	end

	previous.parent = snapshot.parent or previous.parent
	previous.name = snapshot.name or previous.name
	if snapshot.name then
		previous.rawName = snapshot.rawName
	end
	previous.class = snapshot.class or previous.class
	previous.properties, previous.removedProperties = PropertyDelta.merge(
		previous.properties,
		previous.removedProperties,
		snapshot.properties,
		snapshot.removedProperties
	)
	return true
end

function Changes:upsertUpdate(snapshot: Types.UpdatedSnapshot)
	if self:mergeAdditionUpdate(snapshot) or self:mergeUpdate(snapshot) then
		return
	end
	self:update(snapshot)
end

function Changes:remove(object: Types.Ref | Instance)
	table.insert(self.removals, object)
end

function Changes:join(other: Types.Changes, check: boolean?)
	for _, addition in other.additions do
		if check then
			self:removePrevious("additions", addition.id)
		end

		self:add(addition)
	end

	for _, update in other.updates do
		if check and (self:mergeAdditionUpdate(update) or self:mergeUpdate(update)) then
			continue
		end

		self:update(update)
	end

	for _, removal in other.removals do
		if check then
			local removedPendingAddition = self:removePendingAddition(removal)
			self:removePrevious("updates", removal)
			self:removePrevious("removals", removal)
			if removedPendingAddition then
				continue
			end
		end

		self:remove(removal)
	end
end

local function removeChild(self, children, changeId: string): boolean
	for index = #children, 1, -1 do
		local child = children[index]
		if buffer.tostring(child.id) == changeId then
			unindexAddition(self, child)
			table.remove(children, index)
			return true
		end
		if removeChild(self, child.children, changeId) then
			return true
		end
	end
	return false
end

function Changes:removePendingAddition(id: Types.Ref | Instance): boolean
	if typeof(id) ~= "buffer" then
		return false
	end
	local changeId = buffer.tostring(id)
	for index = #self.additions, 1, -1 do
		local addition = self.additions[index]
		if buffer.tostring(addition.id) == changeId then
			unindexAddition(self, addition)
			table.remove(self.additions, index)
			return true
		end
		if removeChild(self, addition.children, changeId) then
			return true
		end
	end
	return false
end

function Changes:removePrevious(kind: "additions" | "updates" | "removals", id: Types.Ref | Instance)
	local changeId = if typeof(id) == "buffer" then buffer.tostring(id) else nil

	for index = #self[kind], 1, -1 do
		local change = self[kind][index]
		local candidate = if kind == "removals" then change else change.id
		local matches = if changeId ~= nil
			then typeof(candidate) == "buffer" and buffer.tostring(candidate) == changeId
			else candidate == id
		if matches then
			if kind == "additions" then
				unindexAddition(self, change)
			elseif kind == "updates" and changeId ~= nil then
				self.updateById[changeId] = nil
			end
			table.remove(self[kind], index)
		end
	end
end

function Changes:total(): number
	return #self.additions + #self.updates + #self.removals
end

function Changes:isEmpty(): boolean
	return self:total() == 0
end

function Changes.Total(changes: Types.Changes): number
	return #changes.additions + #changes.updates + #changes.removals
end

return Changes
