use anyhow::{ensure, Context, Result};
use directories::UserDirs;
use env_logger::WriteStyle;
use log::LevelFilter;
use rbx_dom_weak::types::VariantType;
use rbx_reflection::{
	ClassDescriptor, ClassTag, DataType, EnumDescriptor, PropertyDescriptor, PropertyKind, PropertyMigration,
	PropertySerialization, PropertyTag, ReflectionDatabase, Scriptability,
};
use serde::Deserialize;
use std::{
	env, fs,
	path::PathBuf,
	process::Command,
	sync::{Mutex, OnceLock},
};

use crate::rml;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ApiDump {
	classes: Vec<ApiClass>,
	enums: Vec<ApiEnum>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ApiClass {
	name: String,
	superclass: String,
	#[serde(default)]
	tags: Vec<serde_json::Value>,
	members: Vec<ApiMember>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ApiMember {
	member_type: String,
	name: String,
	value_type: Option<ApiValueType>,
	security: Option<serde_json::Value>,
	#[serde(default)]
	tags: Vec<serde_json::Value>,
	serialization: Option<ApiSerialization>,
	#[serde(default)]
	default: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ApiValueType {
	category: String,
	name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ApiSerialization {
	can_load: Option<bool>,
	can_save: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ApiEnum {
	name: String,
	items: Vec<ApiEnumItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ApiEnumItem {
	name: String,
	value: u32,
}

fn convert_type(value_type: &ApiValueType) -> Option<DataType<'static>> {
	match value_type.category.as_str() {
		"Enum" => Some(DataType::Enum(Box::leak(value_type.name.clone().into_boxed_str()))),
		"Class" => Some(DataType::Value(VariantType::Ref)),
		"Primitive" => match value_type.name.as_str() {
			"bool" => Some(DataType::Value(VariantType::Bool)),
			"double" => Some(DataType::Value(VariantType::Float64)),
			"float" => Some(DataType::Value(VariantType::Float32)),
			"int" => Some(DataType::Value(VariantType::Int32)),
			"int64" => Some(DataType::Value(VariantType::Int64)),
			"string" => Some(DataType::Value(VariantType::String)),
			_ => None,
		},
		"DataType" => match value_type.name.as_str() {
			"Axes" => Some(DataType::Value(VariantType::Axes)),
			"BinaryString" => Some(DataType::Value(VariantType::BinaryString)),
			"BrickColor" => Some(DataType::Value(VariantType::BrickColor)),
			"CFrame" => Some(DataType::Value(VariantType::CFrame)),
			"Color3" => Some(DataType::Value(VariantType::Color3)),
			"Color3uint8" => Some(DataType::Value(VariantType::Color3uint8)),
			"ColorSequence" => Some(DataType::Value(VariantType::ColorSequence)),
			"Content" => Some(DataType::Value(VariantType::Content)),
			"ContentId" => Some(DataType::Value(VariantType::ContentId)),
			"Faces" => Some(DataType::Value(VariantType::Faces)),
			"Font" => Some(DataType::Value(VariantType::Font)),
			"MaterialColors" => Some(DataType::Value(VariantType::MaterialColors)),
			"NetAssetRef" => Some(DataType::Value(VariantType::NetAssetRef)),
			"NumberRange" => Some(DataType::Value(VariantType::NumberRange)),
			"NumberSequence" => Some(DataType::Value(VariantType::NumberSequence)),
			"PhysicalProperties" => Some(DataType::Value(VariantType::PhysicalProperties)),
			"Ray" => Some(DataType::Value(VariantType::Ray)),
			"Rect" => Some(DataType::Value(VariantType::Rect)),
			"Region3" => Some(DataType::Value(VariantType::Region3)),
			"Region3int16" => Some(DataType::Value(VariantType::Region3int16)),
			"SharedString" => Some(DataType::Value(VariantType::SharedString)),
			"SecurityCapabilities" => Some(DataType::Value(VariantType::SecurityCapabilities)),
			"UDim" => Some(DataType::Value(VariantType::UDim)),
			"UDim2" => Some(DataType::Value(VariantType::UDim2)),
			"UniqueId" => Some(DataType::Value(VariantType::UniqueId)),
			"Vector2" => Some(DataType::Value(VariantType::Vector2)),
			"Vector2int16" => Some(DataType::Value(VariantType::Vector2int16)),
			"Vector3" => Some(DataType::Value(VariantType::Vector3)),
			"Vector3int16" => Some(DataType::Value(VariantType::Vector3int16)),
			"OptionalCoordinateFrame" => Some(DataType::Value(VariantType::OptionalCFrame)),
			_ => None,
		},
		_ => None,
	}
}

fn map_class_tag(tag: &str) -> Option<ClassTag> {
	match tag {
		"Deprecated" => Some(ClassTag::Deprecated),
		"NotBrowsable" => Some(ClassTag::NotBrowsable),
		"NotCreatable" => Some(ClassTag::NotCreatable),
		"NotReplicated" => Some(ClassTag::NotReplicated),
		"PlayerReplicated" => Some(ClassTag::PlayerReplicated),
		"Service" => Some(ClassTag::Service),
		"Settings" => Some(ClassTag::Settings),
		"UserSettings" => Some(ClassTag::UserSettings),
		_ => None,
	}
}

fn map_property_tag(tag: &str) -> Option<PropertyTag> {
	match tag {
		"Deprecated" => Some(PropertyTag::Deprecated),
		"Hidden" => Some(PropertyTag::Hidden),
		"NotBrowsable" => Some(PropertyTag::NotBrowsable),
		"NotReplicated" => Some(PropertyTag::NotReplicated),
		"NotScriptable" => Some(PropertyTag::NotScriptable),
		"ReadOnly" => Some(PropertyTag::ReadOnly),
		"WriteOnly" => Some(PropertyTag::WriteOnly),
		_ => None,
	}
}

fn scriptability(member: &ApiMember) -> Scriptability {
	let has_tag = |expected: &str| member.tags.iter().any(|tag| tag.as_str() == Some(expected));
	if has_tag("NotScriptable") {
		return Scriptability::None;
	}
	let sec_level = |direction: &str| -> &str {
		match &member.security {
			Some(serde_json::Value::String(s)) => s.as_str(),
			Some(serde_json::Value::Object(map)) => map.get(direction).and_then(|v| v.as_str()).unwrap_or("None"),
			_ => "None",
		}
	};
	let accessible = |level: &str| level == "None" || level == "PluginSecurity";
	let can_read = accessible(sec_level("Read"));
	let can_write = !has_tag("ReadOnly") && accessible(sec_level("Write"));
	if can_read && can_write {
		Scriptability::ReadWrite
	} else if can_read {
		Scriptability::Read
	} else if can_write {
		Scriptability::Write
	} else {
		Scriptability::None
	}
}

fn round_trips(member: &ApiMember) -> bool {
	member
		.serialization
		.as_ref()
		.is_some_and(|s| s.can_load == Some(true) && s.can_save == Some(true))
}

fn parse_api_dump(api_json: &str) -> Result<ApiDump> {
	serde_json::from_str(api_json).context("failed to parse Studio API Dump JSON")
}

fn parse_api_default_value(
	default_str: &str,
	data_type: &DataType<'static>,
	enums: &std::collections::HashMap<&'static str, EnumDescriptor<'static>>,
) -> Option<rbx_dom_weak::types::Variant> {
	use rbx_dom_weak::types::*;

	let s = default_str.trim();
	if s == "null" || s.starts_with("__api_dump_") {
		return None;
	}

	match data_type {
		DataType::Value(VariantType::Bool) => match s {
			"true" => Some(Variant::Bool(true)),
			"false" => Some(Variant::Bool(false)),
			_ => None,
		},
		DataType::Value(VariantType::Int32) => s.parse::<i32>().ok().map(Variant::Int32),
		DataType::Value(VariantType::Int64) => s.parse::<i64>().ok().map(Variant::Int64),
		DataType::Value(VariantType::Float32) => s.parse::<f32>().ok().map(Variant::Float32),
		DataType::Value(VariantType::Float64) => s.parse::<f64>().ok().map(Variant::Float64),
		DataType::Value(VariantType::String) => Some(Variant::String(s.to_string())),
		DataType::Enum(enum_name) => {
			if let Ok(val) = s.parse::<u32>() {
				Some(Variant::Enum(Enum::from_u32(val)))
			} else if let Some(enum_desc) = enums.get(enum_name) {
				enum_desc
					.items
					.get(s)
					.copied()
					.map(|val| Variant::Enum(Enum::from_u32(val)))
			} else {
				None
			}
		}
		DataType::Value(VariantType::Color3) | DataType::Value(VariantType::Color3uint8) => {
			let parts: Vec<&str> = s.split(',').collect();
			if parts.len() == 3 {
				let r = parts[0].trim().parse::<f32>().ok()?;
				let g = parts[1].trim().parse::<f32>().ok()?;
				let b = parts[2].trim().parse::<f32>().ok()?;
				if matches!(data_type, DataType::Value(VariantType::Color3uint8)) {
					let r_u8 = (r * 255.0).clamp(0.0, 255.0) as u8;
					let g_u8 = (g * 255.0).clamp(0.0, 255.0) as u8;
					let b_u8 = (b * 255.0).clamp(0.0, 255.0) as u8;
					Some(Variant::Color3uint8(Color3uint8::new(r_u8, g_u8, b_u8)))
				} else {
					Some(Variant::Color3(Color3::new(r, g, b)))
				}
			} else {
				None
			}
		}
		DataType::Value(VariantType::Vector2) => {
			let parts: Vec<&str> = s.split(',').collect();
			if parts.len() == 2 {
				let x = parts[0].trim().parse::<f32>().ok()?;
				let y = parts[1].trim().parse::<f32>().ok()?;
				Some(Variant::Vector2(Vector2::new(x, y)))
			} else {
				None
			}
		}
		DataType::Value(VariantType::Vector3) => {
			let parts: Vec<&str> = s.split(',').collect();
			if parts.len() == 3 {
				let x = parts[0].trim().parse::<f32>().ok()?;
				let y = parts[1].trim().parse::<f32>().ok()?;
				let z = parts[2].trim().parse::<f32>().ok()?;
				Some(Variant::Vector3(Vector3::new(x, y, z)))
			} else {
				None
			}
		}
		DataType::Value(VariantType::UDim) => {
			let clean = s.trim_matches(|c| c == '{' || c == '}');
			let parts: Vec<&str> = clean.split(',').collect();
			if parts.len() == 2 {
				let scale = parts[0].trim().parse::<f32>().ok()?;
				let offset = parts[1].trim().parse::<i32>().ok()?;
				Some(Variant::UDim(UDim::new(scale, offset)))
			} else {
				None
			}
		}
		DataType::Value(VariantType::UDim2) => {
			let clean = s.replace(['{', '}'], "");
			let parts: Vec<&str> = clean.split(',').collect();
			if parts.len() == 4 {
				let sx = parts[0].trim().parse::<f32>().ok()?;
				let ox = parts[1].trim().parse::<i32>().ok()?;
				let sy = parts[2].trim().parse::<f32>().ok()?;
				let oy = parts[3].trim().parse::<i32>().ok()?;
				Some(Variant::UDim2(UDim2::new(UDim::new(sx, ox), UDim::new(sy, oy))))
			} else {
				None
			}
		}
		_ => None,
	}
}

fn apply_carbon_reflection_policy(database: &mut ReflectionDatabase<'static>) {
	for (class_name, property_name, data_type) in [
		("Instance", "Attributes", VariantType::Attributes),
		("Instance", "Tags", VariantType::Tags),
		("LocalizationTable", "Contents", VariantType::String),
		("Model", "Scale", VariantType::Float32),
		("Model", "WorldPivotData", VariantType::OptionalCFrame),
		("ModuleScript", "Source", VariantType::String),
		("Script", "Source", VariantType::String),
		("StyleRule", "Properties", VariantType::BinaryString),
		("Terrain", "MaterialColors", VariantType::MaterialColors),
	] {
		if let Some(class) = database.classes.get_mut(class_name) {
			let property = class
				.properties
				.entry(property_name)
				.or_insert_with(|| PropertyDescriptor::new(property_name, DataType::Value(data_type)));
			property.data_type = DataType::Value(data_type);
			property.scriptability = Scriptability::Custom;
		}
	}

	for (class_name, property_name, data_type, serializes) in [
		(
			"BinaryStringValue",
			"Value",
			DataType::Value(VariantType::BinaryString),
			true,
		),
		(
			"TerrainRegion",
			"ExtentsMax",
			DataType::Value(VariantType::Vector3int16),
			true,
		),
		(
			"TerrainRegion",
			"ExtentsMin",
			DataType::Value(VariantType::Vector3int16),
			true,
		),
		(
			"Workspace",
			"CollisionGroupData",
			DataType::Value(VariantType::BinaryString),
			true,
		),
		(
			"PartOperation",
			"SolidMeshHolder",
			DataType::Value(VariantType::NetAssetRef),
			true,
		),
		(
			"MeshPart",
			"SolidMeshHolder",
			DataType::Value(VariantType::NetAssetRef),
			false,
		),
		(
			"StudioData",
			"EnableScriptCollabByDefaultOnLoad",
			DataType::Value(VariantType::Bool),
			true,
		),
		(
			"PlayerEmulatorService",
			"SerializedEmulatedPolicyInfo",
			DataType::Value(VariantType::BinaryString),
			true,
		),
		(
			"VoiceChatService",
			"EnableVoiceVolumeControls",
			DataType::Enum("RolloutState"),
			true,
		),
	] {
		if let Some(class) = database.classes.get_mut(class_name) {
			let property = class
				.properties
				.entry(property_name)
				.or_insert_with(|| PropertyDescriptor::new(property_name, data_type.clone()));
			property.data_type = data_type;
			property.scriptability = Scriptability::None;
			property.tags.insert(PropertyTag::Hidden);
			property.tags.insert(PropertyTag::NotScriptable);
			property.kind = PropertyKind::Canonical {
				serialization: if serializes {
					PropertySerialization::Serializes
				} else {
					PropertySerialization::DoesNotSerialize
				},
			};
		}
	}

	if let Some(instance) = database.classes.get_mut("Instance") {
		instance
			.default_properties
			.insert("Archivable", rbx_dom_weak::types::Variant::Bool(true));
		instance.default_properties.insert(
			"Attributes",
			rbx_dom_weak::types::Variant::Attributes(rbx_dom_weak::types::Attributes::new()),
		);
		instance.default_properties.insert(
			"Tags",
			rbx_dom_weak::types::Variant::Tags(rbx_dom_weak::types::Tags::new()),
		);
	}
	if let Some(lua_source_container) = database.classes.get_mut("LuaSourceContainer") {
		lua_source_container
			.default_properties
			.insert("ScriptGuid", rbx_dom_weak::types::Variant::String(String::new()));
	}
	for (class_name, property_name, serialized_name) in [
		("BasePart", "Color", "Color3uint8"),
		("BasePart", "MaterialVariant", "MaterialVariantSerialized"),
		("BasePart", "Size", "size"),
		("Fire", "Heat", "heat_xml"),
		("Fire", "Size", "size_xml"),
		("FormFactorPart", "FormFactor", "formFactorRaw"),
		("Instance", "Archivable", "archivable"),
		("Instance", "Attributes", "AttributesSerialize"),
		("Instance", "Sandboxed", "DefinesCapabilities"),
		("MaterialService", "Use2022Materials", "Use2022MaterialsXml"),
		("Model", "Scale", "ScaleFactor"),
		("PackageLink", "PackageContent", "PackageContentSerialize"),
		("Part", "Shape", "shape"),
		("Players", "MaxPlayers", "MaxPlayersInternal"),
		("Players", "PreferredPlayers", "PreferredPlayersInternal"),
		("Smoke", "Opacity", "opacity_xml"),
		("Smoke", "RiseVelocity", "riseVelocity_xml"),
		("Smoke", "Size", "size_xml"),
		("Sound", "MaxDistance", "xmlRead_MaxDistance_3"),
		("Sound", "RollOffMaxDistance", "xmlRead_MaxDistance_3"),
		("Sound", "RollOffMinDistance", "EmitterSize"),
		(
			"StarterPlayer",
			"AvatarJointUpgrade",
			"AvatarJointUpgrade_SerializedRollout",
		),
		("StyleRule", "Properties", "PropertiesSerialize"),
		("WeldConstraint", "Part0", "Part0Internal"),
		("WeldConstraint", "Part1", "Part1Internal"),
		("Workspace", "SignalBehavior", "SignalBehavior2"),
	] {
		if let Some(class) = database.classes.get_mut(class_name) {
			if !class.properties.contains_key(serialized_name) {
				if let Some(canonical) = class.properties.get(property_name) {
					let mut serialized = canonical.clone();
					serialized.name = serialized_name;
					serialized.scriptability = Scriptability::None;
					serialized.kind = PropertyKind::Canonical {
						serialization: PropertySerialization::Serializes,
					};
					class.properties.insert(serialized_name, serialized);
				}
			}
		}
		if let Some(property) = database
			.classes
			.get_mut(class_name)
			.and_then(|class| class.properties.get_mut(property_name))
		{
			property.kind = PropertyKind::Canonical {
				serialization: PropertySerialization::SerializesAs(serialized_name),
			};
		}
	}
	for (class_name, property_name, data_type) in [
		("BasePart", "Color3uint8", VariantType::Color3uint8),
		("Instance", "AttributesSerialize", VariantType::BinaryString),
	] {
		if let Some(property) = database
			.classes
			.get_mut(class_name)
			.and_then(|class| class.properties.get_mut(property_name))
		{
			property.data_type = DataType::Value(data_type);
		}
	}

	if let Some(clock_time) = database
		.classes
		.get_mut("Lighting")
		.and_then(|class| class.properties.get_mut("ClockTime"))
	{
		clock_time.kind = PropertyKind::Canonical {
			serialization: PropertySerialization::Serializes,
		};
	}

	for (class_name, alias_name, canonical_name) in [
		("BasePart", "Color3uint8", "Color"),
		("BasePart", "MaterialVariantSerialized", "MaterialVariant"),
		("BasePart", "size", "Size"),
		("BodyAngularVelocity", "angularvelocity", "AngularVelocity"),
		("BodyAngularVelocity", "maxTorque", "MaxTorque"),
		("BodyForce", "force", "Force"),
		("BodyGyro", "cframe", "CFrame"),
		("BodyGyro", "maxTorque", "MaxTorque"),
		("BodyPosition", "maxForce", "MaxForce"),
		("BodyPosition", "position", "Position"),
		("BodyThrust", "force", "Force"),
		("BodyThrust", "location", "Location"),
		("BodyVelocity", "maxForce", "MaxForce"),
		("BodyVelocity", "velocity", "Velocity"),
		("Camera", "CoordinateFrame", "CFrame"),
		("Camera", "focus", "Focus"),
		("Fire", "heat_xml", "Heat"),
		("Fire", "size", "Size"),
		("Fire", "size_xml", "Size"),
		("FormFactorPart", "formFactor", "FormFactor"),
		("FormFactorPart", "Formfactor", "FormFactor"),
		("FormFactorPart", "formFactorRaw", "FormFactor"),
		("Instance", "AttributesSerialize", "Attributes"),
		("Instance", "DefinesCapabilities", "Sandboxed"),
		("Instance", "archivable", "Archivable"),
		("JointInstance", "part1", "Part1"),
		("Model", "ScaleFactor", "Scale"),
		("Object", "className", "ClassName"),
		("PackageLink", "PackageContentSerialize", "PackageContent"),
		("PackageLink", "PackageIdSerialize", "PackageId"),
		("Part", "shape", "Shape"),
		("Players", "MaxPlayersInternal", "MaxPlayers"),
		("Players", "PreferredPlayersInternal", "PreferredPlayers"),
		("Smoke", "opacity_xml", "Opacity"),
		("Smoke", "riseVelocity_xml", "RiseVelocity"),
		("Smoke", "size_xml", "Size"),
		("Sound", "EmitterSize", "RollOffMinDistance"),
		("Sound", "xmlRead_MaxDistance_3", "RollOffMaxDistance"),
		(
			"StarterPlayer",
			"AvatarJointUpgrade_SerializedRollout",
			"AvatarJointUpgrade",
		),
		("StyleRule", "PropertiesSerialize", "Properties"),
		("WeldConstraint", "Part0Internal", "Part0"),
		("WeldConstraint", "Part1Internal", "Part1"),
		("Workspace", "SignalBehavior2", "SignalBehavior"),
	] {
		if let Some(class) = database.classes.get_mut(class_name) {
			if !class.properties.contains_key(alias_name) {
				if let Some(canonical) = class.properties.get(canonical_name) {
					let mut alias = canonical.clone();
					alias.name = alias_name;
					alias.scriptability = Scriptability::None;
					alias.kind = PropertyKind::Alias {
						alias_for: canonical_name,
					};
					class.properties.insert(alias_name, alias);
				}
			}
		}
		if let Some(property) = database
			.classes
			.get_mut(class_name)
			.and_then(|class| class.properties.get_mut(alias_name))
		{
			property.kind = PropertyKind::Alias {
				alias_for: canonical_name,
			};
		}
	}

	fn migration(json: &'static str) -> PropertyMigration<'static> {
		serde_json::from_str(json).expect("Carbon reflection migration policy must be valid")
	}
	for (class_name, property_name, json) in [
		(
			"AdGui",
			"FallbackImage",
			r#"{"To":"FallbackImageContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Animation",
			"AnimationId",
			r#"{"To":"AnimationContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"AudioPlayer",
			"Asset",
			r#"{"To":"AudioContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"BackpackItem",
			"TextureId",
			r#"{"To":"TextureContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"BaseWrap",
			"CageMeshId",
			r#"{"To":"CageMeshContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Beam",
			"Texture",
			r#"{"To":"TextureContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"ClickDetector",
			"CursorIcon",
			r#"{"To":"CursorIconContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Decal",
			"ColorMap",
			r#"{"To":"ColorMapContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Decal",
			"MetalnessMap",
			r#"{"To":"MetalnessMapContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Decal",
			"NormalMap",
			r#"{"To":"NormalMapContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Decal",
			"RoughnessMap",
			r#"{"To":"RoughnessMapContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Decal",
			"Texture",
			r#"{"To":"TextureContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"DragDetector",
			"ActivatedCursorIcon",
			r#"{"To":"ActivatedCursorIconContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"FileMesh",
			"MeshId",
			r#"{"To":"MeshContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"FileMesh",
			"TextureId",
			r#"{"To":"TextureContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"ImageButton",
			"HoverImage",
			r#"{"To":"HoverImageContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"ImageButton",
			"Image",
			r#"{"To":"ImageContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"ImageButton",
			"PressedImage",
			r#"{"To":"PressedImageContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"ImageHandleAdornment",
			"Image",
			r#"{"To":"ImageContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"ImageLabel",
			"Image",
			r#"{"To":"ImageContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"MaterialVariant",
			"ColorMap",
			r#"{"To":"ColorMapContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"MaterialVariant",
			"MetalnessMap",
			r#"{"To":"MetalnessMapContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"MaterialVariant",
			"NormalMap",
			r#"{"To":"NormalMapContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"MaterialVariant",
			"RoughnessMap",
			r#"{"To":"RoughnessMapContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"MeshPart",
			"MeshId",
			r#"{"To":"MeshContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"MeshPart",
			"TextureID",
			r#"{"To":"TextureContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Mouse",
			"Icon",
			r#"{"To":"IconContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"PackageLink",
			"PackageId",
			r#"{"To":"PackageContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"ParticleEmitter",
			"Texture",
			r#"{"To":"TextureContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"ScreenshotHud",
			"CameraButtonIcon",
			r#"{"To":"CameraButtonIconContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"ScrollingFrame",
			"BottomImage",
			r#"{"To":"BottomImageContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"ScrollingFrame",
			"MidImage",
			r#"{"To":"MidImageContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"ScrollingFrame",
			"TopImage",
			r#"{"To":"TopImageContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Sky",
			"MoonTextureId",
			r#"{"To":"MoonTextureContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Sky",
			"SkyboxBk",
			r#"{"To":"SkyboxBackContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Sky",
			"SkyboxDn",
			r#"{"To":"SkyboxDownContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Sky",
			"SkyboxFt",
			r#"{"To":"SkyboxFrontContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Sky",
			"SkyboxLf",
			r#"{"To":"SkyboxLeftContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Sky",
			"SkyboxRt",
			r#"{"To":"SkyboxRightContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Sky",
			"SkyboxUp",
			r#"{"To":"SkyboxUpContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Sky",
			"SunTextureId",
			r#"{"To":"SunTextureContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Sound",
			"SoundId",
			r#"{"To":"AudioContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"SurfaceAppearance",
			"ColorMap",
			r#"{"To":"ColorMapContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"SurfaceAppearance",
			"MetalnessMap",
			r#"{"To":"MetalnessMapContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"SurfaceAppearance",
			"NormalMap",
			r#"{"To":"NormalMapContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"SurfaceAppearance",
			"RoughnessMap",
			r#"{"To":"RoughnessMapContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"TerrainDetail",
			"ColorMap",
			r#"{"To":"ColorMapContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"TerrainDetail",
			"MetalnessMap",
			r#"{"To":"MetalnessMapContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"TerrainDetail",
			"NormalMap",
			r#"{"To":"NormalMapContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"TerrainDetail",
			"RoughnessMap",
			r#"{"To":"RoughnessMapContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"Trail",
			"Texture",
			r#"{"To":"TextureContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"UIDragDetector",
			"ActivatedCursorIcon",
			r#"{"To":"ActivatedCursorIconContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"UIDragDetector",
			"CursorIcon",
			r#"{"To":"CursorIconContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"UserInputService",
			"MouseIcon",
			r#"{"To":"MouseIconContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"VideoFrame",
			"Video",
			r#"{"To":"VideoContent","Migration":"ContentIdToContent"}"#,
		),
		(
			"WrapLayer",
			"ReferenceMeshId",
			r#"{"To":"ReferenceMeshContent","Migration":"ContentIdToContent"}"#,
		),
	] {
		if let Some(property) = database
			.classes
			.get_mut(class_name)
			.and_then(|class| class.properties.get_mut(property_name))
		{
			property.kind = PropertyKind::Canonical {
				serialization: PropertySerialization::Migrate(migration(json)),
			};
		}
	}
	for (class_name, property_name, json) in [
		(
			"BasePart",
			"BrickColor",
			r#"{"To":"Color","Migration":"BrickColorToColor"}"#,
		),
		(
			"BasePart",
			"brickColor",
			r#"{"To":"Color","Migration":"BrickColorToColor"}"#,
		),
		(
			"CharacterMesh",
			"BaseTextureId",
			r#"{"To":"BaseTextureContent","Migration":"Int64ToContent"}"#,
		),
		(
			"CharacterMesh",
			"MeshId",
			r#"{"To":"MeshContent","Migration":"Int64ToContent"}"#,
		),
		(
			"ScreenGui",
			"IgnoreGuiInset",
			r#"{"To":"ScreenInsets","Migration":"IgnoreGuiInsetToScreenInsets"}"#,
		),
		("TextBox", "Font", r#"{"To":"FontFace","Migration":"FontToFontFace"}"#),
		(
			"TextButton",
			"Font",
			r#"{"To":"FontFace","Migration":"FontToFontFace"}"#,
		),
		("TextLabel", "Font", r#"{"To":"FontFace","Migration":"FontToFontFace"}"#),
		(
			"UICorner",
			"CornerRadius",
			r#"{"To":["BottomLeftRadius","BottomRightRadius","TopLeftRadius","TopRightRadius"],"Migration":"CornerRadiusToCornerRadii"}"#,
		),
	] {
		if let Some(property) = database
			.classes
			.get_mut(class_name)
			.and_then(|class| class.properties.get_mut(property_name))
		{
			property.kind = PropertyKind::Canonical {
				serialization: PropertySerialization::Migrate(migration(json)),
			};
		}
	}
}

fn build_database_from_api(dump: ApiDump, version: [u32; 4]) -> ReflectionDatabase<'static> {
	let mut database = ReflectionDatabase::new();
	database.version = version;
	let mut unsupported_properties = Vec::new();

	database.enums = dump
		.enums
		.into_iter()
		.map(|api_enum| {
			let enum_name: &'static str = Box::leak(api_enum.name.into_boxed_str());
			let mut descriptor = EnumDescriptor::new(enum_name);
			descriptor.items = api_enum
				.items
				.into_iter()
				.map(|item| {
					let item_name: &'static str = Box::leak(item.name.into_boxed_str());
					(item_name, item.value)
				})
				.collect();
			(enum_name, descriptor)
		})
		.collect();

	for api_class in dump.classes {
		let class_name: &'static str = Box::leak(api_class.name.into_boxed_str());
		let superclass: Option<&'static str> = if api_class.superclass == "<<<ROOT>>>" {
			None
		} else {
			Some(Box::leak(api_class.superclass.into_boxed_str()))
		};
		let class_tags = api_class
			.tags
			.iter()
			.filter_map(|tag| tag.as_str().and_then(map_class_tag))
			.collect();

		let descriptor = database
			.classes
			.entry(class_name)
			.or_insert_with(|| ClassDescriptor::new(class_name));
		descriptor.name = class_name;
		descriptor.superclass = superclass;
		descriptor.tags = class_tags;

		for member in api_class.members {
			if member.member_type != "Property" {
				continue;
			}
			let Some(api_value_type) = member.value_type.as_ref() else {
				continue;
			};
			let Some(value_type) = convert_type(api_value_type) else {
				if round_trips(&member) && !descriptor.properties.contains_key(member.name.as_str()) {
					unsupported_properties.push(format!(
						"{}.{} ({}.{})",
						descriptor.name, member.name, api_value_type.category, api_value_type.name
					));
				}
				continue;
			};
			let serializes = round_trips(&member);
			let property_scriptability = scriptability(&member);
			let property_tags = member
				.tags
				.iter()
				.filter_map(|tag| tag.as_str().and_then(map_property_tag))
				.collect();
			let property_name: &'static str = Box::leak(member.name.into_boxed_str());

			let kind = PropertyKind::Canonical {
				serialization: if serializes {
					PropertySerialization::Serializes
				} else {
					PropertySerialization::DoesNotSerialize
				},
			};

			let mut property = PropertyDescriptor::new(property_name, value_type.clone());
			property.scriptability = property_scriptability;
			property.tags = property_tags;
			property.kind = kind;

			if let Some(default_val) = member.default.as_ref() {
				let string_buf;
				let default_str = match default_val {
					serde_json::Value::String(s) => Some(s.as_str()),
					serde_json::Value::Bool(b) => {
						if *b {
							Some("true")
						} else {
							Some("false")
						}
					}
					serde_json::Value::Number(n) => {
						string_buf = n.to_string();
						Some(string_buf.as_str())
					}
					_ => None,
				};
				if let Some(str_val) = default_str {
					if let Some(default_variant) = parse_api_default_value(str_val, &value_type, &database.enums) {
						descriptor.default_properties.insert(property_name, default_variant);
					}
				}
			}

			descriptor.properties.insert(property_name, property);
		}
	}

	unsupported_properties.sort();
	if !unsupported_properties.is_empty() {
		log::debug!(
			"Live reflection omitted {} unsupported round-trippable properties: {}",
			unsupported_properties.len(),
			unsupported_properties.join(", ")
		);
	}

	apply_carbon_reflection_policy(&mut database);

	database
}

fn read_api_dump(path: &std::path::Path) -> Result<(String, ApiDump)> {
	let content = fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
	ensure!(!content.trim().is_empty(), "{} is empty", path.display());
	let dump = parse_api_dump(&content)?;
	Ok((content, dump))
}

#[cfg(target_os = "linux")]
const FULL_API_POWERSHELL: &str = r#"
$startInfo = New-Object System.Diagnostics.ProcessStartInfo
$startInfo.FileName = $env:CARBON_REFLECTION_STUDIO
$startInfo.UseShellExecute = $false
$startInfo.Arguments = '--fullApi "' + $env:CARBON_REFLECTION_OUTPUT + '"'
$process = [System.Diagnostics.Process]::Start($startInfo)
$complete = $false
try {
    $lastLength = -1L
    $stablePolls = 0
    $deadline = [DateTime]::UtcNow.AddSeconds(45)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (Test-Path -LiteralPath $env:CARBON_REFLECTION_OUTPUT) {
            $length = (Get-Item -LiteralPath $env:CARBON_REFLECTION_OUTPUT).Length
            if ($length -gt 0 -and $length -eq $lastLength) {
                $stablePolls++
            } else {
                $stablePolls = 0
                $lastLength = $length
            }
            if ($stablePolls -ge 5) {
                $stream = $null
                try {
                    $stream = [System.IO.File]::Open(
                        $env:CARBON_REFLECTION_OUTPUT,
                        [System.IO.FileMode]::Open,
                        [System.IO.FileAccess]::Read,
                        [System.IO.FileShare]::ReadWrite
                    )
                    $tailLength = [Math]::Min(64, $stream.Length)
                    $null = $stream.Seek(-$tailLength, [System.IO.SeekOrigin]::End)
                    $tail = New-Object byte[] ([int]$tailLength)
                    $bytesRead = $stream.Read($tail, 0, $tail.Length)
                    for ($index = $bytesRead - 1; $index -ge 0; $index--) {
                        $byte = $tail[$index]
                        if ($byte -notin @(9, 10, 13, 32)) {
                            $complete = $byte -eq 125
                            break
                        }
                    }
                } catch {
                    $complete = $false
                } finally {
                    if ($null -ne $stream) {
                        $stream.Dispose()
                    }
                }
                if ($complete) {
                    break
                }
            }
        }
        Start-Sleep -Milliseconds 100
    }
    if (!$complete) {
        throw 'Studio produced no complete FullAPI dump'
    }
} finally {
    if ($null -ne $process) {
        taskkill.exe /F /T /PID $process.Id *> $null
    }
}
exit 0
"#;

#[cfg(target_os = "linux")]
fn windows_path(path: &std::path::Path) -> Result<std::ffi::OsString> {
	let output = Command::new("wslpath")
		.arg("-w")
		.arg(path)
		.output()
		.with_context(|| format!("failed to translate {} with wslpath -w", path.display()))?;
	ensure!(output.status.success(), "wslpath -w failed for {}", path.display());
	Ok(String::from_utf8(output.stdout)?.trim().into())
}

#[cfg(target_os = "linux")]
fn generate_api_dump(studio_info: &rml::StudioInfo, output_path: &std::path::Path) -> Result<()> {
	let (studio, output) = (windows_path(&studio_info.executable)?, windows_path(output_path)?);
	let mut command = crate::studio::powershell_command()?;
	command
		.args(["-NoProfile", "-NonInteractive", "-Command", FULL_API_POWERSHELL])
		.env("CARBON_REFLECTION_STUDIO", studio)
		.env("CARBON_REFLECTION_OUTPUT", output);
	let mut wslenv = env::var("WSLENV").unwrap_or_default();
	for variable in ["CARBON_REFLECTION_STUDIO", "CARBON_REFLECTION_OUTPUT"] {
		if !wslenv.is_empty() {
			wslenv.push(':');
		}
		wslenv.push_str(variable);
	}
	command.env("WSLENV", wslenv);

	let output = command.output().context("failed to launch Studio FullAPI extraction")?;
	ensure!(
		output.status.success(),
		"Studio FullAPI extraction failed for {}: {}",
		studio_info.version_text,
		String::from_utf8_lossy(&output.stderr).trim()
	);
	Ok(())
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn generate_api_dump(studio_info: &rml::StudioInfo, output_path: &std::path::Path) -> Result<()> {
	use std::time::{Duration, Instant};

	let mut child = Command::new(&studio_info.executable)
		.arg("--fullApi")
		.arg(output_path)
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null())
		.spawn()
		.with_context(|| format!("failed to launch Studio at {}", studio_info.executable.display()))?;
	#[cfg(target_os = "windows")]
	let process_id = child.id();
	let deadline = Instant::now() + Duration::from_secs(45);
	let mut last_length = 0;
	let mut stable_polls = 0;
	while Instant::now() < deadline {
		if let Ok(metadata) = fs::metadata(output_path) {
			if metadata.len() > 0 && metadata.len() == last_length {
				stable_polls += 1;
			} else {
				last_length = metadata.len();
				stable_polls = 0;
			}
			if stable_polls >= 5 && read_api_dump(output_path).is_ok() {
				break;
			}
		}
		if child.try_wait()?.is_some() {
			break;
		}
		std::thread::sleep(Duration::from_millis(100));
	}
	#[cfg(target_os = "windows")]
	let _ = Command::new("taskkill.exe")
		.args(["/F", "/T", "/PID", &process_id.to_string()])
		.output();
	let _ = child.kill();
	let _ = child.wait();
	ensure!(
		output_path.is_file() && read_api_dump(output_path).is_ok(),
		"Studio produced no complete FullAPI dump"
	);
	Ok(())
}

fn get_or_fetch_api_dump(studio_info: &rml::StudioInfo) -> Result<(String, ApiDump)> {
	let cache_dir = get_carbon_dir()?
		.join("reflection")
		.join(format!("{}-{}", studio_info.build_id, studio_info.version_text));
	fs::create_dir_all(&cache_dir)?;
	let cache_file = cache_dir.join("API-Dump.json");
	let lock_path = cache_dir.join(".lock");
	let lock = fs::OpenOptions::new()
		.create(true)
		.truncate(false)
		.read(true)
		.write(true)
		.open(&lock_path)
		.with_context(|| format!("failed to open reflection cache lock {}", lock_path.display()))?;
	lock.lock()
		.with_context(|| format!("failed to lock reflection cache {}", cache_dir.display()))?;

	if cache_file.is_file() {
		match read_api_dump(&cache_file) {
			Ok(cached) => return Ok(cached),
			Err(error) => {
				log::warn!(
					"Discarding invalid Studio reflection cache {}: {error:#}",
					cache_file.display()
				);
				fs::remove_file(&cache_file)
					.with_context(|| format!("failed to remove invalid reflection cache {}", cache_file.display()))?;
			}
		}
	}

	let temporary = cache_dir.join(format!("API-Dump.json.tmp.{}", std::process::id()));
	if temporary.exists() {
		fs::remove_file(&temporary)
			.with_context(|| format!("failed to remove stale reflection temporary {}", temporary.display()))?;
	}

	let generated = (|| -> Result<(String, ApiDump)> {
		generate_api_dump(studio_info, &temporary)?;
		read_api_dump(&temporary)
	})();

	let (content, dump) = match generated {
		Ok(generated) => generated,
		Err(error) => {
			let _ = fs::remove_file(&temporary);
			return Err(error);
		}
	};
	if let Err(error) = fs::rename(&temporary, &cache_file) {
		let _ = fs::remove_file(&temporary);
		return Err(error).with_context(|| format!("failed to publish reflection cache {}", cache_file.display()));
	}
	Ok((content, dump))
}

pub struct ReflectionSnapshot {
	pub version: [u32; 4],
	pub studio_dir: PathBuf,
	pub api_dump: String,
	pub database: ReflectionDatabase<'static>,
}

static LIVE_SNAPSHOT: OnceLock<ReflectionSnapshot> = OnceLock::new();
static REFLECTION_INIT_LOCK: Mutex<()> = Mutex::new(());

pub fn init_reflection() -> Result<&'static ReflectionSnapshot> {
	if let Some(snapshot) = LIVE_SNAPSHOT.get() {
		return Ok(snapshot);
	}
	if let (Ok(api_dump_path), Ok(version_text)) = (
		env::var("CARBON_REFLECTION_API_DUMP"),
		env::var("CARBON_REFLECTION_VERSION"),
	) {
		let components = version_text
			.split('.')
			.map(str::parse::<u32>)
			.collect::<std::result::Result<Vec<_>, _>>()
			.with_context(|| format!("invalid CARBON_REFLECTION_VERSION {version_text}"))?;
		let version: [u32; 4] = components
			.try_into()
			.map_err(|_| anyhow::anyhow!("CARBON_REFLECTION_VERSION must have four components"))?;
		let api_json = fs::read_to_string(&api_dump_path)
			.with_context(|| format!("failed to read CARBON_REFLECTION_API_DUMP {api_dump_path}"))?;
		return init_reflection_from_json(&api_json, version);
	}
	let _initialization = REFLECTION_INIT_LOCK
		.lock()
		.expect("live reflection initialization lock was poisoned");
	if let Some(snapshot) = LIVE_SNAPSHOT.get() {
		return Ok(snapshot);
	}

	let studio_info = rml::get_studio_info()?;
	let (api_dump, parsed_dump) = get_or_fetch_api_dump(&studio_info)?;
	let database = build_database_from_api(parsed_dump, studio_info.version_components);

	let studio_dir = studio_info
		.executable
		.parent()
		.context("Roblox Studio executable does not have an installation directory")?
		.to_owned();
	let snapshot = ReflectionSnapshot {
		version: studio_info.version_components,
		studio_dir,
		api_dump,
		database,
	};
	LIVE_SNAPSHOT
		.set(snapshot)
		.map_err(|_| anyhow::anyhow!("live reflection snapshot was initialized concurrently"))?;
	Ok(LIVE_SNAPSHOT
		.get()
		.expect("live reflection snapshot was just initialized"))
}

pub fn init_artifact_reflection() -> Result<()> {
	init_reflection().map(|_| ())
}

pub fn init_reflection_from_json(api_json: &str, version: [u32; 4]) -> Result<&'static ReflectionSnapshot> {
	if let Some(snapshot) = LIVE_SNAPSHOT.get() {
		return Ok(snapshot);
	}
	let _initialization = REFLECTION_INIT_LOCK
		.lock()
		.expect("live reflection initialization lock was poisoned");
	if let Some(snapshot) = LIVE_SNAPSHOT.get() {
		return Ok(snapshot);
	}

	let parsed_dump = parse_api_dump(api_json)?;
	let database = build_database_from_api(parsed_dump, version);
	let snapshot = ReflectionSnapshot {
		version,
		studio_dir: PathBuf::new(),
		api_dump: api_json.to_string(),
		database,
	};
	let _ = LIVE_SNAPSHOT.set(snapshot);
	Ok(LIVE_SNAPSHOT
		.get()
		.expect("live reflection snapshot was initialized from JSON"))
}

pub fn get_reflection_snapshot() -> &'static ReflectionSnapshot {
	if let Some(snapshot) = LIVE_SNAPSHOT.get() {
		snapshot
	} else {
		init_reflection().expect("failed to initialize live reflection snapshot")
	}
}

pub fn get_carbon_dir() -> Result<PathBuf> {
	let user_dirs = UserDirs::new().context("Failed to get user directory")?;
	prepare_carbon_dir(user_dirs.home_dir().join(".carbon"))
}

fn prepare_carbon_dir(path: PathBuf) -> Result<PathBuf> {
	fs::create_dir_all(&path).with_context(|| format!("failed to create Carbon state directory {}", path.display()))?;
	Ok(path)
}

pub fn kill_process(pid: u32) {
	#[cfg(not(target_os = "windows"))]
	{
		Command::new("kill").arg(pid.to_string()).output().ok();
		Command::new("pkill").arg("-P").arg(pid.to_string()).output().ok();
	}

	#[cfg(target_os = "windows")]
	Command::new("TASKKILL")
		.arg("/F")
		.arg("/T")
		.args(["/PID", &pid.to_string()])
		.output()
		.ok();
}

pub fn process_exists(pid: u32) -> bool {
	#[cfg(not(target_os = "windows"))]
	{
		Command::new("kill")
			.arg("-0")
			.arg(pid.to_string())
			.output()
			.is_ok_and(|output| output.status.success())
	}

	#[cfg(target_os = "windows")]
	{
		Command::new("TASKLIST")
			.arg("/NH")
			.args(["/FI", &format!("PID eq {}", pid)])
			.output()
			.is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("carbon.exe"))
	}
}

pub fn env_verbosity() -> LevelFilter {
	match env::var("RUST_VERBOSE").unwrap_or_else(|_| "ERROR".into()).as_str() {
		"OFF" => LevelFilter::Off,
		"WARN" => LevelFilter::Warn,
		"INFO" => LevelFilter::Info,
		"DEBUG" => LevelFilter::Debug,
		"TRACE" => LevelFilter::Trace,
		_ => LevelFilter::Error,
	}
}

pub fn env_log_style() -> WriteStyle {
	match env::var("RUST_LOG_STYLE").unwrap_or_else(|_| "auto".into()).as_str() {
		"always" => WriteStyle::Always,
		"never" => WriteStyle::Never,
		_ => WriteStyle::Auto,
	}
}

pub fn env_backtrace() -> bool {
	env::var("RUST_BACKTRACE").unwrap_or_else(|_| "0".into()) == "1"
}

pub fn env_yes() -> bool {
	env::var("RUST_YES").unwrap_or_else(|_| "0".into()) == "1"
}

pub fn get_reflection_database() -> &'static ReflectionDatabase<'static> {
	&get_reflection_snapshot().database
}

pub fn try_get_reflection_database() -> Option<&'static ReflectionDatabase<'static>> {
	LIVE_SNAPSHOT.get().map(|snapshot| &snapshot.database)
}

#[cfg(test)]
mod tests {
	use super::*;
	use rbx_reflection::{DataType, PropertyKind, PropertySerialization};
	use std::time::{SystemTime, UNIX_EPOCH};

	#[test]
	fn renamed_carbon_state_directory_is_created_before_first_write() {
		let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
		let root = env::temp_dir().join(format!("carbon-state-dir-test-{}-{unique}", std::process::id()));
		let carbon_dir = root.join("nested/.carbon");

		let prepared = prepare_carbon_dir(carbon_dir.clone()).unwrap();

		assert_eq!(prepared, carbon_dir);
		assert!(
			prepared.is_dir(),
			"the first Carbon session write needs an existing parent directory"
		);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn reflection_classifies_non_loadable_current_reference() {
		let api_dump = parse_api_dump(
			r#"{
				"Version": 1,
				"Classes": [{
					"Name": "InputAction",
					"Superclass": "Instance",
					"Tags": [],
					"Members": [{
						"MemberType": "Property",
						"Name": "PreferredBinding",
						"ValueType": { "Category": "Class", "Name": "InputBinding" },
						"Security": { "Read": "None", "Write": "None" },
						"Tags": ["ReadOnly", "NotReplicated", {
							"PreferredDescriptorName": "LegacyPreferredBinding",
							"ThreadSafety": "Unknown"
						}],
						"Serialization": { "CanLoad": false, "CanSave": true }
					}, {
						"MemberType": "Property",
						"Name": "Icon",
						"ValueType": { "Category": "DataType", "Name": "ContentId" },
						"Security": { "Read": "None", "Write": "None" },
						"Tags": [],
						"Serialization": { "CanLoad": true, "CanSave": true }
					}]
				}],
				"Enums": []
			}"#,
		)
		.unwrap();
		let database = build_database_from_api(api_dump, [0, 731, 0, 7310942]);
		let input_action = database
			.classes
			.get("InputAction")
			.expect("InputAction reflection is missing");
		let preferred_binding = input_action
			.properties
			.get("PreferredBinding")
			.expect("InputAction.PreferredBinding reflection is missing");

		assert!(matches!(
			preferred_binding.data_type,
			DataType::Value(rbx_dom_weak::types::VariantType::Ref)
		));
		assert!(matches!(
			preferred_binding.kind,
			PropertyKind::Canonical {
				serialization: PropertySerialization::DoesNotSerialize
			}
		));
		let icon = input_action
			.properties
			.get("Icon")
			.expect("InputAction.Icon reflection is missing");
		assert!(matches!(
			icon.data_type,
			DataType::Value(rbx_dom_weak::types::VariantType::ContentId)
		));
	}

	#[test]
	fn test_build_database_from_api_preserves_policy_rules() {
		let api_json = r#"{
			"Version": 1,
			"Classes": [{
				"Name": "Instance",
				"Superclass": "<<<ROOT>>>",
				"Tags": [],
				"Members": [{
					"MemberType": "Property",
					"Name": "Attributes",
					"ValueType": { "Category": "DataType", "Name": "BinaryString" },
					"Security": { "Read": "RobloxSecurity", "Write": "RobloxSecurity" },
					"Tags": [],
					"Serialization": { "CanLoad": false, "CanSave": false }
				}, {
					"MemberType": "Property",
					"Name": "Tags",
					"ValueType": { "Category": "DataType", "Name": "BinaryString" },
					"Security": { "Read": "RobloxSecurity", "Write": "RobloxSecurity" },
					"Tags": [],
					"Serialization": { "CanLoad": false, "CanSave": false }
				}]
			}, {
				"Name": "FormFactorPart",
				"Superclass": "Instance",
				"Tags": [],
				"Members": [{
					"MemberType": "Property",
					"Name": "FormFactor",
					"ValueType": { "Category": "Enum", "Name": "FormFactor" },
					"Security": { "Read": "None", "Write": "None" },
					"Tags": [],
					"Serialization": { "CanLoad": true, "CanSave": true }
				}, {
					"MemberType": "Property",
					"Name": "Formfactor",
					"ValueType": { "Category": "Enum", "Name": "FormFactor" },
					"Security": { "Read": "None", "Write": "None" },
					"Tags": [],
					"Serialization": { "CanLoad": true, "CanSave": true }
				}]
			}, {
				"Name": "StyleRule",
				"Superclass": "Instance",
				"Tags": [],
				"Members": [{
					"MemberType": "Property",
					"Name": "Properties",
					"ValueType": { "Category": "Primitive", "Name": "string" },
					"Security": { "Read": "None", "Write": "None" },
					"Tags": [],
					"Serialization": { "CanLoad": true, "CanSave": true },
					"Default": "test_default"
				}, {
					"MemberType": "Property",
					"Name": "PropertiesSerialize",
					"ValueType": { "Category": "DataType", "Name": "BinaryString" },
					"Security": { "Read": "RobloxSecurity", "Write": "RobloxSecurity" },
					"Tags": ["Hidden", "NotScriptable"],
					"Serialization": { "CanLoad": true, "CanSave": true }
				}]
			}],
			"Enums": [{
				"Name": "FormFactor",
				"Items": [{"Name": "Symmetric", "Value": 0}, {"Name": "Custom", "Value": 1}]
			}]
		}"#;

		let api_dump = parse_api_dump(api_json).unwrap();
		let database = build_database_from_api(api_dump, [0, 731, 0, 1]);

		let instance = database.classes.get("Instance").expect("Instance class");
		let attr = instance.properties.get("Attributes").expect("Attributes property");
		assert!(matches!(attr.data_type, DataType::Value(VariantType::Attributes)));
		assert!(matches!(attr.scriptability, Scriptability::Custom));

		let tags = instance.properties.get("Tags").expect("Tags property");
		assert!(matches!(tags.data_type, DataType::Value(VariantType::Tags)));
		assert!(matches!(tags.scriptability, Scriptability::Custom));

		let style_rule = database.classes.get("StyleRule").expect("StyleRule class");
		let props = style_rule.properties.get("Properties").expect("Properties property");
		assert!(matches!(
			props.kind,
			PropertyKind::Canonical {
				serialization: PropertySerialization::SerializesAs("PropertiesSerialize")
			}
		));
		assert_eq!(
			style_rule.default_properties.get("Properties"),
			Some(&rbx_dom_weak::types::Variant::String("test_default".to_string()))
		);

		let form_factor_part = database.classes.get("FormFactorPart").expect("FormFactorPart class");
		let formfactor_alias = form_factor_part.properties.get("Formfactor").expect("Formfactor alias");
		assert!(matches!(
			formfactor_alias.kind,
			PropertyKind::Alias {
				alias_for: "FormFactor"
			}
		));
	}
}
