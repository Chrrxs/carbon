// Based on Rojo's resolution.rs (https://github.com/rojo-rbx/rojo/blob/master/src/resolution.rs)

use anyhow::{bail, format_err, Context};
use rbx_dom_weak::types::{
	Attributes, Axes, BinaryString, BrickColor, CFrame, Color3, Color3uint8, ColorSequence, ColorSequenceKeypoint,
	Content, ContentId, ContentType, CustomPhysicalProperties, Enum, Faces, Font, MaterialColors, Matrix3, NumberRange,
	NumberSequence, NumberSequenceKeypoint, PhysicalProperties, Ray, Rect, Region3, Region3int16, Tags, UDim, UDim2,
	Variant, VariantType, Vector2, Vector2int16, Vector3, Vector3int16,
};
use rbx_reflection::{DataType, PropertyDescriptor};
use serde::{ser::SerializeSeq, Deserialize, Serialize, Serializer};
use std::{borrow::Borrow, collections::HashMap, fmt::Write};

use crate::{ext::PropertyDescriptorExt, util::get_reflection_database};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UnresolvedValue {
	FullyQualified(Variant),
	SpecialFloat(SpecialFloatValue),
	Ambiguous(AmbiguousValue),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "$type", content = "$value")]
pub enum SpecialFloatValue {
	/// Generic, bit-preserving MessagePack encoding of a fully-qualified
	/// Variant. The hex wrapper keeps the source protocol deterministic and
	/// portable while MessagePack preserves IEEE-754 payload/sign bits.
	Variant(String),
	// Legacy forms remain readable so existing Carbon sources keep building.
	Float32(String),
	Float64(String),
	Vector2([String; 2]),
	Vector3([String; 3]),
	CFrame(Box<[String; 12]>),
}

impl UnresolvedValue {
	pub fn resolve(self, class: &str, property: &str) -> anyhow::Result<Variant> {
		match self {
			UnresolvedValue::FullyQualified(Variant::Color3uint8(color))
				if color3_serializes_as_uint8(class, property) =>
			{
				Ok(Variant::Color3(color.into()))
			}
			UnresolvedValue::FullyQualified(full) => Ok(full),
			UnresolvedValue::SpecialFloat(value) => value.resolve(),
			UnresolvedValue::Ambiguous(partial) => partial.resolve(class, property),
		}
	}

	pub fn resolve_unambiguous(self) -> anyhow::Result<Variant> {
		match self {
			UnresolvedValue::FullyQualified(full) => Ok(full),
			UnresolvedValue::SpecialFloat(value) => value.resolve(),
			UnresolvedValue::Ambiguous(partial) => partial.resolve_unambiguous(),
		}
	}

	pub fn as_str(&self) -> Option<&str> {
		match self {
			UnresolvedValue::Ambiguous(AmbiguousValue::String(s)) => Some(s.as_str()),
			UnresolvedValue::FullyQualified(Variant::String(s)) => Some(s.as_str()),
			_ => None,
		}
	}

	// Based on Uplift Games' Rojo fork (https://github.com/UpliftGames/rojo/blob/syncback-incremental/src/resolution.rs#L43)
	pub fn from_variant(variant: Variant, class: &str, property: &str) -> Self {
		if variant_requires_bit_preserving_float(&variant) {
			return Self::SpecialFloat(SpecialFloatValue::from_variant(&variant));
		}
		if matches!(&variant, Variant::Int64(value) if (*value as f64) as i64 != *value) {
			return Self::FullyQualified(variant);
		}
		if color3_serializes_as_uint8(class, property) {
			let packed = match &variant {
				Variant::Color3(value) => Some(Color3uint8::from(*value)),
				Variant::Color3uint8(value) => Some(*value),
				_ => None,
			};
			if let Some(packed) = packed {
				// Bare three-number arrays remain compatible with existing RGB
				// source when at least one channel disambiguates the 0-255 scale.
				// Values containing only zeroes and ones need the explicit Variant
				// tag or they would rebuild as normalized Color3 channels.
				if packed.r <= 1 && packed.g <= 1 && packed.b <= 1 {
					return Self::FullyQualified(Variant::Color3uint8(packed));
				}
				return Self::Ambiguous(AmbiguousValue::Array3([
					packed.r as f64,
					packed.g as f64,
					packed.b as f64,
				]));
			}
		}

		// The Studio plugin can receive properties newer than the CLI's bundled
		// reflection database. Keep the explicit Variant tag in that case so the
		// value can still survive a rebuild instead of being discarded.
		if !class.is_empty() && find_descriptor(class, property).is_none() {
			return Self::FullyQualified(variant);
		}

		Self::Ambiguous(match variant {
			Variant::Attributes(attr) => {
				let mut object = HashMap::new();

				for (key, value) in attr {
					let value = if key == "RBX_OriginalTechnologyOnFileLoad" {
						match value {
							Variant::Float64(number) => Variant::Int32(number as i32),
							value => value,
						}
					} else {
						value
					};

					object.insert(
						key,
						match value {
							Variant::Bool(bool) => UnresolvedValue::Ambiguous(AmbiguousValue::Bool(bool)),
							Variant::Float64(num) => UnresolvedValue::Ambiguous(AmbiguousValue::Number(num)),
							Variant::String(str) => UnresolvedValue::Ambiguous(AmbiguousValue::String(str)),
							_ => UnresolvedValue::FullyQualified(value),
						},
					);
				}

				AmbiguousValue::Object(object)
			}

			Variant::Axes(axes) => {
				let mut array = Vec::new();

				if axes.contains(Axes::X) {
					array.push("X".into());
				}

				if axes.contains(Axes::Y) {
					array.push("Y".into());
				}

				if axes.contains(Axes::Z) {
					array.push("Z".into());
				}

				AmbiguousValue::StringArray(array)
			}

			Variant::BinaryString(binary) => match std::str::from_utf8(binary.as_ref()) {
				Ok(value) => AmbiguousValue::String(value.to_owned()),
				Err(_) => return Self::FullyQualified(Variant::BinaryString(binary)),
			},

			Variant::Bool(bool) => AmbiguousValue::Bool(bool),

			Variant::BrickColor(color) => AmbiguousValue::String(color.to_string()),

			Variant::CFrame(cf) => AmbiguousValue::Array12([
				cf.position.x as f64,
				cf.position.y as f64,
				cf.position.z as f64,
				cf.orientation.x.x as f64,
				cf.orientation.x.y as f64,
				cf.orientation.x.z as f64,
				cf.orientation.y.x as f64,
				cf.orientation.y.y as f64,
				cf.orientation.y.z as f64,
				cf.orientation.z.x as f64,
				cf.orientation.z.y as f64,
				cf.orientation.z.z as f64,
			]),

			Variant::Color3(color) => AmbiguousValue::Array3([color.r as f64, color.g as f64, color.b as f64]),
			Variant::Color3uint8(color) => AmbiguousValue::Array3([color.r as f64, color.g as f64, color.b as f64]),

			Variant::ColorSequence(sequence) => AmbiguousValue::ColorSequence(sequence.keypoints),

			Variant::Content(content) => match content.value() {
				ContentType::Uri(uri) => AmbiguousValue::String(uri.to_owned()),
				ContentType::None | ContentType::Object(_) => {
					return Self::FullyQualified(Variant::Content(content));
				}
				_ => return Self::FullyQualified(Variant::Content(content)),
			},
			Variant::ContentId(content) => AmbiguousValue::String(content.into_string()),

			Variant::Enum(rbx_enum) => {
				if let Some(property) = find_descriptor(class, property) {
					if let DataType::Enum(enum_name) = &property.data_type {
						if let Some(enum_descriptor) = get_reflection_database().enums.get(enum_name) {
							for (variant_name, id) in &enum_descriptor.items {
								if *id == rbx_enum.to_u32() {
									return Self::Ambiguous(AmbiguousValue::String(variant_name.to_string()));
								}
							}
						}
					}
				}

				return Self::FullyQualified(variant);
			}

			Variant::Faces(faces) => {
				let mut array = Vec::new();

				if faces.contains(Faces::RIGHT) {
					array.push("Right".into());
				}

				if faces.contains(Faces::TOP) {
					array.push("Top".into());
				}

				if faces.contains(Faces::BACK) {
					array.push("Back".into());
				}

				if faces.contains(Faces::LEFT) {
					array.push("Left".into());
				}

				if faces.contains(Faces::BOTTOM) {
					array.push("Bottom".into());
				}

				if faces.contains(Faces::FRONT) {
					array.push("Front".into());
				}

				AmbiguousValue::StringArray(array)
			}

			Variant::Float32(num) => AmbiguousValue::Number(num as f64),
			Variant::Float64(num) => AmbiguousValue::Number(num),

			Variant::Font(font) => AmbiguousValue::Font(font),

			Variant::Int32(num) => AmbiguousValue::Number(num as f64),
			Variant::Int64(num) => AmbiguousValue::Number(num as f64),

			Variant::MaterialColors(colors) => AmbiguousValue::MaterialColors(colors),

			Variant::NumberRange(range) => AmbiguousValue::Array2([range.min as f64, range.max as f64]),

			Variant::NumberSequence(sequence) => AmbiguousValue::NumberSequence(sequence.keypoints),

			Variant::OptionalCFrame(cf) => {
				if let Some(cf) = cf {
					AmbiguousValue::Array12([
						cf.position.x as f64,
						cf.position.y as f64,
						cf.position.z as f64,
						cf.orientation.x.x as f64,
						cf.orientation.x.y as f64,
						cf.orientation.x.z as f64,
						cf.orientation.y.x as f64,
						cf.orientation.y.y as f64,
						cf.orientation.y.z as f64,
						cf.orientation.z.x as f64,
						cf.orientation.z.y as f64,
						cf.orientation.z.z as f64,
					])
				} else {
					AmbiguousValue::String("null".into())
				}
			}

			Variant::PhysicalProperties(PhysicalProperties::Custom(custom)) => {
				AmbiguousValue::PhysicalProperties(custom)
			}
			Variant::PhysicalProperties(PhysicalProperties::Default) => AmbiguousValue::String("Default".into()),

			Variant::Ray(ray) => AmbiguousValue::Array3Array2([
				[ray.origin.x as f64, ray.origin.y as f64, ray.origin.z as f64],
				[ray.direction.x as f64, ray.direction.y as f64, ray.direction.z as f64],
			]),

			Variant::Ref(_) => AmbiguousValue::String(String::new()),

			Variant::Rect(rect) => AmbiguousValue::Array4([
				rect.min.x as f64,
				rect.min.y as f64,
				rect.max.x as f64,
				rect.max.y as f64,
			]),

			Variant::Region3(region) => AmbiguousValue::Array3Array2([
				[region.min.x as f64, region.min.y as f64, region.min.z as f64],
				[region.max.x as f64, region.max.y as f64, region.max.z as f64],
			]),
			Variant::Region3int16(region) => AmbiguousValue::Array3Array2([
				[region.min.x as f64, region.min.y as f64, region.min.z as f64],
				[region.max.x as f64, region.max.y as f64, region.max.z as f64],
			]),

			Variant::SharedString(shared) => match std::str::from_utf8(shared.data()) {
				Ok(value) => AmbiguousValue::String(value.to_owned()),
				Err(_) => return Self::FullyQualified(Variant::SharedString(shared)),
			},
			Variant::String(str) => AmbiguousValue::String(str),

			Variant::Tags(tags) => AmbiguousValue::StringArray(tags.iter().map(|s| s.into()).collect()),

			Variant::UDim(udim) => AmbiguousValue::Array2([udim.scale as f64, udim.offset as f64]),

			Variant::UDim2(udim) => AmbiguousValue::Array2Array2([
				[udim.x.scale as f64, udim.x.offset as f64],
				[udim.y.scale as f64, udim.y.offset as f64],
			]),

			Variant::Vector2(vector) => AmbiguousValue::Array2([vector.x as f64, vector.y as f64]),
			Variant::Vector2int16(vector) => AmbiguousValue::Array2([vector.x as f64, vector.y as f64]),

			Variant::Vector3(vector) => AmbiguousValue::Array3([vector.x as f64, vector.y as f64, vector.z as f64]),
			Variant::Vector3int16(vector) => {
				AmbiguousValue::Array3([vector.x as f64, vector.y as f64, vector.z as f64])
			}

			_ => {
				return Self::FullyQualified(variant);
			}
		})
	}
}

impl SpecialFloatValue {
	fn from_variant(value: &Variant) -> Self {
		let encoded = rmp_serde::to_vec(value).expect("Variant MessagePack serialization cannot fail");
		Self::Variant(encode_hex(&encoded))
	}

	fn resolve(self) -> anyhow::Result<Variant> {
		let (kind, encoded) = match self {
			Self::Variant(value) => return Ok(rmp_serde::from_slice(&decode_hex(&value)?)?),
			Self::Float32(value) => ("Float32", value),
			Self::Float64(value) => ("Float64", value),
			Self::Vector2(values) => {
				return Ok(Variant::Vector2(Vector2::new(
					decode_f32(&values[0])?,
					decode_f32(&values[1])?,
				)));
			}
			Self::Vector3(values) => {
				return Ok(Variant::Vector3(Vector3::new(
					decode_f32(&values[0])?,
					decode_f32(&values[1])?,
					decode_f32(&values[2])?,
				)));
			}
			Self::CFrame(values) => {
				let values = (*values)
					.map(|value| decode_f32(&value))
					.into_iter()
					.collect::<Result<Vec<_>, _>>()?;
				return Ok(Variant::CFrame(CFrame::new(
					Vector3::new(values[0], values[1], values[2]),
					Matrix3::new(
						Vector3::new(values[3], values[4], values[5]),
						Vector3::new(values[6], values[7], values[8]),
						Vector3::new(values[9], values[10], values[11]),
					),
				)));
			}
		};
		let value = match encoded.as_str() {
			"NaN" => f64::NAN,
			"Infinity" => f64::INFINITY,
			"-Infinity" => f64::NEG_INFINITY,
			value => bail!("invalid special float value {value}"),
		};

		match kind {
			"Float32" => Ok(Variant::Float32(value as f32)),
			"Float64" => Ok(Variant::Float64(value)),
			kind => bail!("invalid special float type {kind}"),
		}
	}
}

fn encode_hex(bytes: &[u8]) -> String {
	const HEX: &[u8; 16] = b"0123456789abcdef";
	let mut encoded = String::with_capacity(bytes.len() * 2);
	for byte in bytes {
		encoded.push(HEX[(byte >> 4) as usize] as char);
		encoded.push(HEX[(byte & 0x0f) as usize] as char);
	}
	encoded
}

fn decode_hex(value: &str) -> anyhow::Result<Vec<u8>> {
	fn nibble(byte: u8) -> anyhow::Result<u8> {
		match byte {
			b'0'..=b'9' => Ok(byte - b'0'),
			b'a'..=b'f' => Ok(byte - b'a' + 10),
			b'A'..=b'F' => Ok(byte - b'A' + 10),
			_ => bail!("invalid hex digit in special float value"),
		}
	}
	let bytes = value.as_bytes();
	if !bytes.len().is_multiple_of(2) {
		bail!("special float value has an odd-length hex payload");
	}
	bytes
		.chunks_exact(2)
		.map(|pair| Ok((nibble(pair[0])? << 4) | nibble(pair[1])?))
		.collect()
}

fn decode_f32(value: &str) -> anyhow::Result<f32> {
	match value {
		"NaN" => Ok(f32::NAN),
		"Infinity" => Ok(f32::INFINITY),
		"-Infinity" => Ok(f32::NEG_INFINITY),
		value => Ok(value.parse()?),
	}
}

fn variant_requires_bit_preserving_float(value: &Variant) -> bool {
	fn f32_requires_bits(value: f32) -> bool {
		!value.is_finite() || (value == 0.0 && value.is_sign_negative())
	}
	fn f64_requires_bits(value: f64) -> bool {
		!value.is_finite() || (value == 0.0 && value.is_sign_negative())
	}
	fn vector2_requires_bits(value: &Vector2) -> bool {
		f32_requires_bits(value.x) || f32_requires_bits(value.y)
	}
	fn vector3_requires_bits(value: &Vector3) -> bool {
		f32_requires_bits(value.x) || f32_requires_bits(value.y) || f32_requires_bits(value.z)
	}
	fn cframe_requires_bits(value: &CFrame) -> bool {
		vector3_requires_bits(&value.position)
			|| vector3_requires_bits(&value.orientation.x)
			|| vector3_requires_bits(&value.orientation.y)
			|| vector3_requires_bits(&value.orientation.z)
	}

	match value {
		Variant::Attributes(values) => values
			.iter()
			.any(|(_, value)| variant_requires_bit_preserving_float(value)),
		Variant::CFrame(value) => cframe_requires_bits(value),
		Variant::Color3(value) => {
			f32_requires_bits(value.r) || f32_requires_bits(value.g) || f32_requires_bits(value.b)
		}
		Variant::ColorSequence(value) => value.keypoints.iter().any(|point| {
			f32_requires_bits(point.time)
				|| f32_requires_bits(point.color.r)
				|| f32_requires_bits(point.color.g)
				|| f32_requires_bits(point.color.b)
		}),
		Variant::Float32(value) => f32_requires_bits(*value),
		Variant::Float64(value) => f64_requires_bits(*value),
		Variant::NumberRange(value) => f32_requires_bits(value.min) || f32_requires_bits(value.max),
		Variant::NumberSequence(value) => value.keypoints.iter().any(|point| {
			f32_requires_bits(point.time) || f32_requires_bits(point.value) || f32_requires_bits(point.envelope)
		}),
		Variant::OptionalCFrame(Some(value)) => cframe_requires_bits(value),
		Variant::PhysicalProperties(PhysicalProperties::Custom(value)) => {
			f32_requires_bits(value.density())
				|| f32_requires_bits(value.friction())
				|| f32_requires_bits(value.elasticity())
				|| f32_requires_bits(value.friction_weight())
				|| f32_requires_bits(value.elasticity_weight())
				|| f32_requires_bits(value.acoustic_absorption())
		}
		Variant::Ray(value) => vector3_requires_bits(&value.origin) || vector3_requires_bits(&value.direction),
		Variant::Rect(value) => vector2_requires_bits(&value.min) || vector2_requires_bits(&value.max),
		Variant::Region3(value) => vector3_requires_bits(&value.min) || vector3_requires_bits(&value.max),
		Variant::UDim(value) => f32_requires_bits(value.scale),
		Variant::UDim2(value) => f32_requires_bits(value.x.scale) || f32_requires_bits(value.y.scale),
		Variant::Vector2(value) => vector2_requires_bits(value),
		Variant::Vector3(value) => vector3_requires_bits(value),
		_ => false,
	}
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AmbiguousValue {
	Bool(bool),
	String(String),
	StringArray(Vec<String>),
	#[serde(serialize_with = "serialize_number")]
	Number(f64),
	#[serde(serialize_with = "serialize_array")]
	Array2([f64; 2]),
	#[serde(serialize_with = "serialize_array")]
	Array3([f64; 3]),
	#[serde(serialize_with = "serialize_array")]
	Array4([f64; 4]),
	#[serde(serialize_with = "serialize_array")]
	Array12([f64; 12]),
	#[serde(serialize_with = "serialize_nested_array")]
	Array2Array2([[f64; 2]; 2]),
	#[serde(serialize_with = "serialize_nested_array")]
	Array3Array2([[f64; 3]; 2]),
	Attributes(Attributes),
	MaterialColors(MaterialColors),
	ColorSequence(Vec<ColorSequenceKeypoint>),
	NumberSequence(Vec<NumberSequenceKeypoint>),
	Font(Font),
	PhysicalProperties(CustomPhysicalProperties),
	Object(HashMap<String, UnresolvedValue>),
}

impl AmbiguousValue {
	pub fn resolve(self, class: &str, property: &str) -> anyhow::Result<Variant> {
		let descriptor =
			find_descriptor(class, property).ok_or_else(|| format_err!("Unknown property {}.{}", class, property))?;

		match &descriptor.data_type {
			DataType::Enum(enum_name) => {
				let descriptor = get_reflection_database()
					.enums
					.get(enum_name)
					.ok_or_else(|| format_err!("Unknown enum {}. Probably not implemented yet!", enum_name))?;

				let error = |value: &str| {
					let mut examples = descriptor
						.items
						.keys()
						.map(|value| value.borrow())
						.collect::<Vec<&str>>();

					examples.sort();

					format_err!(
						"Invalid value for property {}.{}. Got {} but expected a member of the {} enum such as {}",
						class,
						property,
						value,
						enum_name,
						list_examples(&examples),
					)
				};

				let value = match self {
					AmbiguousValue::String(value) => value,
					unresolved => return Err(error(unresolved.describe())),
				};

				let resolved = descriptor
					.items
					.get(value.as_str())
					.ok_or_else(|| error(value.as_str()))?;

				Ok(Enum::from_u32(*resolved).into())
			}
			DataType::Value(variant) => match (variant, self) {
				(VariantType::Attributes, AmbiguousValue::Attributes(attr)) => Ok(attr.into()),
				(VariantType::Attributes, AmbiguousValue::Object(value)) => {
					let mut attributes = Attributes::new();

					for (key, unresolved) in value {
						attributes.insert(key, unresolved.resolve_unambiguous()?);
					}

					Ok(attributes.into())
				}

				(VariantType::Axes, AmbiguousValue::StringArray(axes)) => {
					let mut bits = 0;

					for axis in axes {
						match axis.as_ref() {
							"X" => bits |= 1,
							"Y" => bits |= 2,
							"Z" => bits |= 4,
							_ => {
								bail!("invalid axis '{}'", axis);
							}
						}
					}

					Ok(Axes::from_bits(bits).unwrap_or_else(Axes::empty).into())
				}

				(VariantType::BinaryString, AmbiguousValue::String(str)) => {
					Ok(BinaryString::from(str.as_bytes()).into())
				}

				(VariantType::Bool, AmbiguousValue::Bool(bool)) => Ok(bool.into()),

				(VariantType::BrickColor, AmbiguousValue::Number(num)) => Ok(BrickColor::from_number(num as u16)
					.context(format!("{num} is not valid BrickColor number"))?
					.into()),
				(VariantType::BrickColor, AmbiguousValue::String(name)) => Ok(BrickColor::from_name(&name)
					.context(format!("{name} is not valid BrickColor name"))?
					.into()),

				(VariantType::CFrame, AmbiguousValue::Array12(cf)) => {
					let cf = cf.map(|v| v as f32);

					let pos = Vector3::new(cf[0], cf[1], cf[2]);
					let orientation = Matrix3::new(
						Vector3::new(cf[3], cf[4], cf[5]),
						Vector3::new(cf[6], cf[7], cf[8]),
						Vector3::new(cf[9], cf[10], cf[11]),
					);

					Ok(CFrame::new(pos, orientation).into())
				}

				(VariantType::Color3, AmbiguousValue::Array3(color)) => {
					let (r, g, b) = (color[0] as f32, color[1] as f32, color[2] as f32);

					// Fix for the upstream rbx-dom custom BasePart.Color serialization patch.
					if let Some(data_type) = descriptor.get_custom_serialization() {
						if data_type == "Color3uint8" && (r > 1.0 || g > 1.0 || b > 1.0) {
							return Ok(Color3::new(r / 255.0, g / 255.0, b / 255.0).into());
						}
					}

					Ok(Color3::new(r, g, b).into())
				}
				(VariantType::Color3uint8, AmbiguousValue::Array3(color)) => {
					Ok(Color3uint8::new(color[0] as u8, color[1] as u8, color[2] as u8).into())
				}

				(VariantType::ColorSequence, AmbiguousValue::ColorSequence(keypoints)) => {
					Ok(ColorSequence { keypoints }.into())
				}

				(VariantType::Content, AmbiguousValue::String(content)) => Ok(Content::from(content).into()),
				(VariantType::ContentId, AmbiguousValue::String(content)) => Ok(ContentId::from(content).into()),

				(VariantType::Faces, AmbiguousValue::StringArray(faces)) => {
					let mut bits = 0;

					for face in faces {
						match face.as_ref() {
							"Right" => bits |= 1,
							"Top" => bits |= 2,
							"Back" => bits |= 4,
							"Left" => bits |= 8,
							"Bottom" => bits |= 16,
							"Front" => bits |= 32,
							_ => {
								bail!("invalid face '{}'", face);
							}
						}
					}

					Ok(Faces::from_bits(bits).unwrap_or_else(Faces::empty).into())
				}

				(VariantType::Float32, AmbiguousValue::Number(num)) => Ok((num as f32).into()),
				(VariantType::Float64, AmbiguousValue::Number(num)) => Ok(num.into()),

				(VariantType::Font, AmbiguousValue::Font(font)) => Ok(font.into()),

				(VariantType::Int32, AmbiguousValue::Number(num)) => Ok((num as i32).into()),
				(VariantType::Int64, AmbiguousValue::Number(num)) => Ok((num as i64).into()),

				(VariantType::MaterialColors, AmbiguousValue::MaterialColors(colors)) => Ok(colors.into()),

				(VariantType::NumberRange, AmbiguousValue::Array2(range)) => {
					Ok(NumberRange::new(range[0] as f32, range[1] as f32).into())
				}

				(VariantType::NumberSequence, AmbiguousValue::NumberSequence(keypoints)) => {
					Ok(NumberSequence { keypoints }.into())
				}

				(VariantType::OptionalCFrame, AmbiguousValue::Array12(cf)) => {
					let cf = cf.map(|v| v as f32);

					let pos = Vector3::new(cf[0], cf[1], cf[2]);
					let orientation = Matrix3::new(
						Vector3::new(cf[3], cf[4], cf[5]),
						Vector3::new(cf[6], cf[7], cf[8]),
						Vector3::new(cf[9], cf[10], cf[11]),
					);

					Ok(Some(CFrame::new(pos, orientation)).into())
				}
				(VariantType::OptionalCFrame, AmbiguousValue::String(value)) if value == "null" => {
					Ok(Variant::OptionalCFrame(None))
				}

				(VariantType::PhysicalProperties, AmbiguousValue::PhysicalProperties(custom)) => {
					Ok(PhysicalProperties::Custom(custom).into())
				}
				(VariantType::PhysicalProperties, AmbiguousValue::String(default)) => {
					if default != "Default" {
						bail!("string is not 'Default'");
					}

					Ok(PhysicalProperties::Default.into())
				}

				(VariantType::Ray, AmbiguousValue::Array3Array2(ray)) => Ok(Ray::new(
					Vector3::new(ray[0][0] as f32, ray[0][1] as f32, ray[0][2] as f32),
					Vector3::new(ray[1][0] as f32, ray[1][1] as f32, ray[1][2] as f32),
				)
				.into()),

				(VariantType::Rect, AmbiguousValue::Array4(rect)) => Ok(Rect::new(
					Vector2::new(rect[0] as f32, rect[1] as f32),
					Vector2::new(rect[2] as f32, rect[3] as f32),
				)
				.into()),

				(VariantType::Ref, _) => {
					bail!("Ref properties must be a relative path string and are resolved separately")
				}

				(VariantType::Region3, AmbiguousValue::Array3Array2(region)) => Ok(Region3::new(
					Vector3::new(region[0][0] as f32, region[0][1] as f32, region[0][2] as f32),
					Vector3::new(region[1][0] as f32, region[1][1] as f32, region[1][2] as f32),
				)
				.into()),
				(VariantType::Region3int16, AmbiguousValue::Array3Array2(region)) => Ok(Region3int16::new(
					Vector3int16::new(region[0][0] as i16, region[0][1] as i16, region[0][2] as i16),
					Vector3int16::new(region[1][0] as i16, region[1][1] as i16, region[1][2] as i16),
				)
				.into()),

				(VariantType::SharedString, AmbiguousValue::String(str)) => Ok(str.into()),
				(VariantType::String, AmbiguousValue::String(str)) => Ok(str.into()),

				(VariantType::Tags, AmbiguousValue::StringArray(tags)) => Ok(Tags::from(tags).into()),

				(VariantType::UDim, AmbiguousValue::Array2(udim)) => {
					Ok(rbx_dom_weak::types::UDim::new(udim[0] as f32, udim[1] as i32).into())
				}

				(VariantType::UDim2, AmbiguousValue::Array2Array2(udim)) => Ok(UDim2::new(
					UDim::new(udim[0][0] as f32, udim[0][1] as i32),
					UDim::new(udim[1][0] as f32, udim[1][1] as i32),
				)
				.into()),

				(VariantType::Vector2, AmbiguousValue::Array2(vector)) => {
					Ok(Vector2::new(vector[0] as f32, vector[1] as f32).into())
				}
				(VariantType::Vector2int16, AmbiguousValue::Array2(vector)) => {
					Ok(Vector2int16::new(vector[0] as i16, vector[1] as i16).into())
				}

				(VariantType::Vector3, AmbiguousValue::Array3(vector)) => {
					Ok(Vector3::new(vector[0] as f32, vector[1] as f32, vector[2] as f32).into())
				}
				(VariantType::Vector3int16, AmbiguousValue::Array3(vector)) => {
					Ok(Vector3int16::new(vector[0] as i16, vector[1] as i16, vector[2] as i16).into())
				}

				(_, unresolved) => Err(format_err!(
					"Wrong type of value for property {}.{}. Expected {:?}, got {}",
					class,
					property,
					variant,
					unresolved.describe(),
				)),
			},
			_ => Err(format_err!("Unknown data type for property {}.{}", class, property)),
		}
	}

	pub fn resolve_unambiguous(self) -> anyhow::Result<Variant> {
		match self {
			AmbiguousValue::Bool(value) => Ok(value.into()),
			AmbiguousValue::Number(value) => Ok(value.into()),
			AmbiguousValue::String(value) => Ok(value.into()),
			other => bail!("Cannot unambiguously resolve the value {other:?}"),
		}
	}

	fn describe(&self) -> &'static str {
		match self {
			AmbiguousValue::Bool(_) => "a bool",
			AmbiguousValue::String(_) => "a string",
			AmbiguousValue::StringArray(_) => "an array of strings",
			AmbiguousValue::Number(_) => "a number",
			AmbiguousValue::Array2(_) => "an array of two numbers",
			AmbiguousValue::Array3(_) => "an array of three numbers",
			AmbiguousValue::Array4(_) => "an array of four numbers",
			AmbiguousValue::Array12(_) => "an array of twelve numbers",
			AmbiguousValue::Array2Array2(_) => "an array of two arrays of two numbers",
			AmbiguousValue::Array3Array2(_) => "an array of two arrays of three numbers",
			AmbiguousValue::Attributes(_) => "an object containing attributes",
			AmbiguousValue::MaterialColors(_) => "an object describing MaterialColors",
			AmbiguousValue::ColorSequence(_) => "an object describing a ColorSequence",
			AmbiguousValue::NumberSequence(_) => "an object describing a NumberSequence",
			AmbiguousValue::Font(_) => "an object describing a Font",
			AmbiguousValue::PhysicalProperties(_) => "an object describing PhysicalProperties",
			AmbiguousValue::Object(_) => "a generic object",
		}
	}
}

pub fn is_ref_property(class: &str, property: &str) -> bool {
	// Parent is represented by the source tree itself. Serializing it as a Ref
	// duplicates hierarchy, bloats every instance, and can fight two-pass
	// creation when the property is applied.
	if property == "Parent" {
		return false;
	}

	matches!(
		find_descriptor(class, property).map(|descriptor| &descriptor.data_type),
		Some(DataType::Value(VariantType::Ref))
	)
}

fn find_descriptor(class: &str, property: &str) -> Option<&'static PropertyDescriptor<'static>> {
	let database = get_reflection_database();
	let mut current_class = class;

	loop {
		let class = database.classes.get(current_class)?;

		if let Some(descriptor) = class.properties.get(property) {
			return Some(descriptor);
		}

		current_class = class.superclass?;
	}
}

fn color3_serializes_as_uint8(class: &str, property: &str) -> bool {
	find_descriptor(class, property).is_some_and(|descriptor| {
		matches!(descriptor.data_type, DataType::Value(VariantType::Color3))
			&& descriptor.get_custom_serialization() == Some("Color3uint8")
	})
}

fn list_examples(values: &[&str]) -> String {
	let mut output = String::new();
	let length = (values.len() - 1).min(5);

	for value in &values[..length] {
		output.push_str(value);
		output.push_str(", ");
	}

	if values.len() > 5 {
		write!(output, "or {} more", values.len() - length).unwrap();
	} else {
		output.push_str("or ");
		output.push_str(values[values.len() - 1]);
	}

	output
}

#[inline]
fn normalize_number(number: &f64) -> f64 {
	// JSON cannot represent infinity. Roblox uses it for a handful of defaults;
	// keep those values finite until the project format has an explicit marker.
	if number.is_infinite() {
		999_999_999.0 * number.signum()
	} else {
		*number
	}
}

fn serialize_number<S>(number: &f64, serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	let number = normalize_number(number);

	if number.fract() == 0.0 {
		serializer.serialize_i64(number as i64)
	} else {
		serializer.serialize_f64(number)
	}
}

fn serialize_array<S>(array: &[f64], serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	let mut seq = serializer.serialize_seq(Some(array.len()))?;

	for number in array {
		let number = normalize_number(number);

		if number.fract() == 0.0 {
			seq.serialize_element(&(number as i64))?;
		} else {
			seq.serialize_element(&number)?;
		}
	}

	seq.end()
}

fn serialize_nested_array<S, const N: usize>(array: &[[f64; N]; 2], serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	let mut seq = serializer.serialize_seq(Some(2))?;

	for array in array {
		let mut new: Vec<Number> = Vec::with_capacity(array.len());

		for number in array {
			let number = normalize_number(number);

			if number.fract() == 0.0 {
				new.push(Number::Int(number as i64));
			} else {
				new.push(Number::Float(number));
			}
		}

		seq.serialize_element(&new)?;
	}

	seq.end()
}

#[derive(Serialize)]
#[serde(untagged)]
enum Number {
	Int(i64),
	Float(f64),
}
