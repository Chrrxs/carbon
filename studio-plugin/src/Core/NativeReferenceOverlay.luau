--!strict

local NativeReferenceOverlay = {}

function NativeReferenceOverlay.record(overlays, sourceId: buffer, property: string, value)
	local key = buffer.tostring(sourceId)
	local properties = overlays[key]
	if properties == nil then
		properties = {}
		overlays[key] = properties
	end
	properties[property] = value
end

function NativeReferenceOverlay.forSource(overlays, sourceId: buffer)
	return overlays and overlays[buffer.tostring(sourceId)] or nil
end

function NativeReferenceOverlay.get(properties, property: string): (boolean, any?)
	if properties == nil or properties[property] == nil then
		return false, nil
	end
	return true, properties[property]
end

function NativeReferenceOverlay.read(properties, property: string, fallback): (boolean, any?, boolean)
	local overlaid, value = NativeReferenceOverlay.get(properties, property)
	if overlaid then
		return true, value, true
	end
	local success, result = fallback()
	return success, result, false
end

function NativeReferenceOverlay.matchesSource(properties, property: string, sourceValue, equals): boolean?
	local overlaid, value = NativeReferenceOverlay.get(properties, property)
	if not overlaid then
		return nil
	end
	return sourceValue ~= nil and equals(sourceValue, value)
end

return NativeReferenceOverlay
