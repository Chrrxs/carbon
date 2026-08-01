local Carbon = script:FindFirstAncestor("Carbon")

local Dom = require(Carbon.Lib.Dom)

local Log = require(Carbon.Log)
local Types = require(Carbon.Types)

local setWaypoint = require(Carbon.Helpers.setWaypoint)

local Error = require(script.Parent.Parent.Error)
local PropertyWriteOrder = require(script.Parent.Parent.PropertyWriteOrder)

local WriteProcessor = {}
WriteProcessor.__index = WriteProcessor

local function snapshotName(snapshot): string
	return if snapshot.rawName then buffer.tostring(snapshot.rawName) else snapshot.name
end

function WriteProcessor.new(tree)
	return setmetatable({
		tree = tree,
	}, WriteProcessor)
end

function WriteProcessor:applyChanges(changes: Types.Changes, initial: boolean?)
	Log.trace("Applying changes..")

	setWaypoint("Sync")

	-- Create and register the whole addition forest before applying properties.
	-- This lets Ref properties target descendants, later siblings, or another root.
	for _, snapshot in changes.additions do
		assert(self:createAddition(snapshot) ~= nil, "failed to create required addition")
	end

	for _, snapshot in changes.additions do
		self:hydrateAddition(snapshot, initial)
	end

	for _, snapshot in changes.updates do
		self:applyUpdate(snapshot, initial)
	end

	for _, object in changes.removals do
		self:applyRemoval(object, initial)
	end

	if initial then
		setWaypoint("Initial Sync")
	end
end

function WriteProcessor:createInstance(
	snapshot: Types.HierarchySnapshot,
	_suppressExpectedUnavailableWarning: boolean?
): Instance?
	Log.trace("Applying addition of", snapshot)

	local tracked = self.tree:getInstance(snapshot.id)

	if tracked then
		return tracked
	end

	local parent = self.tree:getInstance(snapshot.parent)

	if not parent then
		error(Error.new(Error.NoInstanceAdd, snapshot))
	end

	local instance
	local isService = false

	if Dom.isCreatable(snapshot.class) then
		local createSuccess, created = pcall(Instance.new, snapshot.class)
		instance = if createSuccess then created else nil
	elseif parent == game then
		local serviceSuccess, service = pcall(game.FindFirstChildOfClass, game, snapshot.class)

		if serviceSuccess and service then
			instance = service
			isService = true
		end
	end

	if not instance then
		error(Error.new(Error.HydrationFailed, snapshot))
	end

	local attachSuccess = pcall(function()
		instance.Name = snapshotName(snapshot)
		if not isService then
			instance.Parent = parent
		end
	end)
	if not attachSuccess then
		pcall(function()
			instance:Destroy()
		end)
		error(Error.new(Error.HydrationFailed, snapshot))
	end

	self.tree:insertInstance(instance, snapshot.id)
	return instance
end

function WriteProcessor:createAddition(snapshot: Types.AddedSnapshot): Instance?
	local tracked = self.tree:getInstance(snapshot.id)
	if tracked then
		return tracked
	end

	local instance = self:createInstance(snapshot)
	if not instance then
		error(Error.new(Error.HydrationFailed, snapshot))
	end

	for _, child in snapshot.children do
		child.parent = snapshot.id
		assert(self:createAddition(child) ~= nil, "failed to create required descendant")
	end

	return instance
end

function WriteProcessor:hydrateAddition(snapshot: Types.AddedSnapshot, initial: boolean?): Instance?
	local instance = self.tree:getInstance(snapshot.id)

	if not instance then
		error(Error.new(Error.NoInstanceUpdate, snapshot))
	end

	for _, property in PropertyWriteOrder.keys(snapshot.properties) do
		local value = snapshot.properties[property]
		if not Dom.canWriteProperty(instance.ClassName, property) then
			error(Error.new(Error.WriteFailed, property, instance, "unsupported serialized property"))
		end

		local decodeSuccess, decodedValue = Dom.EncodedValue.decode(value, self.tree)

		if not decodeSuccess then
			local err = Error.new(Error.DecodeFailed, property, value)
			error(err)
		end

		local writeSuccess, reason = Dom.writeProperty(instance, property, decodedValue)

		if not writeSuccess then
			local err = Error.new(Error.WriteFailed, property, instance, reason.kind)
			error(err)
		end
	end

	for _, child in snapshot.children do
		self:hydrateAddition(child, initial)
	end

	if not initial then
		setWaypoint("Add", instance.Name)
	end

	return instance
end

function WriteProcessor:applyUpdate(snapshot: Types.UpdatedSnapshot, initial: boolean?): Instance?
	Log.trace("Applying update of", snapshot)

	local instance = self.tree:getInstance(snapshot.id)

	if not instance then
		error(Error.new(Error.NoInstanceUpdate, snapshot))
	end

	local defaultProperties

	if snapshot.class then
		local newInstance

		if Dom.isCreatable(snapshot.class) then
			local createSuccess, created = pcall(Instance.new, snapshot.class)
			newInstance = if createSuccess then created else nil
		end
		if not newInstance then
			error(Error.new(Error.NotCreatable, snapshot.class))
		end
		defaultProperties = Dom.getDefaultProperties(snapshot.class)

		newInstance.Name = instance.Name

		for _, property in PropertyWriteOrder.keys(defaultProperties) do
			if not Dom.canWriteProperty(newInstance.ClassName, property) then
				error(Error.new(Error.WriteFailed, property, newInstance, "unsupported serialized property"))
			end

			local readSuccess, instanceValue = Dom.readProperty(instance, property)
			if not readSuccess then
				error(Error.new(Error.ReadFailed, property, instance, instanceValue.kind))
			end
			local writeSuccess, reason = Dom.writeProperty(newInstance, property, instanceValue)
			if not writeSuccess then
				error(Error.new(Error.WriteFailed, property, newInstance, reason.kind))
			end
		end
		for _, child in instance:GetChildren() do
			child.Parent = newInstance
		end

		newInstance.Parent = instance.Parent
		instance.Parent = nil

		self.tree:updateInstance(snapshot.id, newInstance)

		instance = newInstance
	end

	if snapshot.name then
		instance.Name = snapshotName(snapshot)
	end

	if snapshot.parent then
		local parent = self.tree:getInstance(snapshot.parent)
		if not parent then
			error(Error.new(Error.NoInstanceMove, snapshot))
		end
		instance.Parent = parent
	end

	if snapshot.properties then
		for _, property in PropertyWriteOrder.keys(snapshot.properties) do
			local value = snapshot.properties[property]
			if not Dom.canWriteProperty(instance.ClassName, property) then
				error(Error.new(Error.WriteFailed, property, instance, "unsupported serialized property"))
			end

			local decodeSuccess, decodedValue = Dom.EncodedValue.decode(value, self.tree)
			if not decodeSuccess then
				local err = Error.new(Error.DecodeFailed, property, value)
				error(err)
			end

			local writeSuccess, reason = Dom.writeProperty(instance, property, decodedValue)
			if not writeSuccess then
				local err = Error.new(Error.WriteFailed, property, instance, reason.kind)
				error(err)
			end
		end
	end

	if not initial and snapshot.removedProperties then
		local defaults = defaultProperties or Dom.getDefaultProperties(instance.ClassName)
		for _, property in PropertyWriteOrder.names(snapshot.removedProperties) do
			if not Dom.canWriteProperty(instance.ClassName, property) then
				error(Error.new(Error.WriteFailed, property, instance, "unsupported serialized property"))
			end
			local default = defaults[property]
			local decodedValue = nil
			if default then
				local decodeSuccess, value = Dom.EncodedValue.decode(default, self.tree)
				if not decodeSuccess then
					local err = Error.new(Error.DecodeFailed, property, default)
					error(err)
				end
				decodedValue = value
			end
			local writeSuccess, reason = Dom.writeProperty(instance, property, decodedValue)
			if not writeSuccess then
				local err = Error.new(Error.WriteFailed, property, instance, reason.kind)
				error(err)
			end
		end
	end

	if not initial then
		setWaypoint("Update", instance.Name)
	end

	return instance
end

function WriteProcessor:applyRemoval(object: Types.Ref | Instance, initial: boolean?): Instance?
	Log.trace("Applying removal of", object)

	if typeof(object) == "Instance" then
		if not Dom.isRemovable(object.ClassName) then
			error(Error.new(Error.NotRemovable, object.ClassName))
		end

		local removeSuccess = pcall(function()
			object.Parent = nil
		end)
		if not removeSuccess then
			error(Error.new(Error.NotRemovable, object.ClassName))
		end
		self.tree:removeByInstance(object)
	else
		local instance = self.tree:getInstance(object)

		if not instance then
			error(Error.new(Error.NoInstanceRemove, object))
		end

		if not Dom.isRemovable(instance.ClassName) then
			error(Error.new(Error.NotRemovable, instance.ClassName))
		end

		local removeSuccess = pcall(function()
			instance.Parent = nil
		end)
		if not removeSuccess then
			error(Error.new(Error.NotRemovable, instance.ClassName))
		end
		self.tree:removeById(object)

		object = instance
	end

	if not initial then
		setWaypoint("Remove", (object :: Instance).Name)
	end

	return object
end

return WriteProcessor
