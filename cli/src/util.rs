use anyhow::{Context, Result};
use directories::UserDirs;
use env_logger::WriteStyle;
use log::LevelFilter;
use rbx_reflection::{ClassDescriptor, ClassTag, DataType, PropertyDescriptor, PropertyTag, ReflectionDatabase};
use std::{env, fs, path::PathBuf, process::Command, sync::LazyLock};

const ROBLOX_729_VERSION: [u32; 4] = [0, 729, 0, 7290838];

static REFLECTION_DATABASE: LazyLock<ReflectionDatabase<'static>> = LazyLock::new(|| {
	let mut database = rbx_reflection_database::get_local()
		.ok()
		.flatten()
		.unwrap_or_else(rbx_reflection_database::get_bundled)
		.clone();

	// rbx_reflection_database 3.0.0 is bundled from Roblox 0.728. Fill only
	// missing 0.729 descriptors so a current/future local database remains
	// authoritative.
	for (name, is_service) in [
		("DeviceDisplayService", true),
		("DisplayWakeLock", false),
		("PopLatencyService", true),
	] {
		database.classes.entry(name).or_insert_with(|| {
			let mut descriptor = ClassDescriptor::new(name);
			descriptor.superclass = Some("Instance");
			descriptor
				.tags
				.extend([ClassTag::NotCreatable, ClassTag::NotReplicated]);
			if is_service {
				descriptor.tags.insert(ClassTag::Service);
			}
			descriptor
		});
	}

	if let Some(voice_chat_service) = database.classes.get_mut("VoiceChatService") {
		voice_chat_service
			.properties
			.entry("EnableVoiceVolumeControls")
			.or_insert_with(|| {
				let mut descriptor =
					PropertyDescriptor::new("EnableVoiceVolumeControls", DataType::Enum("RolloutState"));
				descriptor.tags.insert(PropertyTag::NotBrowsable);
				descriptor
			});
	}

	let overlay_is_complete = ["DeviceDisplayService", "DisplayWakeLock", "PopLatencyService"]
		.iter()
		.all(|name| database.classes.contains_key(name))
		&& database
			.classes
			.get("VoiceChatService")
			.is_some_and(|class| class.properties.contains_key("EnableVoiceVolumeControls"));
	if database.version >= [0, 728, 0, 0] && database.version < ROBLOX_729_VERSION && overlay_is_complete {
		database.version = ROBLOX_729_VERSION;
	}

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
	use rbx_reflection::{PropertyKind, PropertySerialization, Scriptability};
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
	fn bundled_reflection_covers_roblox_729_class_delta() {
		let database = get_reflection_database();
		assert!(database.version >= ROBLOX_729_VERSION);

		for (name, is_service) in [
			("DeviceDisplayService", true),
			("DisplayWakeLock", false),
			("PopLatencyService", true),
		] {
			let descriptor = database.classes.get(name).expect("0.729 class is missing");
			assert_eq!(descriptor.superclass, Some("Instance"));
			assert!(descriptor.properties.is_empty());
			assert!(descriptor.default_properties.is_empty());
			assert!(descriptor.tags.contains(&ClassTag::NotCreatable));
			assert!(descriptor.tags.contains(&ClassTag::NotReplicated));
			assert_eq!(descriptor.tags.contains(&ClassTag::Service), is_service);
		}

		let voice_chat_service = database
			.classes
			.get("VoiceChatService")
			.expect("VoiceChatService is missing");
		let property = voice_chat_service
			.properties
			.get("EnableVoiceVolumeControls")
			.expect("0.729 VoiceChatService property is missing");
		assert!(matches!(property.data_type, DataType::Enum("RolloutState")));
		assert!(matches!(property.scriptability, Scriptability::None));
		assert!(matches!(
			property.kind,
			PropertyKind::Canonical {
				serialization: PropertySerialization::Serializes
			}
		));
		assert_eq!(property.tags.len(), 1);
		assert!(property.tags.contains(&PropertyTag::NotBrowsable));
		assert!(!voice_chat_service
			.default_properties
			.contains_key("EnableVoiceVolumeControls"));
	}
}
