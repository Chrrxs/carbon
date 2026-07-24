local PropertyDelta = {}

local function toSet(properties: { [string]: any }?): { [string]: any }
	return if properties then table.clone(properties) else {}
end

local function toRemovalSet(properties: { string }?): { [string]: boolean }
	local result = {}
	for _, property in properties or {} do
		result[property] = true
	end
	return result
end

function PropertyDelta.merge(
	currentSet: { [string]: any }?,
	currentRemove: { string }?,
	nextSet: { [string]: any }?,
	nextRemove: { string }?
): ({ [string]: any }?, { string }?)
	local set = toSet(currentSet)
	local remove = toRemovalSet(currentRemove)

	for _, property in nextRemove or {} do
		set[property] = nil
		remove[property] = true
	end
	for property, value in nextSet or {} do
		remove[property] = nil
		set[property] = value
	end

	local removedProperties = {}
	for property in remove do
		table.insert(removedProperties, property)
	end
	table.sort(removedProperties)

	return if next(set) then set else nil, if #removedProperties > 0 then removedProperties else nil
end

return PropertyDelta
