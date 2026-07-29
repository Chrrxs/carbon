--!strict

local fs = require("@lune/fs")

local StyleRuleProperties = require("../src/Lib/Dom/StyleRuleProperties")

local LiveReflection = require("../src/Lib/Dom/LiveReflection")
local database = {}
LiveReflection.apply(database, {
	Classes = {
		{
			Name = "StyleRule",
			Superclass = "Instance",
			Tags = {},
			Members = {
				{
					MemberType = "Property",
					Name = "Properties",
					Security = { Read = "None", Write = "None" },
					Serialization = { CanLoad = false, CanSave = false },
					Tags = {},
					ValueType = { Category = "Primitive", Name = "string" },
				},
				{
					MemberType = "Property",
					Name = "PropertiesSerialize",
					Security = { Read = "RobloxSecurity", Write = "RobloxSecurity" },
					Serialization = { CanLoad = true, CanSave = true },
					Tags = { "Hidden", "NotScriptable" },
					ValueType = { Category = "DataType", Name = "BinaryString" },
				},
			},
		},
	},
	Enums = {},
}, { 0, 0, 0, 0 })
local descriptor = database.Classes.StyleRule.Properties.Properties
assert(descriptor.Scriptability == "Custom", "StyleRule.Properties must use its custom interface")
assert(
	descriptor.Kind.Canonical.Serialization.SerializesAs == "PropertiesSerialize",
	"the serialized alias must remain reflection-owned"
)

local values = {
	Color = "red",
	Transparency = 0.25,
}
local fake = {}
function fake:GetProperties()
	return table.clone(values)
end
function fake:SetProperty(name, value)
	values[name] = value
end

local readSuccess, captured = StyleRuleProperties.read(fake :: any)
assert(readSuccess and captured.Color == "red" and captured.Transparency == 0.25, "nonempty capture was lost")
local writeSuccess = StyleRuleProperties.write(fake :: any, "Properties", { Color = "blue" })
assert(writeSuccess and values.Color == "blue" and values.Transparency == nil, "sparse write did not reset stale items")
assert(StyleRuleProperties.write(fake :: any, "Properties", {}), "empty reset failed")
assert(next(values) == nil, "empty reset must remove every authored style property")

local source = fs.readFile("src/Lib/Dom/customProperties.luau")
assert(string.find(source, "Properties = StyleRuleProperties", 1, true), "canonical custom interface is missing")
assert(not string.find(source, "PropertiesSerialize =", 1, true), "serialized alias must not replace the canonical key")

print("PASS: StyleRule canonical custom property captures nonempty state and resets removed items")
