--!native

local Tree = {}
Tree.__index = Tree

function Tree.new()
	local self = {
		instanceMap = {},
		idMap = {},
	}

	return setmetatable(self, Tree)
end

function Tree:insertInstance(instance: Instance, id: buffer)
	local strId = buffer.tostring(id)
	local previousInstance = self.instanceMap[strId]
	local previousId = self.idMap[instance]

	-- Keep both directions strictly one-to-one. Source replacement can remove an
	-- object and materialize another object with the same stable ID before the
	-- watcher's delayed removal callback runs. Leaving the old reverse entry in
	-- place lets that stale callback remove the replacement's live binding.
	if previousInstance and previousInstance ~= instance then
		self.idMap[previousInstance] = nil
	end
	if previousId and previousId ~= strId and self.instanceMap[previousId] == instance then
		self.instanceMap[previousId] = nil
	end

	self.instanceMap[strId] = instance
	self.idMap[instance] = strId
end

function Tree:updateInstance(id: buffer, instance: Instance)
	local strId = buffer.tostring(id)
	local old = self.instanceMap[strId]

	if not old then
		return
	end

	self:insertInstance(instance, id)
end

function Tree:getId(instance: Instance): buffer?
	local strId = self.idMap[instance]
	return strId and buffer.fromstring(strId)
end

function Tree:getInstance(id: buffer): Instance?
	return self.instanceMap[buffer.tostring(id)]
end

function Tree:removeByInstance(instance: Instance): buffer?
	local strId = self.idMap[instance]

	if not strId then
		return nil
	end

	if self.instanceMap[strId] == instance then
		self.instanceMap[strId] = nil
	end
	self.idMap[instance] = nil

	return buffer.fromstring(strId)
end

function Tree:removeById(id: buffer): Instance?
	local strId = buffer.tostring(id)
	local instance = self.instanceMap[strId]

	if not instance then
		return nil
	end

	self.instanceMap[strId] = nil
	if self.idMap[instance] == strId then
		self.idMap[instance] = nil
	end
	return instance
end

return Tree
