--!strict

local roblox = require("@lune/roblox")

Vector2 = roblox.Vector2
Vector3 = roblox.Vector3
Vector3int16 = roblox.Vector3int16
NumberRange = roblox.NumberRange
CFrame = roblox.CFrame

local Codec = require("../src/Lib/Dom/PrivilegedValueCodec")

local function assertBytes(dataType: string, expected: any, components: { number })
	local encoded = Codec.encode(dataType, expected)
	assert(encoded ~= nil, `{dataType} must encode`)
	local decoded = Codec.decode(dataType, encoded)
	local actual
	if dataType == "CFrame" then
		actual = { decoded:GetComponents() }
	elseif dataType == "NumberRange" then
		actual = { decoded.Min, decoded.Max }
	elseif dataType == "Vector2" then
		actual = { decoded.X, decoded.Y }
	else
		actual = { decoded.X, decoded.Y, decoded.Z }
	end
	for index, value in components do
		assert(actual[index] == value, `{dataType} component {index}: expected {value}, got {actual[index]}`)
	end
end

assertBytes("Vector2", Vector2.new(1.5, -2.25), { 1.5, -2.25 })
assertBytes("Vector3", Vector3.new(1.5, -2.25, 4.75), { 1.5, -2.25, 4.75 })
assertBytes("Vector3int16", Vector3int16.new(-32768, 0, 32767), { -32768, 0, 32767 })
assertBytes("NumberRange", NumberRange.new(-2.25, 4.75), { -2.25, 4.75 })

local cframeComponents = { 11.5, -2.25, 7.75, 1, 0, 0, 0, 0, -1, 0, 1, 0 }
assertBytes("CFrame", CFrame.new(table.unpack(cframeComponents)), cframeComponents)

local invalid = pcall(Codec.decode, "CFrame", string.rep("\0", 47))
assert(not invalid, "truncated privileged CFrame bytes must be rejected")

local negativeOne = buffer.fromstring(assert(Codec.databaseDefaultToCanonical("Int64", -1)))
assert(
	buffer.len(negativeOne) == 8
		and buffer.readu32(negativeOne, 0) == 0xFFFFFFFF
		and buffer.readu32(negativeOne, 4) == 0xFFFFFFFF,
	"a native service's -1 SourceAssetId default must be available without constructing the service"
)
local positive = buffer.fromstring(assert(Codec.databaseDefaultToCanonical("Int64", 4294967297)))
assert(
	buffer.readu32(positive, 0) == 1 and buffer.readu32(positive, 4) == 1,
	"reflection-database Int64 defaults must retain both exact words"
)
assert(
	buffer.readf32(buffer.fromstring(assert(Codec.databaseDefaultToCanonical("Float32", 1.9812492))), 0)
		== buffer.readf32(buffer.fromstring(string.pack("f", 1.9812492)), 0),
	"Float32 database defaults must use the exact engine-width representation"
)
local defaultCFrame = buffer.fromstring(assert(Codec.databaseDefaultToCanonical("CFrame", {
	orientation = { { 1, 0, 0 }, { 0, 1, 0 }, { 0, 0, 1 } },
	position = { 11.5, -2.25, 7.75 },
})))
assert(
	buffer.len(defaultCFrame) == 48
		and buffer.readf32(defaultCFrame, 0) == 1
		and buffer.readf32(defaultCFrame, 36) == 11.5
		and buffer.readf32(defaultCFrame, 40) == -2.25
		and buffer.readf32(defaultCFrame, 44) == 7.75,
	"CFrame reflection defaults must retain engine component order"
)
assert(
	Codec.databaseDefaultToCanonical("ColorSequence", {}) == nil,
	"types without a proven database conversion must continue using live engine defaults"
)
assert(
	Codec.databaseDefaultToCanonical("PhysicalProperties", "Default") == string.char(0),
	"default physical properties must use the canonical disengaged wire tag"
)

assert(
	Codec.encode("Content", { SourceType = { Name = "None" }, Uri = "ignored" }) == string.char(0),
	"Content.None must have an unambiguous scalar wire tag"
)
assert(
	Codec.encode("Content", { SourceType = { Name = "Uri" }, Uri = "" }) == string.char(1),
	"an empty Content URI must remain distinct from Content.None"
)
assert(
	Codec.encode("Content", { SourceType = { Name = "Uri" }, Uri = "rbxassetid://123" })
		== string.char(1) .. "rbxassetid://123",
	"Content URI bytes must survive scalar materialization"
)
assert(
	Codec.encode("Content", { SourceType = { Name = "Object" }, Object = {} }) == nil,
	"Content.Object must fail closed instead of entering the scalar materializer"
)

local region = buffer.create(60)
for index, component in { 1, 0, 0, 0, 1, 0, 0, 0, 1 } do
	buffer.writef32(region, (index - 1) * 4, component)
end
for index, component in { 5, 7, 9 } do
	buffer.writef32(region, 36 + (index - 1) * 4, component)
end
for index, component in { 2, 4, 6 } do
	buffer.writef32(region, 48 + (index - 1) * 4, component)
end
local canonicalRegion = buffer.fromstring(Codec.engineToCanonical("Region3", buffer.tostring(region)))
for index, component in { 4, 5, 6, 6, 9, 12 } do
	assert(buffer.readf32(canonicalRegion, (index - 1) * 4) == component, `Region3 canonical component {index}`)
end
buffer.writef32(region, 0, 0)
assert(
	#Codec.engineToCanonical("Region3", buffer.tostring(region)) == 24,
	"Region3 conversion must ignore engine-only orientation state"
)

local colorSequence = buffer.create(44)
buffer.writei32(colorSequence, 0, 2)
for keypoint = 0, 1 do
	for component = 0, 3 do
		buffer.writef32(colorSequence, 4 + keypoint * 20 + component * 4, keypoint * 10 + component + 0.5)
	end
	buffer.writef32(colorSequence, 4 + keypoint * 20 + 16, 0)
end
local canonicalColorSequence =
	buffer.fromstring(Codec.engineToCanonical("ColorSequence", buffer.tostring(colorSequence)))
assert(
	buffer.len(canonicalColorSequence) == 36 and buffer.readu32(canonicalColorSequence, 0) == 2,
	"ColorSequence canonical size"
)
for keypoint = 0, 1 do
	for component = 0, 3 do
		assert(
			buffer.readf32(canonicalColorSequence, 4 + keypoint * 16 + component * 4) == keypoint * 10 + component + 0.5,
			`ColorSequence canonical keypoint {keypoint} component {component}`
		)
	end
end
buffer.writef32(colorSequence, 20, 1)
assert(
	#Codec.engineToCanonical("ColorSequence", buffer.tostring(colorSequence)) == 36,
	"ColorSequence conversion must ignore its engine-only reserved field"
)

print("PASS: privileged blittable value codec is bit-exact")
