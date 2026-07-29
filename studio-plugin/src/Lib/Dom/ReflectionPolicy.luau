--!strict

local ReflectionPolicy = {}

local CustomDataTypes = {
	Instance = {
		Attributes = { Value = "Attributes" },
		Tags = { Value = "Tags" },
	},
	LocalizationTable = {
		Contents = { Value = "String" },
	},
	LocalScript = {
		Source = { Value = "String" },
	},
	MeshPart = {
		InitialSize = { Value = "Vector3" },
		MeshContent = { Value = "Content" },
	},
	Model = {
		Scale = { Value = "Float32" },
		WorldPivotData = { Value = "OptionalCFrame" },
	},
	ModuleScript = {
		Source = { Value = "String" },
	},
	Script = {
		Source = { Value = "String" },
	},
	StyleRule = {
		Properties = { Value = "BinaryString" },
	},
	Terrain = {
		MaterialColors = { Value = "MaterialColors" },
	},
}
local CustomPropertyNames = {
	Instance = { "Attributes", "Tags" },
	LocalizationTable = { "Contents" },
	LocalScript = { "Source" },
	MeshPart = { "InitialSize", "MeshContent" },
	Model = { "Scale", "WorldPivotData" },
	ModuleScript = { "Source" },
	Script = { "Source" },
	StyleRule = { "Properties" },
	Terrain = { "MaterialColors" },
}

local SerializesAs = {
	BasePart = {
		Color = "Color3uint8",
		MaterialVariant = "MaterialVariantSerialized",
		Size = "size",
	},
	Fire = {
		Heat = "heat_xml",
		Size = "size_xml",
	},
	FormFactorPart = {
		FormFactor = "formFactorRaw",
	},
	Instance = {
		Archivable = "archivable",
		Attributes = "AttributesSerialize",
		Sandboxed = "DefinesCapabilities",
	},
	MaterialService = {
		Use2022Materials = "Use2022MaterialsXml",
	},
	Model = {
		Scale = "ScaleFactor",
	},
	PackageLink = {
		PackageContent = "PackageContentSerialize",
	},
	Part = {
		Shape = "shape",
	},
	Players = {
		MaxPlayers = "MaxPlayersInternal",
		PreferredPlayers = "PreferredPlayersInternal",
	},
	Smoke = {
		Opacity = "opacity_xml",
		RiseVelocity = "riseVelocity_xml",
		Size = "size_xml",
	},
	Sound = {
		MaxDistance = "xmlRead_MaxDistance_3",
		RollOffMaxDistance = "xmlRead_MaxDistance_3",
		RollOffMinDistance = "EmitterSize",
	},
	StarterPlayer = {
		AvatarJointUpgrade = "AvatarJointUpgrade_SerializedRollout",
	},
	StyleRule = {
		Properties = "PropertiesSerialize",
	},
	WeldConstraint = {
		Part0 = "Part0Internal",
		Part1 = "Part1Internal",
	},
	Workspace = {
		SignalBehavior = "SignalBehavior2",
	},
}

local Aliases = {
	BasePart = {
		Color3uint8 = {
			AliasFor = "Color",
			DataType = { Value = "Color3uint8" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
		MaterialVariantSerialized = {
			AliasFor = "MaterialVariant",
			DataType = { Value = "String" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
		size = {
			AliasFor = "Size",
			DataType = { Value = "Vector3" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
	},
	BodyAngularVelocity = {
		angularvelocity = {
			AliasFor = "AngularVelocity",
			DataType = { Value = "Vector3" },
			Scriptability = "ReadWrite",
			Tags = { "NotReplicated", "Deprecated" },
		},
		maxTorque = {
			AliasFor = "MaxTorque",
			DataType = { Value = "Vector3" },
			Scriptability = "ReadWrite",
			Tags = { "NotReplicated", "Deprecated" },
		},
	},
	BodyForce = {
		force = {
			AliasFor = "Force",
			DataType = { Value = "Vector3" },
			Scriptability = "ReadWrite",
			Tags = { "NotReplicated", "Deprecated" },
		},
	},
	BodyGyro = {
		cframe = {
			AliasFor = "CFrame",
			DataType = { Value = "CFrame" },
			Scriptability = "ReadWrite",
			Tags = { "NotReplicated", "Deprecated" },
		},
		maxTorque = {
			AliasFor = "MaxTorque",
			DataType = { Value = "Vector3" },
			Scriptability = "ReadWrite",
			Tags = { "NotReplicated", "Deprecated" },
		},
	},
	BodyPosition = {
		maxForce = {
			AliasFor = "MaxForce",
			DataType = { Value = "Vector3" },
			Scriptability = "ReadWrite",
			Tags = { "NotReplicated", "Deprecated" },
		},
		position = {
			AliasFor = "Position",
			DataType = { Value = "Vector3" },
			Scriptability = "ReadWrite",
			Tags = { "NotReplicated", "Deprecated" },
		},
	},
	BodyThrust = {
		force = {
			AliasFor = "Force",
			DataType = { Value = "Vector3" },
			Scriptability = "ReadWrite",
			Tags = { "NotReplicated", "Deprecated" },
		},
		location = {
			AliasFor = "Location",
			DataType = { Value = "Vector3" },
			Scriptability = "ReadWrite",
			Tags = { "NotReplicated", "Deprecated" },
		},
	},
	BodyVelocity = {
		maxForce = {
			AliasFor = "MaxForce",
			DataType = { Value = "Vector3" },
			Scriptability = "ReadWrite",
			Tags = { "NotReplicated", "Deprecated" },
		},
		velocity = {
			AliasFor = "Velocity",
			DataType = { Value = "Vector3" },
			Scriptability = "ReadWrite",
			Tags = { "NotReplicated", "Deprecated" },
		},
	},
	Camera = {
		CoordinateFrame = {
			AliasFor = "CFrame",
			DataType = { Value = "CFrame" },
			Scriptability = "ReadWrite",
			Tags = { "Hidden", "NotReplicated", "Deprecated" },
		},
		focus = {
			AliasFor = "Focus",
			DataType = { Value = "CFrame" },
			Scriptability = "ReadWrite",
			Tags = { "NotReplicated", "Deprecated" },
		},
	},
	Fire = {
		heat_xml = {
			AliasFor = "Heat",
			DataType = { Value = "Float32" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
		size = {
			AliasFor = "Size",
			DataType = { Value = "Float32" },
			Scriptability = "ReadWrite",
			Tags = { "NotReplicated", "Deprecated" },
		},
		size_xml = {
			AliasFor = "Size",
			DataType = { Value = "Float32" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
	},
	FormFactorPart = {
		formFactor = {
			AliasFor = "FormFactor",
			DataType = { Enum = "FormFactor" },
			Scriptability = "ReadWrite",
			Tags = { "Hidden", "NotReplicated", "Deprecated" },
		},
		Formfactor = {
			AliasFor = "FormFactor",
			DataType = { Enum = "FormFactor" },
			Scriptability = "ReadWrite",
			Tags = { "Hidden", "NotReplicated", "Deprecated" },
		},
		formFactorRaw = {
			AliasFor = "FormFactor",
			DataType = { Enum = "FormFactor" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
	},
	Instance = {
		AttributesSerialize = {
			AliasFor = "Attributes",
			DataType = { Value = "BinaryString" },
			Scriptability = "None",
			Tags = { "Hidden", "NotReplicated", "NotScriptable" },
		},
		DefinesCapabilities = {
			AliasFor = "Sandboxed",
			DataType = { Value = "Bool" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
		archivable = {
			AliasFor = "Archivable",
			DataType = { Value = "Bool" },
			Scriptability = "ReadWrite",
			Tags = { "Hidden", "NotReplicated", "Deprecated" },
		},
	},
	JointInstance = {
		part1 = {
			AliasFor = "Part1",
			DataType = { Value = "Ref" },
			Scriptability = "ReadWrite",
			Tags = { "Hidden", "NotReplicated", "Deprecated" },
		},
	},
	Model = {
		ScaleFactor = {
			AliasFor = "Scale",
			DataType = { Value = "Float32" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
	},
	Object = {
		className = {
			AliasFor = "ClassName",
			DataType = { Value = "String" },
			Scriptability = "Read",
			Tags = { "ReadOnly", "NotReplicated", "Deprecated" },
		},
	},
	PackageLink = {
		PackageContentSerialize = {
			AliasFor = "PackageContent",
			DataType = { Value = "Content" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
		PackageIdSerialize = {
			AliasFor = "PackageId",
			DataType = { Value = "ContentId" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
	},
	Part = {
		shape = {
			AliasFor = "Shape",
			DataType = { Enum = "PartType" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
	},
	Players = {
		MaxPlayersInternal = {
			AliasFor = "MaxPlayers",
			DataType = { Value = "Int32" },
			Scriptability = "None",
			Tags = { "Hidden" },
		},
		PreferredPlayersInternal = {
			AliasFor = "PreferredPlayers",
			DataType = { Value = "Int32" },
			Scriptability = "None",
			Tags = { "Hidden" },
		},
	},
	Smoke = {
		opacity_xml = {
			AliasFor = "Opacity",
			DataType = { Value = "Float32" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
		riseVelocity_xml = {
			AliasFor = "RiseVelocity",
			DataType = { Value = "Float32" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
		size_xml = {
			AliasFor = "Size",
			DataType = { Value = "Float32" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
	},
	Sound = {
		EmitterSize = {
			AliasFor = "RollOffMinDistance",
			DataType = { Value = "Float32" },
			Scriptability = "ReadWrite",
			Tags = { "Deprecated" },
		},
		xmlRead_MaxDistance_3 = {
			AliasFor = "RollOffMaxDistance",
			DataType = { Value = "Float32" },
			Scriptability = "None",
			Tags = { "Hidden", "NotReplicated", "NotScriptable", "Deprecated" },
		},
	},
	StarterPlayer = {
		AvatarJointUpgrade_SerializedRollout = {
			AliasFor = "AvatarJointUpgrade",
			DataType = { Enum = "RolloutState" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
	},
	StyleRule = {
		PropertiesSerialize = {
			AliasFor = "Properties",
			DataType = { Value = "BinaryString" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
	},
	WeldConstraint = {
		Part0Internal = {
			AliasFor = "Part0",
			DataType = { Value = "Ref" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
		Part1Internal = {
			AliasFor = "Part1",
			DataType = { Value = "Ref" },
			Scriptability = "None",
			Tags = { "Hidden", "NotScriptable" },
		},
	},
	Workspace = {
		SignalBehavior2 = {
			AliasFor = "SignalBehavior",
			DataType = { Enum = "SignalBehavior" },
			Scriptability = "None",
			Tags = { "Hidden", "NotReplicated", "NotScriptable" },
		},
	},
}

function ReflectionPolicy.apply(database: { [string]: any })
	local classes = database.Classes
	if type(classes) ~= "table" then
		return
	end

	for className, propertyTypes in CustomDataTypes do
		local class = classes[className]
		if class ~= nil then
			for propertyName, dataType in propertyTypes do
				local property = class.Properties[propertyName]
				if property ~= nil then
					property.DataType = dataType
				end
			end
		end
	end

	-- 1. Mark properties backed by Carbon custom interfaces.
	for className, propertyNames in CustomPropertyNames do
		local class = classes[className]
		if class ~= nil then
			for _, propertyName in propertyNames do
				local property = class.Properties[propertyName]
				if property == nil then
					property = {
						Name = propertyName,
						DataType = CustomDataTypes[className][propertyName],
						Scriptability = "Custom",
						Tags = {},
						Kind = { Canonical = { Serialization = "Serializes" } },
					}
					class.Properties[propertyName] = property
				else
					property.Scriptability = "Custom"
				end
			end
		end
	end

	-- 2. Apply SerializesAs overrides
	for className, propMap in SerializesAs do
		local class = classes[className]
		if class ~= nil then
			for propertyName, serializesAsName in propMap do
				local prop = class.Properties[propertyName]
				if prop ~= nil and prop.Kind ~= nil and prop.Kind.Canonical ~= nil then
					prop.Kind.Canonical.Serialization = { SerializesAs = serializesAsName }
				end
			end
		end
	end

	-- 3. Apply Aliases
	for className, aliasMap in Aliases do
		local class = classes[className]
		if class ~= nil then
			for aliasName, aliasInfo in aliasMap do
				local existing = class.Properties[aliasName]
				class.Properties[aliasName] = {
					Name = aliasName,
					DataType = aliasInfo.DataType,
					Scriptability = if existing ~= nil and existing.Scriptability == "Custom"
						then "Custom"
						else aliasInfo.Scriptability,
					Tags = aliasInfo.Tags or {},
					Kind = { Alias = { AliasFor = aliasInfo.AliasFor } },
				}
			end
		end
	end
end

return ReflectionPolicy
