--!strict

local Error = if script then require(script.Parent.Error) else nil

local StyleRuleProperties = {}

function StyleRuleProperties.read(instance: StyleRule)
	return true, instance:GetProperties()
end

function StyleRuleProperties.write(instance: StyleRule, _, value: { [any]: any })
	if typeof(value) ~= "table" then
		assert(Error ~= nil, "StyleRule property validation requires the DOM Error module")
		return false, Error.new(Error.Kind.CannotParseBinaryString)
	end

	local existing = instance:GetProperties()
	for itemName, itemValue in pairs(value) do
		instance:SetProperty(itemName, itemValue)
	end
	for existingItemName in pairs(existing) do
		if value[existingItemName] == nil then
			instance:SetProperty(existingItemName, nil)
		end
	end
	return true
end

return StyleRuleProperties
