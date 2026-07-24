--!strict

local ExactSerializedAccess = require("../src/Lib/Dom/ExactSerializedAccess")

local calls = {}
local failingAccess = {
	supportsType = function(dataType)
		table.insert(calls, `supports:{dataType}`)
		return true
	end,
	readRaw = function(_, propertyName, dataType)
		table.insert(calls, `read:{propertyName}:{dataType}`)
		return false, "injected raw attribute read failure"
	end,
	writeRaw = function(_, propertyName, dataType, _value)
		table.insert(calls, `write:{propertyName}:{dataType}`)
		return false, "injected raw attribute write failure"
	end,
}

local readSuccess, readError = ExactSerializedAccess.read(failingAccess, {}, "AttributesSerialize", "BinaryString")
assert(readSuccess == false, "an advertised exact read failure must not request script-level fallback")
assert(readError == "injected raw attribute read failure", "the exact read failure must reach reconnect logic")

local writeSuccess, writeError =
	ExactSerializedAccess.write(failingAccess, {}, "AttributesSerialize", "BinaryString", "raw attributes")
assert(not writeSuccess, "an advertised exact write failure must not be acknowledged")
assert(writeError == "injected raw attribute write failure", "the exact write failure must reach reconnect logic")
assert(#calls == 4, "fault injection must exercise support and transport for both operations")

local unsupported = {
	supportsType = function()
		return false
	end,
}
local attempted = ExactSerializedAccess.read(unsupported, {}, "AttributesSerialize", "BinaryString")
assert(attempted == nil, "a genuinely unavailable adapter may use the legacy readable Attributes API")

print("PASS: advertised exact serialized access fails closed under injected transport faults")
