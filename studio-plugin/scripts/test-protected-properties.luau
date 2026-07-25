--!strict

local ProtectedProperties = require("../src/Lib/Dom/ProtectedProperties")

local function property(dataType: string, scriptability: string)
	return {
		DataType = { Value = dataType },
		Scriptability = scriptability,
		Kind = { Canonical = { Serialization = "Serializes" } },
	}
end

local database = {
	Classes = {
		Instance = {
			Superclass = nil,
			Properties = {
				Capabilities = property("SecurityCapabilities", "ReadWrite"),
				Name = property("String", "ReadWrite"),
			},
		},
	},
}

local descriptors = ProtectedProperties.supportedForClass(database, "Instance", function(descriptor)
	return ProtectedProperties.requiresMaterialization(descriptor.dataType)
end)

assert(#descriptors == 1, "only capability-gated scriptable values should require materialization")
assert(descriptors[1].name == "Capabilities", "Capabilities must not use ordinary property writes")
assert(descriptors[1].dataType == "SecurityCapabilities", "Capabilities must retain its exact native type")

print("PASS: capability-gated serialized values use materialized access")
