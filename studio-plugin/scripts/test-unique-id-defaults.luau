--!strict

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

local EncodedValue = require("../src/Lib/Dom/EncodedValue")

-- Roblox's Luau-facing serialized UniqueId is index | time | random, while
-- Carbon's canonical rbx_types value is random | time | index. Use distinct
-- nonzero components so an identity codec cannot accidentally pass this test.
local engine = "\0\1\2\3\4\5\6\7\8\9\10\11\12\13\14\255"
local canonical = "\8\9\10\11\12\13\14\255\4\5\6\7\0\1\2\3"
local encodedOk, encoded = EncodedValue.encode(engine, "UniqueId")
assert(encodedOk, "UniqueId exact bytes must encode")
assert(typeof(encoded.UniqueId) == "buffer", "UniqueId wire value must be a buffer")
assert(buffer.tostring(encoded.UniqueId) == canonical, "UniqueId encoding must use canonical component order")

local decodedOk, decoded = EncodedValue.decode(encoded)
assert(decodedOk and decoded == engine, "UniqueId decoding must restore Roblox's engine component order")

local hex = "08090a0b0c0d0eff0405060700010203"
assert(
	buffer.tostring(EncodedValue.uniqueIdFromHex(hex)) == canonical,
	"reflection UniqueId hex defaults must preserve canonical wire bytes"
)

print("PASS: UniqueId defaults and Roblox/canonical component order are exact")
