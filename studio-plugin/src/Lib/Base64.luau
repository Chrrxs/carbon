--!strict

local ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
local DECODE = {}
for index = 1, #ALPHABET do
	DECODE[string.byte(ALPHABET, index)] = index - 1
end

local Base64 = {}

function Base64.encode(value: string): string
	local output = table.create(math.ceil(#value / 3) * 4)
	local outputIndex = 1
	for index = 1, #value, 3 do
		local a = string.byte(value, index)
		local b = string.byte(value, index + 1)
		local c = string.byte(value, index + 2)
		local combined = bit32.lshift(a, 16) + bit32.lshift(b or 0, 8) + (c or 0)
		output[outputIndex] =
			string.sub(ALPHABET, bit32.extract(combined, 18, 6) + 1, bit32.extract(combined, 18, 6) + 1)
		output[outputIndex + 1] =
			string.sub(ALPHABET, bit32.extract(combined, 12, 6) + 1, bit32.extract(combined, 12, 6) + 1)
		output[outputIndex + 2] = if b
			then string.sub(ALPHABET, bit32.extract(combined, 6, 6) + 1, bit32.extract(combined, 6, 6) + 1)
			else "="
		output[outputIndex + 3] = if c
			then string.sub(ALPHABET, bit32.extract(combined, 0, 6) + 1, bit32.extract(combined, 0, 6) + 1)
			else "="
		outputIndex += 4
	end
	return table.concat(output)
end

function Base64.decode(value: string): string
	assert(#value % 4 == 0, "invalid base64 length")
	local output = buffer.create(math.floor(#value / 4) * 3)
	local outputIndex = 0
	for index = 1, #value, 4 do
		local a = DECODE[string.byte(value, index)]
		local b = DECODE[string.byte(value, index + 1)]
		local cByte = string.byte(value, index + 2)
		local dByte = string.byte(value, index + 3)
		local c = cByte == 61 and 0 or DECODE[cByte]
		local d = dByte == 61 and 0 or DECODE[dByte]
		assert(a ~= nil and b ~= nil and c ~= nil and d ~= nil, "invalid base64 character")
		local combined = bit32.lshift(a, 18) + bit32.lshift(b, 12) + bit32.lshift(c, 6) + d
		buffer.writeu8(output, outputIndex, bit32.extract(combined, 16, 8))
		outputIndex += 1
		if cByte ~= 61 then
			buffer.writeu8(output, outputIndex, bit32.extract(combined, 8, 8))
			outputIndex += 1
		end
		if dByte ~= 61 then
			buffer.writeu8(output, outputIndex, bit32.extract(combined, 0, 8))
			outputIndex += 1
		end
	end
	return buffer.readstring(output, 0, outputIndex)
end

return Base64
