//! Ownership of `.carbon-composite-*` staging directories.
//!
//! Every composite has a sibling `<directory>.lock` file. The creating process
//! holds an exclusive lock on it from before the directory exists until after
//! the directory is removed, so a lock file that can be locked proves that its
//! owner exited, crashed, or was killed. Creating a composite sweeps such
//! abandoned siblings. Directories without a lock file predate this scheme and
//! are never removed automatically, because their owner cannot be proven gone.

use std::{
	collections::HashMap,
	fs::{self, File, OpenOptions},
	io,
	path::{Path, PathBuf},
	sync::LazyLock,
};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use uuid::Uuid;

const PREFIX: &str = ".carbon-composite-";
const LOCK_SUFFIX: &str = ".lock";

/// Lock handles for composites owned by this process, keyed by directory.
static OWNED: LazyLock<Mutex<HashMap<PathBuf, File>>> = LazyLock::new(Default::default);

fn lock_path(directory: &Path) -> PathBuf {
	let mut path = directory.as_os_str().to_owned();
	path.push(LOCK_SUFFIX);
	PathBuf::from(path)
}

fn is_composite_id(id: &str) -> bool {
	id.len() == 32 && id.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Create a new owned composite directory beneath `parent`.
pub(crate) fn create(parent: &Path) -> Result<PathBuf> {
	fs::create_dir_all(parent).with_context(|| format!("failed to create composite parent {}", parent.display()))?;
	sweep(parent);
	let id = Uuid::new_v4().simple().to_string();
	let directory = parent.join(format!("{PREFIX}{id}"));
	let lock_path = lock_path(&directory);
	// Lock under a name the sweep ignores, then publish it, so no other process
	// can observe the lock file before it is held.
	let staged_lock_path = parent.join(format!("{PREFIX}{id}{LOCK_SUFFIX}.{}.tmp", std::process::id()));
	let lock = OpenOptions::new()
		.read(true)
		.write(true)
		.create_new(true)
		.open(&staged_lock_path)
		.with_context(|| format!("failed to create composite lock {}", staged_lock_path.display()))?;
	let published = lock
		.try_lock()
		.map_err(|error| anyhow::anyhow!("{error}"))
		.and_then(|()| fs::rename(&staged_lock_path, &lock_path).map_err(anyhow::Error::from))
		.and_then(|()| fs::create_dir(&directory).map_err(anyhow::Error::from));
	if let Err(error) = published {
		let _ = fs::remove_dir(&directory);
		let _ = fs::remove_file(&lock_path);
		let _ = fs::remove_file(&staged_lock_path);
		return Err(error).with_context(|| format!("failed to create composite {}", directory.display()));
	}
	OWNED.lock().insert(directory.clone(), lock);
	Ok(directory)
}

/// Remove a composite directory and release its ownership lock.
///
/// The lock is retained when the directory cannot be removed, so a later sweep
/// can finish the removal once this process exits.
pub(crate) fn remove(directory: &Path) -> io::Result<()> {
	match fs::remove_dir_all(directory) {
		Ok(()) => {}
		Err(error) if error.kind() == io::ErrorKind::NotFound => {}
		Err(error) => return Err(error),
	}
	if let Some(lock) = OWNED.lock().remove(directory) {
		let _ = fs::remove_file(lock_path(directory));
		drop(lock);
	}
	Ok(())
}

/// Remove composites in `parent` whose owning process no longer holds them.
fn sweep(parent: &Path) {
	let Ok(entries) = fs::read_dir(parent) else {
		return;
	};
	for entry in entries.flatten() {
		let name = entry.file_name();
		let Some(id) = name
			.to_str()
			.and_then(|name| name.strip_prefix(PREFIX))
			.and_then(|rest| rest.strip_suffix(LOCK_SUFFIX))
			.filter(|id| is_composite_id(id))
		else {
			continue;
		};
		let directory = parent.join(format!("{PREFIX}{id}"));
		if OWNED.lock().contains_key(&directory) {
			continue;
		}
		let lock_path = entry.path();
		let Ok(lock) = OpenOptions::new().read(true).write(true).open(&lock_path) else {
			continue;
		};
		if lock.try_lock().is_err() {
			continue;
		}
		match fs::remove_dir_all(&directory) {
			Ok(()) => log::debug!("Removed abandoned Carbon composite {}", directory.display()),
			Err(error) if error.kind() == io::ErrorKind::NotFound => {}
			Err(error) => {
				log::warn!(
					"Could not remove abandoned Carbon composite {}: {error}",
					directory.display()
				);
				continue;
			}
		}
		let _ = fs::remove_file(&lock_path);
	}
}

/// Removes an owned composite when dropped unless ownership is handed off.
pub(crate) struct Guard(Option<PathBuf>);

impl Guard {
	pub(crate) fn new(directory: PathBuf) -> Self {
		Self(Some(directory))
	}

	/// Hand the composite to a longer-lived owner without removing it.
	pub(crate) fn keep(mut self) -> PathBuf {
		self.0.take().expect("composite guard was already consumed")
	}

	/// Remove the composite now and report any failure.
	pub(crate) fn release(mut self) -> io::Result<()> {
		match self.0.take() {
			Some(directory) => remove(&directory),
			None => Ok(()),
		}
	}
}

impl Drop for Guard {
	fn drop(&mut self) {
		if let Some(directory) = self.0.take() {
			let _ = remove(&directory);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn entries(parent: &Path) -> Vec<String> {
		let mut names = fs::read_dir(parent)
			.unwrap()
			.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
			.collect::<Vec<_>>();
		names.sort();
		names
	}

	#[test]
	fn bounded_composites_sweep_abandoned_owners_and_keep_live_or_legacy_ones() {
		let temp = tempfile::tempdir().unwrap();
		let parent = temp.path();
		let abandoned = parent.join(format!("{PREFIX}{}", "a".repeat(32)));
		fs::create_dir(&abandoned).unwrap();
		fs::write(abandoned.join("state.carbon"), b"stale").unwrap();
		fs::write(lock_path(&abandoned), b"").unwrap();
		let live = parent.join(format!("{PREFIX}{}", "b".repeat(32)));
		fs::create_dir(&live).unwrap();
		let live_lock = File::create(lock_path(&live)).unwrap();
		live_lock.try_lock().unwrap();
		let legacy = parent.join(format!("{PREFIX}{}", "c".repeat(32)));
		fs::create_dir(&legacy).unwrap();

		let created = create(parent).unwrap();

		assert!(!abandoned.exists(), "abandoned composite survived the sweep");
		assert!(!lock_path(&abandoned).exists(), "abandoned lock survived the sweep");
		assert!(
			live.is_dir() && lock_path(&live).is_file(),
			"a held composite was swept"
		);
		assert!(legacy.is_dir(), "a composite without ownership evidence was swept");
		assert!(created.is_dir() && lock_path(&created).is_file());

		create(parent).map(|again| remove(&again).unwrap()).unwrap();
		assert!(created.is_dir(), "this process swept its own composite");

		remove(&created).unwrap();
		drop(live_lock);
		let mut expected = vec![
			legacy.file_name().unwrap().to_string_lossy().into_owned(),
			live.file_name().unwrap().to_string_lossy().into_owned(),
			lock_path(&live).file_name().unwrap().to_string_lossy().into_owned(),
		];
		expected.sort();
		assert_eq!(entries(parent), expected);
	}

	#[test]
	fn bounded_composite_guard_removes_unless_kept() {
		let temp = tempfile::tempdir().unwrap();
		let dropped = create(temp.path()).unwrap();
		drop(Guard::new(dropped.clone()));
		assert!(!dropped.exists() && !lock_path(&dropped).exists());

		let kept = Guard::new(create(temp.path()).unwrap()).keep();
		assert!(kept.is_dir());
		Guard::new(kept.clone()).release().unwrap();
		assert!(entries(temp.path()).is_empty());
	}
}
