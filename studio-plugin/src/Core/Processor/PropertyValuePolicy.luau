--!strict

local PropertyValuePolicy = {}

function PropertyValuePolicy.validate(value: any, propertyType: string?, isRawType): boolean
	if
		propertyType == "Attributes"
		or propertyType == "BinaryString"
		or propertyType == "SharedString"
		or propertyType == "NetAssetRef"
		or propertyType == "String"
		or (propertyType ~= nil and isRawType(propertyType))
	then
		return true
	end

	local valueType = type(value)
	if valueType == "string" then
		return utf8.len(value) ~= nil
	end
	if valueType == "table" then
		for key, child in value do
			if
				not PropertyValuePolicy.validate(key, nil, isRawType)
				or not PropertyValuePolicy.validate(child, nil, isRawType)
			then
				return false
			end
		end
	end
	return true
end

return PropertyValuePolicy
