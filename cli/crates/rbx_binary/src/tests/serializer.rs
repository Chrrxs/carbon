use std::{collections::BTreeMap, io::Write};

use rbx_dom_weak::{
	types::{
		BinaryString, BrickColor, CFrame, Color3, Color3uint8, Content, Enum, Font, Matrix3, Ref, Region3,
		SharedString, Variant, Vector3,
	},
	InstanceBuilder, Ustr, WeakDom,
};

use crate::{
	chunk::{Chunk, ChunkBuilder},
	core::RbxReadExt,
	from_reader,
	serializer::CompressionType,
	text_deserializer::{DecodedChunk, DecodedModel},
	to_writer, Deserializer, Serializer,
};

const BINARY_HEADER_LEN: usize = 32;

fn rewrite_chunks(buffer: &[u8], mut rewrite: impl FnMut(&mut [u8; 4], &mut Vec<u8>)) -> Vec<u8> {
	let mut input = &buffer[BINARY_HEADER_LEN..];
	let mut output = buffer[..BINARY_HEADER_LEN].to_vec();
	loop {
		let mut chunk = Chunk::decode(&mut input).expect("failed to decode fixture chunk");
		rewrite(&mut chunk.name, &mut chunk.data);
		let name: &'static [u8] = match &chunk.name {
			b"META" => b"META",
			b"SSTR" => b"SSTR",
			b"INST" => b"INST",
			b"PROP" => b"PROP",
			b"PRNT" => b"PRNT",
			b"END\0" => b"END\0",
			b"FUTR" => b"FUTR",
			name => panic!("unexpected fixture chunk {:?}", name),
		};
		let end = name == b"END\0";
		let mut builder = ChunkBuilder::new(name, CompressionType::None);
		builder.write_all(&chunk.data).unwrap();
		builder.dump(&mut output).unwrap();
		if end {
			break;
		}
	}
	output
}

fn mutate_property_chunk(buffer: &[u8], target: &str, mut mutate: impl FnMut(&mut Vec<u8>, usize)) -> Vec<u8> {
	let mut changed = false;
	let output = rewrite_chunks(buffer, |name, data| {
		if changed || name != b"PROP" {
			return;
		}
		let mut remaining = data.as_slice();
		remaining.read_le_u32().unwrap();
		let property = remaining.read_string().unwrap();
		if property != target {
			return;
		}
		let type_offset = data.len() - remaining.len();
		mutate(data, type_offset);
		changed = true;
	});
	assert!(changed, "fixture property {} was not serialized", target);
	output
}

#[test]
fn metadata_is_decoded_and_serialized_canonically() {
	let tree = WeakDom::new(InstanceBuilder::new("Folder"));
	let metadata = BTreeMap::from([
		("ExplicitAutoJoints".to_owned(), "true".to_owned()),
		("AuthoringTool".to_owned(), "Carbon regression".to_owned()),
	]);
	let mut buffer = Vec::new();
	Serializer::new()
		.metadata(metadata.clone())
		.serialize(&mut buffer, &tree, &[tree.root_ref()])
		.expect("failed to encode metadata");

	let structure = Deserializer::new()
		.deserialize_structure(buffer.as_slice())
		.expect("failed to decode metadata");
	assert_eq!(structure.metadata(), &metadata);
	let decoded = DecodedModel::from_reader(buffer.as_slice());
	let entries = decoded.chunks.into_iter().find_map(|chunk| match chunk {
		DecodedChunk::Meta { entries, .. } => Some(entries),
		_ => None,
	});
	assert_eq!(
		entries,
		Some(vec![
			("AuthoringTool".to_owned(), "Carbon regression".to_owned()),
			("ExplicitAutoJoints".to_owned(), "true".to_owned()),
		])
	);
}

/// A basic test to make sure we can serialize the simplest instance: a Folder.
#[test]
fn just_folder() {
	let tree = WeakDom::new(InstanceBuilder::new("Folder"));
	let mut buffer = Vec::new();

	to_writer(&mut buffer, &tree, &[tree.root_ref()]).expect("failed to encode model");

	let decoded = DecodedModel::from_reader(buffer.as_slice());
	insta::assert_yaml_snapshot!(decoded);
}

#[test]
fn parallel_property_encoding_is_byte_identical_to_serial_encoding() {
	let tree = WeakDom::new(InstanceBuilder::new("Folder").with_children((0..20_000).map(|index| {
		InstanceBuilder::new("Part")
			.with_name(format!("Part{index}"))
			.with_property("Anchored", index % 2 == 0)
			.with_property("Color", Color3::new(0.25, 0.5, 0.75))
			.with_property("Size", Vector3::new(4.0, 1.0, 2.0))
			.with_property("Transparency", (index % 100) as f32 / 100.0)
	})));
	let roots = tree.root().children();
	let mut serial = Vec::new();
	let serial_report = Serializer::new()
		.property_workers(1)
		.serialize_source_with_report(&mut serial, &tree, roots)
		.unwrap();
	let mut parallel = Vec::new();
	let parallel_report = Serializer::new()
		.property_workers(4)
		.serialize_source_with_report(&mut parallel, &tree, roots)
		.unwrap();

	assert_eq!(parallel, serial);
	assert_eq!(parallel_report.properties, serial_report.properties);
	assert_eq!(parallel_report.property_columns, serial_report.property_columns);
}

#[test]
fn studio_0729_unknown_classes_use_their_exact_object_format() {
	let tree = WeakDom::new(
		InstanceBuilder::new("Folder")
			.with_child(InstanceBuilder::new("DeviceDisplayService").with_name("Device display"))
			.with_child(InstanceBuilder::new("DisplayWakeLock").with_name("Wake lock"))
			.with_child(InstanceBuilder::new("PopLatencyService").with_name("Pop latency")),
	);
	let mut buffer = Vec::new();
	to_writer(&mut buffer, &tree, tree.root().children()).expect("failed to encode 0.729 class overlay");

	let decoded = DecodedModel::from_reader(buffer.as_slice());
	let formats = decoded
		.chunks
		.into_iter()
		.filter_map(|chunk| match chunk {
			DecodedChunk::Inst {
				type_name,
				object_format,
				remaining,
				..
			} => Some((type_name, (object_format, remaining))),
			_ => None,
		})
		.collect::<BTreeMap<_, _>>();
	assert_eq!(formats.get("DeviceDisplayService"), Some(&(1, vec![1])));
	assert_eq!(formats.get("DisplayWakeLock"), Some(&(0, Vec::new())));
	assert_eq!(formats.get("PopLatencyService"), Some(&(1, vec![1])));

	let roundtrip = from_reader(buffer.as_slice()).expect("failed to decode 0.729 class overlay");
	let roots = roundtrip
		.root()
		.children()
		.iter()
		.map(|referent| {
			let instance = roundtrip.get_by_ref(*referent).unwrap();
			(instance.class.to_string(), instance.name.clone())
		})
		.collect::<Vec<_>>();
	assert_eq!(
		roots,
		[
			("DeviceDisplayService".to_owned(), "Device display".to_owned()),
			("DisplayWakeLock".to_owned(), "Wake lock".to_owned()),
			("PopLatencyService".to_owned(), "Pop latency".to_owned()),
		]
	);
}

#[test]
fn preserves_near_axis_aligned_cframes_exactly() {
	let epsilon = -4.371_139e-8;
	let value = CFrame::new(
		Vector3::new(43.70214, 25.635706, -332.19376),
		Matrix3::new(
			Vector3::new(0.0, epsilon, -1.0),
			Vector3::new(1.0, 0.0, 0.0),
			Vector3::new(0.0, -1.0, -epsilon),
		),
	);
	let tree = WeakDom::new(
		InstanceBuilder::new("Folder")
			.with_child(InstanceBuilder::new("Part").with_property("CFrame", value))
			.with_child(
				InstanceBuilder::new("Model").with_property("WorldPivotData", Variant::OptionalCFrame(Some(value))),
			),
	);
	let mut buffer = Vec::new();
	to_writer(&mut buffer, &tree, tree.root().children()).expect("failed to encode model");
	let decoded = from_reader(buffer.as_slice()).expect("failed to decode model");
	let part = decoded.get_by_ref(decoded.root().children()[0]).unwrap();
	let model = decoded.get_by_ref(decoded.root().children()[1]).unwrap();
	assert_eq!(
		part.properties.get(&Ustr::from("CFrame")),
		Some(&Variant::CFrame(value))
	);
	assert_eq!(
		model.properties.get(&Ustr::from("WorldPivotData")),
		Some(&Variant::OptionalCFrame(Some(value)))
	);
}

#[test]
fn preserves_multiple_content_object_targets_in_property_order() {
	let first_target = InstanceBuilder::new("Folder").with_name("FirstTarget");
	let first_target_ref = first_target.referent();
	let second_target = InstanceBuilder::new("Folder").with_name("SecondTarget");
	let second_target_ref = second_target.referent();
	let tree = WeakDom::new(
		InstanceBuilder::new("Folder")
			.with_child(first_target)
			.with_child(second_target)
			.with_child(
				InstanceBuilder::new("AdGui")
					.with_name("FirstOwner")
					.with_property("FallbackImageContent", Content::from_referent(first_target_ref)),
			)
			.with_child(
				InstanceBuilder::new("AdGui")
					.with_name("SecondOwner")
					.with_property("FallbackImageContent", Content::from_referent(second_target_ref)),
			),
	);
	let mut buffer = Vec::new();
	to_writer(&mut buffer, &tree, tree.root().children()).expect("failed to encode Content.Object model");
	let decoded = from_reader(buffer.as_slice()).expect("failed to decode Content.Object model");
	let target_name = |owner_name: &str| {
		let owner = decoded
			.descendants()
			.find(|instance| instance.name == owner_name)
			.unwrap();
		let Variant::Content(content) = &owner.properties[&Ustr::from("FallbackImageContent")] else {
			panic!("Content.Object changed type");
		};
		decoded
			.get_by_ref(content.as_object().expect("Content.Object target was cleared"))
			.unwrap()
			.name
			.clone()
	};
	assert_eq!(target_name("FirstOwner"), "FirstTarget");
	assert_eq!(target_name("SecondOwner"), "SecondTarget");
}

/// Ensures that a tree containing some instances with a value and others
/// without will correctly fall back to (some) default value.
#[test]
fn partially_present() {
	let tree = WeakDom::new(InstanceBuilder::new("Folder").with_children(vec![
		// This instance's `Value` property should be preserved.
		InstanceBuilder::new("StringValue").with_property("Value", "Hello"),
		// This instance's `Value` property should be the empty string.
		InstanceBuilder::new("StringValue"),
	]));

	let root_refs = tree.root().children();

	let mut buffer = Vec::new();
	to_writer(&mut buffer, &tree, root_refs).expect("failed to encode model");

	let decoded = DecodedModel::from_reader(buffer.as_slice());
	insta::assert_yaml_snapshot!(decoded);
}

#[test]
fn preserves_non_utf8_string_property_bytes() {
	let bytes = vec![0x66, 0x80, 0x6f, 0xff];
	let tree = WeakDom::new(
		InstanceBuilder::new("Folder")
			.with_child(InstanceBuilder::new("StringValue").with_property("Value", BinaryString::from(bytes.clone()))),
	);
	let mut buffer = Vec::new();

	to_writer(&mut buffer, &tree, tree.root().children()).expect("failed to encode model");
	let decoded = from_reader(buffer.as_slice()).expect("failed to decode model");
	let string_value = decoded.get_by_ref(decoded.root().children()[0]).unwrap();

	assert_eq!(
		string_value.properties.get(&Ustr::from("Value")),
		Some(&Variant::BinaryString(BinaryString::from(bytes)))
	);
}

#[test]
fn preserves_non_utf8_instance_name_bytes() {
	let bytes = vec![0x66, 0x80, 0x6f, 0xff];
	let tree = WeakDom::new(
		InstanceBuilder::new("Folder").with_child(
			InstanceBuilder::new("Folder")
				.with_name("f�o�")
				.with_property("__CarbonRawName", BinaryString::from(bytes.clone())),
		),
	);
	let mut buffer = Vec::new();

	to_writer(&mut buffer, &tree, tree.root().children()).expect("failed to encode model");
	let decoded = from_reader(buffer.as_slice()).expect("failed to decode model");
	let folder = decoded.get_by_ref(decoded.root().children()[0]).unwrap();

	assert_eq!(
		folder.properties.get(&Ustr::from("__CarbonRawName")),
		Some(&Variant::BinaryString(BinaryString::from(bytes)))
	);
}

/// Ensures that unknown properties get serialized on instances.
#[test]
fn unknown_property() {
	let tree = WeakDom::new(InstanceBuilder::new("Folder").with_property("WILL_NEVER_EXIST", "Hi, mom!"));

	let mut buffer = Vec::new();
	to_writer(&mut buffer, &tree, &[tree.root_ref()]).expect("failed to encode model");

	let decoded = DecodedModel::from_reader(buffer.as_slice());
	insta::assert_yaml_snapshot!(decoded);
	Deserializer::new()
		.strict(true)
		.deserialize(buffer.as_slice())
		.expect("strict decoding should retain representable unknown properties");
}

#[test]
fn strict_decoder_rejects_unknown_property_type() {
	let tree = WeakDom::new(InstanceBuilder::new("Folder").with_property("WILL_NEVER_EXIST", "preserve me"));
	let mut buffer = Vec::new();
	to_writer(&mut buffer, &tree, &[tree.root_ref()]).unwrap();
	let corrupted = mutate_property_chunk(&buffer, "WILL_NEVER_EXIST", |data, type_offset| {
		data[type_offset] = 0xfe;
	});

	let error = Deserializer::new()
		.strict(true)
		.deserialize(corrupted.as_slice())
		.unwrap_err();
	assert!(error.to_string().contains("unknown property type ID 0xfe"));
}

#[test]
fn strict_decoder_rejects_property_chunks_without_a_type_byte() {
	let tree = WeakDom::new(InstanceBuilder::new("Folder").with_property("WILL_NEVER_EXIST", "preserve me"));
	let mut buffer = Vec::new();
	to_writer(&mut buffer, &tree, &[tree.root_ref()]).unwrap();
	let corrupted = mutate_property_chunk(&buffer, "WILL_NEVER_EXIST", |data, type_offset| {
		data.truncate(type_offset);
	});

	Deserializer::new()
		.deserialize(corrupted.as_slice())
		.expect("compatibility decoding should retain its missing-type behavior");
	let error = Deserializer::new()
		.strict(true)
		.deserialize(corrupted.as_slice())
		.unwrap_err();
	assert!(error.to_string().contains("no property type byte"));
	let structure_error = Deserializer::new()
		.strict(true)
		.deserialize_structure(corrupted.as_slice())
		.err()
		.expect("strict structure decoding accepted a missing property type byte");
	assert!(structure_error.to_string().contains("no property type byte"));
}

#[test]
fn strict_decoder_rejects_unknown_chunks_that_compatibility_ignores() {
	let tree = WeakDom::new(InstanceBuilder::new("Folder"));
	let mut buffer = Vec::new();
	to_writer(&mut buffer, &tree, &[tree.root_ref()]).unwrap();
	let mut injected = false;
	let with_unknown = rewrite_chunks(&buffer, |name, data| {
		if !injected && name == b"PRNT" {
			*name = *b"FUTR";
			data.clear();
			injected = true;
		}
	});
	assert!(injected);

	Deserializer::new().deserialize(with_unknown.as_slice()).unwrap();
	let error = Deserializer::new()
		.strict(true)
		.deserialize(with_unknown.as_slice())
		.unwrap_err();
	assert!(error.to_string().contains("unknown binary chunk FUTR"));
}

/// Ensures that serializing a tree with an unimplemented property type returns
/// an error instead of panicking.
///
/// This test will need to be updated once we implement the type used here.
#[test]
fn unimplemented_type_known_property() {
	let tree = WeakDom::new(InstanceBuilder::new("UIListLayout").with_property(
		"Padding",
		Region3::new(Vector3::new(0.0, 0.0, 50.0), Vector3::new(0.0, 0.0, 50.0)),
	));

	let mut buffer = Vec::new();
	let result = to_writer(&mut buffer, &tree, &[tree.root_ref()]);

	assert!(result.is_err());
}

/// Ensures that serializing a tree with an unimplemented property type AND an
/// unknown property descriptor returns an error instead of panicking.
///
/// Because rbx_binary has additional logic for falling back to values with no
/// known property descriptor, we should make sure that logic works.
///
/// This test will need to be updated once we implement the type used here.
#[test]
fn unimplemented_type_unknown_property() {
	let tree = WeakDom::new(InstanceBuilder::new("Folder").with_property(
		"WILL_NEVER_EXIST",
		Region3::new(Vector3::new(0.0, 0.0, 50.0), Vector3::new(0.0, 0.0, 50.0)),
	));

	let mut buffer = Vec::new();
	let result = to_writer(&mut buffer, &tree, &[tree.root_ref()]);

	assert!(result.is_err());
}

/// Ensures that the serializer returns an error instead of panicking if we give
/// it an ID not present in the tree.
#[test]
fn unknown_id() {
	let tree = WeakDom::new(InstanceBuilder::new("Folder"));

	let mut buffer = Vec::new();
	let result = to_writer(&mut buffer, &tree, &[Ref::new()]);

	assert!(result.is_err());
}

#[test]
fn migrated_properties() {
	let tree = WeakDom::new(InstanceBuilder::new("Folder").with_children([
		InstanceBuilder::new("ScreenGui").with_property("ScreenInsets", Enum::from_u32(0)),
		InstanceBuilder::new("ScreenGui").with_property("IgnoreGuiInset", true),
		InstanceBuilder::new("Part").with_property("Color", Color3::new(1.0, 1.0, 1.0)),
		InstanceBuilder::new("Part").with_property("BrickColor", BrickColor::Alder),
		InstanceBuilder::new("Part").with_property("brickColor", BrickColor::Alder),
		InstanceBuilder::new("TextLabel").with_property("FontFace", Font::default()),
		InstanceBuilder::new("TextLabel").with_property("Font", Enum::from_u32(8)),
	]));

	let mut buffer = Vec::new();

	to_writer(&mut buffer, &tree, &[tree.root_ref()]).expect("failed to encode model");

	let decoded = DecodedModel::from_reader(buffer.as_slice());
	insta::assert_yaml_snapshot!(decoded);
}

/// Ensures that only one name for each logical property is serialized to a
/// file. Here, we use BasePart.Size and BasePart.size, which alias and both
/// serialize to BasePart.size.
///
/// For fun, we also have a part with no size property at all. It should default
/// to (4.0, 1.2, 2.0), a relic of Roblox's distant past.
#[test]
fn logical_properties_basepart_size() {
	let tree = WeakDom::new(
		InstanceBuilder::new("Folder")
			.with_child(InstanceBuilder::new("Part").with_property("Size", Vector3::new(1.0, 2.0, 3.0)))
			.with_child(InstanceBuilder::new("Part").with_property("size", Vector3::new(4.0, 5.0, 6.0)))
			.with_child(InstanceBuilder::new("Part")),
	);

	let mut buffer = Vec::new();
	to_writer(&mut buffer, &tree, tree.root().children()).expect("failed to encode model");

	let decoded = DecodedModel::from_reader(buffer.as_slice());
	insta::assert_yaml_snapshot!(decoded);
}

/// Ensures that all valid combinations of color property names and
/// value types are properly handled.
#[test]
fn part_color() {
	let tree = WeakDom::new(
		InstanceBuilder::new("Folder")
			.with_child(InstanceBuilder::new("Part").with_property("Color3uint8", Color3::new(-0.25, 0.5, 1.2)))
			.with_child(InstanceBuilder::new("Part").with_property("Color3uint8", Color3uint8::new(25, 86, 254)))
			.with_child(InstanceBuilder::new("Part").with_property("Color", Color3::new(0.0, 0.5, 1.0)))
			.with_child(InstanceBuilder::new("Part").with_property("Color", Color3uint8::new(1, 30, 100))),
	);

	let mut buf = Vec::new();
	let _ = to_writer(&mut buf, &tree, tree.root().children());

	let decoded = DecodedModel::from_reader(buf.as_slice());
	insta::assert_yaml_snapshot!(decoded);
}

#[test]
fn default_shared_string() {
	let mut tree = WeakDom::new(InstanceBuilder::new("Folder"));
	let ref_1 = tree.insert(
		tree.root_ref(),
		InstanceBuilder::new("Model").with_property(
			// This is the first SharedString property I saw in the database
			"ModelMeshData",
			SharedString::new(b"arbitrary string".to_vec()),
		),
	);
	let ref_2 = tree.insert(tree.root_ref(), InstanceBuilder::new("Model"));

	let mut buf = Vec::new();
	let _ = to_writer(&mut buf, &tree, &[ref_1, ref_2]);

	let decoded = DecodedModel::from_reader(buf.as_slice());
	insta::assert_yaml_snapshot!(decoded);
}

#[test]
fn does_not_serialize() {
	let default_vector3 = Vector3::new(0.0, 0.0, 0.0);
	let default_cframe = CFrame::identity();

	let root = InstanceBuilder::new("Folder").with_children([
		InstanceBuilder::new("Motor6D").with_property("ChildName", String::new()),
		InstanceBuilder::new("FaceControls").with_property("RightCheekRaiser", 0.0f32),
		InstanceBuilder::new("Motor6D").with_property("ReplicateCurrentOffset6D", default_vector3),
		InstanceBuilder::new("GuiService").with_property("MenuIsOpen", false),
		InstanceBuilder::new("PVInstance").with_property("Origin", default_cframe),
		InstanceBuilder::new("Stats").with_property("RenderCPUFrameTime", 0.0f32),
		InstanceBuilder::new("VRService").with_property("VREnabled", false),
		InstanceBuilder::new("TorsionSpringConstraint").with_property("CurrentAngle", 0.0f32),
		InstanceBuilder::new("Lighting").with_property("ShadowColor", Color3::new(0.0, 0.0, 0.0)),
		InstanceBuilder::new("BasePart").with_property("ExtentsCFrame", default_cframe),
	]);

	let tree = WeakDom::new(root);

	let mut buf = Vec::new();
	let _ = to_writer(&mut buf, &tree, tree.root().children());

	let decoded = DecodedModel::from_reader(buf.as_slice());
	insta::assert_yaml_snapshot!(decoded);
}
