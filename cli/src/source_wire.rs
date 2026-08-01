//! Exact protocol representation shared by the artifact and Studio transport.

use anyhow::{ensure, Context, Result};
use rbx_dom_weak::{
	types::{
		Attributes, CFrame, Color3, ColorSequence, ColorSequenceKeypoint, CustomPhysicalProperties, Enum, Matrix3,
		NumberRange, NumberSequence, NumberSequenceKeypoint, PhysicalProperties, Ray, Rect, Region3, UDim, UDim2,
		Variant, VariantType, Vector2, Vector3, Vector3int16,
	},
	Ustr, UstrMap,
};
use rbx_reflection::{DataType, PropertyKind};
use serde::ser::SerializeMap;

use crate::util;

pub(crate) fn cframe_semantically_equal(left: &CFrame, right: &CFrame) -> bool {
	if left.position != right.position {
		return false;
	}
	let left = [
		left.orientation.x.x,
		left.orientation.x.y,
		left.orientation.x.z,
		left.orientation.y.x,
		left.orientation.y.y,
		left.orientation.y.z,
		left.orientation.z.x,
		left.orientation.z.y,
		left.orientation.z.z,
	];
	let right = [
		right.orientation.x.x,
		right.orientation.x.y,
		right.orientation.x.z,
		right.orientation.y.x,
		right.orientation.y.y,
		right.orientation.y.z,
		right.orientation.z.x,
		right.orientation.z.y,
		right.orientation.z.z,
	];
	left.into_iter().zip(right).all(|(left, right)| {
		left == right || (left.is_finite() && right.is_finite() && (left - right).abs() <= f32::EPSILON)
	})
}

pub(crate) fn normalize_wire_attributes(class: &str, properties: &mut UstrMap<Variant>) -> Result<()> {
	canonicalize_wire_property_names(class, properties);
	let names: Vec<Ustr> = properties
		.iter()
		.filter_map(|(name, value)| {
			matches!(value, Variant::BinaryString(_))
				.then(|| canonical_variant_type(class, name.as_str()))
				.flatten()
				.filter(|ty| *ty == VariantType::Attributes)
				.map(|_| *name)
		})
		.collect();
	for name in names {
		let Some(Variant::BinaryString(raw)) = properties.get(&name).cloned() else {
			continue;
		};
		let raw_bytes: &[u8] = raw.as_ref();
		let attributes = Attributes::from_reader(raw_bytes)
			.with_context(|| format!("invalid raw Attributes value for {class}.{name}"))?;
		properties.insert(name, Variant::Attributes(attributes));
	}
	normalize_wire_exact_values(class, properties)
}

pub(crate) fn adapt_lighting_output_properties(class: &str, properties: &mut UstrMap<Variant>) -> Result<()> {
	if class != "Lighting" {
		return Ok(());
	}
	let Some(Variant::Attributes(attributes)) = properties.get(&Ustr::from("Attributes")) else {
		return Ok(());
	};
	if !matches!(
		attributes.get("RBX_LightingTechnologyUnifiedMigration"),
		Some(Variant::Bool(true))
	) {
		return Ok(());
	}
	let Some(original) = attributes.get("RBX_OriginalTechnologyOnFileLoad") else {
		return Ok(());
	};
	let Variant::Int32(original) = original else {
		anyhow::bail!("Lighting.RBX_OriginalTechnologyOnFileLoad is not an Int32");
	};
	ensure!(
		*original >= 0,
		"Lighting.RBX_OriginalTechnologyOnFileLoad cannot be negative"
	);
	properties.insert(
		Ustr::from("Technology"),
		Variant::Enum(Enum::from_u32(*original as u32)),
	);
	Ok(())
}

fn normalize_wire_exact_values(class: &str, properties: &mut UstrMap<Variant>) -> Result<()> {
	let names: Vec<(Ustr, VariantType)> = properties
		.iter()
		.filter_map(|(name, value)| {
			matches!(value, Variant::BinaryString(_))
				.then(|| canonical_variant_type(class, name.as_str()))
				.flatten()
				.filter(|ty| is_exact_raw_type(*ty))
				.map(|ty| (*name, ty))
		})
		.collect();
	for (name, ty) in names {
		let Some(Variant::BinaryString(raw)) = properties.get(&name) else {
			continue;
		};
		let value = decode_exact_raw(ty, raw.as_ref())
			.with_context(|| format!("invalid exact raw value for {class}.{name} ({ty:?})"))?;
		properties.insert(name, value);
	}
	Ok(())
}

fn is_exact_raw_type(ty: VariantType) -> bool {
	matches!(
		ty,
		VariantType::Int64
			| VariantType::Float32
			| VariantType::Float64
			| VariantType::CFrame
			| VariantType::Color3
			| VariantType::ColorSequence
			| VariantType::OptionalCFrame
			| VariantType::NumberRange
			| VariantType::NumberSequence
			| VariantType::PhysicalProperties
			| VariantType::Ray
			| VariantType::Rect
			| VariantType::Region3
			| VariantType::UDim
			| VariantType::UDim2
			| VariantType::Vector2
			| VariantType::Vector3
			| VariantType::Vector3int16
	)
}

pub(crate) fn decode_exact_raw(ty: VariantType, bytes: &[u8]) -> Result<Variant> {
	fn array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N]> {
		bytes
			.get(offset..offset + N)
			.context("exact raw value is truncated")?
			.try_into()
			.context("exact raw value has an invalid width")
	}
	fn f32_at(bytes: &[u8], offset: usize) -> Result<f32> {
		Ok(f32::from_le_bytes(array(bytes, offset)?))
	}
	fn i32_at(bytes: &[u8], offset: usize) -> Result<i32> {
		Ok(i32::from_le_bytes(array(bytes, offset)?))
	}
	fn sequence_count(bytes: &[u8], keypoint_width: usize) -> Result<usize> {
		let count = u32::from_le_bytes(array(bytes, 0)?) as usize;
		let expected = count
			.checked_mul(keypoint_width)
			.and_then(|width| width.checked_add(4))
			.context("exact raw sequence length overflows")?;
		ensure!(
			bytes.len() == expected,
			"unexpected exact raw byte length {}",
			bytes.len()
		);
		Ok(count)
	}
	let value = match ty {
		VariantType::Int64 if bytes.len() == 8 => Variant::Int64(i64::from_le_bytes(array(bytes, 0)?)),
		VariantType::Float32 if bytes.len() == 4 => Variant::Float32(f32_at(bytes, 0)?),
		VariantType::Float64 if bytes.len() == 8 => Variant::Float64(f64::from_le_bytes(array(bytes, 0)?)),
		VariantType::Vector2 if bytes.len() == 8 => {
			Variant::Vector2(Vector2::new(f32_at(bytes, 0)?, f32_at(bytes, 4)?))
		}
		VariantType::Vector3 if bytes.len() == 12 => {
			Variant::Vector3(Vector3::new(f32_at(bytes, 0)?, f32_at(bytes, 4)?, f32_at(bytes, 8)?))
		}
		VariantType::Vector3int16 if bytes.len() == 6 => Variant::Vector3int16(Vector3int16::new(
			i16::from_le_bytes(array(bytes, 0)?),
			i16::from_le_bytes(array(bytes, 2)?),
			i16::from_le_bytes(array(bytes, 4)?),
		)),
		VariantType::NumberRange if bytes.len() == 8 => {
			Variant::NumberRange(NumberRange::new(f32_at(bytes, 0)?, f32_at(bytes, 4)?))
		}
		VariantType::Color3 if bytes.len() == 12 => {
			Variant::Color3(Color3::new(f32_at(bytes, 0)?, f32_at(bytes, 4)?, f32_at(bytes, 8)?))
		}
		VariantType::Rect if bytes.len() == 16 => Variant::Rect(Rect::new(
			Vector2::new(f32_at(bytes, 0)?, f32_at(bytes, 4)?),
			Vector2::new(f32_at(bytes, 8)?, f32_at(bytes, 12)?),
		)),
		VariantType::Ray if bytes.len() == 24 => Variant::Ray(Ray::new(
			Vector3::new(f32_at(bytes, 0)?, f32_at(bytes, 4)?, f32_at(bytes, 8)?),
			Vector3::new(f32_at(bytes, 12)?, f32_at(bytes, 16)?, f32_at(bytes, 20)?),
		)),
		VariantType::Region3 if bytes.len() == 24 => Variant::Region3(Region3::new(
			Vector3::new(f32_at(bytes, 0)?, f32_at(bytes, 4)?, f32_at(bytes, 8)?),
			Vector3::new(f32_at(bytes, 12)?, f32_at(bytes, 16)?, f32_at(bytes, 20)?),
		)),
		VariantType::UDim if bytes.len() == 8 => Variant::UDim(UDim::new(f32_at(bytes, 0)?, i32_at(bytes, 4)?)),
		VariantType::UDim2 if bytes.len() == 16 => Variant::UDim2(UDim2::new(
			UDim::new(f32_at(bytes, 0)?, i32_at(bytes, 4)?),
			UDim::new(f32_at(bytes, 8)?, i32_at(bytes, 12)?),
		)),
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
			let count = sequence_count(bytes, 16)?;
			let mut keypoints = Vec::with_capacity(count);
			for index in 0..count {
				let offset = 4 + index * 16;
				keypoints.push(ColorSequenceKeypoint::new(
					f32_at(bytes, offset)?,
					Color3::new(
						f32_at(bytes, offset + 4)?,
						f32_at(bytes, offset + 8)?,
						f32_at(bytes, offset + 12)?,
					),
				));
			}
			Variant::ColorSequence(ColorSequence { keypoints })
		}
		VariantType::PhysicalProperties if bytes == [0] => Variant::PhysicalProperties(PhysicalProperties::Default),
		VariantType::PhysicalProperties if bytes.len() == 25 && bytes[0] == 1 => {
			Variant::PhysicalProperties(PhysicalProperties::Custom(CustomPhysicalProperties::new(
				f32_at(bytes, 1)?,
				f32_at(bytes, 5)?,
				f32_at(bytes, 9)?,
				f32_at(bytes, 13)?,
				f32_at(bytes, 17)?,
				f32_at(bytes, 21)?,
			)))
		}
		VariantType::CFrame if bytes.len() == 48 => Variant::CFrame(CFrame::new(
			Vector3::new(f32_at(bytes, 36)?, f32_at(bytes, 40)?, f32_at(bytes, 44)?),
			Matrix3::new(
				Vector3::new(f32_at(bytes, 0)?, f32_at(bytes, 4)?, f32_at(bytes, 8)?),
				Vector3::new(f32_at(bytes, 12)?, f32_at(bytes, 16)?, f32_at(bytes, 20)?),
				Vector3::new(f32_at(bytes, 24)?, f32_at(bytes, 28)?, f32_at(bytes, 32)?),
			),
		)),
		VariantType::OptionalCFrame if bytes == [0] => Variant::OptionalCFrame(None),
		VariantType::OptionalCFrame if bytes.len() == 49 && bytes[0] == 1 => {
			Variant::OptionalCFrame(Some(CFrame::new(
				Vector3::new(f32_at(bytes, 37)?, f32_at(bytes, 41)?, f32_at(bytes, 45)?),
				Matrix3::new(
					Vector3::new(f32_at(bytes, 1)?, f32_at(bytes, 5)?, f32_at(bytes, 9)?),
					Vector3::new(f32_at(bytes, 13)?, f32_at(bytes, 17)?, f32_at(bytes, 21)?),
					Vector3::new(f32_at(bytes, 25)?, f32_at(bytes, 29)?, f32_at(bytes, 33)?),
				),
			)))
		}
		_ => anyhow::bail!("unexpected exact raw byte length {}", bytes.len()),
	};
	Ok(value)
}

pub(crate) fn exact_raw_bytes(value: &Variant) -> Option<Vec<u8>> {
	fn push32(output: &mut Vec<u8>, value: f32) {
		output.extend_from_slice(&value.to_le_bytes());
	}
	fn push_i32(output: &mut Vec<u8>, value: i32) {
		output.extend_from_slice(&value.to_le_bytes());
	}
	fn push_vector2(output: &mut Vec<u8>, value: Vector2) {
		push32(output, value.x);
		push32(output, value.y);
	}
	fn push_vector3(output: &mut Vec<u8>, value: Vector3) {
		push32(output, value.x);
		push32(output, value.y);
		push32(output, value.z);
	}
	match value {
		Variant::Int64(value) => Some(value.to_le_bytes().to_vec()),
		Variant::Float32(value) => Some(value.to_le_bytes().to_vec()),
		Variant::Float64(value) => Some(value.to_le_bytes().to_vec()),
		Variant::Vector2(value) => {
			let mut raw = Vec::with_capacity(8);
			push_vector2(&mut raw, *value);
			Some(raw)
		}
		Variant::Vector3(value) => {
			let mut raw = Vec::with_capacity(12);
			push_vector3(&mut raw, *value);
			Some(raw)
		}
		Variant::Vector3int16(value) => {
			let mut raw = Vec::with_capacity(6);
			raw.extend_from_slice(&value.x.to_le_bytes());
			raw.extend_from_slice(&value.y.to_le_bytes());
			raw.extend_from_slice(&value.z.to_le_bytes());
			Some(raw)
		}
		Variant::Color3(value) => {
			let mut raw = Vec::with_capacity(12);
			push32(&mut raw, value.r);
			push32(&mut raw, value.g);
			push32(&mut raw, value.b);
			Some(raw)
		}
		Variant::NumberRange(value) => {
			let mut raw = Vec::with_capacity(8);
			push32(&mut raw, value.min);
			push32(&mut raw, value.max);
			Some(raw)
		}
		Variant::Rect(value) => {
			let mut raw = Vec::with_capacity(16);
			push_vector2(&mut raw, value.min);
			push_vector2(&mut raw, value.max);
			Some(raw)
		}
		Variant::Ray(value) => {
			let mut raw = Vec::with_capacity(24);
			push_vector3(&mut raw, value.origin);
			push_vector3(&mut raw, value.direction);
			Some(raw)
		}
		Variant::Region3(value) => {
			let mut raw = Vec::with_capacity(24);
			push_vector3(&mut raw, value.min);
			push_vector3(&mut raw, value.max);
			Some(raw)
		}
		Variant::UDim(value) => {
			let mut raw = Vec::with_capacity(8);
			push32(&mut raw, value.scale);
			push_i32(&mut raw, value.offset);
			Some(raw)
		}
		Variant::UDim2(value) => {
			let mut raw = Vec::with_capacity(16);
			push32(&mut raw, value.x.scale);
			push_i32(&mut raw, value.x.offset);
			push32(&mut raw, value.y.scale);
			push_i32(&mut raw, value.y.offset);
			Some(raw)
		}
		Variant::NumberSequence(value) => {
			let count = u32::try_from(value.keypoints.len()).ok()?;
			let mut raw = Vec::with_capacity(4 + value.keypoints.len() * 12);
			raw.extend_from_slice(&count.to_le_bytes());
			for point in &value.keypoints {
				push32(&mut raw, point.time);
				push32(&mut raw, point.value);
				push32(&mut raw, point.envelope);
			}
			Some(raw)
		}
		Variant::ColorSequence(value) => {
			let count = u32::try_from(value.keypoints.len()).ok()?;
			let mut raw = Vec::with_capacity(4 + value.keypoints.len() * 16);
			raw.extend_from_slice(&count.to_le_bytes());
			for point in &value.keypoints {
				push32(&mut raw, point.time);
				push32(&mut raw, point.color.r);
				push32(&mut raw, point.color.g);
				push32(&mut raw, point.color.b);
			}
			Some(raw)
		}
		Variant::PhysicalProperties(PhysicalProperties::Default) => Some(vec![0]),
		Variant::PhysicalProperties(PhysicalProperties::Custom(value)) => {
			let mut raw = Vec::with_capacity(25);
			raw.push(1);
			for component in [
				value.density(),
				value.friction(),
				value.elasticity(),
				value.friction_weight(),
				value.elasticity_weight(),
				value.acoustic_absorption(),
			] {
				push32(&mut raw, component);
			}
			Some(raw)
		}
		Variant::CFrame(value) => {
			let mut raw = Vec::with_capacity(48);
			for component in [
				value.orientation.x.x,
				value.orientation.x.y,
				value.orientation.x.z,
				value.orientation.y.x,
				value.orientation.y.y,
				value.orientation.y.z,
				value.orientation.z.x,
				value.orientation.z.y,
				value.orientation.z.z,
				value.position.x,
				value.position.y,
				value.position.z,
			] {
				push32(&mut raw, component);
			}
			Some(raw)
		}
		Variant::OptionalCFrame(value) => {
			let Some(value) = value else {
				return Some(vec![0]);
			};
			let mut raw = Vec::with_capacity(49);
			raw.push(1);
			for component in [
				value.orientation.x.x,
				value.orientation.x.y,
				value.orientation.x.z,
				value.orientation.y.x,
				value.orientation.y.y,
				value.orientation.y.z,
				value.orientation.z.x,
				value.orientation.z.y,
				value.orientation.z.z,
				value.position.x,
				value.position.y,
				value.position.z,
			] {
				push32(&mut raw, component);
			}
			Some(raw)
		}
		_ => None,
	}
}

fn canonicalize_wire_property_names(class: &str, properties: &mut UstrMap<Variant>) {
	let aliases: Vec<(Ustr, Ustr)> = properties
		.keys()
		.filter_map(|name| {
			let canonical = canonical_property_name(class, name.as_str())?;
			(canonical != name.as_str()).then(|| (*name, Ustr::from(canonical)))
		})
		.collect();
	for (alias, canonical) in aliases {
		if let Some(value) = properties.remove(&alias) {
			properties.entry(canonical).or_insert(value);
		}
	}
}

pub(crate) fn canonical_property_name<'a>(class: &str, property: &'a str) -> Option<&'a str> {
	let database = util::get_reflection_database();
	let mut current = class;
	loop {
		let descriptor = database.classes.get(current)?;
		if let Some(property_descriptor) = descriptor.properties.get(property) {
			return Some(match property_descriptor.kind {
				PropertyKind::Alias { alias_for } => alias_for,
				_ => property,
			});
		}
		current = descriptor.superclass?;
	}
}

pub(crate) fn canonical_variant_type(class: &str, property: &str) -> Option<VariantType> {
	let database = util::get_reflection_database();
	let mut current = class;
	loop {
		let descriptor = database.classes.get(current)?;
		if let Some(property) = descriptor.properties.get(property) {
			return match property.data_type {
				DataType::Value(ty) => Some(ty),
				DataType::Enum(_) => Some(VariantType::Enum),
				_ => None,
			};
		}
		current = descriptor.superclass?;
	}
}

pub(crate) struct WireProperties<'a>(pub(crate) &'a UstrMap<Variant>);

impl serde::Serialize for WireProperties<'_> {
	fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let serialized_len = self.0.keys().filter(|name| name.as_str() != "__CarbonRawName").count();
		let mut properties = serializer.serialize_map(Some(serialized_len))?;
		for (name, value) in self.0 {
			if name.as_str() != "__CarbonRawName" {
				properties.serialize_entry(name, &WireVariant(value))?;
			}
		}
		properties.end()
	}
}

struct WireVariant<'a>(&'a Variant);

impl serde::Serialize for WireVariant<'_> {
	fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match self.0 {
			value if exact_raw_bytes(value).is_some() => {
				let raw = exact_raw_bytes(value).unwrap();
				let mut variant = serializer.serialize_map(Some(1))?;
				variant.serialize_entry("BinaryString", &serde_bytes::Bytes::new(&raw))?;
				variant.end()
			}
			Variant::Attributes(value) => {
				let mut raw = Vec::new();
				value.to_writer(&mut raw).map_err(serde::ser::Error::custom)?;
				let mut variant = serializer.serialize_map(Some(1))?;
				variant.serialize_entry("BinaryString", &serde_bytes::Bytes::new(&raw))?;
				variant.end()
			}
			Variant::BinaryString(value) => {
				let mut variant = serializer.serialize_map(Some(1))?;
				variant.serialize_entry("BinaryString", &serde_bytes::Bytes::new(value.as_ref()))?;
				variant.end()
			}
			Variant::SharedString(value) => {
				let mut variant = serializer.serialize_map(Some(1))?;
				variant.serialize_entry("SharedString", &serde_bytes::Bytes::new(value.data()))?;
				variant.end()
			}
			value => serde::Serialize::serialize(value, serializer),
		}
	}
}
