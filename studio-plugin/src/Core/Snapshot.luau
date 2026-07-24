local Carbon = script:FindFirstAncestor("Carbon")

local MsgPack = require(Carbon.Lib.MsgPack)
local Types = require(Carbon.Types)

local function validateProperties(properties: Types.Properties): Types.Properties
	if not next(properties) then
		return MsgPack.Map.new(properties)
	end

	return properties
end

local Snapshot = {}
Snapshot.__index = Snapshot

function Snapshot.new(id: Types.Ref)
	local self = setmetatable({}, Snapshot)

	self.id = id
	self.parent = nil
	self.name = ""
	self.rawName = nil
	self.class = ""
	self.properties = validateProperties({})
	self.children = {}

	return self
end

function Snapshot.newUpdated(id: Types.Ref)
	local self = setmetatable({}, Snapshot)

	self.id = id
	self.parent = nil
	self.name = nil
	self.rawName = nil
	self.class = nil
	self.properties = nil
	self.removedProperties = nil

	return self
end

function Snapshot:withParent(parent: Types.Ref)
	self.parent = parent
	return self
end

function Snapshot:withName(name: string)
	if utf8.len(name) == nil then
		self.name = "�"
		self.rawName = buffer.fromstring(name)
	else
		self.name = name
		self.rawName = nil
	end
	return self
end

function Snapshot:withClass(class: string)
	self.class = class
	return self
end

function Snapshot:withProperties(properties: Types.Properties)
	self.properties = validateProperties(properties)
	return self
end

function Snapshot:withPropertyDelta(properties: Types.Properties, removedProperties: { string })
	self.properties = if next(properties) then validateProperties(properties) else nil
	self.removedProperties = if #removedProperties > 0 then removedProperties else nil
	return self
end

function Snapshot:withChildren(children: { Types.Snapshot })
	self.children = children
	return self
end

return Snapshot
