--!strict

local MsgPack = require("../src/Lib/MsgPack")
local fs = require("@lune/fs")
local roblox = require("@lune/roblox")

Axes = roblox.Axes
BrickColor = roblox.BrickColor
CFrame = roblox.CFrame
Color3 = roblox.Color3
ColorSequence = roblox.ColorSequence
ColorSequenceKeypoint = roblox.ColorSequenceKeypoint
Faces = roblox.Faces
Font = roblox.Font
NumberRange = roblox.NumberRange
NumberSequence = roblox.NumberSequence
NumberSequenceKeypoint = roblox.NumberSequenceKeypoint
PhysicalProperties = roblox.PhysicalProperties
Ray = roblox.Ray
Rect = roblox.Rect
Region3int16 = roblox.Region3int16
UDim = roblox.UDim
UDim2 = roblox.UDim2
Vector2 = roblox.Vector2
Vector2int16 = roblox.Vector2int16
Vector3 = roblox.Vector3
Vector3int16 = roblox.Vector3int16

local Base64 = require("../src/Lib/Base64")
local DatabaseDefaultValue = require("../src/Lib/Dom/DatabaseDefaultValue")
local function unusedUniqueId(_value: string): buffer
	error("fixture does not contain a UniqueId")
end

local binaryRaw = string.char(0, 1, 2, 0xff, 0x41)
local binaryEncoded = Base64.encode(binaryRaw)
local binary = DatabaseDefaultValue.decode({ BinaryString = binaryEncoded }, Base64.decode, unusedUniqueId)
assert(typeof(binary.BinaryString) == "buffer", "BinaryString database defaults must use the buffer wire form")
assert(buffer.tostring(binary.BinaryString) == binaryRaw, "BinaryString defaults must decode base64 to exact bytes")
assert(buffer.tostring(binary.BinaryString) ~= binaryEncoded, "BinaryString defaults must not retain base64 text")

local sharedRaw = string.char(0xff, 0, 0x53, 0x53)
local sharedEncoded = Base64.encode(sharedRaw)
local shared = DatabaseDefaultValue.decode({ SharedString = sharedEncoded }, Base64.decode, unusedUniqueId)
assert(type(shared.SharedString) == "string", "SharedString database defaults must use the raw string wire form")
assert(shared.SharedString == sharedRaw, "SharedString defaults must decode base64 to exact bytes")
assert(shared.SharedString ~= sharedEncoded, "SharedString defaults must not retain base64 text")

local database = MsgPack.decode(fs.readFile("src/Lib/Dom/database.msgpack"))
local observed = { BinaryString = false, SharedString = false }
for _, class in database.Classes do
	for _, default in class.DefaultProperties do
		for dataType in observed do
			local encoded = default[dataType]
			if type(encoded) == "string" and #encoded > 0 then
				local decoded = DatabaseDefaultValue.decode(default, Base64.decode, unusedUniqueId)[dataType]
				local raw = if dataType == "BinaryString" then buffer.tostring(decoded) else decoded
				assert(raw == Base64.decode(encoded), `{dataType} database default must decode its base64 payload`)
				assert(raw ~= encoded, `{dataType} database default must not retain its base64 text`)
				observed[dataType] = true
			end
		end
	end
end
assert(observed.BinaryString, "reflection database must exercise a nonempty BinaryString default")
assert(observed.SharedString, "reflection database must exercise a nonempty SharedString default")

print("PASS: database BinaryString and SharedString defaults decode from base64 to exact bytes")
