use anyhow::{ensure, Context, Result};
use rbx_dom_weak::{types::Variant, Ustr};
use serde_json::Value;
use std::{collections::BTreeMap, fs::File, io::BufReader, path::Path};

pub fn assert_place_instance(
	place_path: &Path,
	instance_path: &[String],
	class_name: &str,
	properties: &BTreeMap<String, Value>,
	attributes: &BTreeMap<String, Value>,
) -> Result<()> {
	let place = rbx_binary::from_reader_with_database(
		BufReader::new(
			File::open(place_path).with_context(|| format!("failed to open rebuilt place {}", place_path.display()))?,
		),
		carbon::util::get_reflection_database(),
	)
	.with_context(|| format!("failed to decode rebuilt place {}", place_path.display()))?;
	let mut instance = place.root();
	let mut traversed = Vec::new();
	for segment in instance_path {
		traversed.push(segment.as_str());
		let matches = instance
			.children()
			.iter()
			.filter_map(|referent| place.get_by_ref(*referent))
			.filter(|child| child.name == *segment)
			.collect::<Vec<_>>();
		ensure!(
			matches.len() == 1,
			"instance path {} matched {} children named {:?}, expected exactly one",
			traversed.join("/"),
			matches.len(),
			segment
		);
		instance = matches[0];
	}

	let display_path = instance_path.join("/");
	ensure!(
		instance.class.as_str() == class_name,
		"instance {display_path} had class {}, expected {class_name}",
		instance.class
	);
	for (name, expected) in properties {
		let actual = instance
			.properties
			.get(&Ustr::from(name.as_str()))
			.with_context(|| format!("instance {display_path} was missing property {name:?}"))?;
		assert_value(&format!("property {display_path}.{name}"), actual, expected)?;
	}

	if !attributes.is_empty() {
		let actual = instance
			.properties
			.get(&Ustr::from("Attributes"))
			.with_context(|| format!("instance {display_path} was missing Attributes"))?;
		let Variant::Attributes(actual) = actual else {
			anyhow::bail!("instance {display_path} Attributes had unexpected value {actual:?}");
		};
		for (name, expected) in attributes {
			let actual = actual
				.get(name.as_str())
				.with_context(|| format!("instance {display_path} was missing attribute {name:?}"))?;
			assert_value(&format!("attribute {display_path}.{name}"), actual, expected)?;
		}
	}
	Ok(())
}

fn assert_value(label: &str, actual: &Variant, expected: &Value) -> Result<()> {
	let matches = match (actual, expected) {
		(Variant::Bool(actual), Value::Bool(expected)) => actual == expected,
		(Variant::String(actual), Value::String(expected)) => actual == expected,
		(Variant::Float32(actual), Value::Number(expected)) => numeric_matches(f64::from(*actual), expected.as_f64()),
		(Variant::Float64(actual), Value::Number(expected)) => numeric_matches(*actual, expected.as_f64()),
		(Variant::Int32(actual), Value::Number(expected)) => numeric_matches(f64::from(*actual), expected.as_f64()),
		(Variant::Int64(actual), Value::Number(expected)) => numeric_matches(*actual as f64, expected.as_f64()),
		(Variant::Vector3(actual), Value::Array(expected)) if expected.len() == 3 => {
			[f64::from(actual.x), f64::from(actual.y), f64::from(actual.z)]
				.into_iter()
				.zip(expected.iter().map(Value::as_f64))
				.all(|(actual, expected)| numeric_matches(actual, expected))
		}
		_ => false,
	};
	ensure!(matches, "{label} was {actual:?}, expected {expected}");
	Ok(())
}

fn numeric_matches(actual: f64, expected: Option<f64>) -> bool {
	expected.is_some_and(|expected| (actual - expected).abs() <= f64::EPSILON.max(expected.abs() * 1e-6))
}

#[cfg(test)]
mod tests {
	use super::*;
	use rbx_dom_weak::{
		types::{Attributes, Vector3},
		InstanceBuilder, WeakDom,
	};
	use std::fs;

	#[test]
	fn rebuilt_place_assertion_checks_class_properties_and_attributes() {
		let directory = std::env::temp_dir().join(format!("carbon-place-assertion-{}", uuid::Uuid::new_v4()));
		fs::create_dir_all(&directory).unwrap();
		let path = directory.join("capture.rbxl");
		let probe = InstanceBuilder::new("Part")
			.with_name("Probe")
			.with_property("Anchored", true)
			.with_property("Size", Vector3::new(7.0, 3.0, 5.0))
			.with_property(
				"Attributes",
				Attributes::new().with("CapturedThroughAutoRecovery", true),
			);
		let place = WeakDom::new(
			InstanceBuilder::new("DataModel").with_name("game").with_child(
				InstanceBuilder::new("Workspace")
					.with_name("Workspace")
					.with_child(probe),
			),
		);
		rbx_binary::to_writer_with_database(
			File::create(&path).unwrap(),
			&place,
			place.root().children(),
			carbon::util::get_reflection_database(),
		)
		.unwrap();
		let properties = BTreeMap::from([
			("Anchored".to_owned(), Value::Bool(true)),
			("Size".to_owned(), serde_json::json!([7, 3, 5])),
		]);
		let attributes = BTreeMap::from([("CapturedThroughAutoRecovery".to_owned(), Value::Bool(true))]);

		assert_place_instance(
			&path,
			&["Workspace".to_owned(), "Probe".to_owned()],
			"Part",
			&properties,
			&attributes,
		)
		.unwrap();
		let error = assert_place_instance(
			&path,
			&["Workspace".to_owned(), "Probe".to_owned()],
			"Part",
			&BTreeMap::from([("Anchored".to_owned(), Value::Bool(false))]),
			&attributes,
		)
		.unwrap_err();
		assert!(error.to_string().contains("Anchored"));
		fs::remove_dir_all(directory).unwrap();
	}
}
