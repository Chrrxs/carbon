--!strict

-- Keeps the policy decision for exact serialized values separate from the
-- transport so it can be fault-injected outside Studio. A nil first result
-- means that no exact transport was advertised; false means an advertised
-- transport failed and callers must abort instead of degrading the value.

local ExactSerializedAccess = {}

function ExactSerializedAccess.read(access, instance, propertyName: string, dataType: string): (boolean?, any?)
	if not access.supportsType(dataType) then
		return nil, `exact {dataType} reads are unavailable`
	end
	local success, value = access.readRaw(instance, propertyName, dataType)
	if success then
		return true, value
	end
	return false, value or `exact read failed for {propertyName}`
end

function ExactSerializedAccess.write(
	access,
	instance,
	propertyName: string,
	dataType: string,
	value: string
): (boolean, any?)
	if not access.supportsType(dataType) then
		return false, `exact {dataType} writes are unavailable`
	end
	local success, reason = access.writeRaw(instance, propertyName, dataType, value)
	if success then
		return true
	end
	return false, reason or `exact write failed for {propertyName}`
end

return ExactSerializedAccess
