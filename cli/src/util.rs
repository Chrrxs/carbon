use anyhow::{ensure, Context, Result};
use directories::UserDirs;
use env_logger::WriteStyle;
use log::LevelFilter;
use rbx_dom_weak::types::VariantType;
use rbx_reflection::{
	ClassDescriptor, ClassTag, DataType, EnumDescriptor, PropertyDescriptor, PropertyKind, PropertySerialization,
	PropertyTag, ReflectionDatabase, Scriptability,
};
use serde::Deserialize;
#[cfg(test)]
use std::sync::LazyLock;
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

fn build_database_from_api(dump: ApiDump, version: [u32; 4]) -> ReflectionDatabase<'static> {
	let mut database = rbx_reflection_database::get_bundled().clone();
	database.version = version;
	let mut unsupported_properties = Vec::new();

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
			let existing = descriptor.properties.get(property_name);
			let data_type = existing
				.map(|property| property.data_type.clone())
				.unwrap_or(value_type);
			let property_scriptability = existing
				.filter(|property| matches!(&property.scriptability, Scriptability::Custom))
				.map(|property| property.scriptability)
				.unwrap_or(property_scriptability);
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
					serialization: if serializes {
						PropertySerialization::Serializes
					} else {
						PropertySerialization::DoesNotSerialize
					},
				}
			};

			let mut property = PropertyDescriptor::new(property_name, data_type);
			property.scriptability = property_scriptability;
			property.tags = property_tags;
			property.kind = kind;
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
	let mut command = Command::new("powershell.exe");
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
#[cfg(test)]
static BUNDLED_FALLBACK_DATABASE: LazyLock<ReflectionDatabase<'static>> =
	LazyLock::new(|| rbx_reflection_database::get_bundled().clone());

pub fn init_reflection() -> Result<&'static ReflectionSnapshot> {
	if let Some(snapshot) = LIVE_SNAPSHOT.get() {
		return Ok(snapshot);
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
	if let Some(snapshot) = LIVE_SNAPSHOT.get() {
		&snapshot.database
	} else {
		#[cfg(test)]
		{
			&BUNDLED_FALLBACK_DATABASE
		}
		#[cfg(not(test))]
		{
			let snapshot = init_reflection().expect("failed to initialize live reflection database");
			&snapshot.database
		}
	}
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
}
