use actix_msgpack::{MsgPack, MsgPackResponseBuilder};
use actix_web::{post, web, web::Data, HttpResponse, Responder};
use anyhow::Context;
use log::debug;
use rbx_dom_weak::{
	types::{
		Attributes, Axes, BinaryString, BrickColor, Color3, ColorSequence, ColorSequenceKeypoint, Content, ContentType,
		Enum, Faces, NetAssetRef, NumberSequence, NumberSequenceKeypoint, Ref, Region3, SecurityCapabilities,
		SharedString, Tags, UniqueId, Variant, VariantType, Vector3,
	},
	InstanceBuilder, Ustr, WeakDom,
};
use rbx_reflection::{DataType, PropertyKind, PropertySerialization};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use std::{
	collections::{HashMap, HashSet},
	sync::{Arc, Mutex, OnceLock},
};

use crate::{
	artifact_store::{decode_exact_raw, exact_raw_bytes},
	core::{
		queue::StudioRoute,
		snapshot::{AddedSnapshot, Snapshot},
		Core,
	},
	privileged_bridge::{
		Bridge, Capabilities, Changes, CreatedInstance, ManagedHierarchyAttachment, ManagedHierarchyStage,
		ManagedIdentityResolutions, PropertyBatchReads, PropertyRead, ReferenceBatchReads, RootApplyModelResponse,
		RootModel, Roots,
	},
	Properties,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AuthorizedRequest {
	client_id: u32,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestIdentityBootstrapResponse {
	authoritative: bool,
	source_instances: u32,
	digest: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PropertyRequest {
	client_id: u32,
	debug_id: String,
	property: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PropertyLookup {
	debug_id: String,
	property: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PropertyBatchRequest {
	client_id: u32,
	requests: Vec<PropertyLookup>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DefaultPropertiesRequest {
	client_id: u32,
	class_name: String,
	properties: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PropertyWriteRequest {
	client_id: u32,
	debug_id: String,
	property: String,
	value: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReferencePropertyWriteRequest {
	client_id: u32,
	debug_id: String,
	property: String,
	target_debug_id: Option<String>,
}

fn bridge_reference_write_payload(request: &ReferencePropertyWriteRequest) -> serde_json::Value {
	serde_json::json!({
		"debugId": request.debug_id,
		"property": request.property,
		"targetDebugId": request.target_debug_id,
	})
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PropertyCopyRequest {
	client_id: u32,
	source_debug_id: String,
	target_debug_id: String,
	property: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MaterializeRequest {
	client_id: u32,
	class_name: String,
	property: String,
	data_type: String,
	value: ByteBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MaterializedWriteRequest {
	client_id: u32,
	debug_id: String,
	class_name: String,
	property: String,
	data_type: String,
	value: ByteBuf,
}

#[derive(Debug, Serialize)]
struct MaterializedProperty {
	model: ByteBuf,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateRequest {
	client_id: u32,
	class_name: String,
	parent_debug_id: String,
	name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChangesRequest {
	client_id: u32,
	after: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedIdentityRequest {
	client_id: u32,
	request_id: String,
	#[serde(default)]
	source_ids: Vec<Ref>,
	#[serde(default)]
	debug_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedIdentityPollRequest {
	client_id: u32,
	request_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResolvedManagedIdentity {
	id: Ref,
	debug_id: String,
	marker_name: String,
	root_debug_id: String,
	root_id: Ref,
}

#[derive(Debug, Serialize)]
struct ResolvedManagedIdentities {
	pending: bool,
	identities: Vec<ResolvedManagedIdentity>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RootSnapshotsRequest {
	client_id: u32,
	#[serde(default)]
	source_generation: Option<String>,
	parent: Ref,
	debug_ids: Vec<String>,
	#[serde(default)]
	root_source_ids: HashMap<String, Ref>,
	#[serde(default)]
	source_instance_ids: HashMap<String, Ref>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApplyHiddenRootsRequest {
	client_id: u32,
	source_generation: String,
	#[serde(default)]
	preflight: bool,
	#[serde(default)]
	preflight_token: Option<String>,
	roots: Vec<ApplyHiddenRoot>,
	#[serde(default)]
	known_source_instances: Vec<KnownSourceInstance>,
	#[serde(default)]
	external_references: Vec<ApplyReferenceRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApplyHiddenRoot {
	debug_id: String,
	source_id: Ref,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct KnownSourceInstance {
	id: Ref,
	debug_id: String,
}

fn external_known_source_debug_ids(
	known_source_instances: Vec<KnownSourceInstance>,
	bundled_ids: &HashSet<Ref>,
) -> HashMap<String, String> {
	known_source_instances
		.into_iter()
		.filter(|instance| !bundled_ids.contains(&instance.id))
		.map(|instance| (instance.id.to_string(), instance.debug_id))
		.collect()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApplyReferenceRequest {
	owner_source_id: Ref,
	property: Ustr,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApplyHiddenRootsResponse {
	source_instances: Vec<SourceInstance>,
	#[serde(skip_serializing_if = "Option::is_none")]
	preflight_token: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RootApplyReference {
	owner_source_id: String,
	property: String,
	target_source_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct RootSnapshots {
	roots: Vec<RootSnapshot>,
	#[serde(rename = "rootDebugIds")]
	root_debug_ids: Vec<String>,
	#[serde(rename = "sourceInstances")]
	source_instances: Vec<SourceInstance>,
	#[serde(rename = "externalRefs")]
	external_refs: Vec<ExternalRef>,
	#[serde(rename = "referencePatches")]
	reference_patches: Vec<ReferencePatch>,
	#[serde(rename = "changeSequence")]
	change_sequence: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceInstance {
	id: Ref,
	debug_id: String,
	root_debug_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RootSnapshot {
	debug_id: String,
	snapshot: AddedSnapshot,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExternalRef {
	id: Ref,
	debug_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReferencePatch {
	debug_id: String,
	property: Ustr,
	value: Variant,
}

#[derive(Debug, Clone)]
pub(crate) struct SerializedPropertyCandidate {
	canonical_name: Ustr,
	request_name: String,
	data_type: VariantType,
	enum_name: Option<String>,
}

#[derive(Debug, Default)]
struct LiveRootProperties {
	name: String,
	raw_name: Option<ByteBuf>,
	properties: Properties,
	carrier_targets: HashMap<Ref, String>,
}

#[derive(Debug, Clone)]
struct SourceRootTemplate {
	id: Ref,
	class: Ustr,
	properties: Properties,
	descendants: Vec<SourceDescendantTemplate>,
}

#[derive(Debug, Clone)]
struct SourceDescendantTemplate {
	id: Ref,
	path: Vec<usize>,
	class: Ustr,
	unique_id: Option<UniqueId>,
}

#[derive(Debug, Clone)]
enum ReferenceOwner {
	Root(String),
	Model(Ref),
}

#[derive(Debug, Clone)]
struct ReferenceRequest {
	owner: ReferenceOwner,
	canonical_name: Ustr,
	lookup: PropertyLookup,
}

fn model_reference_target(value: &Variant) -> Option<Ref> {
	match value {
		Variant::Ref(target) if target.is_some() => Some(*target),
		Variant::Content(content) => match content.value() {
			ContentType::Object(target) if target.is_some() => Some(*target),
			_ => None,
		},
		_ => None,
	}
}

fn is_model_reference(value: &Variant) -> bool {
	matches!(value, Variant::Ref(_))
		|| matches!(value, Variant::Content(content) if matches!(content.value(), ContentType::Object(_)))
}

fn include_native_model_property(class: &str, property: &str) -> bool {
	let database = crate::util::get_reflection_database();
	let mut owner = class;
	while let Some(descriptor) = database.classes.get(owner) {
		if descriptor.properties.contains_key(property) {
			break;
		}
		let Some(superclass) = descriptor.superclass else {
			break;
		};
		owner = superclass;
	}
	!matches!(
		(owner, property),
		("PartOperation", "TriangleCount") | ("Workspace", "CollisionGroups")
	)
}

fn studio_compat_superclass(class: &str) -> Option<&'static str> {
	match class {
		// Studio 0.729 added these classes after the published 0.728 Rust
		// reflection database. None declares direct serialized properties, so
		// their exact compatibility surface is their Instance inheritance.
		"DeviceDisplayService" | "DisplayWakeLock" | "PopLatencyService" => Some("Instance"),
		_ => None,
	}
}

pub(crate) fn serialized_property_candidates(class: &str) -> anyhow::Result<Vec<SerializedPropertyCandidate>> {
	let database = crate::util::get_reflection_database();
	let mut current = if database.classes.contains_key(class) {
		class
	} else {
		studio_compat_superclass(class).unwrap_or(class)
	};
	let mut seen = HashSet::new();
	let mut candidates = Vec::new();
	loop {
		let descriptor = database
			.classes
			.get(current)
			.with_context(|| format!("reflection database has no class {current}"))?;
		for (name, property) in &descriptor.properties {
			if !seen.insert(*name) || *name == "Parent" || !include_native_model_property(class, name) {
				continue;
			}
			let PropertyKind::Canonical { serialization } = &property.kind else {
				continue;
			};
			let request_name = match serialization {
				PropertySerialization::Serializes => (*name).to_owned(),
				PropertySerialization::SerializesAs(serialized_name) => (*serialized_name).to_owned(),
				_ => continue,
			};
			candidates.push(SerializedPropertyCandidate {
				canonical_name: Ustr::from(name),
				request_name,
				data_type: property.data_type.ty(),
				enum_name: match &property.data_type {
					DataType::Enum(name) => Some((*name).to_owned()),
					_ => None,
				},
			});
		}
		let Some(superclass) = descriptor.superclass else {
			break;
		};
		current = superclass;
	}
	// The plugin pins Studio reflection 0.729 while the newest published Rust
	// database is 0.728. Keep its one additional non-settings serialized
	// property aligned until rbx_reflection_database publishes 0.729.
	if class == "VoiceChatService" && seen.insert("EnableVoiceVolumeControls") {
		candidates.push(SerializedPropertyCandidate {
			canonical_name: Ustr::from("EnableVoiceVolumeControls"),
			request_name: "EnableVoiceVolumeControls".to_owned(),
			data_type: VariantType::Enum,
			enum_name: Some("RolloutState".to_owned()),
		});
	}
	candidates.sort_unstable_by_key(|candidate| candidate.canonical_name);
	Ok(candidates)
}

fn capture_shell_property_candidates(class: &str) -> anyhow::Result<Vec<SerializedPropertyCandidate>> {
	let mut by_request_name = HashMap::<String, SerializedPropertyCandidate>::new();
	for candidate in serialized_property_candidates(class)? {
		// Shell structure and durable identity travel through the hierarchy and
		// reconciliation contracts, never as mutable manifest properties. In
		// particular, Studio assigns process-local identity values to service
		// shells; allowing them into the capture would create a false update on
		// every launch.
		if matches!(candidate.canonical_name.as_str(), "Name" | "HistoryId" | "UniqueId") {
			continue;
		}
		let Some(existing) = by_request_name.get_mut(&candidate.request_name) else {
			by_request_name.insert(candidate.request_name.clone(), candidate);
			continue;
		};
		let existing_is_direct = existing.canonical_name.as_str() == existing.request_name;
		let candidate_is_direct = candidate.canonical_name.as_str() == candidate.request_name;
		match (existing_is_direct, candidate_is_direct) {
			// rbx_binary resolves an incoming serialized name by direct descriptor
			// lookup before considering SerializesAs aliases. Match that precedence.
			(false, true) => *existing = candidate,
			(true, false) => {}
			_ => anyhow::ensure!(
				existing.canonical_name == candidate.canonical_name
					&& existing.data_type == candidate.data_type
					&& existing.enum_name == candidate.enum_name,
				"capture shell schema maps {class}.{} ambiguously to {} {:?} and {} {:?}",
				candidate.request_name,
				existing.canonical_name,
				existing.data_type,
				candidate.canonical_name,
				candidate.data_type
			),
		}
	}
	let mut candidates = by_request_name.into_values().collect::<Vec<_>>();
	candidates.sort_unstable_by_key(|candidate| candidate.canonical_name);
	Ok(candidates)
}

pub(crate) fn capture_shell_property_names(class: &str) -> anyhow::Result<Vec<String>> {
	Ok(capture_shell_property_candidates(class)?
		.into_iter()
		.map(|candidate| candidate.request_name)
		.collect())
}

/// Return every service class known to the pinned reflection schema, plus the
/// DataModel shell itself. Studio may lazily create a service after the initial
/// `/v1/roots` handshake, so that point-in-time root set is not a complete
/// shell-schema authority.
pub(crate) fn capture_shell_class_names() -> Vec<String> {
	let mut classes = crate::util::get_reflection_database()
		.classes
		.iter()
		.filter(|(class, descriptor)| {
			**class == "DataModel" || descriptor.tags.contains(&rbx_reflection::ClassTag::Service)
		})
		.map(|(class, _)| (*class).to_owned())
		.collect::<Vec<_>>();
	classes.sort_unstable();
	classes
}

pub(crate) fn capture_shell_property_identity(class: &str, request_name: &str) -> anyhow::Result<(Ustr, VariantType)> {
	capture_shell_property_candidates(class)?
		.into_iter()
		.find(|candidate| candidate.request_name == request_name)
		.map(|candidate| (candidate.canonical_name, candidate.data_type))
		.with_context(|| format!("capture shell schema has no property {class}.{request_name}"))
}

/// Check the engine reflection spelling carried by RML against the canonical
/// rbx_types value selected by Carbon's pinned reflection database. Roblox uses
/// C++ names for a small number of primitive and legacy ABI types (for example,
/// `bool` and `CoordinateFrame`), so comparing `VariantType`'s Debug spelling
/// is not a valid schema check. Keep every accepted alias explicit: an unknown
/// engine spelling must fail the capture instead of being accepted by a broad
/// case-fold or suffix rule.
fn capture_shell_native_type_matches(data_type: VariantType, native_type: &str, enum_name: Option<&str>) -> bool {
	match data_type {
		VariantType::Axes => native_type == "Axes",
		VariantType::BinaryString => native_type == "BinaryString",
		VariantType::Bool => matches!(native_type, "Bool" | "bool"),
		VariantType::BrickColor => native_type == "BrickColor",
		VariantType::CFrame => matches!(native_type, "CFrame" | "CoordinateFrame"),
		VariantType::Color3 => native_type == "Color3",
		VariantType::Color3uint8 => native_type == "Color3uint8",
		VariantType::ColorSequence => native_type == "ColorSequence",
		VariantType::ContentId => native_type == "ContentId",
		VariantType::Enum => enum_name.is_some_and(|name| native_type == name),
		VariantType::Faces => native_type == "Faces",
		VariantType::Float32 => matches!(native_type, "Float32" | "float"),
		VariantType::Float64 => matches!(native_type, "Float64" | "double"),
		VariantType::Int32 => matches!(native_type, "Int32" | "int"),
		VariantType::Int64 => matches!(native_type, "Int64" | "int64" | "int64_t"),
		VariantType::NumberRange => native_type == "NumberRange",
		VariantType::NumberSequence => native_type == "NumberSequence",
		VariantType::PhysicalProperties => native_type == "PhysicalProperties",
		VariantType::Ray => native_type == "Ray",
		VariantType::Rect => matches!(native_type, "Rect" | "Rect2D"),
		VariantType::Ref => matches!(native_type, "Ref" | "Object" | "Instance"),
		VariantType::Region3 => native_type == "Region3",
		VariantType::Region3int16 => native_type == "Region3int16",
		VariantType::SharedString => native_type == "SharedString",
		VariantType::String => matches!(native_type, "String" | "string"),
		VariantType::UDim => native_type == "UDim",
		VariantType::UDim2 => native_type == "UDim2",
		VariantType::Vector2 => native_type == "Vector2",
		VariantType::Vector2int16 => native_type == "Vector2int16",
		VariantType::Vector3 => native_type == "Vector3",
		VariantType::Vector3int16 => native_type == "Vector3int16",
		VariantType::OptionalCFrame => matches!(native_type, "OptionalCFrame" | "OptionalCoordinateFrame"),
		// These logical values are persisted through BinaryString-backed engine
		// descriptors. RML returns the exact serialized byte payload, which the
		// canonical decoder then interprets as Tags or Attributes respectively.
		VariantType::Tags => matches!(native_type, "Tags" | "BinaryString" | "SharedString"),
		VariantType::Attributes => matches!(native_type, "Attributes" | "BinaryString"),
		VariantType::Font => native_type == "Font",
		VariantType::UniqueId => native_type == "UniqueId",
		VariantType::MaterialColors => native_type == "MaterialColors",
		VariantType::SecurityCapabilities => native_type == "SecurityCapabilities",
		VariantType::EnumItem => native_type == "EnumItem",
		VariantType::Content => matches!(native_type, "Content" | "ContentId"),
		VariantType::NetAssetRef => native_type == "NetAssetRef",
		_ => false,
	}
}

pub(crate) fn capture_shell_property_type_matches(
	class: &str,
	request_name: &str,
	data_type: VariantType,
	native_type: &str,
) -> anyhow::Result<bool> {
	let candidate = capture_shell_property_candidates(class)?
		.into_iter()
		.find(|candidate| candidate.request_name == request_name)
		.with_context(|| format!("capture shell schema has no property {class}.{request_name}"))?;
	Ok(candidate.data_type == data_type
		&& capture_shell_native_type_matches(data_type, native_type, candidate.enum_name.as_deref()))
}

pub(crate) fn decode_capture_shell_property(data_type: VariantType, bytes: &[u8]) -> anyhow::Result<Variant> {
	decode_privileged_variant(data_type, bytes)
}

fn decode_privileged_variant(data_type: VariantType, bytes: &[u8]) -> anyhow::Result<Variant> {
	fn array<const N: usize>(bytes: &[u8], offset: usize) -> anyhow::Result<[u8; N]> {
		bytes
			.get(offset..offset + N)
			.context("native raw property is truncated")?
			.try_into()
			.context("native raw property has an invalid width")
	}
	fn f32_at(bytes: &[u8], offset: usize) -> anyhow::Result<f32> {
		Ok(f32::from_le_bytes(array(bytes, offset)?))
	}
	fn i32_at(bytes: &[u8], offset: usize) -> anyhow::Result<i32> {
		Ok(i32::from_le_bytes(array(bytes, offset)?))
	}
	fn vector3_at(bytes: &[u8], offset: usize) -> anyhow::Result<Vector3> {
		Ok(Vector3::new(
			f32_at(bytes, offset)?,
			f32_at(bytes, offset + 4)?,
			f32_at(bytes, offset + 8)?,
		))
	}
	fn sequence_count(bytes: &[u8], stride: usize) -> anyhow::Result<usize> {
		let count = i32_at(bytes, 0)?;
		anyhow::ensure!(count >= 0, "native raw sequence has a negative keypoint count");
		let count = count as usize;
		let expected = count
			.checked_mul(stride)
			.and_then(|size| size.checked_add(4))
			.context("native raw sequence size overflows")?;
		anyhow::ensure!(
			bytes.len() == expected,
			"native raw sequence has length {}, expected {expected}",
			bytes.len()
		);
		Ok(count)
	}
	fn bit_mask(bytes: &[u8], kind: &str) -> anyhow::Result<u8> {
		anyhow::ensure!(
			bytes.len() == 4,
			"native raw {kind} has length {}, expected 4",
			bytes.len()
		);
		let mask = i32_at(bytes, 0)?;
		u8::try_from(mask).with_context(|| format!("native raw {kind} mask {mask} is outside u8"))
	}

	let text = || std::str::from_utf8(bytes).context("privileged scalar is not UTF-8");
	let value = match data_type {
		VariantType::Attributes => {
			Variant::Attributes(Attributes::from_reader(bytes).context("privileged Attributes payload is invalid")?)
		}
		VariantType::Tags => Variant::Tags(Tags::decode(bytes).context("privileged Tags payload is invalid")?),
		VariantType::BinaryString => Variant::BinaryString(BinaryString::from(bytes)),
		VariantType::SharedString => Variant::SharedString(SharedString::new(bytes.to_vec())),
		VariantType::NetAssetRef => Variant::NetAssetRef(NetAssetRef::new(bytes.to_vec())),
		VariantType::String => match String::from_utf8(bytes.to_vec()) {
			Ok(value) => Variant::String(value),
			Err(_) => Variant::BinaryString(BinaryString::from(bytes)),
		},
		VariantType::Content => Variant::Content(Content::from_uri(text()?)),
		VariantType::UniqueId if bytes.len() == 16 => Variant::UniqueId(UniqueId::new(
			u32::from_be_bytes(bytes[12..16].try_into().unwrap()),
			u32::from_be_bytes(bytes[8..12].try_into().unwrap()),
			i64::from_be_bytes(bytes[0..8].try_into().unwrap()),
		)),
		VariantType::Bool => match text()? {
			"true" | "1" => Variant::Bool(true),
			"false" | "0" => Variant::Bool(false),
			_ => anyhow::bail!("privileged Bool payload is invalid"),
		},
		VariantType::Int32 => Variant::Int32(text()?.parse().context("privileged Int32 payload is invalid")?),
		VariantType::Enum => {
			let value: i32 = text()?.parse().context("privileged Enum payload is invalid")?;
			Variant::Enum(Enum::from_u32(value as u32))
		}
		VariantType::SecurityCapabilities => Variant::SecurityCapabilities(SecurityCapabilities::from_bits(
			text()?
				.parse()
				.context("privileged SecurityCapabilities payload is invalid")?,
		)),
		VariantType::Axes => Variant::Axes(
			Axes::from_bits(bit_mask(bytes, "Axes")?).context("native raw Axes contains unknown flag bits")?,
		),
		VariantType::Faces => Variant::Faces(
			Faces::from_bits(bit_mask(bytes, "Faces")?).context("native raw Faces contains unknown flag bits")?,
		),
		VariantType::BrickColor if bytes.len() == 4 => {
			let number = u16::try_from(i32_at(bytes, 0)?).context("native raw BrickColor number is outside u16")?;
			Variant::BrickColor(
				BrickColor::from_number(number).context("native raw BrickColor number is not in Roblox's palette")?,
			)
		}
		VariantType::Region3 if bytes.len() == 60 => {
			// The engine stores Region3 as an axis-aligned CFrame followed by Size,
			// while rbx_types stores its authored minimum and maximum corners.
			let center = vector3_at(bytes, 36)?;
			let size = vector3_at(bytes, 48)?;
			let half = Vector3::new(size.x * 0.5, size.y * 0.5, size.z * 0.5);
			Variant::Region3(Region3::new(
				Vector3::new(center.x - half.x, center.y - half.y, center.z - half.z),
				Vector3::new(center.x + half.x, center.y + half.y, center.z + half.z),
			))
		}
		VariantType::NumberSequence => {
			let count = sequence_count(bytes, 12)?;
			let mut keypoints = Vec::with_capacity(count);
			for index in 0..count {
				let offset = 4 + index * 12;
				keypoints.push(NumberSequenceKeypoint::new(
					f32_at(bytes, offset)?,
					f32_at(bytes, offset + 4)?,
					f32_at(bytes, offset + 8)?,
				));
			}
			Variant::NumberSequence(NumberSequence { keypoints })
		}
		VariantType::ColorSequence => {
			let count = sequence_count(bytes, 20)?;
			let mut keypoints = Vec::with_capacity(count);
			for index in 0..count {
				let offset = 4 + index * 20;
				keypoints.push(ColorSequenceKeypoint::new(
					f32_at(bytes, offset)?,
					Color3::new(
						f32_at(bytes, offset + 4)?,
						f32_at(bytes, offset + 8)?,
						f32_at(bytes, offset + 12)?,
					),
				));
				// offset + 16 is engine-only state with no authored equivalent.
			}
			Variant::ColorSequence(ColorSequence { keypoints })
		}
		VariantType::Int64
		| VariantType::Float32
		| VariantType::Float64
		| VariantType::CFrame
		| VariantType::Color3
		| VariantType::OptionalCFrame
		| VariantType::NumberRange
		| VariantType::Ray
		| VariantType::Rect
		| VariantType::UDim
		| VariantType::UDim2
		| VariantType::Vector2
		| VariantType::Vector3
		| VariantType::Vector3int16 => decode_exact_raw(data_type, bytes)?,
		_ => anyhow::bail!("unsupported live hidden-root property type {data_type:?}"),
	};
	Ok(value)
}

fn decode_serialized_property_model(model: &str) -> anyhow::Result<WeakDom> {
	let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, model)
		.context("RML bridge returned an invalid serialized-property model")?;
	let model_len = bytes.len();
	debug!("Decoding RML serialized-property carrier model ({model_len} bytes)");
	rbx_binary::from_reader(std::io::Cursor::new(bytes))
		.map_err(|error| anyhow::anyhow!("failed to decode RML serialized-property model ({model_len} bytes): {error}"))
}

fn serialized_property_from_model(dom: &WeakDom, root_debug_id: &str, property: &str) -> anyhow::Result<Variant> {
	let wrapper = dom
		.root()
		.children()
		.iter()
		.filter_map(|id| dom.get_by_ref(*id))
		.find(|instance| instance.class.as_str() == "Folder" && instance.name == root_debug_id)
		.context("RML serialized-property model omitted its identity wrapper")?;
	anyhow::ensure!(
		wrapper.children().len() == 1,
		"RML serialized-property identity wrapper has the wrong number of children"
	);
	let root = wrapper.children()[0];
	let instance = dom
		.get_by_ref(root)
		.context("RML serialized-property model root is missing")?;
	let request_name = Ustr::from(property);
	let canonical_name = serialized_property_candidates(instance.class.as_str())?
		.into_iter()
		.find(|candidate| candidate.request_name == property)
		.map(|candidate| candidate.canonical_name);
	let value = instance
		.properties
		.get(&request_name)
		.or_else(|| canonical_name.and_then(|name| instance.properties.get(&name)))
		.with_context(|| format!("RML serialized-property model omitted {property}"))?;
	Ok(value.clone())
}

fn decode_serialized_property(model: &str, root_debug_id: &str, property: &str) -> anyhow::Result<Variant> {
	let dom = decode_serialized_property_model(model)?;
	serialized_property_from_model(&dom, root_debug_id, property)
}

fn serialized_property_bytes(value: &Variant) -> anyhow::Result<Vec<u8>> {
	if let Some(raw) = exact_raw_bytes(value) {
		return Ok(raw);
	}
	match value {
		Variant::BinaryString(value) => {
			let bytes: &[u8] = value.as_ref();
			Ok(bytes.to_vec())
		}
		Variant::NetAssetRef(value) => Ok(value.data().to_vec()),
		Variant::UniqueId(value) => {
			let mut bytes = Vec::with_capacity(16);
			bytes.extend_from_slice(&value.random().to_be_bytes());
			bytes.extend_from_slice(&value.time().to_be_bytes());
			bytes.extend_from_slice(&value.index().to_be_bytes());
			Ok(bytes)
		}
		Variant::Content(content) => match content.value() {
			ContentType::None => Ok(Vec::new()),
			ContentType::Uri(uri) => Ok(uri.as_bytes().to_vec()),
			ContentType::Object(_) => {
				anyhow::bail!("Content.Object cannot be represented by the scalar property endpoint")
			}
			_ => anyhow::bail!("RML serialized-property model returned an unknown Content representation"),
		},
		other => anyhow::bail!(
			"RML serialized-property model returned unsupported type {:?}",
			other.ty()
		),
	}
}

fn exact_variant_type(data_type: &str) -> Option<VariantType> {
	match data_type {
		"Int64" => Some(VariantType::Int64),
		"Float32" => Some(VariantType::Float32),
		"Float64" => Some(VariantType::Float64),
		"CFrame" => Some(VariantType::CFrame),
		"Color3" => Some(VariantType::Color3),
		"ColorSequence" => Some(VariantType::ColorSequence),
		"OptionalCFrame" => Some(VariantType::OptionalCFrame),
		"NumberRange" => Some(VariantType::NumberRange),
		"NumberSequence" => Some(VariantType::NumberSequence),
		"PhysicalProperties" => Some(VariantType::PhysicalProperties),
		"Ray" => Some(VariantType::Ray),
		"Rect" => Some(VariantType::Rect),
		"Region3" => Some(VariantType::Region3),
		"UDim" => Some(VariantType::UDim),
		"UDim2" => Some(VariantType::UDim2),
		"Vector2" => Some(VariantType::Vector2),
		"Vector3" => Some(VariantType::Vector3),
		"Vector3int16" => Some(VariantType::Vector3int16),
		_ => None,
	}
}

fn is_persistent_read_only_property(class_name: &str, property: &str) -> bool {
	matches!(
		(class_name, property),
		("Chat", "LoadDefaultChat")
			| ("HttpService", "HttpEnabled")
			| ("Lighting", "LightingStyle")
			| ("Lighting", "PrioritizeLightingQuality")
			| ("MeshPart", "HasJointOffset")
			| ("MeshPart", "HasSkinnedMesh")
			| ("MeshPart", "JointOffset")
			| ("MeshPart", "MeshContent")
			| ("PackageLink", "DefaultName")
			| ("PackageLink", "PackageContent")
			| ("Players", "MaxPlayers")
			| ("Players", "PreferredPlayers")
			| ("StarterPlayer", "AllowCustomAnimations")
			| ("TextChatService", "ChatVersion")
	)
}

fn materialized_property_value(data_type: &str, bytes: &[u8]) -> anyhow::Result<Variant> {
	if let Some(data_type) = exact_variant_type(data_type) {
		return decode_exact_raw(data_type, bytes);
	}
	match data_type {
		"Bool" => decode_privileged_variant(VariantType::Bool, bytes),
		"Int32" => decode_privileged_variant(VariantType::Int32, bytes),
		"Enum" => decode_privileged_variant(VariantType::Enum, bytes),
		"String" => decode_privileged_variant(VariantType::String, bytes),
		"Content" => match bytes.split_first() {
			Some((0, [])) => Ok(Variant::Content(Content::none())),
			Some((1, uri)) => Ok(Variant::Content(Content::from_uri(
				std::str::from_utf8(uri).context("materialized Content URI is not UTF-8")?,
			))),
			Some((2, _)) => {
				anyhow::bail!("Content.Object requires full connected-graph materialization")
			}
			_ => anyhow::bail!("materialized Content requires a None or URI wire tag"),
		},
		"SharedString" => Ok(Variant::SharedString(SharedString::new(bytes.to_vec()))),
		"NetAssetRef" => Ok(Variant::NetAssetRef(NetAssetRef::new(bytes.to_vec()))),
		"UniqueId" => {
			anyhow::ensure!(bytes.len() == 16, "UniqueId materialization requires exactly 16 bytes");
			let random = i64::from_be_bytes(bytes[0..8].try_into().unwrap());
			let time = u32::from_be_bytes(bytes[8..12].try_into().unwrap());
			let index = u32::from_be_bytes(bytes[12..16].try_into().unwrap());
			Ok(Variant::UniqueId(UniqueId::new(index, time, random)))
		}
		_ => anyhow::bail!("unsupported materialized property type {data_type}"),
	}
}

fn materialized_property_model(
	class_name: &str,
	property: &str,
	data_type: &str,
	bytes: &[u8],
) -> anyhow::Result<Vec<u8>> {
	if matches!(data_type, "Bool" | "Int32" | "Enum" | "String" | "Content") {
		anyhow::ensure!(
			is_persistent_read_only_property(class_name, property),
			"scalar materialization is not allowed for {class_name}.{property}"
		);
	}
	let value = materialized_property_value(data_type, bytes)?;
	let mut dom = WeakDom::new(InstanceBuilder::new("Folder").with_name("__CarbonMaterializationRoot"));
	let root = dom.root_ref();
	let materialized = dom.insert(
		root,
		InstanceBuilder::new(class_name)
			.with_name("__CarbonMaterializedProperty")
			.with_property(property, value),
	);
	let mut model = Vec::new();
	rbx_binary::to_writer(&mut model, &dom, &[materialized])
		.context("failed to encode privileged binary property materialization")?;
	Ok(model)
}

fn read_property_batch(
	bridge: &Bridge,
	requests: &[PropertyLookup],
) -> anyhow::Result<Vec<crate::privileged_bridge::PropertyBatchRead>> {
	let mut output = Vec::with_capacity(requests.len());
	for chunk in requests.chunks(4096) {
		let mut response =
			bridge.post::<_, PropertyBatchReads>("v1/properties/read", &serde_json::json!({ "requests": chunk }))?;
		anyhow::ensure!(
			response.values.len() == chunk.len(),
			"RML serialized property batch returned the wrong number of values"
		);
		let serialized_model = if response.values.iter().any(|value| value.model_root_debug_id.is_some()) {
			Some(decode_serialized_property_model(
				response
					.model
					.as_deref()
					.context("RML serialized property batch omitted its model")?,
			)?)
		} else {
			None
		};
		for (value, lookup) in response.values.iter_mut().zip(chunk) {
			if let Some(root_debug_id) = value.model_root_debug_id.take() {
				value.serialized_value = Some(serialized_property_from_model(
					serialized_model
						.as_ref()
						.context("RML serialized property batch model was not decoded")?,
					&root_debug_id,
					&lookup.property,
				)?);
			}
		}
		output.extend(response.values);
	}
	Ok(output)
}

fn property_carrier_debug_ids(
	root_entries: &[crate::privileged_bridge::Root],
	requested: &HashSet<String>,
) -> anyhow::Result<Vec<String>> {
	root_entries
		.iter()
		.filter(|root| requested.contains(&root.debug_id))
		.filter_map(|root| match serialized_property_candidates(&root.class_name) {
			Ok(candidates)
				if candidates
					.iter()
					.any(|candidate| candidate.data_type == VariantType::Content) =>
			{
				Some(Ok(root.debug_id.clone()))
			}
			Ok(_) => None,
			Err(error) => Some(Err(error)),
		})
		.collect()
}

fn read_live_root_properties(
	bridge: &Bridge,
	request: &RootSnapshotsRequest,
	response: &RootModel,
	dom: &WeakDom,
) -> anyhow::Result<HashMap<String, LiveRootProperties>> {
	let requested: HashSet<_> = request.debug_ids.iter().map(String::as_str).collect();
	let mut lookups = Vec::new();
	let mut pending_candidates = Vec::new();
	let mut root_properties = HashMap::new();
	for root in response
		.roots
		.iter()
		.filter(|root| requested.contains(root.debug_id.as_str()))
	{
		let root_candidates = serialized_property_candidates(&root.class_name)?;
		let carrier = root_candidates
			.iter()
			.any(|candidate| candidate.data_type == VariantType::Content)
			.then(|| -> anyhow::Result<_> {
				let carrier_ref = root_property_carrier(dom, response, &root.debug_id)?;
				let carrier_instance_refs = dom
					.descendants_of(carrier_ref)
					.map(|instance| instance.referent())
					.collect::<Vec<_>>();
				let carrier_debug_ids = response
					.root_property_carrier_instance_debug_ids
					.get(&root.debug_id)
					.with_context(|| {
						format!(
							"RML hidden-root model omitted property carrier instance identities for {}",
							root.debug_id
						)
					})?;
				anyhow::ensure!(
					carrier_instance_refs.len() == carrier_debug_ids.len(),
					"RML hidden-root property carrier instance identity count does not match its hierarchy"
				);
				let targets: HashMap<Ref, String> = carrier_instance_refs
					.into_iter()
					.zip(carrier_debug_ids.iter().cloned())
					.collect();
				Ok((carrier_ref, targets))
			})
			.transpose()?;
		root_properties.insert(
			root.debug_id.clone(),
			LiveRootProperties {
				name: root.name.clone(),
				carrier_targets: carrier.as_ref().map(|(_, targets)| targets.clone()).unwrap_or_default(),
				..Default::default()
			},
		);
		for candidate in root_candidates {
			if candidate.data_type == VariantType::Ref {
				continue;
			}
			if candidate.data_type == VariantType::Content {
				let carrier_ref = carrier
					.as_ref()
					.map(|(carrier_ref, _)| *carrier_ref)
					.context("hidden-root Content property has no carrier")?;
				let carrier = dom
					.get_by_ref(carrier_ref)
					.context("hidden-root property carrier is missing")?;
				let value = carrier
					.properties
					.get(&candidate.canonical_name)
					.or_else(|| carrier.properties.get(&Ustr::from(&candidate.request_name)))
					.with_context(|| {
						format!(
							"hidden-root property carrier omitted {}.{}",
							root.class_name, candidate.canonical_name
						)
					})?;
				anyhow::ensure!(
					matches!(value, Variant::Content(_)),
					"hidden-root property carrier returned the wrong type for {}.{}",
					root.class_name,
					candidate.canonical_name
				);
				root_properties
					.get_mut(&root.debug_id)
					.context("live hidden-root property has no root")?
					.properties
					.insert(candidate.canonical_name, value.clone());
				continue;
			}
			lookups.push(PropertyLookup {
				debug_id: root.debug_id.clone(),
				property: candidate.request_name.clone(),
			});
			pending_candidates.push((root.debug_id.clone(), candidate));
		}
	}
	let values = read_property_batch(bridge, &lookups)?;
	for ((root_debug_id, candidate), value) in pending_candidates.into_iter().zip(values) {
		if let Some(error) = value.error {
			anyhow::bail!(
				"failed to read live hidden-root property {}.{}: {error}",
				root_debug_id,
				candidate.canonical_name
			);
		}
		let root = root_properties
			.get_mut(&root_debug_id)
			.context("live hidden-root property has no root")?;
		if let Some(serialized) = value.serialized_value {
			root.properties.insert(candidate.canonical_name, serialized);
			continue;
		}
		let encoded = value.value.context("RML hidden-root property read returned no value")?;
		let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
			.context("RML hidden-root property read returned invalid base64")?;
		if candidate.canonical_name.as_str() == "Name" {
			match String::from_utf8(bytes) {
				Ok(name) => root.name = name,
				Err(error) => {
					root.name = String::from_utf8_lossy(error.as_bytes()).into_owned();
					root.raw_name = Some(ByteBuf::from(error.into_bytes()));
				}
			}
		} else {
			root.properties.insert(
				candidate.canonical_name,
				decode_privileged_variant(candidate.data_type, &bytes).with_context(|| {
					format!(
						"failed to decode live hidden-root property {}.{}",
						root_debug_id, candidate.canonical_name
					)
				})?,
			);
		}
	}
	Ok(root_properties)
}

fn decode_root_model(response: &RootModel) -> anyhow::Result<WeakDom> {
	let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &response.model)
		.context("RML bridge returned an invalid root model")?;
	rbx_binary::from_reader(std::io::Cursor::new(bytes)).context("failed to decode RML hidden-root model")
}

fn root_property_carrier_roots(dom: &WeakDom, response: &RootModel) -> anyhow::Result<HashSet<Ref>> {
	let expected = response
		.root_property_carriers
		.values()
		.map(String::as_str)
		.collect::<HashSet<_>>();
	let carriers = dom
		.root()
		.children()
		.iter()
		.copied()
		.filter(|id| {
			dom.get_by_ref(*id).is_some_and(|instance| {
				instance.class.as_str() == "Folder" && expected.contains(instance.name.as_str())
			})
		})
		.collect::<HashSet<_>>();
	anyhow::ensure!(
		carriers.len() == expected.len(),
		"RML hidden-root model property carrier count does not match its identities"
	);
	Ok(carriers)
}

fn root_property_carrier(dom: &WeakDom, response: &RootModel, debug_id: &str) -> anyhow::Result<Ref> {
	let name = response
		.root_property_carriers
		.get(debug_id)
		.with_context(|| format!("RML hidden-root model omitted the property carrier for {debug_id}"))?;
	let mut matches = dom.root().children().iter().filter_map(|id| {
		let instance = dom.get_by_ref(*id)?;
		(instance.class.as_str() == "Folder" && instance.name == *name).then_some(instance)
	});
	let wrapper = matches
		.next()
		.with_context(|| format!("RML hidden-root model omitted property carrier {name}"))?;
	anyhow::ensure!(
		matches.next().is_none(),
		"RML hidden-root model duplicated property carrier {name}"
	);
	anyhow::ensure!(
		wrapper.children().len() == 1,
		"RML hidden-root property carrier {name} has the wrong number of children"
	);
	Ok(wrapper.children()[0])
}

fn serialized_model_roots(dom: &WeakDom, response: &RootModel) -> anyhow::Result<Vec<Ref>> {
	let carriers = root_property_carrier_roots(dom, response)?;
	Ok(dom
		.root()
		.children()
		.iter()
		.copied()
		.filter(|id| !carriers.contains(id))
		.collect())
}

fn serialized_model_instances(dom: &WeakDom, response: &RootModel) -> anyhow::Result<Vec<Ref>> {
	let excluded_roots = root_property_carrier_roots(dom, response)?;
	let excluded_ids = excluded_roots
		.into_iter()
		.flat_map(|root| dom.descendants_of(root).map(|instance| instance.referent()))
		.collect::<HashSet<_>>();
	Ok(dom
		.descendants()
		.skip(1)
		.map(|instance| instance.referent())
		.filter(|id| !excluded_ids.contains(id))
		.collect())
}

fn collect_reference_requests(
	request: &RootSnapshotsRequest,
	response: &RootModel,
	dom: &WeakDom,
) -> anyhow::Result<Vec<ReferenceRequest>> {
	let requested: HashSet<_> = request.debug_ids.iter().map(String::as_str).collect();
	let mut references = Vec::new();
	for root in response
		.roots
		.iter()
		.filter(|root| requested.contains(root.debug_id.as_str()))
	{
		for candidate in serialized_property_candidates(&root.class_name)? {
			if candidate.data_type != VariantType::Ref {
				continue;
			}
			references.push(ReferenceRequest {
				owner: ReferenceOwner::Root(root.debug_id.clone()),
				canonical_name: candidate.canonical_name,
				lookup: PropertyLookup {
					debug_id: root.debug_id.clone(),
					property: candidate.request_name,
				},
			});
		}
	}

	let model_instance_refs = serialized_model_instances(dom, response)?;
	anyhow::ensure!(
		model_instance_refs.len() == response.instance_debug_ids.len(),
		"RML hidden-root model instance identity count does not match its hierarchy"
	);
	for (instance_id, debug_id) in model_instance_refs.into_iter().zip(&response.instance_debug_ids) {
		let instance = dom
			.get_by_ref(instance_id)
			.context("serialized model instance is missing")?;
		let candidates = serialized_property_candidates(instance.class.as_str())?;
		for candidate in candidates {
			if candidate.data_type != VariantType::Ref {
				continue;
			}
			let serialized_value = instance.properties.get(&candidate.canonical_name);
			let intact_internal = matches!(
				serialized_value,
				Some(Variant::Ref(target)) if target.is_some() && dom.get_by_ref(*target).is_some()
			);
			if intact_internal {
				continue;
			}
			references.push(ReferenceRequest {
				owner: ReferenceOwner::Model(instance.referent()),
				canonical_name: candidate.canonical_name,
				lookup: PropertyLookup {
					debug_id: debug_id.clone(),
					property: candidate.request_name,
				},
			});
		}
	}
	Ok(references)
}

fn read_reference_targets(bridge: &Bridge, requests: &[ReferenceRequest]) -> anyhow::Result<Vec<Option<String>>> {
	let mut targets = Vec::with_capacity(requests.len());
	for chunk in requests.chunks(4096) {
		let lookups = chunk.iter().map(|request| &request.lookup).collect::<Vec<_>>();
		let response =
			bridge.post::<_, ReferenceBatchReads>("v1/references/read", &serde_json::json!({ "requests": lookups }))?;
		anyhow::ensure!(
			response.values.len() == chunk.len(),
			"RML reference batch returned the wrong number of values"
		);
		for (request, value) in chunk.iter().zip(response.values) {
			if let Some(error) = value.error {
				anyhow::bail!(
					"failed to read live serialized reference {}.{}: {error}",
					request.lookup.debug_id,
					request.canonical_name
				);
			}
			targets.push(value.target_debug_id);
		}
	}
	Ok(targets)
}

fn studio_route(core: &Core, client_id: u32) -> Result<StudioRoute, HttpResponse> {
	if !core.queue().is_subscribed(client_id) {
		return Err(HttpResponse::Unauthorized().finish());
	}
	core.queue()
		.studio_route(client_id)
		.ok_or_else(|| HttpResponse::ServiceUnavailable().body("Studio routing identity is unavailable"))
}

fn bridge_id(core: &Core, client_id: u32) -> Result<String, HttpResponse> {
	studio_route(core, client_id)?
		.bridge_id
		.filter(|bridge_id| !bridge_id.is_empty())
		.ok_or_else(|| HttpResponse::ServiceUnavailable().body("RML bridge is not bound to this Studio session"))
}

async fn bridge_call<R: serde::Serialize + Send + 'static>(
	bridge_id: String,
	call: impl FnOnce(Bridge) -> anyhow::Result<R> + Send + 'static,
) -> HttpResponse {
	match web::block(move || Bridge::discover(&bridge_id).and_then(call)).await {
		Ok(Ok(response)) => HttpResponse::Ok().msgpack(response),
		Ok(Err(error)) => HttpResponse::ServiceUnavailable().body(error.to_string()),
		Err(error) => HttpResponse::InternalServerError().body(error.to_string()),
	}
}

#[post("/privileged/capabilities")]
pub(crate) async fn capabilities(request: MsgPack<AuthorizedRequest>, core: Data<Arc<Core>>) -> impl Responder {
	let client_id = request.client_id;
	let route = match studio_route(&core, client_id) {
		Ok(route) => route,
		Err(response) => return response,
	};
	let managed_contract = if core.has_managed_worktree() {
		match core.managed_hierarchy_contract() {
			Ok(contract) => Some(contract),
			Err(error) => return HttpResponse::ServiceUnavailable().body(error.to_string()),
		}
	} else {
		None
	};
	let result = web::block(move || -> anyhow::Result<(Capabilities, String)> {
		let bridge = match route.bridge_id.as_deref() {
			Some(bridge_id) => Bridge::discover(bridge_id)?,
			None => Bridge::discover_studio(&route.studio_session_id, &route.instance_id)?,
		};
		let bridge_id = bridge.bridge_id().to_owned();
		let mut capabilities = bridge.get::<Capabilities>("v1/capabilities")?;
		anyhow::ensure!(
			capabilities.bridge_id == bridge_id,
			"RML capability response has the wrong bridge identity"
		);
		anyhow::ensure!(
			capabilities.capture_lease_protocol == crate::capture_provider::CAPTURE_ENVELOPE_VERSION,
			"RML capture lease protocol is incompatible with this Carbon server"
		);
		capabilities.studio_session_id = route.studio_session_id;
		capabilities.instance_id = route.instance_id;
		if capabilities.managed_hierarchy_attachment {
			if let Some(contract) = managed_contract {
				let staged = bridge.post_bytes::<ManagedHierarchyStage>(
					&format!("v1/managed/stage/{}", contract.contract_id),
					contract.payload,
				)?;
				anyhow::ensure!(
					staged.contract_id == contract.contract_id && staged.source_instances == contract.source_instances,
					"RML staged the wrong managed hierarchy contract"
				);
			}
		}
		Ok((capabilities, bridge_id))
	})
	.await;

	match result {
		Ok(Ok((capabilities, bridge_id))) => match core.queue().bind_studio_bridge(client_id, &bridge_id) {
			Ok(()) => {
				if let Err(error) = core
					.queue()
					.set_manifest_identities_authoritative(client_id, capabilities.manifest_identities_authoritative)
				{
					return HttpResponse::ServiceUnavailable().body(error.to_string());
				}
				debug!(
					"Bound Carbon Studio session {} ({}) to RML bridge {} in process {}",
					capabilities.studio_session_id, capabilities.instance_id, bridge_id, capabilities.process_id
				);
				HttpResponse::Ok().msgpack(capabilities)
			}
			Err(error) => HttpResponse::ServiceUnavailable().body(error.to_string()),
		},
		Ok(Err(error)) => HttpResponse::ServiceUnavailable().body(error.to_string()),
		Err(error) => HttpResponse::InternalServerError().body(error.to_string()),
	}
}

#[post("/privileged/managed/attach")]
pub(crate) async fn attach_managed_hierarchy(
	request: MsgPack<AuthorizedRequest>,
	core: Data<Arc<Core>>,
) -> impl Responder {
	let bridge_id = match bridge_id(&core, request.client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	let core = core.get_ref().clone();
	let result = web::block(move || {
		let contract = core.managed_hierarchy_contract()?;
		let bridge = Bridge::discover(&bridge_id)?;
		let staged = bridge.post_bytes::<ManagedHierarchyStage>(
			&format!("v1/managed/stage/{}", contract.contract_id),
			contract.payload.clone(),
		)?;
		anyhow::ensure!(
			staged.contract_id == contract.contract_id && staged.source_instances == contract.source_instances,
			"RML staged the wrong managed hierarchy contract"
		);
		let mut response = bridge.post::<_, ManagedHierarchyAttachment>(
			"v1/managed/attach-staged",
			&serde_json::json!({ "contractId": contract.contract_id }),
		)?;
		anyhow::ensure!(
			response.source_instances == contract.source_instances,
			"RML verified the wrong managed source instance count"
		);
		response.excluded_source_ids = contract
			.excluded_source_ids
			.into_iter()
			.map(|id| id.to_string())
			.collect();
		Ok(response)
	})
	.await;
	match result {
		Ok(Ok(response)) if response.attached => HttpResponse::Ok().msgpack(response),
		Ok(Ok(_)) => HttpResponse::Conflict().body("RML rejected the managed hierarchy attachment"),
		Ok(Err(error)) => HttpResponse::ServiceUnavailable().body(error.to_string()),
		Err(error) => HttpResponse::InternalServerError().body(error.to_string()),
	}
}

#[post("/privileged/manifest-identities/bootstrap")]
pub(crate) async fn bootstrap_manifest_identities(
	request: MsgPack<AuthorizedRequest>,
	core: Data<Arc<Core>>,
) -> impl Responder {
	let client_id = request.client_id;
	let bridge_id = match bridge_id(&core, client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	let contract = match core.manifest_identity_bootstrap() {
		Ok(contract) => contract,
		Err(error) => return HttpResponse::Conflict().body(error.to_string()),
	};
	let expected = contract.clone();
	let epoch_core = core.get_ref().clone();
	let result = web::block(move || {
		let bridge = Bridge::discover(&bridge_id)?;
		let bridge_capabilities: Capabilities = bridge.get("v1/capabilities")?;
		anyhow::ensure!(
			bridge_capabilities.manifest_identity_ledger,
			"RML does not support manifest identity bootstrap"
		);
		let response: ManifestIdentityBootstrapResponse = bridge.post("v1/manifest-identities/bootstrap", &contract)?;
		anyhow::ensure!(
			response.authoritative
				&& response.source_instances == expected.expected_source_instances
				&& response.digest == expected.expected_digest,
			"RML bootstrapped the wrong manifest identity contract"
		);
		let current_capabilities: Capabilities = bridge.get("v1/capabilities")?;
		anyhow::ensure!(
			current_capabilities.manifest_identities_authoritative,
			"RML did not retain the bootstrapped manifest identity authority"
		);
		epoch_core.remember_trusted_managed_launch_epoch_if_current(&bridge, &current_capabilities);
		Ok(response)
	})
	.await;
	match result {
		Ok(Ok(response)) => match core.queue().mark_manifest_identities_authoritative(client_id) {
			Ok(()) => HttpResponse::Ok().msgpack(response),
			Err(error) => HttpResponse::Conflict().body(error.to_string()),
		},
		Ok(Err(error)) => HttpResponse::ServiceUnavailable().body(error.to_string()),
		Err(error) => HttpResponse::InternalServerError().body(error.to_string()),
	}
}

#[post("/privileged/managed/resolve")]
pub(crate) async fn resolve_managed_identities(
	request: MsgPack<ManagedIdentityRequest>,
	core: Data<Arc<Core>>,
) -> impl Responder {
	let request = request.0;
	if request.source_ids.len() + request.debug_ids.len() > 4096
		|| request.request_id.is_empty()
		|| request.request_id.len() > 128
	{
		return HttpResponse::BadRequest().body("invalid managed identity request");
	}
	if request
		.source_ids
		.iter()
		.any(|id| !core.is_managed_identity_authorized(*id))
	{
		return HttpResponse::BadRequest().body("managed identity request references unknown source");
	}
	let bridge_id = match bridge_id(&core, request.client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	let payload = serde_json::json!({
		"requestId": request.request_id,
		"sourceIds": request.source_ids.iter().map(ToString::to_string).collect::<Vec<_>>(),
		"debugIds": request.debug_ids,
	});
	bridge_call(bridge_id, move |bridge| {
		let response = bridge.post::<_, ManagedIdentityResolutions>("v1/managed/resolve/start", &payload)?;
		anyhow::ensure!(response.pending, "RML did not queue managed identity resolution");
		Ok(ResolvedManagedIdentities {
			pending: true,
			identities: Vec::new(),
		})
	})
	.await
}

#[post("/privileged/managed/resolve/poll")]
pub(crate) async fn poll_managed_identities(
	request: MsgPack<ManagedIdentityPollRequest>,
	core: Data<Arc<Core>>,
) -> impl Responder {
	let request = request.0;
	if request.request_id.is_empty() || request.request_id.len() > 128 {
		return HttpResponse::BadRequest().body("invalid managed identity poll request");
	}
	let bridge_id = match bridge_id(&core, request.client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	let payload = serde_json::json!({ "requestId": request.request_id });
	let core = core.get_ref().clone();
	bridge_call(bridge_id, move |bridge| {
		let response = bridge.post::<_, ManagedIdentityResolutions>("v1/managed/resolve/poll", &payload)?;
		if response.pending {
			return Ok(ResolvedManagedIdentities {
				pending: true,
				identities: Vec::new(),
			});
		}
		let mut identities = Vec::with_capacity(response.identities.len());
		for identity in response.identities {
			let id = identity
				.source_id
				.parse::<Ref>()
				.context("RML returned an invalid source identity")?;
			let root_id = identity
				.root_source_id
				.parse::<Ref>()
				.context("RML returned an invalid root source identity")?;
			anyhow::ensure!(
				core.is_managed_identity_authorized(id),
				"RML returned an identity outside the verified managed hierarchy history"
			);
			anyhow::ensure!(
				core.is_managed_identity_authorized(root_id),
				"RML returned a root outside the verified managed hierarchy history"
			);
			identities.push(ResolvedManagedIdentity {
				id,
				debug_id: identity.debug_id,
				marker_name: identity.marker_name,
				root_debug_id: identity.root_debug_id,
				root_id,
			});
		}
		Ok(ResolvedManagedIdentities {
			pending: false,
			identities,
		})
	})
	.await
}

#[post("/privileged/property/read")]
pub(crate) async fn read_property(request: MsgPack<PropertyRequest>, core: Data<Arc<Core>>) -> impl Responder {
	let request = request.0;
	let bridge_id = match bridge_id(&core, request.client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	bridge_call(bridge_id, move |bridge| {
		let mut response = bridge.post::<_, PropertyRead>(
			"v1/property/read",
			&serde_json::json!({ "debugId": request.debug_id, "property": request.property }),
		)?;
		if let Some(root_debug_id) = response.model_root_debug_id.take() {
			let value = decode_serialized_property(
				response
					.model
					.as_deref()
					.context("RML serialized property read omitted its model")?,
				&root_debug_id,
				&request.property,
			)?;
			response.value = base64::Engine::encode(
				&base64::engine::general_purpose::STANDARD,
				serialized_property_bytes(&value)?,
			);
			response.model = None;
		}
		Ok(response)
	})
	.await
}

#[post("/privileged/properties/read")]
pub(crate) async fn read_properties(request: MsgPack<PropertyBatchRequest>, core: Data<Arc<Core>>) -> impl Responder {
	let request = request.0;
	if request.requests.len() > 4096 {
		return HttpResponse::PayloadTooLarge().body("privileged property batch exceeds 4096 items");
	}
	let bridge_id = match bridge_id(&core, request.client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	bridge_call(bridge_id, move |bridge| {
		let mut response = bridge.post::<_, PropertyBatchReads>(
			"v1/properties/read",
			&serde_json::json!({ "requests": &request.requests }),
		)?;
		anyhow::ensure!(
			response.values.len() == request.requests.len(),
			"RML serialized property batch returned the wrong number of values"
		);
		let serialized_model = if response.values.iter().any(|value| value.model_root_debug_id.is_some()) {
			Some(decode_serialized_property_model(
				response
					.model
					.as_deref()
					.context("RML serialized property batch omitted its model")?,
			)?)
		} else {
			None
		};
		for (value, lookup) in response.values.iter_mut().zip(&request.requests) {
			if let Some(root_debug_id) = value.model_root_debug_id.take() {
				let serialized = serialized_property_from_model(
					serialized_model
						.as_ref()
						.context("RML serialized property batch model was not decoded")?,
					&root_debug_id,
					&lookup.property,
				)?;
				value.value = Some(base64::Engine::encode(
					&base64::engine::general_purpose::STANDARD,
					serialized_property_bytes(&serialized)?,
				));
			}
		}
		response.model = None;
		Ok(response)
	})
	.await
}

#[post("/privileged/references/read")]
pub(crate) async fn read_references(request: MsgPack<PropertyBatchRequest>, core: Data<Arc<Core>>) -> impl Responder {
	let request = request.0;
	if request.requests.len() > 4096 {
		return HttpResponse::PayloadTooLarge().body("privileged reference batch exceeds 4096 items");
	}
	let bridge_id = match bridge_id(&core, request.client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	bridge_call(bridge_id, move |bridge| {
		let response = bridge.post::<_, ReferenceBatchReads>(
			"v1/references/read",
			&serde_json::json!({ "requests": &request.requests }),
		)?;
		anyhow::ensure!(
			response.values.len() == request.requests.len(),
			"RML serialized reference batch returned the wrong number of values"
		);
		Ok(response)
	})
	.await
}

#[post("/privileged/defaults/read")]
pub(crate) async fn read_default_properties(
	request: MsgPack<DefaultPropertiesRequest>,
	core: Data<Arc<Core>>,
) -> impl Responder {
	let request = request.0;
	if request.properties.len() > 4096 {
		return HttpResponse::PayloadTooLarge().body("privileged default property batch exceeds 4096 items");
	}
	let bridge_id = match bridge_id(&core, request.client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	bridge_call(bridge_id, move |bridge| {
		let mut response = bridge.post::<_, PropertyBatchReads>(
			"v1/defaults/read",
			&serde_json::json!({
				"className": request.class_name,
				"properties": request.properties,
			}),
		)?;
		anyhow::ensure!(
			response.values.len() == request.properties.len(),
			"RML default property batch returned the wrong number of values"
		);
		let serialized_model = if response.values.iter().any(|value| value.model_root_debug_id.is_some()) {
			Some(decode_serialized_property_model(
				response
					.model
					.as_deref()
					.context("RML default property batch omitted its model")?,
			)?)
		} else {
			None
		};
		for (value, property) in response.values.iter_mut().zip(&request.properties) {
			if let Some(root_debug_id) = value.model_root_debug_id.take() {
				let serialized = serialized_property_from_model(
					serialized_model
						.as_ref()
						.context("RML default property batch model was not decoded")?,
					&root_debug_id,
					property,
				)?;
				value.value = Some(base64::Engine::encode(
					&base64::engine::general_purpose::STANDARD,
					serialized_property_bytes(&serialized)?,
				));
			}
		}
		response.model = None;
		Ok(response)
	})
	.await
}

#[post("/privileged/property/write")]
pub(crate) async fn write_property(request: MsgPack<PropertyWriteRequest>, core: Data<Arc<Core>>) -> impl Responder {
	let request = request.0;
	let bridge_id = match bridge_id(&core, request.client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	bridge_call(bridge_id, move |bridge| {
		bridge.post::<_, serde_json::Value>(
			"v1/property/write",
			&serde_json::json!({
				"debugId": request.debug_id,
				"property": request.property,
				"value": request.value,
			}),
		)
	})
	.await
}

#[post("/privileged/reference/write")]
pub(crate) async fn write_reference(
	request: MsgPack<ReferencePropertyWriteRequest>,
	core: Data<Arc<Core>>,
) -> impl Responder {
	let request = request.0;
	let bridge_id = match bridge_id(&core, request.client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	bridge_call(bridge_id, move |bridge| {
		bridge.post::<_, serde_json::Value>("v1/reference/write", &bridge_reference_write_payload(&request))
	})
	.await
}

#[post("/privileged/property/copy")]
pub(crate) async fn copy_property(request: MsgPack<PropertyCopyRequest>, core: Data<Arc<Core>>) -> impl Responder {
	let request = request.0;
	let bridge_id = match bridge_id(&core, request.client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	bridge_call(bridge_id, move |bridge| {
		bridge.post::<_, serde_json::Value>(
			"v1/property/copy",
			&serde_json::json!({
				"sourceDebugId": request.source_debug_id,
				"targetDebugId": request.target_debug_id,
				"property": request.property,
			}),
		)
	})
	.await
}

#[post("/privileged/property/materialize")]
pub(crate) async fn materialize_property(
	request: MsgPack<MaterializeRequest>,
	core: Data<Arc<Core>>,
) -> impl Responder {
	let request = request.0;
	if !core.queue().is_subscribed(request.client_id) {
		return HttpResponse::Unauthorized().finish();
	}

	let result = web::block(move || -> anyhow::Result<MaterializedProperty> {
		let model = materialized_property_model(
			&request.class_name,
			&request.property,
			&request.data_type,
			request.value.as_ref(),
		)?;
		Ok(MaterializedProperty {
			model: ByteBuf::from(model),
		})
	})
	.await;

	match result {
		Ok(Ok(response)) => HttpResponse::Ok().msgpack(response),
		Ok(Err(error)) => HttpResponse::BadRequest().body(error.to_string()),
		Err(error) => HttpResponse::InternalServerError().body(error.to_string()),
	}
}

#[post("/privileged/property/materialized-write")]
pub(crate) async fn write_materialized_property(
	request: MsgPack<MaterializedWriteRequest>,
	core: Data<Arc<Core>>,
) -> impl Responder {
	let request = request.0;
	let bridge_id = match bridge_id(&core, request.client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	bridge_call(bridge_id, move |bridge| {
		let model = materialized_property_model(
			&request.class_name,
			&request.property,
			&request.data_type,
			request.value.as_ref(),
		)?;
		bridge.post::<_, serde_json::Value>(
			"v1/property/materialized-write",
			&serde_json::json!({
				"debugId": request.debug_id,
				"property": request.property,
				"model": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, model),
			}),
		)
	})
	.await
}

#[post("/privileged/instance/create")]
pub(crate) async fn create_instance(request: MsgPack<CreateRequest>, core: Data<Arc<Core>>) -> impl Responder {
	let request = request.0;
	let bridge_id = match bridge_id(&core, request.client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	bridge_call(bridge_id, move |bridge| {
		bridge.post::<_, CreatedInstance>(
			"v1/instance/create",
			&serde_json::json!({
				"className": request.class_name,
				"parentDebugId": request.parent_debug_id,
				"name": request.name,
			}),
		)
	})
	.await
}

#[post("/privileged/roots")]
pub(crate) async fn roots(request: MsgPack<AuthorizedRequest>, core: Data<Arc<Core>>) -> impl Responder {
	let bridge_id = match bridge_id(&core, request.client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	bridge_call(bridge_id, |bridge| bridge.get::<Roots>("v1/roots")).await
}

fn remap_model_variant(value: &Variant, refs: &HashMap<Ref, Ref>) -> Variant {
	match value {
		Variant::Ref(target) => Variant::Ref(refs.get(target).copied().unwrap_or_else(Ref::none)),
		Variant::Content(content) => match content.value() {
			ContentType::Object(target) => Variant::Content(Content::from_referent(
				refs.get(target).copied().unwrap_or_else(Ref::none),
			)),
			_ => value.clone(),
		},
		_ => value.clone(),
	}
}

fn model_snapshot(
	dom: &WeakDom,
	id: Ref,
	refs: &HashMap<Ref, Ref>,
	reference_overrides: &HashMap<(Ref, Ustr), Variant>,
) -> anyhow::Result<Snapshot> {
	let instance = dom
		.get_by_ref(id)
		.context("serialized hidden root instance is missing")?;
	let mut properties: Properties = instance
		.properties
		.iter()
		.filter(|(name, _)| include_native_model_property(instance.class.as_str(), name.as_str()))
		.map(|(name, value)| {
			(
				*name,
				reference_overrides
					.get(&(id, *name))
					.cloned()
					.unwrap_or_else(|| remap_model_variant(value, refs)),
			)
		})
		.collect();
	for ((owner, property), value) in reference_overrides {
		if *owner == id {
			properties.insert(*property, value.clone());
		}
	}
	let raw_name = match properties.remove(&Ustr::from("__CarbonRawName")) {
		Some(Variant::BinaryString(value)) => Some(ByteBuf::from(value.into_vec())),
		Some(_) => anyhow::bail!("serialized hidden root has an invalid raw name payload"),
		None => None,
	};
	let children = instance
		.children()
		.iter()
		.map(|child| model_snapshot(dom, *child, refs, reference_overrides))
		.collect::<anyhow::Result<Vec<_>>>()?;
	Ok(Snapshot {
		id: refs
			.get(&id)
			.copied()
			.context("serialized hidden root has no remapped id")?,
		name: instance.name.clone(),
		raw_name,
		class: instance.class,
		properties,
		children,
	})
}

fn resolve_reference_target(
	target_debug_id: Option<String>,
	debug_to_ref: &HashMap<String, Ref>,
	internal_debug_ids: &HashSet<String>,
	external_by_debug_id: &mut HashMap<String, Ref>,
	external_refs: &mut Vec<ExternalRef>,
	allocated: &mut HashSet<Ref>,
) -> Ref {
	let Some(target_debug_id) = target_debug_id else {
		return Ref::none();
	};
	if let Some(id) = debug_to_ref.get(&target_debug_id).copied() {
		if !internal_debug_ids.contains(&target_debug_id)
			&& external_by_debug_id.insert(target_debug_id.clone(), id).is_none()
		{
			external_refs.push(ExternalRef {
				id,
				debug_id: target_debug_id,
			});
		}
		return id;
	}
	if let Some(id) = external_by_debug_id.get(&target_debug_id).copied() {
		return id;
	}
	let id = loop {
		let candidate = Ref::new();
		if allocated.insert(candidate) {
			break candidate;
		}
	};
	external_by_debug_id.insert(target_debug_id.clone(), id);
	external_refs.push(ExternalRef {
		id,
		debug_id: target_debug_id,
	});
	id
}

fn source_root_template<'a>(
	request: &RootSnapshotsRequest,
	root: &crate::privileged_bridge::Root,
	templates: &'a [SourceRootTemplate],
) -> anyhow::Result<Option<&'a SourceRootTemplate>> {
	if let Some(source_id) = request.root_source_ids.get(&root.debug_id) {
		let template = templates
			.iter()
			.find(|template| template.id == *source_id)
			.with_context(|| format!("source identity for hidden root {} is missing", root.debug_id))?;
		anyhow::ensure!(
			template.class.as_str() == root.class_name,
			"source identity for hidden root {} changed class from {} to {}",
			root.debug_id,
			template.class,
			root.class_name
		);
		return Ok(Some(template));
	}

	// The first capture has no debug-id mapping yet. Roblox services are
	// singleton roots, so class identity is stable even when their mutable Name
	// differs from the filesystem. Refuse an ambiguous match rather than attach
	// forward-version properties to the wrong instance.
	let mut matches = templates
		.iter()
		.filter(|template| template.class.as_str() == root.class_name);
	let template = matches.next();
	if matches.next().is_some() {
		return Ok(None);
	}
	Ok(template)
}

struct RootDecodeInputs {
	source_refs: HashSet<Ref>,
	source_roots: Vec<SourceRootTemplate>,
	live_roots: HashMap<String, LiveRootProperties>,
	reference_requests: Vec<ReferenceRequest>,
	reference_targets: Vec<Option<String>>,
}

fn authored_unique_id(properties: &Properties) -> Option<UniqueId> {
	match properties.get(&Ustr::from("UniqueId")) {
		Some(Variant::UniqueId(value)) if *value != UniqueId::new(0, 0, 0) => Some(*value),
		_ => None,
	}
}

fn source_descendant_templates(
	tree: &crate::core::tree::Tree,
	root: Ref,
) -> anyhow::Result<Vec<SourceDescendantTemplate>> {
	let root = tree.get_instance(root).context("source root template is missing")?;
	let mut output = Vec::new();
	let mut stack = root
		.children()
		.iter()
		.enumerate()
		.rev()
		.map(|(index, id)| (*id, vec![index]))
		.collect::<Vec<_>>();
	while let Some((id, path)) = stack.pop() {
		let instance = tree.get_instance(id).context("source descendant template is missing")?;
		output.push(SourceDescendantTemplate {
			id,
			path: path.clone(),
			class: instance.class,
			unique_id: authored_unique_id(&instance.properties),
		});
		for (index, child) in instance.children().iter().enumerate().rev() {
			let mut child_path = path.clone();
			child_path.push(index);
			stack.push((*child, child_path));
		}
	}
	Ok(output)
}

fn source_root_decode_templates(
	tree: &crate::core::tree::Tree,
	parent: Ref,
) -> anyhow::Result<(HashSet<Ref>, Vec<SourceRootTemplate>)> {
	let source_refs = tree.subtree_refs(tree.root_ref())?.into_iter().collect();
	let mut templates = Vec::new();
	if let Some(parent) = tree.get_instance(parent) {
		for child in parent.children() {
			let instance = tree
				.get_instance(*child)
				.context("source root template child is missing")?;
			let properties = instance
				.properties
				.iter()
				.filter(|(name, _)| {
					name.as_str() != "__CarbonRawName"
						&& include_native_model_property(instance.class.as_str(), name.as_str())
				})
				.map(|(name, value)| (*name, value.clone()))
				.collect();
			templates.push(SourceRootTemplate {
				id: *child,
				class: instance.class,
				properties,
				descendants: source_descendant_templates(tree, *child)?,
			});
		}
	}
	Ok((source_refs, templates))
}

fn decode_root_snapshots(
	request: &RootSnapshotsRequest,
	response: RootModel,
	dom: WeakDom,
	inputs: RootDecodeInputs,
) -> anyhow::Result<RootSnapshots> {
	let RootDecodeInputs {
		source_refs,
		source_roots,
		mut live_roots,
		reference_requests,
		reference_targets,
	} = inputs;
	let native_root_debug_ids = request.debug_ids.clone();
	let model_roots = serialized_model_roots(&dom, &response)?;
	anyhow::ensure!(
		model_roots.len() == response.model_root_parent_debug_ids.len(),
		"RML hidden-root model parent identity count does not match its roots"
	);
	let requested: HashSet<_> = request.debug_ids.iter().cloned().collect();
	let native_roots = response
		.roots
		.iter()
		.enumerate()
		.filter(|(_, root)| requested.contains(&root.debug_id))
		.collect::<Vec<_>>();
	anyhow::ensure!(
		native_roots.len() == requested.len(),
		"RML hidden-root model omitted a requested root"
	);
	let model_instances = serialized_model_instances(&dom, &response)?;
	anyhow::ensure!(
		model_instances.len() == response.instance_debug_ids.len(),
		"RML hidden-root model instance identity count does not match its hierarchy"
	);
	let debug_ids: HashMap<_, _> = model_instances
		.iter()
		.copied()
		.zip(response.instance_debug_ids.iter().cloned())
		.collect();
	let mut hidden_ids = HashSet::new();
	let mut hidden_root_debug_ids = HashMap::new();
	let mut hidden_paths = HashMap::new();
	let mut root_child_indexes = HashMap::<String, usize>::new();
	for (root, parent_debug_id) in model_roots.iter().zip(&response.model_root_parent_debug_ids) {
		if requested.contains(parent_debug_id) {
			let root_index = root_child_indexes.entry(parent_debug_id.clone()).or_default();
			let mut stack = vec![(*root, vec![*root_index])];
			*root_index += 1;
			while let Some((id, path)) = stack.pop() {
				let instance = dom.get_by_ref(id).context("hidden model path instance is missing")?;
				hidden_ids.insert(id);
				hidden_root_debug_ids.insert(id, parent_debug_id.clone());
				hidden_paths.insert(id, path.clone());
				for (index, child) in instance.children().iter().enumerate().rev() {
					let mut child_path = path.clone();
					child_path.push(index);
					stack.push((*child, child_path));
				}
			}
		}
	}

	let mut templates_by_debug_id = HashMap::new();
	for (_, root) in &native_roots {
		let template = source_root_template(request, root, &source_roots)?;
		templates_by_debug_id.insert(root.debug_id.clone(), template);
	}

	let mut refs = HashMap::new();
	let mut allocated = source_refs;
	let mut claimed = HashSet::new();
	for id in &model_instances {
		let mapped = if hidden_ids.contains(id) {
			let debug_id = debug_ids
				.get(id)
				.context("hidden model instance has no native identity")?;
			if let Some(existing) = request.source_instance_ids.get(debug_id).copied() {
				Some(existing)
			} else {
				let root_debug_id = &hidden_root_debug_ids[id];
				let template = templates_by_debug_id.get(root_debug_id).copied().flatten();
				let live = dom.get_by_ref(*id).context("hidden model instance is missing")?;
				let unique_id = authored_unique_id(&live.properties);
				let unique_matches = unique_id
					.map(|unique_id| {
						template
							.into_iter()
							.flat_map(|template| &template.descendants)
							.filter(|candidate| candidate.class == live.class && candidate.unique_id == Some(unique_id))
							.map(|candidate| candidate.id)
							.collect::<Vec<_>>()
					})
					.unwrap_or_default();
				anyhow::ensure!(
					unique_matches.len() <= 1,
					"hidden {} UniqueId is ambiguous in the canonical source",
					live.class
				);
				unique_matches.first().copied().or_else(|| {
					template.and_then(|template| {
						template
							.descendants
							.iter()
							.find(|candidate| candidate.class == live.class && candidate.path == hidden_paths[id])
							.map(|candidate| candidate.id)
					})
				})
			}
		} else {
			None
		};
		let mapped = if let Some(mapped) = mapped {
			anyhow::ensure!(
				allocated.contains(&mapped),
				"reused hidden source identity is outside the canonical tree"
			);
			anyhow::ensure!(
				claimed.insert(mapped),
				"native hidden instances resolved to one source identity"
			);
			mapped
		} else {
			loop {
				let candidate = Ref::new();
				if allocated.insert(candidate) && claimed.insert(candidate) {
					break candidate;
				}
			}
		};
		refs.insert(*id, mapped);
	}
	let mut root_ids = HashMap::new();
	for (_, root) in &native_roots {
		let reused = request
			.source_instance_ids
			.get(&root.debug_id)
			.copied()
			.or_else(|| request.root_source_ids.get(&root.debug_id).copied())
			.or_else(|| {
				templates_by_debug_id
					.get(&root.debug_id)
					.copied()
					.flatten()
					.map(|template| template.id)
			});
		let mapped = if let Some(mapped) = reused {
			anyhow::ensure!(
				allocated.contains(&mapped),
				"reused hidden root identity is outside the canonical tree"
			);
			anyhow::ensure!(
				claimed.insert(mapped),
				"native hidden roots resolved to one source identity"
			);
			mapped
		} else {
			loop {
				let candidate = Ref::new();
				if allocated.insert(candidate) && claimed.insert(candidate) {
					break candidate;
				}
			}
		};
		root_ids.insert(root.debug_id.clone(), mapped);
	}
	let mut source_instances = Vec::new();
	for (_, root) in &native_roots {
		source_instances.push(SourceInstance {
			id: root_ids[&root.debug_id],
			debug_id: root.debug_id.clone(),
			root_debug_id: root.debug_id.clone(),
		});
	}
	for id in &model_instances {
		let Some(root_debug_id) = hidden_root_debug_ids.get(id) else {
			continue;
		};
		source_instances.push(SourceInstance {
			id: refs[id],
			debug_id: debug_ids
				.get(id)
				.context("hidden model instance has no native identity")?
				.clone(),
			root_debug_id: root_debug_id.clone(),
		});
	}

	let mut debug_to_ref = HashMap::new();
	for (id, debug_id) in &debug_ids {
		debug_to_ref.insert(debug_id.clone(), refs[id]);
	}
	debug_to_ref.extend(root_ids.iter().map(|(debug_id, id)| (debug_id.clone(), *id)));
	let mut internal_debug_ids: HashSet<String> = requested.clone();
	for id in &hidden_ids {
		internal_debug_ids.insert(
			debug_ids
				.get(id)
				.context("hidden model instance has no native identity")?
				.clone(),
		);
	}
	let mut external_by_debug_id = HashMap::new();
	let mut external_refs = Vec::new();

	anyhow::ensure!(
		reference_requests.len() == reference_targets.len(),
		"RML reference result count does not match its requests"
	);
	let mut reference_overrides = HashMap::new();
	for (reference, target) in reference_requests.into_iter().zip(reference_targets) {
		let value = Variant::Ref(resolve_reference_target(
			target,
			&debug_to_ref,
			&internal_debug_ids,
			&mut external_by_debug_id,
			&mut external_refs,
			&mut allocated,
		));
		match reference.owner {
			ReferenceOwner::Root(debug_id) => {
				live_roots
					.get_mut(&debug_id)
					.context("live reference owner root is missing")?
					.properties
					.insert(reference.canonical_name, value);
			}
			ReferenceOwner::Model(id) => {
				reference_overrides.insert((id, reference.canonical_name), value);
			}
		}
	}
	for (root_debug_id, live) in &mut live_roots {
		for value in live.properties.values_mut() {
			let Variant::Content(content) = value else {
				continue;
			};
			let ContentType::Object(target) = content.value() else {
				continue;
			};
			if target.is_none() {
				continue;
			}
			let target_debug_id = if let Some(debug_id) = live.carrier_targets.get(target) {
				debug_id.clone()
			} else if let Some(debug_id) = debug_ids.get(target) {
				debug_id.clone()
			} else {
				anyhow::bail!(
					"serialized root Content.Object on {root_debug_id} targets an object outside the durable model"
				);
			};
			*value = Variant::Content(Content::from_referent(resolve_reference_target(
				Some(target_debug_id),
				&debug_to_ref,
				&internal_debug_ids,
				&mut external_by_debug_id,
				&mut external_refs,
				&mut allocated,
			)));
		}
	}
	let mut reference_patches = Vec::new();
	for id in &model_instances {
		let instance = dom.get_by_ref(*id).context("serialized model instance is missing")?;
		for (property, value) in &instance.properties {
			if !include_native_model_property(instance.class.as_str(), property.as_str()) {
				continue;
			}
			let override_value = reference_overrides.get(&(*id, *property));
			let remapped = override_value
				.cloned()
				.unwrap_or_else(|| remap_model_variant(value, &refs));
			if let Some(target) = override_value
				.is_none()
				.then(|| model_reference_target(value))
				.flatten()
			{
				if !hidden_ids.contains(&target) && refs.contains_key(&target) {
					let target_debug_id = debug_ids
						.get(&target)
						.context("external model target has no native identity")?
						.clone();
					let target_id = refs
						.get(&target)
						.copied()
						.context("external model target has no remapped id")?;
					if external_by_debug_id
						.insert(target_debug_id.clone(), target_id)
						.is_none()
					{
						external_refs.push(ExternalRef {
							id: target_id,
							debug_id: target_debug_id,
						});
					}
				}
			}
			if !hidden_ids.contains(id) && is_model_reference(value) && override_value.is_none() {
				reference_patches.push(ReferencePatch {
					debug_id: debug_ids
						.get(id)
						.context("visible model instance has no native identity")?
						.clone(),
					property: *property,
					value: remapped,
				});
			}
		}
	}
	for ((id, property), value) in &reference_overrides {
		if !hidden_ids.contains(id) {
			reference_patches.push(ReferencePatch {
				debug_id: debug_ids
					.get(id)
					.context("visible model instance has no native identity")?
					.clone(),
				property: *property,
				value: value.clone(),
			});
		}
	}
	let mut snapshots = Vec::with_capacity(native_roots.len());
	for (_, root) in native_roots {
		let live = live_roots
			.remove(&root.debug_id)
			.context("RML hidden-root snapshot has no live wrapper properties")?;
		let mut properties = source_root_template(request, root, &source_roots)?
			.map(|template| template.properties.clone())
			.unwrap_or_default();
		// A source template is only a forward-version baseline. Every property
		// understood by this reflection database must come from the live engine so
		// stale filesystem state can never win; unknown newer properties remain.
		for candidate in serialized_property_candidates(&root.class_name)? {
			properties.remove(&candidate.canonical_name);
		}
		properties.extend(live.properties);
		let id = root_ids[&root.debug_id];
		let mut children = Vec::new();
		for (model_root, parent_debug_id) in model_roots.iter().zip(&response.model_root_parent_debug_ids) {
			if parent_debug_id == &root.debug_id {
				children.push(model_snapshot(&dom, *model_root, &refs, &reference_overrides)?);
			}
		}
		snapshots.push(RootSnapshot {
			debug_id: root.debug_id.clone(),
			snapshot: AddedSnapshot {
				id,
				parent: request.parent,
				name: live.name,
				raw_name: live.raw_name,
				class: Ustr::from(&root.class_name),
				properties,
				children,
			},
		});
	}
	Ok(RootSnapshots {
		roots: snapshots,
		root_debug_ids: native_root_debug_ids,
		source_instances,
		external_refs,
		reference_patches,
		change_sequence: response.change_sequence,
	})
}

fn snapshot_engine_is_stable(initial: &Capabilities, current: &Capabilities) -> bool {
	initial.engine_generation == current.engine_generation
}

#[post("/privileged/roots/snapshots")]
pub(crate) async fn root_snapshots(request: MsgPack<RootSnapshotsRequest>, core: Data<Arc<Core>>) -> impl Responder {
	let request = request.0;
	let bridge_id = match bridge_id(&core, request.client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	let Some(source_generation) = request.source_generation.as_deref() else {
		return HttpResponse::BadRequest().body("hidden-root snapshot requires sourceGeneration");
	};
	let tree = match core.materialized_source_tree(source_generation) {
		Ok(tree) => tree,
		Err(error) => return HttpResponse::Conflict().body(error.to_string()),
	};
	let (source_refs, source_roots) = match source_root_decode_templates(&tree, request.parent) {
		Ok(inputs) => inputs,
		Err(error) => return HttpResponse::InternalServerError().body(error.to_string()),
	};
	let result = web::block(move || -> anyhow::Result<RootSnapshots> {
		if request.debug_ids.is_empty() {
			return Ok(RootSnapshots {
				roots: Vec::new(),
				root_debug_ids: Vec::new(),
				source_instances: Vec::new(),
				external_refs: Vec::new(),
				reference_patches: Vec::new(),
				change_sequence: 0,
			});
		}
		let bridge = Bridge::discover(&bridge_id)?;
		for _ in 0..3 {
			let initial_capabilities = bridge.get::<Capabilities>("v1/capabilities")?;
			anyhow::ensure!(initial_capabilities.engine_ready, "Studio edit DataModel is not ready");
			let root_list = bridge.get::<Roots>("v1/roots")?;
			let requested = request.debug_ids.iter().cloned().collect::<HashSet<_>>();
			let observed = root_list
				.roots
				.iter()
				.map(|root| root.debug_id.clone())
				.collect::<HashSet<_>>();
			anyhow::ensure!(
				requested.is_subset(&observed),
				"RML hidden-root list omitted a requested root"
			);
			let property_carriers = property_carrier_debug_ids(&root_list.roots, &requested)?;
			let response =
				bridge.post::<_, RootModel>("v1/roots/model", &serde_json::json!({ "debugIds": property_carriers }))?;
			let dom = decode_root_model(&response)?;
			let live_roots = read_live_root_properties(&bridge, &request, &response, &dom)?;
			let reference_requests = collect_reference_requests(&request, &response, &dom)?;
			let reference_targets = read_reference_targets(&bridge, &reference_requests)?;
			let current = bridge.get::<Capabilities>("v1/capabilities")?;
			// `response.change_sequence` is the replay baseline. Changes that race
			// this capture remain in RML's journal and are applied by the native
			// watcher after the snapshot identities are installed.
			if !snapshot_engine_is_stable(&initial_capabilities, &current) {
				continue;
			}
			return decode_root_snapshots(
				&request,
				response,
				dom,
				RootDecodeInputs {
					source_refs,
					source_roots,
					live_roots,
					reference_requests,
					reference_targets,
				},
			);
		}
		anyhow::bail!("edit DataModel changed during three hidden-root snapshot attempts")
	})
	.await;

	match result {
		Ok(Ok(response)) => HttpResponse::Ok().msgpack(response),
		Ok(Err(error)) => HttpResponse::ServiceUnavailable().body(error.to_string()),
		Err(error) => HttpResponse::InternalServerError().body(error.to_string()),
	}
}

#[derive(Clone, Debug)]
struct PreparedHiddenRoot {
	debug_id: String,
	root_debug_id: String,
	source_ids: Vec<Ref>,
	root_properties: Vec<String>,
	references: Vec<PreparedRootReference>,
}

#[derive(Clone, Debug)]
struct PreparedRootReference {
	owner: Ref,
	property: String,
	target: Option<Ref>,
}

type PreparedRootBundle = (Vec<PreparedHiddenRoot>, Vec<PreparedRootReference>, String);

struct PreparedHiddenApply {
	bridge_id: String,
	client_id: u32,
	source_generation: String,
	roots: Vec<PreparedHiddenRoot>,
	bundle_model: String,
	external_references: Vec<PreparedRootReference>,
	in_flight: bool,
	completed: Option<ApplyHiddenRootsResponse>,
}

static HIDDEN_APPLY_PREFLIGHTS: OnceLock<Mutex<HashMap<String, PreparedHiddenApply>>> = OnceLock::new();

fn hidden_apply_preflights() -> &'static Mutex<HashMap<String, PreparedHiddenApply>> {
	HIDDEN_APPLY_PREFLIGHTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn purge_hidden_apply_preflights(client_id: u32, bridge_id: Option<&str>) {
	let Some(bridge_id) = bridge_id else { return };
	hidden_apply_preflights()
		.lock()
		.unwrap()
		.retain(|_, prepared| prepared.client_id != client_id || prepared.bridge_id.as_str() != bridge_id);
}

fn prepare_external_reference(
	tree: &crate::core::tree::Tree,
	reference: &ApplyReferenceRequest,
) -> anyhow::Result<PreparedRootReference> {
	let owner = tree
		.get_instance(reference.owner_source_id)
		.with_context(|| format!("external reference owner {} is unavailable", reference.owner_source_id))?;
	let candidate = serialized_property_candidates(owner.class.as_str())?
		.into_iter()
		.find(|candidate| candidate.canonical_name == reference.property)
		.context("external reference property is not canonical serialized state")?;
	anyhow::ensure!(
		candidate.data_type == VariantType::Ref,
		"external native reference routing accepts only Ref properties"
	);
	let target = match owner.properties.get(&candidate.canonical_name) {
		Some(Variant::Ref(target)) if target.is_some() => Some(*target),
		Some(Variant::Ref(_)) | None => None,
		Some(_) => anyhow::bail!("external Ref property has a mismatched source value"),
	};
	Ok(PreparedRootReference {
		owner: reference.owner_source_id,
		property: candidate.request_name,
		target,
	})
}

fn append_incoming_references(
	tree: &crate::core::tree::Tree,
	replaced_ids: &HashSet<Ref>,
	references: &mut Vec<PreparedRootReference>,
) -> anyhow::Result<()> {
	let mut explicit = references
		.iter()
		.map(|reference| (reference.owner, reference.property.clone()))
		.collect::<HashSet<_>>();
	for owner_id in tree.subtree_refs(tree.root_ref())? {
		if replaced_ids.contains(&owner_id) {
			continue;
		}
		let owner = tree
			.get_instance(owner_id)
			.context("incoming reference owner is missing")?;
		let candidates = serialized_property_candidates(owner.class.as_str())?
			.into_iter()
			.map(|candidate| (candidate.canonical_name, candidate.request_name))
			.collect::<HashMap<_, _>>();
		for (property, value) in &owner.properties {
			let target = match value {
				Variant::Ref(target) if replaced_ids.contains(target) => Some((*target, false)),
				Variant::Content(content) => match content.value() {
					ContentType::Object(target) if replaced_ids.contains(target) => Some((*target, true)),
					_ => None,
				},
				_ => None,
			};
			let Some((target, is_content)) = target else {
				continue;
			};
			let request_name = candidates.get(property).with_context(|| {
				format!(
					"incoming reference {}.{} has no canonical descriptor",
					owner.class, property
				)
			})?;
			anyhow::ensure!(
				!is_content,
				"incoming {}.{} Content.Object targets replaced native state and requires full-graph materialization",
				owner.class,
				property
			);
			if explicit.insert((owner_id, request_name.clone())) {
				references.push(PreparedRootReference {
					owner: owner_id,
					property: request_name.clone(),
					target: Some(target),
				});
			}
		}
	}
	Ok(())
}

fn prepare_hidden_root_bundle(
	tree: &crate::core::tree::Tree,
	prepared_roots: &[PreparedHiddenRoot],
) -> anyhow::Result<String> {
	let root_ids = prepared_roots
		.iter()
		.map(|root| {
			root.source_ids
				.first()
				.copied()
				.context("prepared hidden root has no source id")
		})
		.collect::<anyhow::Result<Vec<_>>>()?;
	let root_set = root_ids.iter().copied().collect::<HashSet<_>>();
	let bundled_ids = prepared_roots
		.iter()
		.flat_map(|root| root.source_ids.iter().copied())
		.collect::<HashSet<_>>();
	for owner_id in &bundled_ids {
		let owner = tree
			.get_instance(*owner_id)
			.context("bundled hidden owner is missing")?;
		for (property, value) in &owner.properties {
			let Variant::Content(content) = value else { continue };
			let ContentType::Object(target) = content.value() else {
				continue;
			};
			anyhow::ensure!(
				target.is_none() || bundled_ids.contains(target),
				"hidden {}.{} Content.Object target is outside the atomic native bundle",
				owner.class,
				property
			);
			anyhow::ensure!(
				target.is_none() || !root_set.contains(target),
				"hidden {}.{} Content.Object targets a non-replaceable DataModel service root",
				owner.class,
				property
			);
		}
	}
	let mut bytes = Vec::new();
	rbx_binary::Serializer::new()
		.serialize_source(&mut bytes, tree, &root_ids)
		.context("failed to serialize authoritative hidden root bundle")?;
	Ok(base64::Engine::encode(
		&base64::engine::general_purpose::STANDARD,
		bytes,
	))
}

fn prepare_hidden_root_apply(
	tree: &crate::core::tree::Tree,
	root: &ApplyHiddenRoot,
) -> anyhow::Result<PreparedHiddenRoot> {
	let instance = tree
		.get_instance(root.source_id)
		.with_context(|| format!("hidden source root {} is unavailable", root.source_id))?;
	anyhow::ensure!(
		instance.parent() == tree.root_ref(),
		"hidden apply source {} is not a DataModel root",
		root.source_id
	);
	let source_ids = tree.subtree_refs(root.source_id)?;
	let candidates = serialized_property_candidates(instance.class.as_str())?;
	let mut root_properties = Vec::with_capacity(candidates.len());
	let mut default_root_references = Vec::new();
	for candidate in candidates {
		if candidate.data_type == VariantType::Ref && !instance.properties.contains_key(&candidate.canonical_name) {
			default_root_references.push(PreparedRootReference {
				owner: root.source_id,
				property: candidate.request_name.clone(),
				target: None,
			});
		}
		root_properties.push(candidate.request_name);
	}
	root_properties.sort();
	root_properties.dedup();

	let mut references = default_root_references;
	for source_id in &source_ids {
		let node = tree
			.get_instance(*source_id)
			.context("hidden apply subtree instance is missing")?;
		let node_candidates = serialized_property_candidates(node.class.as_str())?
			.into_iter()
			.map(|candidate| (candidate.canonical_name, candidate.request_name))
			.collect::<HashMap<_, _>>();
		for (property, value) in &node.properties {
			if let Variant::Ref(target) = value {
				let request_name = node_candidates.get(property).with_context(|| {
					format!("serialized Ref {}.{} has no canonical descriptor", node.class, property)
				})?;
				references.push(PreparedRootReference {
					owner: *source_id,
					property: request_name.clone(),
					target: target.is_some().then_some(*target),
				});
			}
		}
	}

	Ok(PreparedHiddenRoot {
		debug_id: root.debug_id.clone(),
		root_debug_id: root.debug_id.clone(),
		source_ids,
		root_properties,
		references,
	})
}

#[post("/privileged/roots/apply")]
pub(crate) async fn apply_hidden_roots(
	request: MsgPack<ApplyHiddenRootsRequest>,
	core: Data<Arc<Core>>,
) -> impl Responder {
	let request = request.0;
	let bridge_id = match bridge_id(&core, request.client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	if request.preflight && request.preflight_token.is_some() {
		return HttpResponse::BadRequest().body("preflight request must not include a preflight token");
	}
	if !request.preflight && request.preflight_token.is_none() {
		return HttpResponse::BadRequest().body("hidden-root commit requires a valid preflight token");
	}
	let commit_token = request.preflight_token.clone();
	let cached = if let Some(token) = &commit_token {
		let mut preflights = hidden_apply_preflights().lock().unwrap();
		let Some(prepared) = preflights.get_mut(token) else {
			return HttpResponse::Conflict().body("hidden-root preflight token is missing");
		};
		if prepared.bridge_id != bridge_id
			|| prepared.client_id != request.client_id
			|| prepared.source_generation != request.source_generation
		{
			return HttpResponse::Conflict().body("hidden-root preflight token does not match this Studio transaction");
		}
		if let Some(response) = &prepared.completed {
			return HttpResponse::Ok().msgpack(response.clone());
		}
		if prepared.in_flight {
			return HttpResponse::Conflict().body("hidden-root preflight commit is already in flight");
		}
		prepared.in_flight = true;
		Some((
			prepared.roots.clone(),
			prepared.external_references.clone(),
			prepared.bundle_model.clone(),
		))
	} else {
		None
	};
	let (prepared, external_references, bundle_model) = if let Some(prepared) = cached {
		prepared
	} else {
		let tree = match core.materialized_source_tree(&request.source_generation) {
			Ok(tree) => tree,
			Err(error) => return HttpResponse::Conflict().body(error.to_string()),
		};
		let mut prepared_roots = Vec::with_capacity(request.roots.len());
		for root in &request.roots {
			match prepare_hidden_root_apply(&tree, root) {
				Ok(root) => prepared_roots.push(root),
				Err(error) => return HttpResponse::BadRequest().body(error.to_string()),
			}
		}
		let replaced_ids = prepared_roots
			.iter()
			.flat_map(|root| root.source_ids.iter().copied())
			.collect::<HashSet<_>>();
		let mut external_references = Vec::with_capacity(request.external_references.len());
		for reference in &request.external_references {
			match prepare_external_reference(&tree, reference) {
				Ok(reference) => external_references.push(reference),
				Err(error) => return HttpResponse::BadRequest().body(error.to_string()),
			}
		}
		if let Err(error) = append_incoming_references(&tree, &replaced_ids, &mut external_references) {
			return HttpResponse::BadRequest().body(error.to_string());
		}
		let bundle_model = match prepare_hidden_root_bundle(&tree, &prepared_roots) {
			Ok(model) => model,
			Err(error) => return HttpResponse::BadRequest().body(error.to_string()),
		};
		(prepared_roots, external_references, bundle_model)
	};
	if request.preflight {
		let prepared_bridge_id = bridge_id.clone();
		let result = web::block(move || -> anyhow::Result<PreparedRootBundle> {
			let bridge = Bridge::discover(&bridge_id)?;
			let bundled_ids = prepared
				.iter()
				.flat_map(|root| root.source_ids.iter().copied())
				.collect::<HashSet<_>>();
			let immediate = prepared
				.iter()
				.flat_map(|root| root.references.iter())
				.filter(|reference| {
					reference.target.is_none() || reference.target.is_some_and(|target| bundled_ids.contains(&target))
				})
				.map(|reference| RootApplyReference {
					owner_source_id: reference.owner.to_string(),
					property: reference.property.clone(),
					target_source_id: reference.target.map(|target| target.to_string()),
				})
				.collect::<Vec<_>>();
			bridge.post::<_, serde_json::Value>(
				"v1/roots/validate-bundle",
				&serde_json::json!({
					"model": bundle_model,
					"roots": prepared.iter().map(|root| serde_json::json!({
						"debugId": root.debug_id,
						"sourceIds": root.source_ids.iter().map(ToString::to_string).collect::<Vec<_>>(),
						"rootProperties": root.root_properties,
					})).collect::<Vec<_>>(),
					"references": immediate,
					"knownSourceDebugIds": {},
				}),
			)?;
			Ok((prepared, external_references, bundle_model))
		})
		.await;
		return match result {
			Ok(Ok((prepared, external_references, bundle_model))) => {
				let token = uuid::Uuid::new_v4().to_string();
				let active_bridges = Bridge::active_bridge_ids().ok();
				let mut preflights = hidden_apply_preflights().lock().unwrap();
				if let Some(active) = &active_bridges {
					preflights.retain(|_, prepared| active.contains(&prepared.bridge_id));
				}
				// One Core processes SyncChanges serially. Superseding an abandoned
				// lease from this exact Studio client bounds memory without placing a
				// wall-clock deadline on a legitimately large regular apply.
				preflights.retain(|_, prepared| {
					prepared.bridge_id != prepared_bridge_id || prepared.client_id != request.client_id
				});
				preflights.insert(
					token.clone(),
					PreparedHiddenApply {
						bridge_id: prepared_bridge_id,
						client_id: request.client_id,
						source_generation: request.source_generation,
						roots: prepared,
						bundle_model,
						external_references,
						in_flight: false,
						completed: None,
					},
				);
				HttpResponse::Ok().msgpack(ApplyHiddenRootsResponse {
					source_instances: Vec::new(),
					preflight_token: Some(token),
				})
			}
			Ok(Err(error)) => HttpResponse::ServiceUnavailable().body(error.to_string()),
			Err(error) => HttpResponse::InternalServerError().body(error.to_string()),
		};
	}
	let bundled_ids = prepared
		.iter()
		.flat_map(|root| root.source_ids.iter().copied())
		.collect::<HashSet<_>>();
	let known = external_known_source_debug_ids(request.known_source_instances, &bundled_ids);
	let result = web::block(move || -> anyhow::Result<ApplyHiddenRootsResponse> {
		let bridge = Bridge::discover(&bridge_id)?;
		let mut source_debug_ids = known;
		let mut source_instances = Vec::new();
		let mut deferred_references = external_references;
		let root_by_source_id = prepared
			.iter()
			.flat_map(|root| {
				root.source_ids
					.iter()
					.copied()
					.map(|source_id| (source_id, root.root_debug_id.clone()))
			})
			.collect::<HashMap<_, _>>();
		let mut immediate = Vec::new();
		for reference in prepared.iter().flat_map(|root| root.references.iter()) {
			if reference.target.is_none() || reference.target.is_some_and(|target| bundled_ids.contains(&target)) {
				immediate.push(RootApplyReference {
					owner_source_id: reference.owner.to_string(),
					property: reference.property.clone(),
					target_source_id: reference.target.map(|target| target.to_string()),
				});
			} else {
				deferred_references.push(reference.clone());
			}
		}
		let response = bridge.post::<_, RootApplyModelResponse>(
			"v1/roots/apply-bundle",
			&serde_json::json!({
				"model": bundle_model,
				"roots": prepared.iter().map(|root| serde_json::json!({
					"debugId": root.debug_id,
					"sourceIds": root.source_ids.iter().map(ToString::to_string).collect::<Vec<_>>(),
					"rootProperties": root.root_properties,
				})).collect::<Vec<_>>(),
				"references": immediate,
				"knownSourceDebugIds": source_debug_ids,
			}),
		)?;
		anyhow::ensure!(
			response.source_instances.len() == bundled_ids.len(),
			"RML hidden-root bundle returned the wrong source identity count"
		);
		let mut returned_ids = HashSet::with_capacity(response.source_instances.len());
		for identity in response.source_instances {
			let source = identity
				.source_id
				.parse::<Ref>()
				.context("RML hidden-root bundle returned an invalid source identity")?;
			anyhow::ensure!(
				bundled_ids.contains(&source) && returned_ids.insert(source),
				"RML hidden-root bundle returned an unknown or duplicate source identity"
			);
			let root_debug_id = root_by_source_id
				.get(&source)
				.context("bundled source identity has no owning hidden root")?
				.clone();
			source_debug_ids.insert(identity.source_id, identity.debug_id.clone());
			source_instances.push(SourceInstance {
				id: source,
				debug_id: identity.debug_id,
				root_debug_id,
			});
		}
		for reference in deferred_references {
			let owner = source_debug_ids
				.get(&reference.owner.to_string())
				.context("hidden reference owner has no post-apply native identity")?;
			let target = reference
				.target
				.map(|target| {
					source_debug_ids
						.get(&target.to_string())
						.cloned()
						.context("hidden reference target has no post-apply native identity")
				})
				.transpose()?;
			bridge.post::<_, serde_json::Value>(
				"v1/reference/write",
				&serde_json::json!({
					"debugId": owner,
					"property": reference.property,
					"targetDebugId": target,
				}),
			)?;
		}
		Ok(ApplyHiddenRootsResponse {
			source_instances,
			preflight_token: None,
		})
	})
	.await;

	match result {
		Ok(Ok(response)) => {
			if let Some(token) = &commit_token {
				if let Some(prepared) = hidden_apply_preflights().lock().unwrap().get_mut(token) {
					prepared.in_flight = false;
					prepared.roots.clear();
					prepared.bundle_model.clear();
					prepared.external_references.clear();
					prepared.completed = Some(response.clone());
				}
			}
			HttpResponse::Ok().msgpack(response)
		}
		Ok(Err(error)) => {
			if let Some(token) = &commit_token {
				if let Some(prepared) = hidden_apply_preflights().lock().unwrap().get_mut(token) {
					prepared.in_flight = false;
				}
			}
			HttpResponse::ServiceUnavailable().body(error.to_string())
		}
		Err(error) => {
			if let Some(token) = &commit_token {
				if let Some(prepared) = hidden_apply_preflights().lock().unwrap().get_mut(token) {
					prepared.in_flight = false;
				}
			}
			HttpResponse::InternalServerError().body(error.to_string())
		}
	}
}

#[post("/privileged/changes")]
pub(crate) async fn changes(request: MsgPack<ChangesRequest>, core: Data<Arc<Core>>) -> impl Responder {
	let request = request.0;
	let bridge_id = match bridge_id(&core, request.client_id) {
		Ok(bridge_id) => bridge_id,
		Err(response) => return response,
	};
	let core = core.get_ref().clone();
	bridge_call(bridge_id, move |bridge| {
		let response = bridge.get::<Changes>(&format!("v1/changes?after={}", request.after))?;
		ensure_no_bridge_diagnostics(&response)?;
		let tree = core.tree();
		for change in &response.changes {
			let Some(source_id) = &change.source_id else { continue };
			let id = source_id
				.parse::<Ref>()
				.context("RML returned an invalid managed change identity")?;
			anyhow::ensure!(tree.exists(id), "RML returned a managed change outside the source tree");
		}
		Ok(response)
	})
	.await
}

fn ensure_no_bridge_diagnostics(response: &Changes) -> anyhow::Result<()> {
	if response.diagnostics.is_empty() {
		return Ok(());
	}
	for diagnostic in &response.diagnostics {
		crate::carbon_error!(
			"RML bridge {} at sequence {}: {}",
			diagnostic.severity,
			diagnostic.sequence,
			diagnostic.message
		);
	}
	anyhow::bail!(
		"RML bridge reported {} observer diagnostic(s); synchronization stopped before acknowledging them",
		response.diagnostics.len()
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::privileged_bridge::BridgeDiagnostic;
	use rbx_dom_weak::{
		types::{
			CFrame, CustomPhysicalProperties, Matrix3, NumberRange, PhysicalProperties, Ray, Rect, UDim, UDim2,
			Vector2, Vector3int16,
		},
		InstanceBuilder,
	};
	use rbx_reflection::ClassTag;

	#[test]
	fn capture_shell_native_type_table_covers_every_canonical_variant() {
		let canonical = [
			(VariantType::Axes, "Axes"),
			(VariantType::BinaryString, "BinaryString"),
			(VariantType::Bool, "Bool"),
			(VariantType::BrickColor, "BrickColor"),
			(VariantType::CFrame, "CFrame"),
			(VariantType::Color3, "Color3"),
			(VariantType::Color3uint8, "Color3uint8"),
			(VariantType::ColorSequence, "ColorSequence"),
			(VariantType::ContentId, "ContentId"),
			(VariantType::Enum, "CameraType"),
			(VariantType::Faces, "Faces"),
			(VariantType::Float32, "Float32"),
			(VariantType::Float64, "Float64"),
			(VariantType::Int32, "Int32"),
			(VariantType::Int64, "Int64"),
			(VariantType::NumberRange, "NumberRange"),
			(VariantType::NumberSequence, "NumberSequence"),
			(VariantType::PhysicalProperties, "PhysicalProperties"),
			(VariantType::Ray, "Ray"),
			(VariantType::Rect, "Rect"),
			(VariantType::Ref, "Ref"),
			(VariantType::Region3, "Region3"),
			(VariantType::Region3int16, "Region3int16"),
			(VariantType::SharedString, "SharedString"),
			(VariantType::String, "String"),
			(VariantType::UDim, "UDim"),
			(VariantType::UDim2, "UDim2"),
			(VariantType::Vector2, "Vector2"),
			(VariantType::Vector2int16, "Vector2int16"),
			(VariantType::Vector3, "Vector3"),
			(VariantType::Vector3int16, "Vector3int16"),
			(VariantType::OptionalCFrame, "OptionalCFrame"),
			(VariantType::Tags, "Tags"),
			(VariantType::Attributes, "Attributes"),
			(VariantType::Font, "Font"),
			(VariantType::UniqueId, "UniqueId"),
			(VariantType::MaterialColors, "MaterialColors"),
			(VariantType::SecurityCapabilities, "SecurityCapabilities"),
			(VariantType::EnumItem, "EnumItem"),
			(VariantType::Content, "Content"),
			(VariantType::NetAssetRef, "NetAssetRef"),
		];
		for (data_type, native_type) in canonical {
			let enum_name = (data_type == VariantType::Enum).then_some(native_type);
			assert!(
				capture_shell_native_type_matches(data_type, native_type, enum_name),
				"canonical {data_type:?} was missing from the native compatibility table"
			);
		}
	}

	#[test]
	fn capture_shell_native_type_aliases_are_explicit_and_fail_closed() {
		for (data_type, native_type) in [
			(VariantType::Bool, "bool"),
			(VariantType::CFrame, "CoordinateFrame"),
			(VariantType::Float32, "float"),
			(VariantType::Float64, "double"),
			(VariantType::Int32, "int"),
			(VariantType::Int64, "int64"),
			(VariantType::Int64, "int64_t"),
			(VariantType::OptionalCFrame, "OptionalCoordinateFrame"),
			(VariantType::Rect, "Rect2D"),
			(VariantType::Ref, "Object"),
			(VariantType::Ref, "Instance"),
			(VariantType::String, "string"),
			(VariantType::Content, "ContentId"),
			(VariantType::Tags, "BinaryString"),
			(VariantType::Tags, "SharedString"),
			(VariantType::Attributes, "BinaryString"),
		] {
			assert!(capture_shell_native_type_matches(data_type, native_type, None));
		}
		for (data_type, native_type) in [
			(VariantType::Bool, "bOoL"),
			(VariantType::Int32, "Integer"),
			(VariantType::Float32, "double"),
			(VariantType::String, "std::string"),
		] {
			assert!(!capture_shell_native_type_matches(data_type, native_type, None));
		}
		assert!(capture_shell_native_type_matches(
			VariantType::Enum,
			"CameraType",
			Some("CameraType")
		));
		assert!(!capture_shell_native_type_matches(
			VariantType::Enum,
			"CameraType",
			Some("RolloutState")
		));
	}

	#[test]
	fn capture_shell_schema_covers_late_created_services() {
		let classes = capture_shell_class_names();
		assert!(classes.windows(2).all(|pair| pair[0] < pair[1]));
		assert!(classes.iter().any(|class| class == "DataModel"));
		assert!(classes.iter().any(|class| class == "UIDragDetectorService"));
		for root in [
			"Workspace",
			"NonReplicatedCSGDictionaryService",
			"CSGDictionaryService",
			"PlayerEmulatorService",
			"StudioData",
			"StarterPlayer",
			"AvatarSettings",
			"ReplicatedStorage",
			"ServerScriptService",
			"ServerStorage",
			"LodDataService",
		] {
			assert!(
				classes.iter().any(|class| class == root),
				"schema omitted live service {root}"
			);
		}
		for class in classes {
			let candidates = capture_shell_property_candidates(&class).unwrap();
			assert!(candidates.len() <= 4096, "{class} has too many properties");
			for structural in ["Name", "HistoryId", "UniqueId"] {
				assert!(
					candidates
						.iter()
						.all(|candidate| candidate.canonical_name.as_str() != structural),
					"{class} shell schema exposed identity-only property {structural}"
				);
			}
			let mut unique = HashSet::new();
			for candidate in candidates {
				assert!(!candidate.request_name.is_empty(), "{class} has an empty property name");
				assert!(
					unique.insert(candidate.request_name.clone()),
					"{class} repeats serialized property {}",
					candidate.request_name
				);
			}
		}
		let material = capture_shell_property_candidates("MaterialService").unwrap();
		let use_2022 = material
			.iter()
			.find(|candidate| candidate.request_name == "Use2022MaterialsXml")
			.unwrap();
		assert_eq!(use_2022.canonical_name, Ustr::from("Use2022MaterialsXml"));
		assert_eq!(use_2022.data_type, VariantType::Bool);
	}

	fn snapshot_capabilities(engine_generation: u64, change_sequence: u64) -> Capabilities {
		Capabilities {
			protocol_version: 2,
			bridge_id: "bridge".to_owned(),
			process_id: 1,
			engine_ready: true,
			engine_generation,
			studio_session_id: "studio".to_owned(),
			instance_id: "instance".to_owned(),
			hierarchy_sequence: 0,
			change_sequence,
			binary_types: Vec::new(),
			scalar_types: Vec::new(),
			blittable_types: Vec::new(),
			raw_types: Vec::new(),
			native_observation: true,
			engine_creation: true,
			per_root_availability: true,
			serialized_references: true,
			managed_hierarchy_attachment: true,
			managed_contract_id: String::new(),
			managed_contract_source_instances: 0,
			manifest_identity_ledger: true,
			manifest_identities_authoritative: true,
			capture_lease_protocol: crate::capture_provider::CAPTURE_ENVELOPE_VERSION,
			local_place_save_diagnostic: true,
		}
	}

	#[test]
	fn snapshot_capture_accepts_journaled_property_drift_but_rejects_engine_replacement() {
		let initial = snapshot_capabilities(7, 100);
		let property_drift = snapshot_capabilities(7, 10_000);
		let replacement = snapshot_capabilities(8, 100);

		assert!(snapshot_engine_is_stable(&initial, &property_drift));
		assert!(!snapshot_engine_is_stable(&initial, &replacement));
	}

	fn native_f32s(values: &[f32]) -> Vec<u8> {
		values.iter().flat_map(|value| value.to_le_bytes()).collect()
	}

	#[test]
	fn reference_write_proxy_keeps_null_distinct_from_a_target_debug_id() {
		let null = ReferencePropertyWriteRequest {
			client_id: 7,
			debug_id: "owner".to_owned(),
			property: "Value".to_owned(),
			target_debug_id: None,
		};
		assert!(bridge_reference_write_payload(&null)["targetDebugId"].is_null());

		let target = ReferencePropertyWriteRequest {
			target_debug_id: Some("target".to_owned()),
			..null
		};
		assert_eq!(bridge_reference_write_payload(&target)["targetDebugId"], "target");
	}

	#[test]
	fn hidden_bundle_known_identities_exclude_every_replaced_root_and_descendant() {
		let bundled_root = Ref::new();
		let bundled_descendant = Ref::new();
		let external_visible = Ref::new();
		let bundled = HashSet::from([bundled_root, bundled_descendant]);
		let filtered = external_known_source_debug_ids(
			vec![
				KnownSourceInstance {
					id: bundled_root,
					debug_id: "old-hidden-root".to_owned(),
				},
				KnownSourceInstance {
					id: bundled_descendant,
					debug_id: "old-hidden-descendant".to_owned(),
				},
				KnownSourceInstance {
					id: external_visible,
					debug_id: "visible-target".to_owned(),
				},
			],
			&bundled,
		);

		assert_eq!(
			filtered,
			HashMap::from([(external_visible.to_string(), "visible-target".to_owned())]),
			"replacement identities must come from the new bundle while external reference targets stay resolvable"
		);
	}

	#[test]
	fn baseplate_lighting_color3_native_bytes_decode_exactly() {
		// OutdoorAmbient on a new Baseplate is a Color3 property read directly
		// from the live Lighting service rather than from an RBXM clone.
		let raw = native_f32s(&[0.5, 0.5, 0.5]);
		assert_eq!(
			decode_privileged_variant(VariantType::Color3, &raw).unwrap(),
			Variant::Color3(Color3::new(0.5, 0.5, 0.5))
		);
	}

	#[test]
	fn every_advertised_exact_raw_value_materializes_from_its_canonical_bytes() {
		let nan = f32::from_bits(0x7fc0_0042);
		let values = [
			("Color3", Variant::Color3(Color3::new(nan, -0.0, f32::INFINITY))),
			(
				"ColorSequence",
				Variant::ColorSequence(ColorSequence {
					keypoints: vec![ColorSequenceKeypoint::new(nan, Color3::new(-0.0, 0.5, 1.0))],
				}),
			),
			(
				"PhysicalProperties",
				Variant::PhysicalProperties(PhysicalProperties::Custom(CustomPhysicalProperties::new(
					nan, 0.5, -0.0, 1.0, 2.0, 0.25,
				))),
			),
			(
				"Region3",
				Variant::Region3(Region3::new(
					Vector3::new(-0.0, nan, 1.0),
					Vector3::new(2.0, 3.0, f32::INFINITY),
				)),
			),
		];
		for (data_type, value) in values {
			let raw = exact_raw_bytes(&value).unwrap();
			let materialized = materialized_property_value(data_type, &raw).unwrap();
			assert_eq!(exact_raw_bytes(&materialized).unwrap(), raw, "{data_type}");
		}
		assert!(materialized_property_value("PhysicalProperties", &[1, 0]).is_err());
	}

	#[test]
	fn native_raw_decoder_covers_all_blittable_and_sequence_layouts() {
		let cframe_raw = native_f32s(&[
			1.0, 0.0, 0.0, // rotation row 0
			0.0, 1.0, 0.0, // rotation row 1
			0.0, 0.0, 1.0, // rotation row 2
			4.0, 6.0, 8.0, // position
		]);
		let cframe = CFrame::new(Vector3::new(4.0, 6.0, 8.0), Matrix3::identity());
		assert_eq!(
			decode_privileged_variant(VariantType::CFrame, &cframe_raw).unwrap(),
			Variant::CFrame(cframe)
		);
		let mut optional_cframe_raw = vec![1];
		optional_cframe_raw.extend_from_slice(&cframe_raw);
		assert_eq!(
			decode_privileged_variant(VariantType::OptionalCFrame, &optional_cframe_raw).unwrap(),
			Variant::OptionalCFrame(Some(cframe))
		);

		assert_eq!(
			decode_privileged_variant(VariantType::Int64, &i64::MIN.to_le_bytes()).unwrap(),
			Variant::Int64(i64::MIN)
		);
		assert_eq!(
			decode_privileged_variant(VariantType::Float32, &(-0.0_f32).to_le_bytes()).unwrap(),
			Variant::Float32(-0.0)
		);
		assert_eq!(
			decode_privileged_variant(VariantType::Float64, &f64::INFINITY.to_le_bytes()).unwrap(),
			Variant::Float64(f64::INFINITY)
		);
		assert_eq!(
			decode_privileged_variant(VariantType::Vector2, &native_f32s(&[1.0, 2.0])).unwrap(),
			Variant::Vector2(Vector2::new(1.0, 2.0))
		);
		assert_eq!(
			decode_privileged_variant(VariantType::Vector3, &native_f32s(&[1.0, 2.0, 3.0])).unwrap(),
			Variant::Vector3(Vector3::new(1.0, 2.0, 3.0))
		);
		let vector3int16_raw = [(-1_i16).to_le_bytes(), 2_i16.to_le_bytes(), i16::MAX.to_le_bytes()].concat();
		assert_eq!(
			decode_privileged_variant(VariantType::Vector3int16, &vector3int16_raw).unwrap(),
			Variant::Vector3int16(Vector3int16::new(-1, 2, i16::MAX))
		);
		assert_eq!(
			decode_privileged_variant(VariantType::NumberRange, &native_f32s(&[-1.0, 5.0])).unwrap(),
			Variant::NumberRange(NumberRange::new(-1.0, 5.0))
		);
		assert_eq!(
			decode_privileged_variant(VariantType::Color3, &native_f32s(&[0.25, 0.5, 0.75])).unwrap(),
			Variant::Color3(Color3::new(0.25, 0.5, 0.75))
		);
		assert_eq!(
			decode_privileged_variant(VariantType::Rect, &native_f32s(&[1.0, 2.0, 3.0, 4.0])).unwrap(),
			Variant::Rect(Rect::new(Vector2::new(1.0, 2.0), Vector2::new(3.0, 4.0)))
		);
		assert_eq!(
			decode_privileged_variant(VariantType::Ray, &native_f32s(&[1.0, 2.0, 3.0, -4.0, -5.0, -6.0])).unwrap(),
			Variant::Ray(Ray::new(Vector3::new(1.0, 2.0, 3.0), Vector3::new(-4.0, -5.0, -6.0)))
		);
		let mut udim_raw = Vec::new();
		udim_raw.extend_from_slice(&0.5_f32.to_le_bytes());
		udim_raw.extend_from_slice(&(-12_i32).to_le_bytes());
		assert_eq!(
			decode_privileged_variant(VariantType::UDim, &udim_raw).unwrap(),
			Variant::UDim(UDim::new(0.5, -12))
		);
		let mut udim2_raw = udim_raw.clone();
		udim2_raw.extend_from_slice(&1.0_f32.to_le_bytes());
		udim2_raw.extend_from_slice(&34_i32.to_le_bytes());
		assert_eq!(
			decode_privileged_variant(VariantType::UDim2, &udim2_raw).unwrap(),
			Variant::UDim2(UDim2::new(UDim::new(0.5, -12), UDim::new(1.0, 34)))
		);

		assert_eq!(
			decode_privileged_variant(VariantType::BrickColor, &21_i32.to_le_bytes()).unwrap(),
			Variant::BrickColor(BrickColor::from_number(21).unwrap())
		);
		assert_eq!(
			decode_privileged_variant(VariantType::Axes, &5_i32.to_le_bytes()).unwrap(),
			Variant::Axes(Axes::from_bits(5).unwrap())
		);
		assert_eq!(
			decode_privileged_variant(VariantType::Faces, &34_i32.to_le_bytes()).unwrap(),
			Variant::Faces(Faces::from_bits(34).unwrap())
		);

		let mut region_raw = cframe_raw;
		region_raw.extend_from_slice(&native_f32s(&[8.0, 12.0, 16.0]));
		assert_eq!(region_raw.len(), 60);
		assert_eq!(
			decode_privileged_variant(VariantType::Region3, &region_raw).unwrap(),
			Variant::Region3(Region3::new(Vector3::new(0.0, 0.0, 0.0), Vector3::new(8.0, 12.0, 16.0)))
		);

		let mut number_sequence_raw = 2_i32.to_le_bytes().to_vec();
		number_sequence_raw.extend_from_slice(&native_f32s(&[0.0, 1.0, 0.25, 1.0, 2.0, 0.5]));
		assert_eq!(
			decode_privileged_variant(VariantType::NumberSequence, &number_sequence_raw).unwrap(),
			Variant::NumberSequence(NumberSequence {
				keypoints: vec![
					NumberSequenceKeypoint::new(0.0, 1.0, 0.25),
					NumberSequenceKeypoint::new(1.0, 2.0, 0.5),
				],
			})
		);

		let mut color_sequence_raw = 2_i32.to_le_bytes().to_vec();
		color_sequence_raw.extend_from_slice(&native_f32s(&[
			0.0, 1.0, 0.0, 0.0, 123.0, // engine-only fifth float is ignored
			1.0, 0.0, 0.0, 1.0, -456.0,
		]));
		assert_eq!(
			decode_privileged_variant(VariantType::ColorSequence, &color_sequence_raw).unwrap(),
			Variant::ColorSequence(ColorSequence {
				keypoints: vec![
					ColorSequenceKeypoint::new(0.0, Color3::new(1.0, 0.0, 0.0)),
					ColorSequenceKeypoint::new(1.0, Color3::new(0.0, 0.0, 1.0)),
				],
			})
		);
	}

	#[test]
	fn native_raw_decoder_rejects_invalid_masks_palette_and_sequence_lengths() {
		assert!(decode_privileged_variant(VariantType::Axes, &8_i32.to_le_bytes()).is_err());
		assert!(decode_privileged_variant(VariantType::Faces, &64_i32.to_le_bytes()).is_err());
		assert!(decode_privileged_variant(VariantType::BrickColor, &4_i32.to_le_bytes()).is_err());
		assert!(decode_privileged_variant(VariantType::NumberSequence, &(-1_i32).to_le_bytes()).is_err());
		assert!(decode_privileged_variant(VariantType::NumberSequence, &1_i32.to_le_bytes()).is_err());
		assert!(decode_privileged_variant(VariantType::ColorSequence, &[0, 0, 0, 0, 0]).is_err());
	}

	#[test]
	fn native_root_unique_id_equals_its_canonical_serialized_identity() {
		let expected = UniqueId::new(0xa1b2_c3d4, 0x1020_3040, -0x0123_4567_8123_4567);
		let mut raw = Vec::new();
		raw.extend_from_slice(&expected.random().to_be_bytes());
		raw.extend_from_slice(&expected.time().to_be_bytes());
		raw.extend_from_slice(&expected.index().to_be_bytes());
		assert_eq!(
			decode_privileged_variant(VariantType::UniqueId, &raw).unwrap(),
			Variant::UniqueId(expected)
		);
	}

	fn cross_root_model() -> (RootModel, Ref) {
		let mut dom = WeakDom::new(InstanceBuilder::new("DataModel").with_name("Fixture"));
		let parent = dom.root_ref();
		let visible = dom.insert(parent, InstanceBuilder::new("Folder").with_name("Visible"));
		let hidden = dom.insert(
			parent,
			InstanceBuilder::new("Folder")
				.with_name("Hidden")
				.with_property("UniqueId", UniqueId::new(7, 8, 9)),
		);
		dom.insert(
			visible,
			InstanceBuilder::new("ObjectValue")
				.with_name("VisibleToHidden")
				.with_property("Value", hidden),
		);
		dom.insert(
			hidden,
			InstanceBuilder::new("ObjectValue")
				.with_name("HiddenToVisible")
				.with_property("Value", visible),
		);
		let mut bytes = Vec::new();
		rbx_binary::to_writer(&mut bytes, &dom, &[visible, hidden]).unwrap();
		(
			RootModel {
				model: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes),
				roots: vec![
					crate::privileged_bridge::Root {
						class_name: "Folder".into(),
						name: "VisibleService".into(),
						debug_id: "visible-root".into(),
						initially_present: true,
					},
					crate::privileged_bridge::Root {
						class_name: "Folder".into(),
						name: "HiddenService".into(),
						debug_id: "hidden-root".into(),
						initially_present: true,
					},
				],
				model_root_parent_debug_ids: vec!["visible-root".into(), "hidden-root".into()],
				instance_debug_ids: vec![
					"visible-model".into(),
					"hidden-model".into(),
					"visible-value".into(),
					"hidden-value".into(),
				],
				root_property_carriers: HashMap::new(),
				root_property_carrier_instance_debug_ids: HashMap::new(),
				change_sequence: 41,
			},
			Ref::new(),
		)
	}

	fn hidden_root_template() -> SourceRootTemplate {
		let child = Ref::new();
		let grandchild = Ref::new();
		SourceRootTemplate {
			id: Ref::new(),
			class: Ustr::from("Folder"),
			properties: Properties::from_iter([
				(Ustr::from("Archivable"), Variant::Bool(true)),
				(Ustr::from("UniqueId"), Variant::UniqueId(UniqueId::new(10, 11, 12))),
				(
					Ustr::from("FutureSerializedProperty"),
					Variant::String("preserved".to_owned()),
				),
			]),
			descendants: vec![
				SourceDescendantTemplate {
					id: child,
					path: vec![0],
					class: Ustr::from("Folder"),
					unique_id: Some(UniqueId::new(7, 8, 9)),
				},
				SourceDescendantTemplate {
					id: grandchild,
					path: vec![0, 0],
					class: Ustr::from("ObjectValue"),
					unique_id: None,
				},
			],
		}
	}

	fn hidden_root_live() -> HashMap<String, LiveRootProperties> {
		HashMap::from([(
			"hidden-root".to_owned(),
			LiveRootProperties {
				name: "HiddenService".to_owned(),
				properties: Properties::from_iter([
					(Ustr::from("Archivable"), Variant::Bool(false)),
					(Ustr::from("UniqueId"), Variant::UniqueId(UniqueId::new(10, 11, 12))),
				]),
				raw_name: None,
				carrier_targets: HashMap::new(),
			},
		)])
	}

	fn live_reference_fixture(dom: &WeakDom) -> (Vec<ReferenceRequest>, Vec<Option<String>>) {
		let visible_value = dom
			.descendants()
			.find(|instance| instance.name == "VisibleToHidden")
			.unwrap()
			.referent();
		let hidden_value = dom
			.descendants()
			.find(|instance| instance.name == "HiddenToVisible")
			.unwrap()
			.referent();
		let root = |property: &str| ReferenceRequest {
			owner: ReferenceOwner::Root("hidden-root".to_owned()),
			canonical_name: Ustr::from(property),
			lookup: PropertyLookup {
				debug_id: "hidden-root".to_owned(),
				property: property.to_owned(),
			},
		};
		let model = |id, debug_id: &str| ReferenceRequest {
			owner: ReferenceOwner::Model(id),
			canonical_name: Ustr::from("Value"),
			lookup: PropertyLookup {
				debug_id: debug_id.to_owned(),
				property: "Value".to_owned(),
			},
		};
		(
			vec![
				root("NullRef"),
				root("VisibleServiceRef"),
				root("HiddenServiceRef"),
				root("HiddenChildRef"),
				root("VisibleChildRef"),
				model(hidden_value, "hidden-value"),
				model(visible_value, "visible-value"),
			],
			vec![
				None,
				Some("visible-root".to_owned()),
				Some("hidden-root".to_owned()),
				Some("hidden-model".to_owned()),
				Some("visible-model".to_owned()),
				Some("visible-root".to_owned()),
				Some("hidden-root".to_owned()),
			],
		)
	}

	#[test]
	fn hidden_root_decode_remaps_each_launch_and_preserves_cross_refs() {
		let (first_model, parent) = cross_root_model();
		let source_template = hidden_root_template();
		let source_refs = std::iter::once(parent)
			.chain(std::iter::once(source_template.id))
			.chain(source_template.descendants.iter().map(|entry| entry.id))
			.collect::<HashSet<_>>();
		let request = RootSnapshotsRequest {
			client_id: 7,
			source_generation: None,
			parent,
			debug_ids: vec!["hidden-root".into()],
			root_source_ids: HashMap::new(),
			source_instance_ids: HashMap::new(),
		};
		let first_dom = decode_root_model(&first_model).unwrap();
		let first_live = hidden_root_live();
		let (first_references, first_targets) = live_reference_fixture(&first_dom);
		let first = decode_root_snapshots(
			&request,
			first_model,
			first_dom,
			RootDecodeInputs {
				source_refs: source_refs.clone(),
				source_roots: vec![source_template.clone()],
				live_roots: first_live,
				reference_requests: first_references,
				reference_targets: first_targets,
			},
		)
		.unwrap();
		let (second_model, _) = cross_root_model();
		let second_dom = decode_root_model(&second_model).unwrap();
		let second_live = hidden_root_live();
		let (second_references, second_targets) = live_reference_fixture(&second_dom);
		let second = decode_root_snapshots(
			&request,
			second_model,
			second_dom,
			RootDecodeInputs {
				source_refs,
				source_roots: vec![source_template],
				live_roots: second_live,
				reference_requests: second_references,
				reference_targets: second_targets,
			},
		)
		.unwrap();

		assert_eq!(first.change_sequence, 41);
		assert_eq!(first.roots.len(), 1);
		let root_identity = first
			.source_instances
			.iter()
			.find(|entry| entry.debug_id == "hidden-root")
			.expect("hidden root native identity is missing");
		assert_eq!(root_identity.id, first.roots[0].snapshot.id);
		assert_eq!(root_identity.root_debug_id, "hidden-root");
		let child_identity = first
			.source_instances
			.iter()
			.find(|entry| entry.debug_id == "hidden-model")
			.expect("hidden descendant native identity is missing");
		assert_eq!(child_identity.id, first.roots[0].snapshot.children[0].id);
		assert_eq!(child_identity.root_debug_id, "hidden-root");
		assert_eq!(
			first.roots[0].snapshot.id, second.roots[0].snapshot.id,
			"a no-op reconnect must reuse the canonical hidden root source ID"
		);
		assert_eq!(
			first.roots[0].snapshot.children[0].id, second.roots[0].snapshot.children[0].id,
			"a no-op reconnect must reuse a hidden descendant source ID by authored UniqueId"
		);
		assert_eq!(
			first.roots[0].snapshot.children[0].children[0].id, second.roots[0].snapshot.children[0].children[0].id,
			"zero-UniqueId descendants need deterministic structural identity fallback"
		);
		assert_eq!(
			first.roots[0].snapshot.properties[&Ustr::from("FutureSerializedProperty")],
			Variant::String("preserved".to_owned()),
			"a source property from a newer reflection database must survive"
		);
		assert_eq!(
			first.roots[0].snapshot.properties[&Ustr::from("Archivable")],
			Variant::Bool(false),
			"a known source property must be replaced by its live value"
		);
		assert_eq!(
			first.roots[0].snapshot.properties[&Ustr::from("UniqueId")],
			Variant::UniqueId(UniqueId::new(10, 11, 12)),
			"the hidden service wrapper UniqueId must remain exact"
		);
		assert_eq!(
			first.roots[0].snapshot.children[0].properties[&Ustr::from("UniqueId")],
			Variant::UniqueId(UniqueId::new(7, 8, 9)),
			"serialized hidden descendants must retain their UniqueId"
		);
		assert_eq!(
			first.roots[0].snapshot.properties[&Ustr::from("NullRef")],
			Variant::Ref(Ref::none())
		);
		assert_eq!(
			first.roots[0].snapshot.properties[&Ustr::from("HiddenServiceRef")],
			Variant::Ref(first.roots[0].snapshot.id)
		);
		assert_eq!(
			first.roots[0].snapshot.properties[&Ustr::from("HiddenChildRef")],
			Variant::Ref(first.roots[0].snapshot.children[0].id)
		);
		let hidden_to_visible = &first.roots[0].snapshot.children[0].children[0];
		let Variant::Ref(external_target) = hidden_to_visible.properties[&Ustr::from("Value")] else {
			panic!("hidden-to-visible reference was not retained")
		};
		assert!(first
			.external_refs
			.iter()
			.any(|external| external.id == external_target && external.debug_id == "visible-root"));
		let visible_patch = first
			.reference_patches
			.iter()
			.find(|patch| patch.debug_id == "visible-value" && patch.property == Ustr::from("Value"))
			.expect("visible-to-hidden reference patch is missing");
		assert_eq!(visible_patch.value, Variant::Ref(first.roots[0].snapshot.id));
	}

	#[test]
	fn hidden_root_apply_materializes_the_authoritative_subtree_and_reference_plan() {
		let data_model = Ref::new();
		let hidden = Ref::new();
		let value = Ref::new();
		let tree = crate::core::tree::Tree::new(
			Snapshot::new()
				.with_id(data_model)
				.with_class("DataModel")
				.with_name("Fixture")
				.with_children(vec![Snapshot::new()
					.with_id(hidden)
					.with_class("Folder")
					.with_name("Hidden")
					.with_children(vec![Snapshot::new()
						.with_id(value)
						.with_class("ObjectValue")
						.with_name("BackToRoot")
						.with_properties(Properties::from_iter([(
							Ustr::from("Value"),
							Variant::Ref(hidden),
						)]))])]),
		);
		let prepared = prepare_hidden_root_apply(
			&tree,
			&ApplyHiddenRoot {
				debug_id: "hidden-debug".to_owned(),
				source_id: hidden,
			},
		)
		.unwrap();
		assert_eq!(prepared.source_ids, vec![hidden, value]);
		assert_eq!(prepared.references.len(), 1);
		assert_eq!(prepared.references[0].owner, value);
		assert_eq!(prepared.references[0].property, "Value");
		assert_eq!(prepared.references[0].target, Some(hidden));
		let model = prepare_hidden_root_bundle(&tree, std::slice::from_ref(&prepared)).unwrap();
		let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, model).unwrap();
		let decoded = rbx_binary::from_reader(bytes.as_slice()).unwrap();
		assert_eq!(decoded.root().children().len(), 1);
		let root = decoded.get_by_ref(decoded.root().children()[0]).unwrap();
		assert_eq!(root.class.as_str(), "Folder");
		assert_eq!(root.name, "Hidden");
		assert_eq!(root.children().len(), 1);
	}

	#[test]
	fn hidden_root_apply_resets_an_omitted_root_reference_to_its_default() {
		let data_model = Ref::new();
		let hidden = Ref::new();
		let tree = crate::core::tree::Tree::new(
			Snapshot::new()
				.with_id(data_model)
				.with_class("DataModel")
				.with_children(vec![Snapshot::new()
					.with_id(hidden)
					.with_class("ObjectValue")
					.with_name("HiddenReferenceRoot")]),
		);
		let prepared = prepare_hidden_root_apply(
			&tree,
			&ApplyHiddenRoot {
				debug_id: "hidden-reference-root".to_owned(),
				source_id: hidden,
			},
		)
		.unwrap();
		assert!(
			prepared.references.iter().any(|reference| {
				reference.owner == hidden && reference.property == "Value" && reference.target.is_none()
			}),
			"an omitted root Ref must explicitly clear the old live target"
		);
	}

	#[test]
	fn external_visible_reference_apply_uses_authoritative_target_and_reset() {
		let data_model = Ref::new();
		let hidden = Ref::new();
		let owner = Ref::new();
		let tree = crate::core::tree::Tree::new(
			Snapshot::new()
				.with_id(data_model)
				.with_class("DataModel")
				.with_children(vec![
					Snapshot::new().with_id(hidden).with_class("Folder").with_name("Hidden"),
					Snapshot::new()
						.with_id(owner)
						.with_class("ObjectValue")
						.with_name("VisibleOwner")
						.with_properties(Properties::from_iter([(Ustr::from("Value"), Variant::Ref(hidden))])),
				]),
		);
		let request = ApplyReferenceRequest {
			owner_source_id: owner,
			property: Ustr::from("Value"),
		};
		let prepared = prepare_external_reference(&tree, &request).unwrap();
		assert_eq!(prepared.owner, owner);
		assert_eq!(prepared.property, "Value");
		assert_eq!(prepared.target, Some(hidden));

		let reset_tree = crate::core::tree::Tree::new(
			Snapshot::new()
				.with_id(data_model)
				.with_class("DataModel")
				.with_children(vec![Snapshot::new()
					.with_id(owner)
					.with_class("ObjectValue")
					.with_name("VisibleOwner")]),
		);
		assert_eq!(prepare_external_reference(&reset_tree, &request).unwrap().target, None);
	}

	#[test]
	fn native_rebuild_rewires_unchanged_incoming_visible_reference() {
		let data_model = Ref::new();
		let hidden = Ref::new();
		let child = Ref::new();
		let visible_owner = Ref::new();
		let tree = crate::core::tree::Tree::new(
			Snapshot::new()
				.with_id(data_model)
				.with_class("DataModel")
				.with_children(vec![
					Snapshot::new()
						.with_id(hidden)
						.with_class("Folder")
						.with_name("Hidden")
						.with_children(vec![Snapshot::new()
							.with_id(child)
							.with_class("Folder")
							.with_name("Child")]),
					Snapshot::new()
						.with_id(visible_owner)
						.with_class("ObjectValue")
						.with_name("UnchangedIncoming")
						.with_properties(Properties::from_iter([(Ustr::from("Value"), Variant::Ref(child))])),
				]),
		);
		let mut references = Vec::new();
		append_incoming_references(&tree, &HashSet::from([hidden, child]), &mut references).unwrap();
		assert_eq!(references.len(), 1);
		assert_eq!(references[0].owner, visible_owner);
		assert_eq!(references[0].property, "Value");
		assert_eq!(references[0].target, Some(child));
	}

	#[test]
	fn native_model_policy_matches_identity_and_runtime_cache_exclusions() {
		assert!(!include_native_model_property("UnionOperation", "TriangleCount"));
		assert!(include_native_model_property("Part", "RotVelocity"));
		assert!(serialized_property_candidates("Part")
			.unwrap()
			.iter()
			.any(|candidate| candidate.canonical_name == Ustr::from("RotVelocity")
				&& candidate.data_type == VariantType::Vector3));
		for (class, property) in [
			("Model", "SlimHash"),
			("WrapLayer", "TemporaryReferenceId"),
			("ReflectionMetadataItem", "FFlag"),
		] {
			assert!(
				include_native_model_property(class, property),
				"authored values must not be excluded by unproven name heuristics: {class}.{property}"
			);
		}
		assert!(include_native_model_property("ObjectValue", "Value"));
		for (class, property) in [
			("AuroraScriptObject", "BehaviorWeak"),
			("AuroraScriptObject", "BoundInstanceWeak"),
			("CoreGui", "SelectionImageObject"),
			("StarterGui", "StudioDefaultStyleSheet"),
			("StarterGui", "StudioInsertWidgetLayerCollectorAutoLinkStyleSheet"),
		] {
			assert!(include_native_model_property(class, property), "{class}.{property}");
		}
		assert!(include_native_model_property("Folder", "HistoryId"));
		assert!(include_native_model_property("Folder", "UniqueId"));
		assert!(include_native_model_property(
			"StudioData",
			"EnableScriptCollabByDefaultOnLoad"
		));
		assert!(include_native_model_property(
			"PlayerEmulatorService",
			"SerializedEmulatedPolicyInfo"
		));
	}

	#[test]
	fn hidden_root_candidates_use_canonical_types_and_serialized_engine_names() {
		let instance = serialized_property_candidates("Instance").unwrap();
		let attributes = instance
			.iter()
			.find(|candidate| candidate.canonical_name == Ustr::from("Attributes"))
			.unwrap();
		assert_eq!(attributes.request_name, "AttributesSerialize");
		assert_eq!(attributes.data_type, VariantType::Attributes);
		assert!(instance.iter().any(|candidate| {
			candidate.canonical_name == Ustr::from("Tags") && candidate.data_type == VariantType::Tags
		}));
		assert!(serialized_property_candidates("StudioData")
			.unwrap()
			.iter()
			.any(|candidate| { candidate.canonical_name == Ustr::from("EnableScriptCollabByDefaultOnLoad") }));
		assert!(serialized_property_candidates("PlayerEmulatorService")
			.unwrap()
			.iter()
			.any(|candidate| candidate.canonical_name == Ustr::from("SerializedEmulatedPolicyInfo")));
		assert!(serialized_property_candidates("VoiceChatService")
			.unwrap()
			.iter()
			.any(
				|candidate| candidate.canonical_name == Ustr::from("EnableVoiceVolumeControls")
					&& candidate.data_type == VariantType::Enum
			));
		let workspace = serialized_property_candidates("Workspace").unwrap();
		let collision_group_data = workspace
			.iter()
			.find(|candidate| candidate.request_name == "CollisionGroupData")
			.expect("Workspace.CollisionGroupData should be included in root capture");
		assert_eq!(collision_group_data.canonical_name, Ustr::from("CollisionGroupData"));
		assert_eq!(collision_group_data.data_type, VariantType::BinaryString);
		assert!(workspace.iter().all(|candidate| {
			candidate.request_name != "CollisionGroups" && candidate.canonical_name != Ustr::from("CollisionGroups")
		}));
	}

	#[test]
	fn studio_0729_class_overlay_inherits_instance_candidates() {
		let inherited = serialized_property_candidates("Instance").unwrap();
		let signature = |candidates: &[SerializedPropertyCandidate]| {
			candidates
				.iter()
				.map(|candidate| {
					(
						candidate.canonical_name,
						candidate.request_name.clone(),
						candidate.data_type,
					)
				})
				.collect::<Vec<_>>()
		};
		for class in ["DeviceDisplayService", "DisplayWakeLock", "PopLatencyService"] {
			let candidates = serialized_property_candidates(class).unwrap();
			assert_eq!(signature(&candidates), signature(&inherited), "{class}");
		}
		assert!(serialized_property_candidates("__CarbonUnknownClass").is_err());
	}

	#[test]
	fn collision_group_data_uses_its_native_binary_model_property() {
		let payload = b"exact-collision-groups\0payload".to_vec();
		let mut dom = WeakDom::new(InstanceBuilder::new("DataModel").with_name("Fixture"));
		let wrapper = dom.insert(
			dom.root_ref(),
			InstanceBuilder::new("Folder").with_name("owner-debug-id"),
		);
		dom.insert(
			wrapper,
			InstanceBuilder::new("Workspace")
				.with_name("Workspace")
				.with_property("CollisionGroupData", BinaryString::from(payload.clone())),
		);
		let mut bytes = Vec::new();
		rbx_binary::to_writer(&mut bytes, &dom, &[wrapper]).unwrap();
		let model = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
		let value = decode_serialized_property(&model, "owner-debug-id", "CollisionGroupData").unwrap();
		assert_eq!(serialized_property_bytes(&value).unwrap(), payload);
	}

	#[test]
	fn persistent_read_only_materialization_is_exact_and_fail_closed() {
		for (class_name, property) in [
			("Chat", "LoadDefaultChat"),
			("HttpService", "HttpEnabled"),
			("Lighting", "LightingStyle"),
			("Lighting", "PrioritizeLightingQuality"),
			("MeshPart", "HasJointOffset"),
			("MeshPart", "HasSkinnedMesh"),
			("MeshPart", "JointOffset"),
			("MeshPart", "MeshContent"),
			("PackageLink", "DefaultName"),
			("PackageLink", "PackageContent"),
			("Players", "MaxPlayers"),
			("Players", "PreferredPlayers"),
			("StarterPlayer", "AllowCustomAnimations"),
			("TextChatService", "ChatVersion"),
		] {
			assert!(is_persistent_read_only_property(class_name, property));
		}
		assert!(!is_persistent_read_only_property("AudioRecorder", "IsRecording"));
		assert!(!is_persistent_read_only_property("PartOperation", "TriangleCount"));
		assert!(!is_persistent_read_only_property("Folder", "LoadDefaultChat"));

		assert_eq!(
			materialized_property_value("Bool", b"true").unwrap(),
			Variant::Bool(true)
		);
		assert_eq!(materialized_property_value("Int32", b"37").unwrap(), Variant::Int32(37));
		assert_eq!(
			materialized_property_value("Enum", b"2").unwrap(),
			Variant::Enum(Enum::from_u32(2))
		);
		assert_eq!(
			materialized_property_value("String", b"Carbon package").unwrap(),
			Variant::String("Carbon package".to_owned())
		);

		let Variant::Content(none) = materialized_property_value("Content", &[0]).unwrap() else {
			panic!("None wire tag did not decode to Content");
		};
		assert!(matches!(none.value(), ContentType::None));
		let Variant::Content(empty_uri) = materialized_property_value("Content", &[1]).unwrap() else {
			panic!("URI wire tag did not decode to Content");
		};
		assert!(matches!(empty_uri.value(), ContentType::Uri(uri) if uri.is_empty()));
		let Variant::Content(uri) = materialized_property_value("Content", b"\x01rbxassetid://123").unwrap() else {
			panic!("URI wire tag did not decode to Content");
		};
		assert!(matches!(uri.value(), ContentType::Uri(uri) if uri == "rbxassetid://123"));
		assert!(materialized_property_value("Content", &[2]).is_err());
		assert!(materialized_property_value("Content", b"").is_err());

		assert!(materialized_property_model("Chat", "LoadDefaultChat", "Bool", b"true").is_ok());
		assert!(materialized_property_model("MeshPart", "MeshContent", "Content", &[0]).is_ok());
		let empty_uri_model = materialized_property_model("PackageLink", "PackageContent", "Content", b"\x01").unwrap();
		let empty_uri_dom = rbx_binary::from_reader(std::io::Cursor::new(empty_uri_model)).unwrap();
		let empty_uri_owner = empty_uri_dom.get_by_ref(empty_uri_dom.root().children()[0]).unwrap();
		let Variant::Content(empty_uri) = empty_uri_owner
			.properties
			.get(&Ustr::from("PackageContent"))
			.or_else(|| empty_uri_owner.properties.get(&Ustr::from("PackageContentSerialize")))
			.expect("materialized PackageContent is missing")
		else {
			panic!("materialized PackageContent changed type");
		};
		assert!(matches!(empty_uri.value(), ContentType::Uri(uri) if uri.is_empty()));
		let invalid_string_model =
			materialized_property_model("PackageLink", "DefaultName", "String", b"\xffname").unwrap();
		let invalid_string_dom = rbx_binary::from_reader(std::io::Cursor::new(invalid_string_model)).unwrap();
		let invalid_string_owner = invalid_string_dom
			.get_by_ref(invalid_string_dom.root().children()[0])
			.unwrap();
		let Variant::BinaryString(invalid_string) = invalid_string_owner
			.properties
			.get(&Ustr::from("DefaultName"))
			.expect("materialized DefaultName is missing")
		else {
			panic!("invalid-UTF8 DefaultName did not retain binary bytes");
		};
		let invalid_string_bytes: &[u8] = invalid_string.as_ref();
		assert_eq!(invalid_string_bytes, b"\xffname");
		assert!(materialized_property_model("Folder", "LoadDefaultChat", "Bool", b"true").is_err());
		assert!(materialized_property_model("AudioRecorder", "IsRecording", "Bool", b"true").is_err());
	}

	#[test]
	fn net_asset_ref_identity_wrapper_decodes_exact_owner_property() {
		let payload = b"exact-net-asset-ref\0payload".to_vec();
		let mut dom = WeakDom::new(InstanceBuilder::new("DataModel").with_name("Fixture"));
		let wrapper = dom.insert(
			dom.root_ref(),
			InstanceBuilder::new("Folder").with_name("owner-debug-id"),
		);
		dom.insert(
			wrapper,
			InstanceBuilder::new("UnionOperation")
				.with_name("Owner")
				.with_property("SolidMeshHolder", NetAssetRef::new(payload.clone())),
		);
		let mut bytes = Vec::new();
		rbx_binary::to_writer(&mut bytes, &dom, &[wrapper]).unwrap();
		let model = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
		let value = decode_serialized_property(&model, "owner-debug-id", "SolidMeshHolder").unwrap();
		let encoded = base64::Engine::encode(
			&base64::engine::general_purpose::STANDARD,
			serialized_property_bytes(&value).unwrap(),
		);
		assert_eq!(
			base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded).unwrap(),
			payload
		);
	}

	#[test]
	fn current_service_root_content_properties_are_covered_by_the_exact_carrier() {
		let database = crate::util::get_reflection_database();
		let mut direct_content = Vec::new();
		for (class_name, class) in &database.classes {
			if !class.tags.contains(&ClassTag::Service) {
				continue;
			}
			for candidate in serialized_property_candidates(class_name).unwrap() {
				if candidate.data_type == VariantType::Content {
					direct_content.push(format!("{class_name}.{}", candidate.canonical_name));
				}
			}
		}
		assert_eq!(direct_content, ["UserInputService.MouseIconContent"]);
	}

	#[test]
	fn marker_like_authored_folders_are_not_filtered_or_interpreted_as_identity_transport() {
		let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
		let parent = dom.root_ref();
		let authored = dom.insert(
			parent,
			InstanceBuilder::new("Folder").with_name("__CarbonServiceReferenceMarkers"),
		);
		let marker_name = "__CarbonServiceReferenceMarkers:unique-capture";
		let marker = dom.insert(parent, InstanceBuilder::new("Folder").with_name(marker_name));
		let child = dom.insert(
			marker,
			InstanceBuilder::new("ObjectValue")
				.with_name("service-root")
				.with_property("Value", Ref::new()),
		);
		let response = RootModel {
			model: String::new(),
			roots: Vec::new(),
			model_root_parent_debug_ids: Vec::new(),
			instance_debug_ids: Vec::new(),
			root_property_carriers: HashMap::new(),
			root_property_carrier_instance_debug_ids: HashMap::new(),
			change_sequence: 0,
		};

		assert_eq!(serialized_model_roots(&dom, &response).unwrap(), [authored, marker]);
		assert_eq!(
			serialized_model_instances(&dom, &response)
				.unwrap()
				.into_iter()
				.collect::<HashSet<_>>(),
			HashSet::from([authored, marker, child])
		);
	}

	#[test]
	fn root_content_carrier_preserves_durable_content_without_fabricating_external_identity() {
		let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
		let parent = dom.root_ref();
		let _visible = dom.insert(parent, InstanceBuilder::new("Folder").with_name("VisibleModel"));
		let hidden = dom.insert(parent, InstanceBuilder::new("Folder").with_name("HiddenModel"));
		dom.insert(hidden, InstanceBuilder::new("Folder").with_name("HiddenChild"));
		let carrier_name = "__unique-root-carrier";
		let carrier = dom.insert(parent, InstanceBuilder::new("Folder").with_name(carrier_name));
		let carrier_clone = dom.insert(carrier, InstanceBuilder::new("Folder").with_name("HiddenService"));
		let carrier_clone_child = dom.insert(carrier_clone, InstanceBuilder::new("Folder").with_name("HiddenChild"));

		let response = RootModel {
			model: String::new(),
			roots: vec![
				crate::privileged_bridge::Root {
					class_name: "Folder".to_owned(),
					name: "VisibleService".to_owned(),
					debug_id: "visible-root".to_owned(),
					initially_present: true,
				},
				crate::privileged_bridge::Root {
					class_name: "Folder".to_owned(),
					name: "HiddenService".to_owned(),
					debug_id: "hidden-root".to_owned(),
					initially_present: true,
				},
			],
			model_root_parent_debug_ids: vec!["visible-root".to_owned(), "hidden-root".to_owned()],
			instance_debug_ids: vec![
				"visible-model".to_owned(),
				"hidden-model".to_owned(),
				"hidden-child".to_owned(),
			],
			root_property_carriers: HashMap::from([("hidden-root".to_owned(), carrier_name.to_owned())]),
			root_property_carrier_instance_debug_ids: HashMap::from([(
				"hidden-root".to_owned(),
				vec!["hidden-root".to_owned(), "hidden-child".to_owned()],
			)]),
			change_sequence: 10,
		};
		let request = RootSnapshotsRequest {
			client_id: 1,
			source_generation: None,
			parent: Ref::new(),
			debug_ids: vec!["hidden-root".to_owned()],
			root_source_ids: HashMap::new(),
			source_instance_ids: HashMap::new(),
		};
		let live_roots = HashMap::from([(
			"hidden-root".to_owned(),
			LiveRootProperties {
				name: "HiddenService".to_owned(),
				raw_name: None,
				properties: Properties::from_iter([
					(
						Ustr::from("SelfContent"),
						Variant::Content(Content::from_referent(carrier_clone)),
					),
					(
						Ustr::from("ChildContent"),
						Variant::Content(Content::from_referent(carrier_clone_child)),
					),
					(Ustr::from("ServiceContent"), Variant::Content(Content::none())),
					(
						Ustr::from("UriContent"),
						Variant::Content(Content::from_uri("rbxassetid://123")),
					),
					(Ustr::from("NoneContent"), Variant::Content(Content::none())),
				]),
				carrier_targets: HashMap::from([
					(carrier_clone, "hidden-root".to_owned()),
					(carrier_clone_child, "hidden-child".to_owned()),
				]),
			},
		)]);

		let decoded = decode_root_snapshots(
			&request,
			response,
			dom,
			RootDecodeInputs {
				source_refs: HashSet::from([request.parent]),
				source_roots: Vec::new(),
				live_roots,
				reference_requests: Vec::new(),
				reference_targets: Vec::new(),
			},
		)
		.unwrap();

		let snapshot = &decoded.roots[0].snapshot;
		let content_target = |property: &str| {
			let Variant::Content(content) = &snapshot.properties[&Ustr::from(property)] else {
				panic!("{property} is not Content");
			};
			content.as_object().expect("Content.Object target is missing")
		};
		assert_eq!(content_target("SelfContent"), snapshot.id);
		assert_eq!(content_target("ChildContent"), snapshot.children[0].children[0].id);
		assert_eq!(
			snapshot.properties[&Ustr::from("ServiceContent")],
			Variant::Content(Content::none())
		);
		assert!(decoded.external_refs.is_empty());
		assert_eq!(
			snapshot.properties[&Ustr::from("UriContent")],
			Variant::Content(Content::from_uri("rbxassetid://123"))
		);
		assert_eq!(
			snapshot.properties[&Ustr::from("NoneContent")],
			Variant::Content(Content::none())
		);
		assert_eq!(
			snapshot.children.len(),
			1,
			"the carrier wrapper leaked into the authored hierarchy"
		);
		assert_eq!(snapshot.children[0].name, "HiddenModel");
	}

	#[test]
	fn source_template_uses_stable_source_identity_across_a_rename() {
		let first_id = Ref::new();
		let second_id = Ref::new();
		let request = RootSnapshotsRequest {
			client_id: 1,
			source_generation: None,
			parent: Ref::new(),
			debug_ids: vec!["hidden-root".to_owned()],
			root_source_ids: HashMap::from([("hidden-root".to_owned(), second_id)]),
			source_instance_ids: HashMap::new(),
		};
		let root = crate::privileged_bridge::Root {
			class_name: "Folder".to_owned(),
			name: "RenamedInStudio".to_owned(),
			debug_id: "hidden-root".to_owned(),
			initially_present: true,
		};
		let templates = [
			SourceRootTemplate {
				id: first_id,
				class: Ustr::from("Folder"),
				properties: Properties::from_iter([(Ustr::from("Marker"), Variant::Int32(1))]),
				descendants: Vec::new(),
			},
			SourceRootTemplate {
				id: second_id,
				class: Ustr::from("Folder"),
				properties: Properties::from_iter([(Ustr::from("Marker"), Variant::Int32(2))]),
				descendants: Vec::new(),
			},
		];
		let selected = source_root_template(&request, &root, &templates).unwrap().unwrap();
		assert_eq!(selected.properties[&Ustr::from("Marker")], Variant::Int32(2));
	}

	#[test]
	fn hidden_snapshot_templates_keep_forward_properties_and_descendant_unique_ids() {
		let data_model = Ref::new();
		let service = Ref::new();
		let descendant = Ref::new();
		let unique_id = UniqueId::new(7, 8, 9);
		let tree = crate::core::tree::Tree::new(
			Snapshot::new()
				.with_id(data_model)
				.with_class("DataModel")
				.with_children(vec![Snapshot::new()
					.with_id(service)
					.with_class("VoiceChatService")
					.with_properties(Properties::from_iter([(
						Ustr::from("EnableVoiceVolumeControls"),
						Variant::Bool(true),
					)]))
					.with_children(vec![Snapshot::new()
						.with_id(descendant)
						.with_class("Folder")
						.with_properties(Properties::from_iter([(
							Ustr::from("UniqueId"),
							Variant::UniqueId(unique_id),
						)]))])]),
		);
		let (source_refs, templates) = source_root_decode_templates(&tree, data_model).unwrap();
		assert!(source_refs.contains(&descendant));
		assert_eq!(
			templates[0].properties[&Ustr::from("EnableVoiceVolumeControls")],
			Variant::Bool(true),
			"forward-version root properties must come from the full source store"
		);
		assert_eq!(templates[0].descendants[0].unique_id, Some(unique_id));
	}

	#[test]
	fn bridge_observer_warnings_fail_the_polled_change_request_explicitly() {
		let response = Changes {
			changes: Vec::new(),
			diagnostics: vec![BridgeDiagnostic {
				sequence: 17,
				severity: "Warning".to_owned(),
				message: "managed identity is duplicated".to_owned(),
			}],
		};
		let error = ensure_no_bridge_diagnostics(&response).unwrap_err().to_string();
		assert!(error.contains("observer diagnostic"));
	}
}
