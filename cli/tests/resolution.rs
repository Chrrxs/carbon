mod unresolved_value {
	use carbon::resolution::UnresolvedValue;

	use rbx_dom_weak::types::{
		Attributes, Axes, BinaryString, BrickColor, CFrame, Color3, Color3uint8, ColorSequence, ColorSequenceKeypoint,
		Content, ContentId, CustomPhysicalProperties, Enum, Faces, Font, FontStyle, FontWeight, Matrix3, NumberRange,
		NumberSequence, NumberSequenceKeypoint, PhysicalProperties, Ray, Rect, Region3int16, Tags, UDim, UDim2,
		Variant, Vector2, Vector3, Vector3int16,
	};

	// Based on Rojo's resolution::test (https://github.com/rojo-rbx/rojo/blob/master/src/resolution.rs#L249)
	fn resolve(class: &str, property: &str, value: &str) -> Variant {
		let unresolved: UnresolvedValue = serde_json::from_str(value).unwrap();
		unresolved.resolve(class, property).unwrap()
	}

	fn resolve_unambiguous(value: &str) -> Variant {
		let unresolved: UnresolvedValue = serde_json::from_str(value).unwrap();
		unresolved.resolve_unambiguous().unwrap()
	}

	#[test]
	fn attributes() {
		let mut attributes = Attributes::new();
		attributes.insert("String".into(), Variant::String("Hello, world!".into()));
		attributes.insert("Number".into(), Variant::Float64(13.37));
		attributes.insert("Bool".into(), Variant::Bool(true));
		attributes.insert("3D".into(), Variant::Vector3(Vector3::new(1.0, 2.0, 3.0)));
		attributes.insert(
			"2D".into(),
			Variant::UDim2(UDim2::new(UDim::new(0.5, 200), UDim::new(0.5, 100))),
		);

		assert_eq!(
			resolve(
				"Instance",
				"Attributes",
				r#"{"String": "Hello, world!", "Number": 13.37, "Bool": true, "3D": {"Vector3": [1, 2, 3]}, "2D": {"UDim2": [[0.5, 200], [0.5, 100]]}}"#
			),
			attributes.into()
		);
	}

	#[test]
	fn axes() {
		assert_eq!(resolve("ArcHandles", "Axes", "[]"), Axes::empty().into());
		assert_eq!(resolve("ArcHandles", "Axes", r#"["X"]"#), Axes::X.into());
		assert_eq!(resolve("ArcHandles", "Axes", r#"["X", "Y", "Z"]"#), Axes::all().into());
	}

	#[test]
	fn binary_string() {
		assert_eq!(
			resolve("BinaryStringValue", "Value", r#""Hello, world!""#),
			BinaryString::from("Hello, world!".as_bytes().to_vec()).into()
		);
	}

	#[test]
	fn bool() {
		assert_eq!(resolve("Part", "Anchored", "true"), true.into());
		assert_eq!(resolve_unambiguous("false"), false.into());
	}

	#[test]
	fn brick_color() {
		assert_eq!(
			resolve("Part", "BrickColor", "1032"),
			BrickColor::from_number(1032).unwrap().into()
		);
		assert_eq!(
			resolve("Part", "BrickColor", r#""Electric blue""#),
			BrickColor::from_name("Electric blue").unwrap().into()
		);
	}

	#[test]
	fn cframe() {
		assert_eq!(
			resolve("Part", "CFrame", "[1.2, 3.4, 5.6, 1, 0, 0, 0, 1, 0, 0, 0, 1]"),
			CFrame::new(Vector3::new(1.2, 3.4, 5.6), Matrix3::identity()).into()
		);
	}

	#[test]
	fn color3() {
		assert_eq!(
			resolve("Lighting", "Ambient", "[0.3, 0.6, 0.9]"),
			Color3::new(0.3, 0.6, 0.9).into()
		);
	}

	#[test]
	fn color3_uint8() {
		assert_eq!(
			resolve("Part", "Color3uint8", "[0, 100, 200]"),
			Color3uint8::new(0, 100, 200).into()
		);
	}

	#[test]
	fn color_sequence() {
		let keypoints = vec![
			ColorSequenceKeypoint::new(0.0, Color3::new(1.0, 0.0, 0.0)),
			ColorSequenceKeypoint::new(0.5, Color3::new(0.0, 1.0, 0.0)),
			ColorSequenceKeypoint::new(1.0, Color3::new(0.0, 0.0, 1.0)),
		];

		assert_eq!(
			resolve(
				"Beam",
				"Color",
				r#"[{"time": 0, "color": [1, 0, 0]}, {"time": 0.5, "color": [0, 1, 0]}, {"time": 1, "color": [0, 0, 1]}]"#
			),
			ColorSequence { keypoints }.into()
		);
	}

	#[test]
	fn content() {
		assert_eq!(
			resolve("Decal", "TextureContent", r#""rbxasset://some-uri.png""#),
			Content::from("rbxasset://some-uri.png").into(),
		);
	}

	#[test]
	fn content_id() {
		assert_eq!(
			resolve("Decal", "Texture", r#""rbxassetid://1234567890""#),
			ContentId::from("rbxassetid://1234567890").into(),
		);
	}

	#[test]
	fn enums() {
		assert_eq!(resolve("Part", "Shape", r#""Ball""#), Enum::from_u32(0).into());
		assert_eq!(resolve("Part", "Shape", r#""Block""#), Enum::from_u32(1).into());
		assert_eq!(resolve("Part", "Shape", r#""Cylinder""#), Enum::from_u32(2).into());
	}

	#[test]
	fn faces() {
		assert_eq!(resolve("Handles", "Faces", "[]"), Faces::empty().into());
		assert_eq!(resolve("Handles", "Faces", r#"["Right"]"#), Faces::RIGHT.into());
		assert_eq!(
			resolve(
				"Handles",
				"Faces",
				r#"["Right", "Top", "Back", "Left", "Bottom", "Front"]"#
			),
			Faces::all().into()
		);
	}

	#[test]
	fn float32() {
		assert_eq!(resolve("Players", "RespawnTime", "4.2"), Variant::Float32(4.2));
		assert_eq!(
			resolve("Players", "RespawnTime", "12345.678"),
			Variant::Float32(12345.678)
		);
	}

	#[test]
	fn float64() {
		assert_eq!(resolve("Sound", "PlaybackLoudness", "4.2"), Variant::Float64(4.2));
		assert_eq!(
			resolve("Sound", "PlaybackLoudness", "12345.6789"),
			Variant::Float64(12345.6789)
		);
		assert_eq!(resolve_unambiguous("6.9"), Variant::Float64(6.9));
	}

	#[test]
	fn font() {
		let font = Font::new(
			"rbxasset://fonts/families/Ubuntu.json",
			FontWeight::Bold,
			FontStyle::Italic,
		);

		assert_eq!(
			resolve(
				"TextLabel",
				"FontFace",
				r#"{"family": "rbxasset://fonts/families/SourceSansPro.json", "weight": "Regular", "style": "Normal"}"#
			),
			Font::default().into()
		);
		assert_eq!(
			resolve(
				"TextLabel",
				"FontFace",
				r#"{"family": "rbxasset://fonts/families/Ubuntu.json", "weight": "Bold", "style": "Italic"}"#
			),
			font.into()
		);
	}

	#[test]
	fn int32() {
		assert_eq!(resolve("Frame", "ZIndex", "8"), Variant::Int32(8));
		assert_eq!(resolve("Frame", "ZIndex", "999999999"), Variant::Int32(999999999));
	}

	#[test]
	fn int64() {
		assert_eq!(resolve("Player", "UserId", "8"), Variant::Int64(8));
		assert_eq!(resolve("Player", "UserId", "999999999"), Variant::Int64(999999999));
	}

	#[test]
	fn number_range() {
		assert_eq!(
			resolve("ParticleEmitter", "Lifetime", "[1.2, 3.4]"),
			NumberRange::new(1.2, 3.4).into()
		);
	}

	#[test]
	fn number_sequence() {
		let keypoints = vec![
			NumberSequenceKeypoint::new(0.0, 0.0, 0.0),
			NumberSequenceKeypoint::new(0.5, 0.3, 0.0),
			NumberSequenceKeypoint::new(1.0, 0.8, 0.0),
		];

		assert_eq!(
			resolve(
				"Beam",
				"Transparency",
				r#"[{"time": 0, "value": 0, "envelope": 0}, {"time": 0.5, "value": 0.3, "envelope": 0}, {"time": 1, "value": 0.8, "envelope": 0}]"#
			),
			NumberSequence { keypoints }.into()
		);
	}

	#[test]
	fn optional_cframe() {
		assert_eq!(
			resolve("Model", "WorldPivotData", "[1.2, 3.4, 5.6, 1, 0, 0, 0, 1, 0, 0, 0, 1]"),
			Some(CFrame::new(Vector3::new(1.2, 3.4, 5.6), Matrix3::identity())).into()
		);
	}

	#[test]
	fn empty_optional_cframe() {
		assert_eq!(
			resolve("Model", "WorldPivotData", r#""null""#),
			Variant::OptionalCFrame(None)
		);
	}

	#[test]
	fn physical_properties() {
		let properties = PhysicalProperties::Custom(CustomPhysicalProperties::new(1.2, 3.4, 5.6, 7.8, 9.0, 10.0));

		assert_eq!(
			resolve(
				"Part",
				"CustomPhysicalProperties",
				r#"{"density": 1.2, "friction": 3.4, "elasticity": 5.6, "frictionWeight": 7.8, "elasticityWeight": 9, "acousticAbsorption": 10}"#
			),
			properties.into()
		);
		assert_eq!(
			resolve("Part", "CustomPhysicalProperties", r#""Default""#),
			PhysicalProperties::Default.into()
		);
	}

	#[test]
	fn ray() {
		assert_eq!(
			resolve("RayValue", "Value", "[[1.2, 3.4, 5.6], [1.2, 3.4, 5.6]]"),
			Ray::new(Vector3::new(1.2, 3.4, 5.6), Vector3::new(1.2, 3.4, 5.6)).into()
		);
	}

	#[test]
	fn rect() {
		assert_eq!(
			resolve("ImageButton", "SliceCenter", "[1.2, 3.4, 5.6, 7.8]"),
			Rect::new(Vector2::new(1.2, 3.4), Vector2::new(5.6, 7.8)).into()
		);
	}

	#[test]
	fn referent() {
		// TODO: Implement Ref
		// assert_eq!(resolve("Model", "PrimaryPart", ""), Ref::none().into());
	}

	#[test]
	fn region3() {
		// Currently Region3 property does not exist

		// assert_eq!(
		// 	resolve("Not", "Available", "[[1.2, 3.4, 5.6], [1.2, 3.4, 5.6]]"),
		// 	Region3::new(Vector3::new(1.2, 3.4, 5.6), Vector3::new(1.2, 3.4, 5.6)).into()
		// );
	}

	#[test]
	fn region3_int16() {
		assert_eq!(
			resolve("Terrain", "MaxExtents", "[[1, 2, 3], [4, 5, 6]]"),
			Region3int16::new(Vector3int16::new(1, 2, 3), Vector3int16::new(4, 5, 6)).into()
		);
	}

	#[test]
	fn shared_string() {
		// Currently there is no valid SharedString property to test

		// assert_eq!(
		// 	resolve("Not", "Available", r#""Hello, world!""#),
		// 	SharedString::new("Hello, world!".as_bytes().to_vec()).into()
		// );
	}

	#[test]
	fn string() {
		assert_eq!(resolve("Instance", "Name", r#""Carbon""#), "Carbon".into());
		assert_eq!(resolve_unambiguous(r#""Cool!""#), "Cool!".into());
	}

	#[test]
	fn tags() {
		let mut tags = Tags::new();
		tags.push("foo");
		tags.push("bar");

		assert_eq!(resolve("Instance", "Tags", r#"["foo", "bar"]"#), tags.into());
	}

	#[test]
	fn udim() {
		assert_eq!(
			resolve("UIListLayout", "Padding", "[0.5, 500]"),
			UDim::new(0.5, 500).into()
		);
	}

	#[test]
	fn udim2() {
		assert_eq!(
			resolve("Frame", "Size", "[[0.5, 500], [1, 1000]]"),
			UDim2::new(UDim::new(0.5, 500), UDim::new(1.0, 1000)).into()
		);
	}

	#[test]
	fn vector2() {
		assert_eq!(
			resolve("ImageLabel", "ImageRectSize", "[1.2, 3.4]"),
			Vector2::new(1.2, 3.4).into()
		);
	}

	#[test]
	fn vector2_int16() {
		// Currently Vector2int16 property does not exist

		// assert_eq!(
		// 	resolve("Not", "Available", "[1, 2]"),
		// 	Vector2int16::new(1, 2).into()
		// );
	}

	#[test]
	fn vector3() {
		assert_eq!(
			resolve("Part", "Size", "[1.2, 3.4, 5.6]"),
			Vector3::new(1.2, 3.4, 5.6).into()
		);
	}

	#[test]
	fn vector3_int16() {
		assert_eq!(
			resolve("TerrainRegion", "ExtentsMax", "[1, 2, 3]"),
			Vector3int16::new(1, 2, 3).into()
		);
	}
}

mod resolved_value {

	use approx::assert_relative_eq;
	use carbon::resolution::UnresolvedValue;

	use rbx_dom_weak::types::{
		Attributes, Axes, BinaryString, BrickColor, CFrame, Color3, Color3uint8, ColorSequence, ColorSequenceKeypoint,
		Content, ContentId, ContentType, CustomPhysicalProperties, Enum, Faces, Font, FontStyle, FontWeight, Matrix3,
		NumberRange, NumberSequence, NumberSequenceKeypoint, PhysicalProperties, Ray, Rect, Ref, Region3, Region3int16,
		SharedString, Tags, UDim, UDim2, Variant, Vector2, Vector2int16, Vector3, Vector3int16,
	};
	use serde_json::{json, Value};

	fn from_variant<V: Into<Variant>>(variant: V) -> Value {
		let unresolved = UnresolvedValue::from_variant(variant.into(), "", "");
		serde_json::to_value(unresolved).unwrap()
	}

	fn from_variant_enum(value: u32, class: &str, property: &str) -> Value {
		let unresolved = UnresolvedValue::from_variant(Enum::from_u32(value).into(), class, property);
		serde_json::to_value(&unresolved).unwrap()
	}

	fn roundtrip_variant(variant: Variant, class: &str, property: &str) -> Variant {
		let unresolved = UnresolvedValue::from_variant(variant, class, property);
		let encoded = serde_json::to_vec(&unresolved).unwrap();
		let decoded: UnresolvedValue = serde_json::from_slice(&encoded).unwrap();
		decoded.resolve(class, property).unwrap()
	}

	#[test]
	fn custom_serialized_color_has_one_canonical_source_encoding() {
		let packed =
			UnresolvedValue::from_variant(Variant::Color3uint8(Color3uint8::new(163, 162, 165)), "Part", "Color");
		let logical = UnresolvedValue::from_variant(
			Variant::Color3(Color3::new(163.0 / 255.0, 162.0 / 255.0, 165.0 / 255.0)),
			"Part",
			"Color",
		);
		assert_eq!(serde_json::to_value(packed).unwrap(), json!([163, 162, 165]));
		assert_eq!(serde_json::to_value(logical).unwrap(), json!([163, 162, 165]));
	}

	#[test]
	fn low_custom_serialized_color_uses_typed_source_encoding() {
		let packed = UnresolvedValue::from_variant(Variant::Color3uint8(Color3uint8::new(0, 1, 1)), "Part", "Color");
		let logical = UnresolvedValue::from_variant(
			Variant::Color3(Color3::new(0.0, 1.0 / 255.0, 1.0 / 255.0)),
			"Part",
			"Color",
		);
		let expected = json!({ "Color3uint8": [0, 1, 1] });
		assert_eq!(serde_json::to_value(&packed).unwrap(), expected);
		assert_eq!(serde_json::to_value(logical).unwrap(), expected);
		assert_eq!(
			packed.resolve("Part", "Color").unwrap(),
			Variant::Color3(Color3::new(0.0, 1.0 / 255.0, 1.0 / 255.0))
		);
	}

	#[test]
	fn preserves_large_int64_values_exactly() {
		for expected in [i64::MAX - 123, i64::MIN + 123] {
			assert_eq!(
				roundtrip_variant(Variant::Int64(expected), "IntValue", "Value"),
				Variant::Int64(expected)
			);
		}
	}

	#[test]
	fn preserves_negative_zero_bits_for_scalars_and_composites() {
		let Variant::Float64(scalar) = roundtrip_variant(Variant::Float64(-0.0), "NumberValue", "Value") else {
			panic!("NumberValue.Value changed type");
		};
		assert_eq!(scalar.to_bits(), (-0.0_f64).to_bits());

		let Variant::NumberRange(range) = roundtrip_variant(
			Variant::NumberRange(NumberRange::new(-0.0, 1.0)),
			"ParticleEmitter",
			"Lifetime",
		) else {
			panic!("ParticleEmitter.Lifetime changed type");
		};
		assert_eq!(range.min.to_bits(), (-0.0_f32).to_bits());
	}

	#[test]
	fn preserves_non_finite_components_in_composite_values() {
		let Variant::NumberRange(range) = roundtrip_variant(
			Variant::NumberRange(NumberRange::new(f32::NEG_INFINITY, f32::INFINITY)),
			"ParticleEmitter",
			"Lifetime",
		) else {
			panic!("ParticleEmitter.Lifetime changed type");
		};
		assert_eq!(range.min.to_bits(), f32::NEG_INFINITY.to_bits());
		assert_eq!(range.max.to_bits(), f32::INFINITY.to_bits());

		let nan = f32::from_bits(0x7fc0_0123);
		let Variant::Color3(color) = roundtrip_variant(
			Variant::Color3(Color3::new(nan, f32::INFINITY, f32::NEG_INFINITY)),
			"Lighting",
			"Ambient",
		) else {
			panic!("Lighting.Ambient changed type");
		};
		assert_eq!(color.r.to_bits(), nan.to_bits());
		assert_eq!(color.g.to_bits(), f32::INFINITY.to_bits());
		assert_eq!(color.b.to_bits(), f32::NEG_INFINITY.to_bits());
	}

	#[test]
	fn preserves_non_finite_components_across_all_resolved_float_composites() {
		let infinity = f32::INFINITY;
		let cframe = CFrame::new(Vector3::new(infinity, 2.0, 3.0), Matrix3::identity());
		let cases = vec![
			Variant::Attributes(Attributes::new().with("Special", Variant::Float64(f64::INFINITY))),
			Variant::CFrame(cframe),
			Variant::Color3(Color3::new(infinity, 0.0, 0.0)),
			Variant::ColorSequence(ColorSequence {
				keypoints: vec![ColorSequenceKeypoint::new(infinity, Color3::new(1.0, 0.0, 0.0))],
			}),
			Variant::NumberRange(NumberRange::new(infinity, infinity)),
			Variant::NumberSequence(NumberSequence {
				keypoints: vec![NumberSequenceKeypoint::new(infinity, 1.0, 0.0)],
			}),
			Variant::OptionalCFrame(Some(cframe)),
			Variant::PhysicalProperties(PhysicalProperties::Custom(CustomPhysicalProperties::new(
				infinity, 0.5, 0.5, 1.0, 1.0, 1.0,
			))),
			Variant::Ray(Ray::new(Vector3::new(infinity, 0.0, 0.0), Vector3::new(0.0, 1.0, 0.0))),
			Variant::Rect(Rect::new(Vector2::new(infinity, 0.0), Vector2::new(1.0, 1.0))),
			Variant::Region3(Region3::new(
				Vector3::new(infinity, 0.0, 0.0),
				Vector3::new(1.0, 1.0, 1.0),
			)),
			Variant::UDim(UDim::new(infinity, 0)),
			Variant::UDim2(UDim2::new(UDim::new(infinity, 0), UDim::new(1.0, 0))),
			Variant::Vector2(Vector2::new(infinity, 0.0)),
			Variant::Vector3(Vector3::new(infinity, 0.0, 0.0)),
		];

		for expected in cases {
			let actual = roundtrip_variant(expected.clone(), "Part", "FutureProperty");
			assert_eq!(actual, expected);
		}
	}

	#[test]
	fn distinguishes_content_none_uri_and_object() {
		let target = Ref::new();
		for expected in [Content::none(), Content::from_uri(""), Content::from_referent(target)] {
			let Variant::Content(actual) =
				roundtrip_variant(Variant::Content(expected.clone()), "AdGui", "FallbackImageContent")
			else {
				panic!("AdGui.FallbackImageContent changed type");
			};
			assert_eq!(actual, expected);
		}

		assert!(matches!(Content::none().value(), ContentType::None));
	}

	#[test]
	fn preserves_finite_float_precision() {
		let position = 0.123_456_79_f32;
		let value = UnresolvedValue::from_variant(
			CFrame::new(Vector3::new(position, 0.0, 0.0), Matrix3::identity()).into(),
			"Part",
			"CFrame",
		);
		let json = serde_json::to_value(value).unwrap();

		assert_eq!(json[0].as_f64(), Some(position as f64));
	}

	#[test]
	fn tags_unknown_properties_with_their_variant_type() {
		let unresolved = UnresolvedValue::from_variant(Variant::Bool(true), "HumanoidDescription", "FutureProperty");
		let json = serde_json::to_value(&unresolved).unwrap();

		assert_eq!(json, json!({ "Bool": true }));
		assert_eq!(
			unresolved.resolve("HumanoidDescription", "FutureProperty").unwrap(),
			Variant::Bool(true)
		);
	}

	#[test]
	fn preserves_lighting_migration_attribute_integer_type() {
		let mut attributes = Attributes::new();
		attributes.insert("RBX_OriginalTechnologyOnFileLoad".into(), Variant::Float64(3.0));

		let unresolved = UnresolvedValue::from_variant(attributes.into(), "Lighting", "Attributes");
		assert_eq!(
			serde_json::to_value(unresolved).unwrap(),
			json!({ "RBX_OriginalTechnologyOnFileLoad": { "Int32": 3 } })
		);
	}

	fn assert_eq(mut value: Value, mut expected: Value) {
		if let Some(num) = value.as_number() {
			assert_relative_eq!(
				num.as_f64().unwrap(),
				expected.as_number().unwrap().as_f64().unwrap(),
				epsilon = 0.001
			);
		} else if let Some(arr) = value.as_array_mut() {
			let expected = expected.as_array_mut().unwrap();

			if arr.len() != expected.len() {
				assert_eq!(arr, expected);
			}

			for (index, value) in arr.iter_mut().enumerate() {
				assert_eq(value.take(), expected[index].take());
			}
		} else if let Some(obj) = value.as_object_mut() {
			let expected = expected.as_object_mut().unwrap();

			if obj.len() != expected.len() {
				assert_eq!(obj, expected);
			}

			for (key, value) in obj.iter_mut() {
				assert_eq(value.take(), expected[key].take());
			}
		} else {
			assert_eq!(value, expected);
		}
	}

	#[test]
	fn attributes() {
		let mut attributes = Attributes::new();
		attributes.insert("String".into(), Variant::String("Hello, world!".into()));
		attributes.insert("Number".into(), Variant::Float64(13.37));
		attributes.insert("Bool".into(), Variant::Bool(true));
		attributes.insert("3D".into(), Variant::Vector3(Vector3::new(1.0, 2.0, 3.0)));
		attributes.insert(
			"2D".into(),
			Variant::UDim2(UDim2::new(UDim::new(0.5, 200), UDim::new(0.5, 100))),
		);

		assert_eq(
			from_variant(attributes),
			json!({
				"String": "Hello, world!",
				"Number": 13.37,
				"Bool": true,
				"3D": {
					"Vector3": [1, 2, 3]
				},
				"2D": {
					"UDim2": [[0.5, 200], [0.5, 100]]
				}
			}),
		);
	}

	#[test]
	fn axes() {
		assert_eq(from_variant(Axes::empty()), json!([]));
		assert_eq(from_variant(Axes::X), json!(["X"]));
		assert_eq(from_variant(Axes::all()), json!(["X", "Y", "Z"]));
	}

	#[test]
	fn binary_string() {
		assert_eq(
			from_variant(BinaryString::from("Hello, world!".as_bytes())),
			json!("Hello, world!"),
		);
	}

	#[test]
	fn bool() {
		assert_eq(from_variant(true), json!(true));
		assert_eq(from_variant(false), json!(false));
	}

	#[test]
	fn brick_color() {
		assert_eq(
			from_variant(BrickColor::from_name("Electric blue").unwrap()),
			json!("Electric blue"),
		);
	}

	#[test]
	fn cframe() {
		assert_eq(
			from_variant(CFrame::new(Vector3::new(1.2, 3.4, 5.6), Matrix3::identity())),
			json!([1.2, 3.4, 5.6, 1, 0, 0, 0, 1, 0, 0, 0, 1]),
		);
	}

	#[test]
	fn color3() {
		assert_eq(from_variant(Color3::new(0.3, 0.6, 0.9)), json!([0.3, 0.6, 0.9]));
	}

	#[test]
	fn color3_uint8() {
		assert_eq(from_variant(Color3uint8::new(0, 100, 200)), json!([0, 100, 200]));
	}

	#[test]
	fn color_sequence() {
		let keypoints = vec![
			ColorSequenceKeypoint::new(0.0, Color3::new(1.0, 0.0, 0.0)),
			ColorSequenceKeypoint::new(0.5, Color3::new(0.0, 1.0, 0.0)),
			ColorSequenceKeypoint::new(1.0, Color3::new(0.0, 0.0, 1.0)),
		];

		assert_eq(
			from_variant(ColorSequence { keypoints }),
			json!([
				{"time": 0, "color": [1, 0, 0]},
				{"time": 0.5, "color": [0, 1, 0]},
				{"time": 1, "color": [0, 0, 1]},
			]),
		);
	}

	#[test]
	fn content() {
		assert_eq(
			from_variant(Content::from("rbxasset://some-uri.png")),
			json!("rbxasset://some-uri.png"),
		);
	}

	#[test]
	fn content_id() {
		assert_eq(
			from_variant(ContentId::from("rbxassetid://1234567890")),
			json!("rbxassetid://1234567890"),
		);
	}

	#[test]
	fn enums() {
		assert_eq(from_variant_enum(0, "Part", "Shape"), json!("Ball"));
		assert_eq(from_variant_enum(1, "Part", "Shape"), json!("Block"));
		assert_eq(from_variant_enum(2, "Part", "Shape"), json!("Cylinder"));
	}

	#[test]
	fn faces() {
		assert_eq(from_variant(Faces::empty()), json!([]));
		assert_eq(from_variant(Faces::RIGHT), json!(["Right"]));
		assert_eq(
			from_variant(Faces::all()),
			json!(["Right", "Top", "Back", "Left", "Bottom", "Front"]),
		);
	}

	#[test]
	fn float32() {
		assert_eq(from_variant(4.2f32), json!(4.2));
		assert_eq(from_variant(12345.678f32), json!(12345.678));
	}

	#[test]
	fn float64() {
		assert_eq(from_variant(4.2f64), json!(4.2));
		assert_eq(from_variant(12345.6789f64), json!(12345.6789));
	}

	#[test]
	fn font() {
		let font = Font::new(
			"rbxasset://fonts/families/Ubuntu.json",
			FontWeight::Bold,
			FontStyle::Italic,
		);

		let mut test1 =
			json!({"family": "rbxasset://fonts/families/SourceSansPro.json", "weight": "Regular", "style": "Normal"});
		let mut test2 = json!({"family": "rbxasset://fonts/families/Ubuntu.json", "weight": "Bold", "style": "Italic"});

		test1["cachedFaceId"] = Value::Null;
		test2["cachedFaceId"] = Value::Null;

		assert_eq(from_variant(Font::default()), test1);
		assert_eq(from_variant(font), test2);
	}

	#[test]
	fn int32() {
		assert_eq(from_variant(8i32), json!(8));
		assert_eq(from_variant(999999999i32), json!(999999999));
	}

	#[test]
	fn int64() {
		assert_eq(from_variant(8i64), json!(8));
		assert_eq(from_variant(999999999i64), json!(999999999));
	}

	#[test]
	fn number_range() {
		assert_eq(from_variant(NumberRange::new(1.2, 3.4)), json!([1.2, 3.4]));
	}

	#[test]
	fn number_sequence() {
		let keypoints = vec![
			NumberSequenceKeypoint::new(0.0, 0.0, 0.0),
			NumberSequenceKeypoint::new(0.5, 0.3, 0.0),
			NumberSequenceKeypoint::new(1.0, 0.8, 0.0),
		];

		assert_eq(
			from_variant(NumberSequence { keypoints }),
			json!([
				{"time": 0, "value": 0, "envelope": 0},
				{"time": 0.5, "value": 0.3, "envelope": 0},
				{"time": 1, "value": 0.8, "envelope": 0},
			]),
		);
	}

	#[test]
	fn optional_cframe() {
		assert_eq(
			from_variant(CFrame::new(Vector3::new(1.2, 3.4, 5.6), Matrix3::identity())),
			json!([1.2, 3.4, 5.6, 1, 0, 0, 0, 1, 0, 0, 0, 1]),
		);
	}

	#[test]
	fn physical_properties() {
		let properties = PhysicalProperties::Custom(CustomPhysicalProperties::new(1.2, 3.4, 5.6, 7.8, 9.0, 10.0));

		assert_eq(
			from_variant(properties),
			json!({
				"density": 1.2,
				"friction": 3.4,
				"elasticity": 5.6,
				"frictionWeight": 7.8,
				"elasticityWeight": 9,
				"acousticAbsorption": 10,
			}),
		);
		assert_eq(from_variant(PhysicalProperties::Default), json!("Default"));
	}

	#[test]
	fn ray() {
		assert_eq(
			from_variant(Ray::new(Vector3::new(1.2, 3.4, 5.6), Vector3::new(1.2, 3.4, 5.6))),
			json!([[1.2, 3.4, 5.6], [1.2, 3.4, 5.6]]),
		);
	}

	#[test]
	fn rect() {
		assert_eq(
			from_variant(Rect::new(Vector2::new(1.2, 3.4), Vector2::new(5.6, 7.8))),
			json!([1.2, 3.4, 5.6, 7.8]),
		);
	}

	#[test]
	fn referent() {
		// TODO: Implement Ref
		// assert_eq(from_variant(Ref::none()), json!(null));
	}

	#[test]
	fn region3() {
		assert_eq(
			from_variant(Region3::new(Vector3::new(1.2, 3.4, 5.6), Vector3::new(1.2, 3.4, 5.6))),
			json!([[1.2, 3.4, 5.6], [1.2, 3.4, 5.6]]),
		);
	}

	#[test]
	fn region3_int16() {
		assert_eq(
			from_variant(Region3int16::new(
				Vector3int16::new(1, 2, 3),
				Vector3int16::new(4, 5, 6),
			)),
			json!([[1, 2, 3], [4, 5, 6]]),
		);
	}

	#[test]
	fn shared_string() {
		assert_eq(
			from_variant(SharedString::new("Hello, world!".as_bytes().to_vec())),
			json!("Hello, world!"),
		);
	}

	#[test]
	fn string() {
		assert_eq(from_variant("Carbon"), json!("Carbon"));
	}

	#[test]
	fn tags() {
		let mut tags = Tags::new();
		tags.push("foo");
		tags.push("bar");

		assert_eq(from_variant(tags), json!(["foo", "bar"]));
	}

	#[test]
	fn udim() {
		assert_eq(from_variant(UDim::new(0.5, 500)), json!([0.5, 500]));
	}

	#[test]
	fn udim2() {
		assert_eq(
			from_variant(UDim2::new(UDim::new(0.5, 500), UDim::new(1.0, 1000))),
			json!([[0.5, 500], [1, 1000]]),
		);
	}

	#[test]
	fn vector2() {
		assert_eq(from_variant(Vector2::new(1.2, 3.4)), json!([1.2, 3.4]));
	}

	#[test]
	fn vector2_int16() {
		assert_eq(from_variant(Vector2int16::new(1, 2)), json!([1, 2]));
	}

	#[test]
	fn vector3() {
		assert_eq(from_variant(Vector3::new(1.2, 3.4, 5.6)), json!([1.2, 3.4, 5.6]));
	}

	#[test]
	fn vector3_int16() {
		assert_eq(from_variant(Vector3int16::new(1, 2, 3)), json!([1, 2, 3]));
	}
}
