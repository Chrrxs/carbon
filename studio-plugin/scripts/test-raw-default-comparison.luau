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
local DefaultComparison = require("../src/Core/Processor/DefaultComparison")
local PropertyValuePolicy = require("../src/Core/Processor/PropertyValuePolicy")
local function equals(left, right)
	return left == right
end

-- An exact NaN payload is intentionally invalid UTF-8. Raw values must still
-- survive both addition and update validation without string normalization.
local defaultRaw = string.char(0x42, 0x00, 0xc0, 0x7f)
local exactDefault = { BinaryString = buffer.fromstring(defaultRaw) }
assert(
	PropertyValuePolicy.validate(defaultRaw, "Float32", function(dataType)
		return dataType == "Float32"
	end),
	"invalid-UTF8 exact raw must be binary-safe for additions and updates"
)
assert(
	DefaultComparison.matches(defaultRaw, exactDefault, EncodedValue.decode, equals, nil),
	"an omitted exact Float32 at its engine default must produce no diff"
)

local changed = string.char(0x43, 0x00, 0xc0, 0x7f)
assert(
	not DefaultComparison.matches(changed, exactDefault, EncodedValue.decode, equals, nil),
	"a one-bit exact Float32 change must remain an update"
)

for _, width in { 12, 48 } do
	local raw = string.rep("\0", width)
	local encoded = { BinaryString = buffer.fromstring(raw) }
	assert(
		DefaultComparison.matches(raw, encoded, EncodedValue.decode, equals, nil),
		`omitted exact raw default of width {width} must remain absent`
	)
end

print("PASS: raw defaults compare canonically and invalid UTF-8 bits remain exact")
