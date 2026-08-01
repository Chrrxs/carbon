local Carbon = script:FindFirstAncestor("Carbon")

local Dom = require(Carbon.Lib.Dom)

local Log = require(Carbon.Log)
local Types = require(Carbon.Types)
local equals = require(Carbon.Helpers.equals)
local TransientMetadata = require(Carbon.TransientMetadata)
local DefaultComparison = require(script.DefaultComparison)

local Error = require(script.Parent.Error)
local Changes = require(script.Parent.Changes)
local RuntimeRootPolicy = require(script.Parent.RuntimeRootPolicy)
local NativeReferenceOverlay = require(script.Parent.NativeReferenceOverlay)
local InitialReconciliation = require(script.Parent.InitialReconciliation)
local InitialRootBaseline = require(script.Parent.InitialRootBaseline)
local RuntimeServices = require(Carbon.RuntimeServices)

local ReadProcessor = require(script.Read)
local WriteProcessor = require(script.Write)

local Processor = {}
Processor.__index = Processor

local function snapshotName(snapshot): string
	return if snapshot.rawName then buffer.tostring(snapshot.rawName) else snapshot.name
end

function Processor.new(tree)
	local self = {
		tree = tree,
		read = ReadProcessor.new(tree),
		write = WriteProcessor.new(tree),
		initialExistingIds = nil,
		initialChildren = nil,
		initialNames = nil,
		initialMissingIds = nil,
		initialMissingRoots = nil,
		preserveInitialExistingState = false,
		initialRootDebugIds = nil,
		sourceRootKeys = {},
	}

	return setmetatable(self, Processor)
end

function Processor:hasSourceRoot(className: string, name: string): boolean
	return self.sourceRootKeys[`{className}\0{name}`] == true
end

function Processor:setInitialRoots(roots)
	self.initialRootDebugIds = InitialRootBaseline.indexCurrent(roots)
end

function Processor:beginInitialStream(
	preserveExistingProperties: boolean,
	initialChildren: Types.InitialChildren?,
	initialNames: Types.InitialNames?
)
	self.preserveInitialExistingState = preserveExistingProperties
	self.initialExistingIds = if preserveExistingProperties then {} else nil
	self.initialMissingIds = if preserveExistingProperties then {} else nil
	self.initialMissingRoots = if preserveExistingProperties then {} else nil
	self.initialChildren = {}
	self.initialNames = initialNames
	self.frozenInitialChildren = initialChildren
end

local ZERO_REF = string.rep("\0", 16)

function Processor:hydratePage(instances: { Types.HierarchySnapshot })
	for _, snapshot in instances do
		local parentId = buffer.tostring(snapshot.parent)
		local snapshotId = buffer.tostring(snapshot.id)
		if self.initialMissingIds and self.initialMissingIds[parentId] then
			self.initialMissingIds[snapshotId] = true
			continue
		end
		local parent = if buffer.tostring(snapshot.parent) == ZERO_REF
			then game
			else self.tree:getInstance(snapshot.parent)
		if parent == game and snapshot.class ~= "DataModel" then
			self.sourceRootKeys[`{snapshot.class}\0{snapshotName(snapshot)}`] = true
		end

		if snapshot.unavailable then
			if parent ~= game or snapshot.class == "DataModel" then
				error(Error.new(Error.InvalidUnavailableRoot, snapshot))
			end
			self:rememberUnavailable(snapshot.id)
			Log.debug("Skipping Studio-unavailable root:", snapshot.class)
			continue
		end

		if
			parent == game
			and snapshot.class ~= "DataModel"
			and InitialRootBaseline.shouldPreserveUnavailableClass(Dom.hasClass(snapshot.class))
		then
			self:rememberUnavailable(snapshot.id)
			Log.debug("Skipping root unavailable in this Studio DataModel:", snapshot.class, snapshot.name)
			continue
		end

		if not parent then
			if self.unavailableIds and self.unavailableIds[parentId] then
				self:rememberUnavailable(snapshot.id)
				continue
			end
			error(Error.new(Error.NoInstanceAdd, snapshot))
		end

		local instance = if parent == game and snapshot.class == "DataModel" then game else nil
		if not instance then
			local parentChildren = self.initialChildren[parent]
			if not parentChildren then
				-- Root services can be instantiated between the initial hierarchy
				-- snapshot and the native availability probe. Match roots against the
				-- current DataModel; descendants remain frozen so exact-place startup
				-- does not adopt unrelated instances created while hydration runs.
				local ordered = if parent == game
					then InitialRootBaseline.eligibleCurrentChildren(parent:GetChildren(), function(child)
						return RuntimeServices.isMaterialized(child)
					end)
					elseif self.frozenInitialChildren then self.frozenInitialChildren[parent] or {}
					else parent:GetChildren()
				parentChildren = {
					byClassAndName = {},
					ordered = ordered,
				}
				for _, child in parentChildren.ordered do
					local byName = parentChildren.byClassAndName[child.ClassName]
					if not byName then
						byName = {}
						parentChildren.byClassAndName[child.ClassName] = byName
					end
					local initialName = InitialReconciliation.identityName(self.initialNames, child)
					local bucket = byName[initialName]
					if not bucket then
						bucket = {}
						byName[initialName] = bucket
					end
					InitialReconciliation.appendIdentityCandidate(bucket, self.initialNames, child)
				end
				self.initialChildren[parent] = parentChildren
			end

			local byName = parentChildren.byClassAndName[snapshot.class]
			local bucket = byName and byName[snapshotName(snapshot)]
			if bucket then
				for _, child in bucket do
					if not self.tree:getId(child) then
						instance = child
						break
					end
				end
			end
		end

		if instance then
			self.tree:insertInstance(instance, snapshot.id)
			if self.initialExistingIds then
				self.initialExistingIds[buffer.tostring(snapshot.id)] = true
			end
		else
			if self.initialMissingIds then
				self.initialMissingIds[snapshotId] = true
				table.insert(self.initialMissingRoots, snapshot.id)
				continue
			end
			local created = self.write:createInstance(snapshot, true)
			if not created then
				self:rememberUnavailable(snapshot.id)
				Log.debug("Skipping individually unavailable instance:", snapshot.class, snapshot.name)
			end
		end
	end
end

function Processor:rememberUnavailable(id: Types.Ref)
	if not self.unavailableIds then
		self.unavailableIds = {}
	end
	self.unavailableIds[buffer.tostring(id)] = true
end

function Processor:diffPropertyPage(instances: { Types.PropertySnapshot }): Types.Changes
	local changes = Changes.new()
	for _, snapshot in instances do
		local instance = self.tree:getInstance(snapshot.id)
		if not instance then
			local id = buffer.tostring(snapshot.id)
			if
				(self.unavailableIds and self.unavailableIds[id])
				or (self.initialMissingIds and self.initialMissingIds[id])
			then
				continue
			end
			error(Error.new(Error.NoInstanceUpdate, snapshot))
		end
		if self.initialExistingIds and self.initialExistingIds[buffer.tostring(snapshot.id)] then
			continue
		end
		changes:join(self:diffProperties(snapshot))
	end
	return changes
end

function Processor:diffMetadataPage(instances: { Types.PropertySnapshot }): Types.Changes
	local changes = Changes.new()

	for _, snapshot in instances do
		local instance = self.tree:getInstance(snapshot.id)
		if not instance then
			continue
		end
		self:rememberAuthoredTransientAttributes(snapshot, instance)

		local defaults = Dom.getDefaultProperties(instance.ClassName)
		for _, property in { "Attributes", "Tags" } do
			local encoded = snapshot.properties[property] or defaults[property]
			if not encoded then
				continue
			end

			local readSuccess, current = Dom.readProperty(instance, property)
			local decodeSuccess, canonical = Dom.EncodedValue.decode(encoded, self.tree)
			if not readSuccess or not decodeSuccess or equals(current, canonical) then
				continue
			end

			local update = self.read:onChange(instance, property)
			if update then
				changes:update(update)
			end
		end
	end

	return changes
end

function Processor:diffSyncbackPage(instances: { Types.PropertySnapshot }, referenceOverlays): Types.Changes
	local changes = Changes.new()
	local function isLive(instance): boolean
		if instance == game then
			return true
		end
		local success, live = pcall(instance.IsDescendantOf, instance, game)
		return success and live
	end
	local success, result = pcall(function()
		for _, snapshot in instances do
			local instance = self.tree:getInstance(snapshot.id)
			if not instance or not isLive(instance) then
				continue
			end

			-- Arbitrary-place adoption performs the complete canonical comparison,
			-- including readable defaults and references. Managed builds
			-- skip this entire path when their embedded generation matches.
			local instanceOverlays = NativeReferenceOverlay.forSource(referenceOverlays, snapshot.id)
			if self:diffProperties(snapshot, instanceOverlays):total() > 0 then
				local update = self.read:onChange(instance, nil, instanceOverlays)
				if update then
					changes:update(update)
				end
			end
		end
	end)
	if not success then
		error(result)
	end

	return changes
end

function Processor:rememberAuthoredTransientAttributes(snapshot: Types.PropertySnapshot, instance: Instance)
	local defaults = Dom.getDefaultProperties(instance.ClassName)
	local encoded = snapshot.properties.Attributes or defaults.Attributes
	if encoded == nil then
		TransientMetadata.rememberAuthoredAttributes(instance, {})
		return
	end
	local success, attributes = Dom.EncodedValue.decode(encoded, self.tree)
	if success and type(attributes) == "table" then
		TransientMetadata.rememberAuthoredAttributes(instance, attributes)
	end
end

function Processor:reconcileReferencePage(instances: { Types.PropertySnapshot })
	for _, snapshot in instances do
		local instance = self.tree:getInstance(snapshot.id)
		if not instance then
			continue
		end

		for _, property in Dom.getRefProperties(instance.ClassName) do
			local value = snapshot.properties[property]
			if value == nil then
				continue
			end

			local decodeSuccess, canonicalValue = Dom.EncodedValue.decode(value, self.tree)
			if not decodeSuccess then
				local err = Error.new(Error.DecodeFailed, property, value)
				Log.warn(err)
				continue
			end

			local readSuccess, instanceValue = Dom.readProperty(instance, property)
			if not readSuccess then
				local err = Error.new(Error.ReadFailed, property, instance, instanceValue.kind)
				Log.warn(err)
				continue
			end
			if equals(instanceValue, canonicalValue) then
				continue
			end

			local writeSuccess, reason = Dom.writeProperty(instance, property, canonicalValue)
			if not writeSuccess then
				local err = Error.new(Error.WriteFailed, property, instance, reason.kind)
				Log.warn(err)
			end
		end
	end
end

function Processor:finishStream(captureExistingDifferences: boolean?): Types.Changes
	local changes = Changes.new()
	if self.preserveInitialExistingState and captureExistingDifferences ~= false then
		for _, id in self.initialMissingRoots or {} do
			changes:remove(id)
		end

		local additionRoots = {}
		for _, parent in self.tree.instanceMap do
			-- Adopt only the hierarchy frozen before connection. This includes
			-- authored services already in an arbitrary place while excluding lazy
			-- services materialized as a side effect of reconciliation.
			local initialChildren = self.frozenInitialChildren and self.frozenInitialChildren[parent] or {}
			for _, child in initialChildren do
				local existedBeforeCarbon = true
				if parent == game and self.initialRootDebugIds ~= nil then
					local success, debugId = pcall(child.GetDebugId, child, 128)
					existedBeforeCarbon = success and self.initialRootDebugIds[debugId] == true
				end
				local shouldAdoptRoot = parent ~= game
					or RuntimeRootPolicy.shouldAdoptInitial(
						Dom.isService(child.ClassName),
						TransientMetadata.isRoot(child.ClassName, child.Name),
						false
					)
				if
					InitialRootBaseline.shouldAdoptFrozen(
						existedBeforeCarbon,
						self.tree:getId(child) ~= nil,
						shouldAdoptRoot,
						RuntimeServices.isMaterialized(child),
						Dom.isStudioTransient(child)
					)
				then
					table.insert(additionRoots, child)
				end
			end
		end
		for _, addition in self.read:captureAdditions(additionRoots) do
			changes:add(addition)
		end
	end
	self.unavailableIds = nil
	self.initialExistingIds = nil
	self.initialMissingIds = nil
	self.initialMissingRoots = nil
	self.initialChildren = nil
	self.initialNames = nil
	self.frozenInitialChildren = nil
	self.initialRootDebugIds = nil

	if not self.preserveInitialExistingState then
		for _, instance in self.tree.instanceMap do
			for _, child in instance:GetChildren() do
				if
					not self.tree:getId(child)
					and Dom.isRemovable(child.ClassName)
					and (instance ~= game or RuntimeRootPolicy.shouldAdoptInitial(
						Dom.isService(child.ClassName),
						TransientMetadata.isRoot(child.ClassName, child.Name),
						false
					))
					and not Dom.isStudioTransient(child)
				then
					changes:remove(child)
				end
			end
		end
	end
	self.preserveInitialExistingState = false
	return changes
end

function Processor:diffProperties(snapshot: Types.PropertySnapshot, referenceOverlays): Types.Changes
	local changes = Changes.new()

	local instance = self.tree:getInstance(snapshot.id)
	self:rememberAuthoredTransientAttributes(snapshot, instance)

	local updatedProperties = {}
	local defaults = Dom.getDefaultProperties(instance.ClassName)

	for property, default in defaults do
		local value = snapshot.properties[property]
		local overlaid, liveValue = NativeReferenceOverlay.get(referenceOverlays, property)
		if overlaid then
			if not NativeReferenceOverlay.matchesSource(referenceOverlays, property, value, equals) then
				updatedProperties[property] = liveValue
			end
			continue
		end

		if value then
			local readSuccess, instanceValue = Dom.readProperty(instance, property)

			if not readSuccess then
				error(Error.new(Error.ReadFailed, property, instance, instanceValue.kind))
			end
			if next(default) == "Content" and typeof(instanceValue) == "Content" then
				self.read:trackReference(
					instance,
					property,
					if instanceValue.SourceType == Enum.ContentSourceType.Object then instanceValue.Object else nil
				)
			end

			local decodeSuccess, snapshotValue = Dom.EncodedValue.decode(value, self.tree)

			if not decodeSuccess then
				error(Error.new(Error.DecodeFailed, property, value))
			end

			if not equals(instanceValue, snapshotValue) then
				updatedProperties[property] = value
			end

			-- If snapshot does not have the property we want it to be default
		else
			local readSuccess, instanceValue = Dom.readProperty(instance, property)

			if not readSuccess then
				error(Error.new(Error.ReadFailed, property, instance, instanceValue.kind))
			end
			if next(default) == "Content" and typeof(instanceValue) == "Content" then
				self.read:trackReference(
					instance,
					property,
					if instanceValue.SourceType == Enum.ContentSourceType.Object then instanceValue.Object else nil
				)
			end

			if not DefaultComparison.matches(instanceValue, default, Dom.EncodedValue.decode, equals, self.tree) then
				updatedProperties[property] = default
			end
		end
	end

	for _, property in Dom.getRefProperties(instance.ClassName) do
		local value = snapshot.properties[property]
		local overlaid, liveValue = NativeReferenceOverlay.get(referenceOverlays, property)
		if overlaid then
			if not NativeReferenceOverlay.matchesSource(referenceOverlays, property, value, equals) then
				updatedProperties[property] = liveValue
			end
			continue
		end
		local readSuccess, instanceValue = Dom.readProperty(instance, property)

		if not readSuccess then
			error(Error.new(Error.ReadFailed, property, instance, instanceValue.kind))
		end
		self.read:trackReference(instance, property, instanceValue)

		local snapshotValue = nil

		if value then
			local decodeSuccess, decoded = Dom.EncodedValue.decode(value, self.tree)

			if not decodeSuccess then
				error(Error.new(Error.DecodeFailed, property, value))
			end

			snapshotValue = decoded
		end

		if not equals(instanceValue, snapshotValue) then
			if value then
				updatedProperties[property] = value
			else
				local _, encoded = Dom.EncodedValue.encode(nil, "Ref", self.tree)
				updatedProperties[property] = encoded
			end
		end
	end

	if next(updatedProperties) then
		changes:update({
			id = snapshot.id,
			properties = updatedProperties,
		})
	end

	return changes
end

return Processor
