--!strict

-- RML transports reflection values as opaque bytes. Binary/string/scalar
-- descriptors retain their existing textual encoding; fixed-layout Roblox
-- value types use the engine's little-endian in-memory representation so
-- hidden CFrames and vectors round-trip without decimal conversion loss.

local MsgPack = if script then require(script:FindFirstAncestor("Carbon").Lib.MsgPack) else require("../MsgPack")

local BLITTABLE_SIZES = {
	CFrame = 48,
	NumberRange = 8,
	Vector2 = 8,
	Vector3 = 12,
	Vector3int16 = 6,
}

local RAW_TYPES = {
	Int64 = true,
	Float32 = true,
	Float64 = true,
	CFrame = true,
	Color3 = true,
	ColorSequence = true,
	OptionalCFrame = true,
	NumberRange = true,
	NumberSequence = true,
	PhysicalProperties = true,
	Ray = true,
	Rect = true,
	Region3 = true,
	UDim = true,
	UDim2 = true,
	Vector2 = true,
	Vector3 = true,
	Vector3int16 = true,
}

local function bytes(raw: string, dataType: string): buffer
	local value = buffer.fromstring(raw)
	local expected = BLITTABLE_SIZES[dataType]
	assert(expected ~= nil and buffer.len(value) == expected, `invalid privileged {dataType} byte length`)
	return value
end

local Codec = {}

local function f32(value: number): number
	local encoded = buffer.create(4)
	buffer.writef32(encoded, 0, value)
	return buffer.readf32(encoded, 0)
end

function Codec.engineToCanonical(dataType: string, raw: string): string
	if dataType == "Region3" then
		local source = buffer.fromstring(raw)
		if buffer.len(source) == 24 then
			return raw
		end
		assert(buffer.len(source) == 60, "invalid privileged Region3 byte length")
		local output = buffer.create(24)
		for axis = 0, 2 do
			local center = buffer.readf32(source, 36 + axis * 4)
			local size = buffer.readf32(source, 48 + axis * 4)
			local half = f32(size * 0.5)
			buffer.writef32(output, axis * 4, f32(center - half))
			buffer.writef32(output, 12 + axis * 4, f32(center + half))
		end
		return buffer.tostring(output)
	elseif dataType == "ColorSequence" then
		local source = buffer.fromstring(raw)
		assert(buffer.len(source) >= 4, "invalid privileged ColorSequence byte length")
		local count = buffer.readi32(source, 0)
		assert(count >= 0, "invalid privileged ColorSequence keypoint count")
		if buffer.len(source) == 4 + count * 16 then
			return raw
		end
		assert(buffer.len(source) == 4 + count * 20, "invalid privileged ColorSequence keypoints")
		local output = buffer.create(4 + count * 16)
		buffer.writeu32(output, 0, count)
		for index = 0, count - 1 do
			local sourceOffset = 4 + index * 20
			local outputOffset = 4 + index * 16
			buffer.copy(output, outputOffset, source, sourceOffset, 16)
		end
		return buffer.tostring(output)
	end
	return raw
end

function Codec.databaseDefaultToCanonical(dataType: string, value: any): string?
	if dataType == "Int64" then
		local exact
		if MsgPack.Int64.is(value) then
			exact = value
		elseif typeof(value) == "number" and value % 1 == 0 and math.abs(value) <= 9007199254740991 then
			exact = MsgPack.Int64.fromDecimal(string.format("%.0f", value), true)
		else
			return nil
		end

		local raw = buffer.create(8)
		buffer.writeu32(raw, 0, exact.low)
		buffer.writeu32(raw, 4, exact.high)
		return buffer.tostring(raw)
	elseif dataType == "Float32" and typeof(value) == "number" then
		local raw = buffer.create(4)
		buffer.writef32(raw, 0, value)
		return buffer.tostring(raw)
	elseif dataType == "Float64" and typeof(value) == "number" then
		local raw = buffer.create(8)
		buffer.writef64(raw, 0, value)
		return buffer.tostring(raw)
	elseif dataType == "Color3" or dataType == "Vector3" then
		if type(value) ~= "table" or #value ~= 3 then
			return nil
		end
		local raw = buffer.create(12)
		for index = 1, 3 do
			if typeof(value[index]) ~= "number" then
				return nil
			end
			buffer.writef32(raw, (index - 1) * 4, value[index])
		end
		return buffer.tostring(raw)
	elseif dataType == "NumberRange" then
		if type(value) ~= "table" or #value ~= 2 then
			return nil
		end
		local raw = buffer.create(8)
		for index = 1, 2 do
			if typeof(value[index]) ~= "number" then
				return nil
			end
			buffer.writef32(raw, (index - 1) * 4, value[index])
		end
		return buffer.tostring(raw)
	elseif dataType == "PhysicalProperties" and value == "Default" then
		return string.char(0)
	elseif dataType == "CFrame" then
		if
			type(value) ~= "table"
			or type(value.orientation) ~= "table"
			or type(value.position) ~= "table"
			or #value.orientation ~= 3
			or #value.position ~= 3
		then
			return nil
		end
		local raw = buffer.create(48)
		local offset = 0
		for row = 1, 3 do
			if type(value.orientation[row]) ~= "table" or #value.orientation[row] ~= 3 then
				return nil
			end
			for column = 1, 3 do
				local component = value.orientation[row][column]
				if typeof(component) ~= "number" then
					return nil
				end
				buffer.writef32(raw, offset, component)
				offset += 4
			end
		end
		for index = 1, 3 do
			if typeof(value.position[index]) ~= "number" then
				return nil
			end
			buffer.writef32(raw, offset, value.position[index])
			offset += 4
		end
		return buffer.tostring(raw)
	end

	return nil
end

function Codec.decode(dataType: string, raw: string): any
	if dataType == "Bool" then
		return raw == "true" or raw == "1"
	elseif dataType == "Int64" then
		if #raw == 8 then
			local value = buffer.fromstring(raw)
			local exact = MsgPack.Int64.new(buffer.readu32(value, 4), buffer.readu32(value, 0), true)
			local decimal = MsgPack.Int64.toDecimal(exact)
			local number = tonumber(decimal)
			return if number ~= nil and math.abs(number) <= 9007199254740991 then number else exact
		end
		return MsgPack.Int64.fromDecimal(raw, true)
	elseif dataType == "SecurityCapabilities" then
		return MsgPack.Int64.fromDecimal(raw, false)
	elseif dataType == "Float32" and #raw == 4 then
		return buffer.readf32(buffer.fromstring(raw), 0)
	elseif dataType == "Float64" and #raw == 8 then
		return buffer.readf64(buffer.fromstring(raw), 0)
	elseif dataType == "Int32" or dataType == "Float32" or dataType == "Float64" or dataType == "Enum" then
		local value = tonumber(raw)
		assert(value ~= nil, `RML returned a non-numeric {dataType} value`)
		return value
	elseif dataType == "Vector2" then
		local value = bytes(raw, dataType)
		return Vector2.new(buffer.readf32(value, 0), buffer.readf32(value, 4))
	elseif dataType == "Vector3" then
		local value = bytes(raw, dataType)
		return Vector3.new(buffer.readf32(value, 0), buffer.readf32(value, 4), buffer.readf32(value, 8))
	elseif dataType == "Vector3int16" then
		local value = bytes(raw, dataType)
		return Vector3int16.new(buffer.readi16(value, 0), buffer.readi16(value, 2), buffer.readi16(value, 4))
	elseif dataType == "NumberRange" then
		local value = bytes(raw, dataType)
		return NumberRange.new(buffer.readf32(value, 0), buffer.readf32(value, 4))
	elseif dataType == "CFrame" then
		local value = bytes(raw, dataType)
		return CFrame.new(
			buffer.readf32(value, 36),
			buffer.readf32(value, 40),
			buffer.readf32(value, 44),
			buffer.readf32(value, 0),
			buffer.readf32(value, 4),
			buffer.readf32(value, 8),
			buffer.readf32(value, 12),
			buffer.readf32(value, 16),
			buffer.readf32(value, 20),
			buffer.readf32(value, 24),
			buffer.readf32(value, 28),
			buffer.readf32(value, 32)
		)
	end
	return raw
end

function Codec.encode(dataType: string, value: any): string?
	if dataType == "Bool" then
		return if value then "true" else "false"
	elseif dataType == "Content" then
		local success, sourceType, uri = pcall(function()
			return value.SourceType.Name, value.Uri
		end)
		if not success then
			return nil
		elseif sourceType == "None" then
			return string.char(0)
		elseif sourceType == "Uri" and type(uri) == "string" then
			return string.char(1) .. uri
		end
		-- Content.Object is identity-bearing state and must travel with its
		-- connected serialized target graph, never through this scalar codec.
		return nil
	elseif (dataType == "Int64" or dataType == "SecurityCapabilities") and MsgPack.Int64.is(value) then
		return MsgPack.Int64.toDecimal(value)
	elseif dataType == "Enum" and typeof(value) == "EnumItem" then
		return tostring(value.Value)
	elseif typeof(value) == "number" or typeof(value) == "string" then
		return tostring(value)
	elseif typeof(value) == "buffer" then
		return buffer.tostring(value)
	end

	local size = BLITTABLE_SIZES[dataType]
	if size == nil then
		return nil
	end
	local encoded = buffer.create(size)
	if dataType == "Vector2" and typeof(value) == "Vector2" then
		buffer.writef32(encoded, 0, value.X)
		buffer.writef32(encoded, 4, value.Y)
	elseif dataType == "Vector3" and typeof(value) == "Vector3" then
		buffer.writef32(encoded, 0, value.X)
		buffer.writef32(encoded, 4, value.Y)
		buffer.writef32(encoded, 8, value.Z)
	elseif dataType == "Vector3int16" and typeof(value) == "Vector3int16" then
		buffer.writei16(encoded, 0, value.X)
		buffer.writei16(encoded, 2, value.Y)
		buffer.writei16(encoded, 4, value.Z)
	elseif dataType == "NumberRange" and typeof(value) == "NumberRange" then
		buffer.writef32(encoded, 0, value.Min)
		buffer.writef32(encoded, 4, value.Max)
	elseif dataType == "CFrame" and typeof(value) == "CFrame" then
		local x, y, z, r00, r01, r02, r10, r11, r12, r20, r21, r22 = value:GetComponents()
		for index, component in { r00, r01, r02, r10, r11, r12, r20, r21, r22, x, y, z } do
			buffer.writef32(encoded, (index - 1) * 4, component)
		end
	else
		return nil
	end
	return buffer.tostring(encoded)
end

Codec.blittableSizes = BLITTABLE_SIZES
Codec.rawTypes = RAW_TYPES

return Codec
