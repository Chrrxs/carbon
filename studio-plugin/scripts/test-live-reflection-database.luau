--!strict

local LiveReflection = require("../src/Lib/Dom/LiveReflection")

local database = {
	Version = { 0, 1, 0, 1 },
	Classes = {
		FutureInput = {
			Name = "FutureInput",
			Superclass = "Instance",
			Tags = {},
			DefaultProperties = {},
			Properties = {
				HistoricalProperty = {
					Name = "HistoricalProperty",
					DataType = { Value = "String" },
					Scriptability = "ReadWrite",
					Tags = {},
					Kind = { Canonical = { Serialization = "Serializes" } },
				},
				LegacyAlias = {
					Name = "LegacyAlias",
					DataType = { Value = "Ref" },
					Scriptability = "Custom",
					Tags = { "Deprecated" },
					Kind = { Alias = { AliasFor = "PersistentBinding" } },
				},
			},
		},
	},
	Enums = {
		ObsoleteEnum = { name = "ObsoleteEnum", items = { Old = 0 } },
	},
}

local function property(name: string, valueType, canLoad: boolean, canSave: boolean)
	return {
		MemberType = "Property",
		Name = name,
		Security = { Read = "None", Write = "None" },
		Serialization = { CanLoad = canLoad, CanSave = canSave },
		Tags = {},
		ValueType = valueType,
	}
end

local preferredBinding = property("PreferredBinding", { Category = "Class", Name = "InputBinding" }, false, true)
preferredBinding.Tags = {
	"ReadOnly",
	{ PreferredDescriptorName = "LegacyPreferredBinding", ThreadSafety = "Unknown" },
}

local api = {
	Classes = {
		{
			Name = "FutureInput",
			Superclass = "Instance",
			Tags = { "NotReplicated" },
			Members = {
				preferredBinding,
				property("PersistentBinding", { Category = "Class", Name = "InputBinding" }, true, true),
				property("LegacyAlias", { Category = "Class", Name = "InputBinding" }, true, true),
				property("Icon", { Category = "DataType", Name = "ContentId" }, true, true),
				property("HistoricalProperty", { Category = "DataType", Name = "FutureType" }, true, true),
				property("NewUnsupported", { Category = "DataType", Name = "FutureType" }, true, true),
				property("NewPrimitiveUnsupported", { Category = "Primitive", Name = "uint128" }, true, true),
			},
		},
	},
	Enums = {
		{
			Name = "FutureMode",
			Items = {
				{ Name = "Enabled", Value = 1 },
			},
		},
	},
}

local result = LiveReflection.apply(database, api, { 0, 999, 0, 9990001 })
local class = database.Classes.FutureInput
local preferred = class.Properties.PreferredBinding
assert(preferred.DataType.Value == "Ref", "future class references must be classified without hand editing")
assert(
	preferred.Kind.Canonical.Serialization == "DoesNotSerialize",
	"save-only references must not enter Carbon's round-trip artifact"
)
assert(
	class.Properties.PersistentBinding.Kind.Canonical.Serialization == "Serializes",
	"loadable and saveable references must round-trip"
)
assert(class.Properties.LegacyAlias.Kind.Alias.AliasFor == "PersistentBinding", "patched aliases must survive refresh")
assert(class.Properties.LegacyAlias.Scriptability == "Custom", "custom property interfaces must survive refresh")
assert(class.Properties.Icon.DataType.Value == "ContentId", "ContentId must retain legacy wire semantics")
assert(class.Properties.HistoricalProperty ~= nil, "historical descriptors must remain readable for old artifacts")
assert(database.Enums.ObsoleteEnum == nil and database.Enums.FutureMode.items.Enabled == 1, "enums must synchronize")
assert(
	result.skippedProperties["FutureInput.NewUnsupported"] == "DataType.FutureType"
		and result.skippedProperties["FutureInput.NewPrimitiveUnsupported"] == "Primitive.uint128"
		and result.skippedProperties["FutureInput.HistoricalProperty"] == nil,
	"unsupported new properties must be diagnosed while bundled adapters remain available"
)
assert(database.Version[2] == 999 and result.addedProperties == 3, "refresh result must describe generated additions")

print("Live reflection database overlay checks passed")
