use std::{
	collections::HashMap,
	fs::{self, Metadata},
	path::{Path, PathBuf},
	process::Command,
	thread,
	time::{Duration, Instant, SystemTime},
};

use anyhow::{ensure, Context, Result};

pub(crate) const CAPTURE_TIMEOUT: Duration = Duration::from_secs(6 * 60);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const STABLE_POLLS: usize = 2;
const MODIFIED_TIME_SLOP: Duration = Duration::from_secs(2);
const AUTOSAVES_OVERRIDE: &str = "CARBON_STUDIO_AUTOSAVES_DIR";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RecoveryFingerprint {
	pub(crate) len: u64,
	pub(crate) modified: SystemTime,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RecoveryKind {
	StudioAutoRecovery,
	ServedPlace,
}

impl RecoveryKind {
	pub(crate) fn label(self) -> &'static str {
		match self {
			Self::StudioAutoRecovery => "Studio auto-recovery",
			Self::ServedPlace => "manually saved temporary place",
		}
	}
}

#[derive(Clone, Debug)]
enum RecoveryLocation {
	Directory(PathBuf),
	File(PathBuf),
}

#[derive(Clone, Debug)]
pub(crate) struct RecoverySource {
	kind: RecoveryKind,
	location: RecoveryLocation,
	baseline: HashMap<PathBuf, RecoveryFingerprint>,
}

impl RecoverySource {
	pub(crate) fn studio_auto_recovery(directory: PathBuf) -> Result<Self> {
		let baseline = inventory(&directory)?;
		Ok(Self {
			kind: RecoveryKind::StudioAutoRecovery,
			location: RecoveryLocation::Directory(directory),
			baseline,
		})
	}

	pub(crate) fn served_place(path: PathBuf) -> Result<Self> {
		let baseline = inventory_file(&path)?;
		Ok(Self {
			kind: RecoveryKind::ServedPlace,
			location: RecoveryLocation::File(path),
			baseline,
		})
	}

	fn inventory(&self) -> Result<HashMap<PathBuf, RecoveryFingerprint>> {
		match &self.location {
			RecoveryLocation::Directory(directory) => inventory(directory),
			RecoveryLocation::File(path) => inventory_file(path),
		}
	}

	fn description(&self) -> String {
		match &self.location {
			RecoveryLocation::Directory(directory) => directory.display().to_string(),
			RecoveryLocation::File(path) => path.display().to_string(),
		}
	}
}

impl RecoveryFingerprint {
	pub(crate) fn from_metadata(metadata: &Metadata) -> std::io::Result<Self> {
		Ok(Self {
			len: metadata.len(),
			modified: metadata.modified()?,
		})
	}
}

fn modified_after_capture_started(modified: SystemTime, started_at: SystemTime) -> bool {
	modified
		>= started_at
			.checked_sub(MODIFIED_TIME_SLOP)
			.unwrap_or(SystemTime::UNIX_EPOCH)
}

#[cfg(test)]
pub(crate) fn select_new_recovery(
	baseline: &HashMap<PathBuf, RecoveryFingerprint>,
	candidates: impl IntoIterator<Item = (PathBuf, RecoveryFingerprint)>,
	started_at: SystemTime,
) -> Option<PathBuf> {
	candidates
		.into_iter()
		.filter(|(path, fingerprint)| {
			is_recovery_place(path)
				&& fingerprint.len > 0
				&& modified_after_capture_started(fingerprint.modified, started_at)
				&& baseline.get(path) != Some(fingerprint)
		})
		.max_by_key(|(_, fingerprint)| fingerprint.modified)
		.map(|(path, _)| path)
}

pub(crate) fn is_recovery_place(path: &Path) -> bool {
	matches!(
		path.extension().and_then(|extension| extension.to_str()),
		Some(extension) if extension.eq_ignore_ascii_case("rbxl")
	)
}

pub(crate) fn autosaves_dir() -> Result<PathBuf> {
	if let Some(path) = std::env::var_os(AUTOSAVES_OVERRIDE) {
		let path = PathBuf::from(path);
		ensure!(
			path.is_dir(),
			"{AUTOSAVES_OVERRIDE} is not a directory: {}",
			path.display()
		);
		return Ok(path);
	}

	#[cfg(target_os = "windows")]
	{
		let local = std::env::var_os("LOCALAPPDATA").context("Windows LOCALAPPDATA is unavailable")?;
		return Ok(PathBuf::from(local).join("Roblox/RobloxStudio/AutoSaves"));
	}

	#[cfg(target_os = "macos")]
	{
		let home = directories::BaseDirs::new().context("macOS home directory is unavailable")?;
		return Ok(home
			.home_dir()
			.join("Library/Application Support/Roblox/RobloxStudio/AutoSaves"));
	}

	#[cfg(target_os = "linux")]
	{
		ensure!(
			std::env::var_os("WSL_DISTRO_NAME").is_some(),
			"Roblox Studio auto-recovery capture is supported on Linux only through WSL"
		);
		let output = Command::new("powershell.exe")
			.args([
				"-NoProfile",
				"-NonInteractive",
				"-Command",
				"[Environment]::GetFolderPath('LocalApplicationData')",
			])
			.output()
			.context("failed to resolve Windows LOCALAPPDATA for Studio auto-recovery")?;
		ensure!(
			output.status.success(),
			"PowerShell could not resolve Windows LOCALAPPDATA for Studio auto-recovery"
		);
		let local = String::from_utf8(output.stdout)?.trim().to_owned();
		ensure!(!local.is_empty(), "Windows LOCALAPPDATA is empty");
		let windows = Path::new(&local).join("Roblox/RobloxStudio/AutoSaves");
		let translated = Command::new("wslpath")
			.arg("-u")
			.arg(&windows)
			.output()
			.context("failed to translate the Studio auto-recovery directory into WSL")?;
		ensure!(
			translated.status.success(),
			"wslpath could not translate the Studio auto-recovery directory"
		);
		Ok(PathBuf::from(String::from_utf8(translated.stdout)?.trim()))
	}

	#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
	anyhow::bail!("Roblox Studio auto-recovery capture is unsupported on this platform")
}

pub(crate) fn inventory(directory: &Path) -> Result<HashMap<PathBuf, RecoveryFingerprint>> {
	if !directory.exists() {
		return Ok(HashMap::new());
	}
	ensure!(
		directory.is_dir(),
		"Studio auto-recovery path is not a directory: {}",
		directory.display()
	);
	let mut files = HashMap::new();
	for entry in fs::read_dir(directory)
		.with_context(|| format!("failed to read Studio auto-recovery directory {}", directory.display()))?
	{
		let entry = entry?;
		let path = entry.path();
		if !is_recovery_place(&path) {
			continue;
		}
		let metadata = match entry.metadata() {
			Ok(metadata) if metadata.is_file() => metadata,
			Ok(_) => continue,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
			Err(error) => return Err(error.into()),
		};
		files.insert(path, RecoveryFingerprint::from_metadata(&metadata)?);
	}
	Ok(files)
}

fn inventory_file(path: &Path) -> Result<HashMap<PathBuf, RecoveryFingerprint>> {
	let metadata = match fs::metadata(path) {
		Ok(metadata) if metadata.is_file() => metadata,
		Ok(_) => return Ok(HashMap::new()),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
		Err(error) => return Err(error.into()),
	};
	if !is_recovery_place(path) {
		return Ok(HashMap::new());
	}
	Ok(HashMap::from([(
		path.to_owned(),
		RecoveryFingerprint::from_metadata(&metadata)?,
	)]))
}

/// Wait for a recovery file which did not exist in this state when capture
/// began. A candidate must keep the same size and modification time across two
/// polls before it is handed to the decoder, so Studio never races Carbon with
/// a partially written place.
pub(crate) fn wait_for_new_recovery<T>(
	sources: &[RecoverySource],
	started_at: SystemTime,
	timeout: Duration,
	cancelled: impl Fn() -> bool,
	mut accept: impl FnMut(&Path) -> Result<Option<T>>,
) -> Result<(RecoveryKind, PathBuf, T)> {
	ensure!(
		!sources.is_empty(),
		"Capture Manifest has no recovery source to monitor"
	);
	let deadline = Instant::now() + timeout;
	let mut stable = HashMap::<(RecoveryKind, PathBuf), (RecoveryFingerprint, usize)>::new();
	let mut rejected = HashMap::<(RecoveryKind, PathBuf), RecoveryFingerprint>::new();
	loop {
		ensure!(
			!cancelled(),
			"Capture Manifest was cancelled while waiting for Studio auto-recovery"
		);
		let mut candidates = Vec::new();
		for source in sources {
			let current = source.inventory()?;
			candidates.extend(current.into_iter().filter_map(|(path, fingerprint)| {
				let key = (source.kind, path.clone());
				(is_recovery_place(&path)
					&& fingerprint.len > 0
					&& modified_after_capture_started(fingerprint.modified, started_at)
					&& source.baseline.get(&path) != Some(&fingerprint)
					&& rejected.get(&key) != Some(&fingerprint))
				.then_some((source.kind, path, fingerprint))
			}));
		}
		candidates.sort_by_key(|(_, _, fingerprint)| std::cmp::Reverse(fingerprint.modified));

		for (kind, path, fingerprint) in candidates {
			let key = (kind, path.clone());
			let entry = stable.entry(key.clone()).or_insert((fingerprint, 0));
			if entry.0 == fingerprint {
				entry.1 += 1;
			} else {
				*entry = (fingerprint, 1);
			}
			if entry.1 < STABLE_POLLS {
				continue;
			}
			match accept(&path)? {
				Some(value) => return Ok((kind, path, value)),
				None => {
					rejected.insert(key.clone(), fingerprint);
					stable.remove(&key);
				}
			}
		}

		if Instant::now() >= deadline {
			let locations = sources
				.iter()
				.map(RecoverySource::description)
				.collect::<Vec<_>>()
				.join(", ");
			anyhow::bail!(
				"timed out after {} seconds waiting for Roblox Studio to write a new auto-recovery or save the temporary served place in {}",
				timeout.as_secs(),
				locations
			);
		}
		thread::sleep(POLL_INTERVAL);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::fs;
	use std::time::Duration;

	fn fingerprint(seconds: u64, len: u64) -> RecoveryFingerprint {
		RecoveryFingerprint {
			len,
			modified: SystemTime::UNIX_EPOCH + Duration::from_secs(seconds),
		}
	}

	#[test]
	fn selects_only_a_new_or_updated_recovery_after_capture_started() {
		let old = PathBuf::from("old.rbxl");
		let updated = PathBuf::from("updated.rbxl");
		let newest = PathBuf::from("newest.rbxl");
		let baseline = HashMap::from([
			(old.clone(), fingerprint(10, 10)),
			(updated.clone(), fingerprint(20, 20)),
		]);

		let selected = select_new_recovery(
			&baseline,
			[
				(old, fingerprint(10, 10)),
				(updated.clone(), fingerprint(31, 30)),
				(newest.clone(), fingerprint(32, 40)),
			],
			SystemTime::UNIX_EPOCH + Duration::from_secs(30),
		);

		assert_eq!(selected, Some(newest));
	}

	#[test]
	fn ignores_preexisting_and_non_place_files() {
		let baseline = HashMap::new();
		let selected = select_new_recovery(
			&baseline,
			[
				(PathBuf::from("notes.txt"), fingerprint(40, 10)),
				(PathBuf::from("stale.rbxl"), fingerprint(20, 10)),
			],
			SystemTime::UNIX_EPOCH + Duration::from_secs(30),
		);

		assert_eq!(selected, None);
	}

	#[test]
	fn accepts_a_changed_recovery_with_coarse_timestamp_precision() {
		let path = PathBuf::from("coarse.rbxl");
		let baseline = HashMap::from([(path.clone(), fingerprint(20, 10))]);
		let selected = select_new_recovery(
			&baseline,
			[(path.clone(), fingerprint(29, 20))],
			SystemTime::UNIX_EPOCH + Duration::from_secs(30),
		);

		assert_eq!(selected, Some(path));
	}

	#[test]
	fn updated_temporary_served_place_wins_the_recovery_wait() {
		let directory = std::env::temp_dir().join(format!("carbon-recovery-race-{}", uuid::Uuid::new_v4()));
		fs::create_dir_all(&directory).unwrap();
		let autosaves = directory.join("autosaves");
		fs::create_dir_all(&autosaves).unwrap();
		let served = directory.join("carbon-serve-test.rbxl");
		fs::write(&served, b"original").unwrap();
		let sources = vec![
			RecoverySource::studio_auto_recovery(autosaves).unwrap(),
			RecoverySource::served_place(served.clone()).unwrap(),
		];
		let started_at = SystemTime::now();
		fs::write(&served, b"manually saved place").unwrap();

		let (kind, path, value) = wait_for_new_recovery(
			&sources,
			started_at,
			Duration::from_secs(2),
			|| false,
			|path| Ok(Some(fs::read(path)?)),
		)
		.unwrap();

		assert_eq!(kind, RecoveryKind::ServedPlace);
		assert_eq!(path, served);
		assert_eq!(value, b"manually saved place");
		fs::remove_dir_all(directory).unwrap();
	}
}
