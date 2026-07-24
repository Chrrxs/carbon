local Carbon = script:FindFirstAncestor("Carbon")

local Dom = require(Carbon.Lib.Dom)

local Types = require(Carbon.Types)
local Log = require(Carbon.Log)

local equals = require(Carbon.Helpers.equals)
local generateRef = require(Carbon.Helpers.generateRef)

local Snapshot = require(script.Parent.Parent.Snapshot)
local AdditionBoundary = require(script.Parent.Parent.AdditionBoundary)
local Error = require(script.Parent.Parent.Error)
local DefaultComparison = require(script.Parent.DefaultComparison)
local PropertyValuePolicy = require(script.Parent.PropertyValuePolicy)
local NativeReferenceOverlay = require(script.Parent.Parent.NativeReferenceOverlay)

local function validateValue(value: any, propertyType: string?): boolean
	return PropertyValuePolicy.validate(value, propertyType, Dom.isRawType)
end

local function encodePropertyValue(value: any, propertyType: string, tree): (boolean, any)
	if
		type(value) == "string"
		and (
			propertyType == "Attributes"
			or Dom.isRawType(propertyType)
			or (propertyType == "String" and utf8.len(value) == nil)
		)
	then
		return true, { BinaryString = buffer.fromstring(value) }
	end
	if
		propertyType == "Content"
		and typeof(value) == "Content"
		and value.SourceType == Enum.ContentSourceType.Object
	then
		local object = value.Object
		if object ~= nil and tree:getId(object) == nil then
			local encoded = Dom.encodePrivilegedContentObject(object)
			if encoded ~= nil then
				return true, encoded
			end
		end
	end
	return Dom.EncodedValue.encode(value, propertyType, tree)
end

local ReadProcessor = {}
ReadProcessor.__index = ReadProcessor

function ReadProcessor.new(tree)
	return setmetatable({
		tree = tree,
		pendingReferences = setmetatable({}, { __mode = "k" }),
		outgoingReferences = setmetatable({}, { __mode = "k" }),
		incomingReferences = setmetatable({}, { __mode = "k" }),
	}, ReadProcessor)
end

function ReadProcessor:trackReference(source: Instance, property: string, target: any)
	if target ~= nil and typeof(target) ~= "Instance" then
		target = nil
	end
	local outgoing = self.outgoingReferences[source]
	local previous = outgoing and outgoing[property]
	if previous == target then
		return
	end
	if previous then
		local incoming = self.incomingReferences[previous]
		local properties = incoming and incoming[source]
		if properties then
			properties[property] = nil
			if next(properties) == nil then
				incoming[source] = nil
			end
		end
	end
	if not outgoing then
		outgoing = {}
		self.outgoingReferences[source] = outgoing
	end
	outgoing[property] = target
	if target then
		local incoming = self.incomingReferences[target]
		if not incoming then
			incoming = setmetatable({}, { __mode = "k" })
			self.incomingReferences[target] = incoming
		end
		local properties = incoming[source]
		if not properties then
			properties = {}
			incoming[source] = properties
		end
		properties[property] = true
	end
end

function ReadProcessor:forgetSourceReferences(source: Instance)
	local outgoing = self.outgoingReferences[source]
	if not outgoing then
		return
	end
	local properties = {}
	for property in outgoing do
		table.insert(properties, property)
	end
	for _, property in properties do
		self:trackReference(source, property, nil)
	end
	self.outgoingReferences[source] = nil
end

function ReadProcessor:rememberPendingReference(source: Instance, property: string, target: Instance)
	local properties = self.pendingReferences[source]
	if not properties then
		properties = {}
		self.pendingReferences[source] = properties
	end
	properties[property] = target
end

function ReadProcessor:retryPendingReferences(): { Types.UpdatedSnapshot }
	local updates = {}
	local completed = {}
	for source, properties in self.pendingReferences do
		if not self.tree:getId(source) then
			self.pendingReferences[source] = nil
			continue
		end
		for property, target in properties do
			if not self.tree:getId(target) then
				continue
			end
			local update = self:onChange(source, property)
			if update then
				table.insert(updates, update)
			end
			table.insert(completed, { source = source, property = property })
		end
	end
	-- Do not consume any pending edge until every read succeeds. The caller
	-- discards its local Changes when conversion throws, so partial consumption
	-- would make an otherwise replayable Add silently lose earlier updates.
	for _, entry in completed do
		local properties = self.pendingReferences[entry.source]
		if properties then
			properties[entry.property] = nil
			if next(properties) == nil then
				self.pendingReferences[entry.source] = nil
			end
		end
	end
	return updates
end

function ReadProcessor:reserve(
	instance: Instance,
	reserved: { [Instance]: boolean }?,
	claimSourceId: ((Instance) -> Types.Ref?)?
): Types.Ref
	local existing = self.tree:getId(instance)

	if existing then
		return existing
	end

	local id = if claimSourceId then claimSourceId(instance) else nil
	id = id or generateRef()
	local claimed = self.tree:getInstance(id)
	assert(claimed == nil or claimed == instance, "claimed source identity already belongs to another visible instance")
	self.tree:insertInstance(instance, id)
	if reserved then
		reserved[instance] = true
	end

	for _, child in instance:GetChildren() do
		if not Dom.isStudioTransient(child) then
			self:reserve(child, reserved, claimSourceId)
		end
	end

	return id
end

function ReadProcessor:captureAdditions(instances: { Instance }): { Types.AddedSnapshot }
	local reserved = {}
	for _, instance in instances do
		if not Dom.isStudioTransient(instance) then
			self:reserve(instance, reserved)
		end
	end
	local batchInstances = {}
	for instance in reserved do
		table.insert(batchInstances, instance)
	end
	local batchSuccess, batchError = Dom.beginPrivilegedBatch(batchInstances)
	if not batchSuccess then
		for instance in reserved do
			self.tree:removeByInstance(instance)
		end
		error(`Failed to batch exact serialized-property reads for added instances: {tostring(batchError)}`)
	end

	local knownDescendants = {}
	for instance in self.tree.idMap do
		if not reserved[instance] then
			knownDescendants[instance] = true
		end
	end
	local state = {
		failed = false,
		knownDescendants = knownDescendants,
	}
	local additions = {}
	local captureSuccess, captureError = pcall(function()
		for _, instance in instances do
			local parentId = self.tree:getId(instance.Parent)
			if not parentId then
				continue
			end
			local snapshot = self:onAdd(instance, parentId, true, state)
			if snapshot then
				snapshot.parent = parentId
				table.insert(additions, snapshot)
			end
		end
	end)
	Dom.endPrivilegedBatch()
	if not captureSuccess then
		for instance in reserved do
			self.tree:removeByInstance(instance)
		end
		error(captureError)
	end

	if state.failed then
		for instance in reserved do
			self.tree:removeByInstance(instance)
		end
		local root = instances[1]
		local err = Error.new(
			Error.UnsafeAddition,
			root and root:GetFullName() or "initial place",
			`{state.failedInstance:GetFullName()} ({state.failedInstance.ClassName})`,
			state.failedProperty
		)
		error(err)
	end

	return additions
end

function ReadProcessor:unreserve(instance: Instance)
	for _, child in instance:GetChildren() do
		self:unreserve(child)
	end
	self.tree:removeByInstance(instance)
end

function ReadProcessor.findUnsafeAddition(instance: Instance, knownDescendants: any?): (Instance?, { string }?)
	if knownDescendants and not AdditionBoundary.shouldInclude(knownDescendants, instance) then
		return nil, nil
	end
	local properties = Dom.getProtectedSerializedProperties(instance.ClassName)
	if #properties > 0 then
		return instance, properties
	end
	for _, child in instance:GetChildren() do
		local unsafeInstance, unsafeProperties = ReadProcessor.findUnsafeAddition(child, knownDescendants)
		if unsafeInstance then
			return unsafeInstance, unsafeProperties
		end
	end
	return nil, nil
end

function ReadProcessor:onAdd(
	instance: Instance,
	__parentId: Types.Ref?,
	__reserved: boolean?,
	__state: any?,
	__claimSourceId: ((Instance) -> Types.Ref?)?
): Types.AddedSnapshot?
	Log.trace("Detected addition of", instance)
	if Dom.isStudioTransient(instance) then
		return nil
	end
	local state = __state or { failed = false, knownDescendants = {} }
	if __state and not AdditionBoundary.shouldInclude(state.knownDescendants, instance) then
		Log.trace("Existing instance moved under new subtree. Skipping duplicate addition..")
		return nil
	end

	if self.tree:getId(instance) and not __reserved then
		Log.trace("Instance already tracked. Skipping..")
		return nil
	end

	if not __reserved then
		state.knownDescendants = AdditionBoundary.captureKnownDescendants(instance, function(descendant)
			return descendant:GetChildren()
		end, function(descendant)
			return self.tree:getId(descendant) ~= nil
		end)
		local unsafeInstance, unsafeProperties = ReadProcessor.findUnsafeAddition(instance, state.knownDescendants)
		if unsafeInstance and unsafeProperties then
			local err = Error.new(
				Error.UnsafeAddition,
				instance:GetFullName(),
				`{unsafeInstance:GetFullName()} ({unsafeInstance.ClassName})`,
				table.concat(unsafeProperties, ", ")
			)
			error(err)
		end
	end

	local parentId = __parentId or self.tree:getId(instance.Parent)

	if not parentId then
		Log.trace("Unknown instance. Skipping..")
		return nil
	end

	if not __reserved then
		-- One batch covers the whole inclusion boundary. Recursive calls below are
		-- marked reserved and consume this cache instead of issuing one HTTP request
		-- per protected property per instance.
		local reserved = {}
		self:reserve(instance, reserved, __claimSourceId)
		local batchInstances = {}
		for descendant in reserved do
			table.insert(batchInstances, descendant)
		end
		local batchSuccess, batchError = Dom.beginPrivilegedBatch(batchInstances)
		if not batchSuccess then
			for descendant in reserved do
				self.tree:removeByInstance(descendant)
			end
			error(`Failed to batch exact serialized-property reads for added subtree: {tostring(batchError)}`)
		end

		local captureSuccess, snapshot = pcall(function()
			return self:onAdd(instance, parentId, true, state, __claimSourceId)
		end)
		Dom.endPrivilegedBatch()
		if not captureSuccess then
			for descendant in reserved do
				self.tree:removeByInstance(descendant)
			end
			error(snapshot)
		end
		if state.failed then
			for descendant in reserved do
				self.tree:removeByInstance(descendant)
			end
			local err = Error.new(
				Error.UnsafeAddition,
				instance:GetFullName(),
				`{state.failedInstance:GetFullName()} ({state.failedInstance.ClassName})`,
				state.failedProperty
			)
			error(err)
		end
		if snapshot then
			snapshot.parent = parentId
		end
		return snapshot
	end

	-- Reserve the complete subtree before reading any properties. Ref values can
	-- then point forward to descendants or later siblings without being dropped.
	local id = self.tree:getId(instance) or self:reserve(instance, nil, __claimSourceId)

	local properties = {}
	local children = {}
	local defaults = Dom.getDefaultProperties(instance.ClassName)
	local privilegedDescriptors = Dom.getPrivilegedSerializedProperties(instance.ClassName)

	for property, default in defaults do
		local readSuccess, instanceValue = Dom.readProperty(instance, property)

		if not readSuccess then
			local err = Error.new(Error.ReadFailed, property, instance, instanceValue.kind)
			error(err)
		end

		local propertyType = next(default)
		if propertyType == "Content" and typeof(instanceValue) == "Content" then
			self:trackReference(
				instance,
				property,
				if instanceValue.SourceType == Enum.ContentSourceType.Object then instanceValue.Object else nil
			)
		end

		if not DefaultComparison.matches(instanceValue, default, Dom.EncodedValue.decode, equals, self.tree) then
			if not validateValue(instanceValue, propertyType) then
				local err = Error.new(Error.ValidationFailed, property, instance, instanceValue)
				error(err)
			end

			local encodeSuccess, encodedValue = encodePropertyValue(instanceValue, propertyType, self.tree)

			if not encodeSuccess then
				local pending = propertyType == "Content"
					and typeof(instanceValue) == "Content"
					and instanceValue.SourceType == Enum.ContentSourceType.Object
					and instanceValue.Object ~= nil
				if pending then
					self:rememberPendingReference(instance, property, instanceValue.Object)
					continue
				end
				error(Error.new(Error.EncodeFailed, property, instanceValue))
			end

			properties[property] = encodedValue
		end
	end

	for _, descriptor in privilegedDescriptors do
		if state.failed then
			break
		end
		if defaults[descriptor.name] ~= nil then
			continue
		end
		local readSuccess, instanceValue = Dom.readProperty(instance, descriptor.name)
		if not readSuccess then
			local err = Error.new(Error.ReadFailed, descriptor.name, instance, instanceValue.kind)
			Log.warn(err)
			state.failed = true
			state.failedInstance = instance
			state.failedProperty = descriptor.name
			break
		end
		local encodeSuccess, encodedValue = encodePropertyValue(instanceValue, descriptor.dataType, self.tree)
		if not encodeSuccess then
			local err = Error.new(Error.EncodeFailed, descriptor.name, instanceValue)
			Log.warn(err)
			state.failed = true
			state.failedInstance = instance
			state.failedProperty = descriptor.name
			break
		end
		properties[descriptor.name] = encodedValue
	end

	for _, property in Dom.getRefProperties(instance.ClassName) do
		local readSuccess, instanceValue = Dom.readProperty(instance, property)

		if not readSuccess then
			error(Error.new(Error.ReadFailed, property, instance, instanceValue.kind))
		end
		self:trackReference(instance, property, instanceValue)

		local encodeSuccess, encodedValue = Dom.EncodedValue.encode(instanceValue, "Ref", self.tree)

		if not encodeSuccess or encodedValue.Ref == nil then
			if instanceValue ~= nil then
				self:rememberPendingReference(instance, property, instanceValue)
			end
			continue
		end

		properties[property] = encodedValue
	end
	for _, child in instance:GetChildren() do
		local snapshot = self:onAdd(child, id, true, state, __claimSourceId)

		if snapshot then
			table.insert(children, snapshot)
		end
	end

	local snapshot = Snapshot.new(id)
		:withName(instance.Name)
		:withClass(instance.ClassName)
		:withProperties(properties)
		:withChildren(children)

	if state.failed then
		return nil
	end

	if __parentId then
		return snapshot
	end

	snapshot.parent = parentId
	return snapshot
end

function ReadProcessor:onChange(instance: Instance, property: string?, referenceOverlays): Types.UpdatedSnapshot?
	Log.trace("Detected change of", instance, property)
	local id = self.tree:getId(instance)

	if not id then
		Log.trace("Unknown instance. Skipping..")
		return nil
	end

	if property then
		property = Dom.canonicalPropertyName(instance.ClassName, property)
	end

	if property and not self.shouldSyncProperties(instance, property) then
		Log.trace("Instance does not pass sync filter. Skipping..")
		return nil
	end

	if property == "Name" then
		return Snapshot.newUpdated(id):withName(instance.Name)
	end

	local properties = {}
	local removed = {}
	local defaults = Dom.getDefaultProperties(instance.ClassName)
	local refs = Dom.getRefProperties(instance.ClassName)
	local privileged = {}
	for _, descriptor in Dom.getPrivilegedSerializedProperties(instance.ClassName) do
		privileged[descriptor.name] = descriptor
	end

	local function readDefault(changedProperty: string, default)
		local readSuccess, instanceValue, overlaid = NativeReferenceOverlay.read(
			referenceOverlays,
			changedProperty,
			function()
				return Dom.readProperty(instance, changedProperty)
			end
		)

		if not readSuccess then
			error(Error.new(Error.ReadFailed, changedProperty, instance, instanceValue.kind))
		end
		if overlaid then
			removed[changedProperty] = nil
			properties[changedProperty] = instanceValue
			return
		end
		local propertyType = next(default)
		if propertyType == "Content" and typeof(instanceValue) == "Content" then
			self:trackReference(
				instance,
				changedProperty,
				if instanceValue.SourceType == Enum.ContentSourceType.Object then instanceValue.Object else nil
			)
		end

		if DefaultComparison.matches(instanceValue, default, Dom.EncodedValue.decode, equals, self.tree) then
			properties[changedProperty] = nil
			removed[changedProperty] = true
			return
		end

		if not validateValue(instanceValue, propertyType) then
			error(Error.new(Error.ValidationFailed, changedProperty, instance, instanceValue))
		end

		local encodeSuccess, encodedValue = encodePropertyValue(instanceValue, propertyType, self.tree)
		if not encodeSuccess then
			local pending = propertyType == "Content"
				and typeof(instanceValue) == "Content"
				and instanceValue.SourceType == Enum.ContentSourceType.Object
				and instanceValue.Object ~= nil
			if pending then
				self:rememberPendingReference(instance, changedProperty, instanceValue.Object)
				return
			end
			error(Error.new(Error.EncodeFailed, changedProperty, instanceValue))
		end

		removed[changedProperty] = nil
		properties[changedProperty] = encodedValue
	end

	local function readRef(changedProperty: string)
		local readSuccess, instanceValue, overlaid = NativeReferenceOverlay.read(
			referenceOverlays,
			changedProperty,
			function()
				return Dom.readProperty(instance, changedProperty)
			end
		)
		if not readSuccess then
			error(Error.new(Error.ReadFailed, changedProperty, instance, instanceValue.kind))
		end
		if overlaid then
			removed[changedProperty] = nil
			properties[changedProperty] = instanceValue
			return
		end
		self:trackReference(instance, changedProperty, instanceValue)
		if instanceValue == nil then
			local pending = self.pendingReferences[instance]
			if pending then
				pending[changedProperty] = nil
			end
			properties[changedProperty] = nil
			removed[changedProperty] = true
			return
		end

		local encodeSuccess, encodedValue = Dom.EncodedValue.encode(instanceValue, "Ref", self.tree)
		if not encodeSuccess or encodedValue.Ref == nil then
			self:rememberPendingReference(instance, changedProperty, instanceValue)
			local err = Error.new(Error.EncodeFailed, changedProperty, instanceValue)
			Log.warn(err)
			return
		end

		removed[changedProperty] = nil
		properties[changedProperty] = encodedValue
		local pending = self.pendingReferences[instance]
		if pending then
			pending[changedProperty] = nil
		end
	end

	local function readPrivileged(descriptor)
		local readSuccess, instanceValue, overlaid = NativeReferenceOverlay.read(
			referenceOverlays,
			descriptor.name,
			function()
				return Dom.readProperty(instance, descriptor.name)
			end
		)
		if not readSuccess then
			error(Error.new(Error.ReadFailed, descriptor.name, instance, instanceValue.kind))
		end
		if overlaid then
			properties[descriptor.name] = instanceValue
			return
		end
		local encodeSuccess, encodedValue = encodePropertyValue(instanceValue, descriptor.dataType, self.tree)
		if not encodeSuccess then
			error(Error.new(Error.EncodeFailed, descriptor.name, instanceValue))
		end
		properties[descriptor.name] = encodedValue
	end

	if property then
		local default = defaults[property]
		if default then
			readDefault(property, default)
		elseif table.find(refs, property) then
			readRef(property)
		elseif privileged[property] then
			readPrivileged(privileged[property])
		end
	else
		for changedProperty, default in defaults do
			readDefault(changedProperty, default)
		end
		for _, changedProperty in refs do
			readRef(changedProperty)
		end
		for changedProperty, descriptor in privileged do
			if defaults[changedProperty] == nil then
				readPrivileged(descriptor)
			end
		end
	end

	local removedProperties = {}
	for changedProperty in removed do
		table.insert(removedProperties, changedProperty)
	end
	table.sort(removedProperties)

	if not next(properties) and #removedProperties == 0 then
		return nil
	end

	return Snapshot.newUpdated(id):withPropertyDelta(properties, removedProperties)
end

function ReadProcessor:onRemove(instance: Instance, capturedInstances: { Instance }?): Types.Ref?
	Log.trace("Detected removal of", instance)

	local id = self.tree:getId(instance)

	if not id then
		Log.trace("Unknown instance. Skipping..")
		return nil
	end

	-- The reverse live-reference index makes removal proportional to the removed
	-- subtree plus its actual incoming edges. A full-place scan per descendant
	-- removal is catastrophic in large places.
	local removedInstances = capturedInstances or { instance }
	for _, target in removedInstances do
		local incoming = self.incomingReferences[target]
		if incoming then
			for source, properties in incoming do
				if source ~= target and self.tree:getId(source) then
					for property in properties do
						self:rememberPendingReference(source, property, target)
					end
				end
			end
		end
		self.incomingReferences[target] = nil
		self:forgetSourceReferences(target)
		self.pendingReferences[target] = nil
	end

	for _, target in removedInstances do
		self.tree:removeByInstance(target)
	end

	return id
end

function ReadProcessor:onMove(instance: Instance): Types.UpdatedSnapshot?
	if Dom.isStudioTransient(instance) then
		return nil
	end
	local id = self.tree:getId(instance)
	local parentId = self.tree:getId(instance.Parent)

	if not id or not parentId then
		Log.trace("Unknown move target. Skipping..")
		return nil
	end

	return Snapshot.newUpdated(id):withParent(parentId)
end

function ReadProcessor.shouldSyncProperties(instance: Instance, property: string?): boolean
	if property == nil or property == "Name" then
		return true
	end

	if
		Dom.getDefaultProperties(instance.ClassName)[property] ~= nil
		or table.find(Dom.getRefProperties(instance.ClassName), property) ~= nil
	then
		return true
	end
	for _, descriptor in Dom.getPrivilegedSerializedProperties(instance.ClassName) do
		if descriptor.name == property then
			return true
		end
	end
	return false
end

return ReadProcessor
