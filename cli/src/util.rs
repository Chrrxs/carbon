use anyhow::{Context, Result};
use directories::UserDirs;
use env_logger::WriteStyle;
use log::LevelFilter;
use rbx_dom_weak::types::VariantType;
use rbx_reflection::{
	ClassDescriptor, ClassTag, DataType, EnumDescriptor, PropertyDescriptor, PropertyKind, PropertySerialization,
	PropertyTag, ReflectionDatabase, Scriptability,
};
use serde::Deserialize;
use std::{collections::HashMap, env, fs, path::PathBuf, process::Command, sync::LazyLock};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReflectionOverlay {
	version: [u32; 4],
	classes: HashMap<String, OverlayClass>,
	enums: HashMap<String, OverlayEnum>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OverlayClass {
	superclass: Option<String>,
	tags: Vec<ClassTag>,
	properties: HashMap<String, OverlayProperty>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OverlayProperty {
	data_type: OverlayDataType,
	scriptability: Scriptability,
	tags: Vec<PropertyTag>,
	serializes: bool,
}

#[derive(Deserialize)]
enum OverlayDataType {
	Value(VariantType),
	Enum(String),
}

#[derive(Deserialize)]
struct OverlayEnum {
	items: HashMap<String, u32>,
}

static REFLECTION_OVERLAY: LazyLock<ReflectionOverlay> = LazyLock::new(|| {
	serde_json::from_slice(include_bytes!("../../studio-plugin/src/Lib/Dom/reflection.json"))
		.expect("bundled Carbon reflection overlay should decode")
});

static REFLECTION_DATABASE: LazyLock<ReflectionDatabase<'static>> = LazyLock::new(|| {
	let overlay = &*REFLECTION_OVERLAY;
	let mut database = rbx_reflection_database::get_bundled().clone();
	database.version = overlay.version;

	for (class_name, current) in &overlay.classes {
		let class_name = class_name.as_str();
		let descriptor = database
			.classes
			.entry(class_name)
			.or_insert_with(|| ClassDescriptor::new(class_name));
		descriptor.name = class_name;
		descriptor.superclass = current.superclass.as_deref();
		descriptor.tags = current.tags.iter().copied().collect();

		for (property_name, current) in &current.properties {
			let property_name = property_name.as_str();
			let existing = descriptor.properties.get(property_name);
			let data_type =
				existing
					.map(|property| property.data_type.clone())
					.unwrap_or_else(|| match &current.data_type {
						OverlayDataType::Value(data_type) => DataType::Value(*data_type),
						OverlayDataType::Enum(enum_name) => DataType::Enum(enum_name.as_str()),
					});
			let preserve_kind = existing.is_some_and(|property| {
				matches!(
					&property.kind,
					PropertyKind::Alias { .. }
						| PropertyKind::Canonical {
							serialization: PropertySerialization::SerializesAs(_) | PropertySerialization::Migrate(_)
						}
				)
			});
			let kind = if preserve_kind {
				existing.expect("checked above").kind.clone()
			} else {
				PropertyKind::Canonical {
					serialization: if current.serializes {
						PropertySerialization::Serializes
					} else {
						PropertySerialization::DoesNotSerialize
					},
				}
			};
			let mut property = PropertyDescriptor::new(property_name, data_type);
			property.scriptability = current.scriptability;
			property.tags = current.tags.iter().copied().collect();
			property.kind = kind;
			descriptor.properties.insert(property_name, property);
		}
	}

	database.enums = overlay
		.enums
		.iter()
		.map(|(enum_name, current)| {
			let enum_name = enum_name.as_str();
			let mut descriptor = EnumDescriptor::new(enum_name);
			descriptor.items = current
				.items
				.iter()
				.map(|(name, value)| (name.as_str(), *value))
				.collect();
			(enum_name, descriptor)
		})
		.collect();
	database
});

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
	&REFLECTION_DATABASE
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
		let database = get_reflection_database();
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
	}
}
