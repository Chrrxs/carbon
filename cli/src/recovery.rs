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
const CONSUMED_RECOVERY_DIRECTORY: &str = ".carbon-consumed";

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryFreshness {
	New,
	Preexisting,
}

pub(crate) enum RecoveryAcceptance<T> {
	Accept(T),
	Reject,
	Retry,
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

/// Move exact-session Studio recovery evidence out of Roblox's scan directory
/// only after the caller has committed it successfully. The archive remains on
/// the same volume so the move is atomic; any failure leaves the source intact.
pub(crate) fn quarantine_consumed_recovery(kind: RecoveryKind, path: &Path) -> Result<Option<PathBuf>> {
	if kind != RecoveryKind::StudioAutoRecovery {
		return Ok(None);
	}
	ensure!(
		is_recovery_place(path),
		"consumed Studio recovery is not an .rbxl place: {}",
		path.display()
	);
	let metadata = fs::symlink_metadata(path)
		.with_context(|| format!("failed to inspect consumed Studio recovery {}", path.display()))?;
	ensure!(
		metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
		"consumed Studio recovery is not a regular file: {}",
		path.display()
	);
	let parent = path
		.parent()
		.with_context(|| format!("consumed Studio recovery has no parent: {}", path.display()))?;
	let file_name = path
		.file_name()
		.with_context(|| format!("consumed Studio recovery has no file name: {}", path.display()))?;
	let archive = parent.join(CONSUMED_RECOVERY_DIRECTORY);
	fs::create_dir_all(&archive).with_context(|| {
		format!(
			"failed to create consumed Studio recovery archive {}",
			archive.display()
		)
	})?;
	let mut archived_name = std::ffi::OsString::from(uuid::Uuid::new_v4().simple().to_string());
	archived_name.push("-");
	archived_name.push(file_name);
	archived_name.push(".consumed");
	let destination = archive.join(archived_name);
	ensure!(
		!destination.exists(),
		"consumed Studio recovery archive destination already exists: {}",
		destination.display()
	);
	fs::rename(path, &destination).with_context(|| {
		format!(
			"failed to atomically archive consumed Studio recovery {} at {}",
			path.display(),
			destination.display()
		)
	})?;
	Ok(Some(destination))
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

/// Wait for either a new recovery file or a preexisting file which the caller
/// can prove represents the current Studio state. A candidate must keep the
/// same size and modification time across two polls before it is handed to the
/// decoder, so Studio never races Carbon with a partially written place.
pub(crate) fn wait_for_recovery<T>(
	sources: &[RecoverySource],
	started_at: SystemTime,
	timeout: Duration,
	cancelled: impl Fn() -> bool,
	mut accept: impl FnMut(&Path, RecoveryFreshness) -> Result<RecoveryAcceptance<T>>,
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
				if !is_recovery_place(&path) || fingerprint.len == 0 || rejected.get(&key) == Some(&fingerprint) {
					return None;
				}
				let freshness = if source.baseline.get(&path) == Some(&fingerprint) {
					RecoveryFreshness::Preexisting
				} else if modified_after_capture_started(fingerprint.modified, started_at) {
					RecoveryFreshness::New
				} else {
					return None;
				};
				Some((source.kind, path, fingerprint, freshness))
			}));
		}
		candidates.sort_by_key(|(_, _, fingerprint, freshness)| {
			std::cmp::Reverse((*freshness == RecoveryFreshness::New, fingerprint.modified))
		});

		for (kind, path, fingerprint, freshness) in candidates {
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
			match accept(&path, freshness)? {
				RecoveryAcceptance::Accept(value) => return Ok((kind, path, value)),
				RecoveryAcceptance::Reject => {
					rejected.insert(key.clone(), fingerprint);
					stable.remove(&key);
				}
				RecoveryAcceptance::Retry => {}
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
	fn accepts_preexisting_recovery_when_acceptor_proves_it_matches_unchanged_studio() {
		let directory = std::env::temp_dir().join(format!("carbon-recovery-unchanged-{}", uuid::Uuid::new_v4()));
		fs::create_dir_all(&directory).unwrap();
		let autosaves = directory.join("unchanged.rbxl");
		fs::write(&autosaves, b"studio-generation-1").unwrap();
		let sources = vec![RecoverySource::studio_auto_recovery(directory.clone()).unwrap()];

		let (kind, path, value) = wait_for_recovery(
			&sources,
			SystemTime::now(),
			Duration::from_secs(1),
			|| false,
			|path, freshness| {
				assert_eq!(freshness, RecoveryFreshness::Preexisting);
				let value = fs::read(path)?;
				Ok(if value == b"studio-generation-1" {
					RecoveryAcceptance::Accept(value)
				} else {
					RecoveryAcceptance::Reject
				})
			},
		)
		.unwrap();

		assert_eq!(kind, RecoveryKind::StudioAutoRecovery);
		assert_eq!(path, autosaves);
		assert_eq!(value, b"studio-generation-1");
		fs::remove_dir_all(directory).unwrap();
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

		let (kind, path, value) = wait_for_recovery(
			&sources,
			started_at,
			Duration::from_secs(2),
			|| false,
			|path, freshness| {
				assert_eq!(freshness, RecoveryFreshness::New);
				Ok(RecoveryAcceptance::Accept(fs::read(path)?))
			},
		)
		.unwrap();

		assert_eq!(kind, RecoveryKind::ServedPlace);
		assert_eq!(path, served);
		assert_eq!(value, b"manually saved place");
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn committed_studio_recovery_is_atomically_moved_out_of_the_scan_directory() {
		let directory = std::env::temp_dir().join(format!("carbon-consumed-recovery-{}", uuid::Uuid::new_v4()));
		fs::create_dir_all(&directory).unwrap();
		let source = directory.join("Carbon AutoSave.rbxl");
		fs::write(&source, b"committed exact-session recovery").unwrap();

		let archived = quarantine_consumed_recovery(RecoveryKind::StudioAutoRecovery, &source)
			.unwrap()
			.unwrap();

		assert!(!source.exists());
		assert_eq!(
			archived.parent(),
			Some(directory.join(CONSUMED_RECOVERY_DIRECTORY).as_path())
		);
		assert_eq!(fs::read(&archived).unwrap(), b"committed exact-session recovery");
		assert_eq!(
			archived.extension().and_then(|extension| extension.to_str()),
			Some("consumed")
		);
		assert!(inventory(&directory).unwrap().is_empty());
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn temporary_served_place_is_never_quarantined() {
		let directory = std::env::temp_dir().join(format!("carbon-served-recovery-{}", uuid::Uuid::new_v4()));
		fs::create_dir_all(&directory).unwrap();
		let source = directory.join("served.rbxl");
		fs::write(&source, b"manual save").unwrap();

		assert_eq!(
			quarantine_consumed_recovery(RecoveryKind::ServedPlace, &source).unwrap(),
			None
		);
		assert_eq!(fs::read(&source).unwrap(), b"manual save");
		fs::remove_dir_all(directory).unwrap();
	}
}
