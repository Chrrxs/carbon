use anyhow::{bail, Context, Result};
use blake3::Hasher;
use serde::Serialize;
use std::{
	collections::BTreeMap,
	fs::{self, File},
	io::{BufReader, Read},
	path::Path,
	time::UNIX_EPOCH,
};

#[derive(Clone, Debug)]
pub struct PathSnapshot {
	entries: BTreeMap<String, Entry>,
	pub digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct Entry {
	kind: &'static str,
	bytes: u64,
	content_hash: String,
	modified_ns: u128,
}

impl PathSnapshot {
	pub fn capture(path: &Path) -> Result<Self> {
		if !path.exists() {
			bail!("cannot snapshot missing path {}", path.display());
		}
		let mut entries = BTreeMap::new();
		capture_entry(path, path, &mut entries)?;
		let digest = digest_entries(&entries, true);
		Ok(Self { entries, digest })
	}

	pub fn entry_count(&self) -> usize {
		self.entries.len()
	}

	pub fn compare(&self, path: &Path, check_mtime: bool) -> Result<()> {
		let current = Self::capture(path)?;
		let mut differences = Vec::new();
		for (name, expected) in &self.entries {
			match current.entries.get(name) {
				None => differences.push(format!("removed {name}")),
				Some(actual) => {
					if expected.kind != actual.kind {
						differences.push(format!(
							"{name}: kind changed from {} to {}",
							expected.kind, actual.kind
						));
					} else if expected.bytes != actual.bytes || expected.content_hash != actual.content_hash {
						differences.push(format!("{name}: content changed"));
					} else if check_mtime && expected.modified_ns != actual.modified_ns {
						differences.push(format!("{name}: modification time changed"));
					}
				}
			}
			if differences.len() == 20 {
				break;
			}
		}
		if differences.len() < 20 {
			for name in current.entries.keys() {
				if !self.entries.contains_key(name) {
					differences.push(format!("added {name}"));
					if differences.len() == 20 {
						break;
					}
				}
			}
		}
		if differences.is_empty() {
			Ok(())
		} else {
			bail!(
				"path {} changed since snapshot:\n{}",
				path.display(),
				differences.join("\n")
			)
		}
	}
}

fn capture_entry(root: &Path, path: &Path, entries: &mut BTreeMap<String, Entry>) -> Result<()> {
	let metadata = fs::symlink_metadata(path).with_context(|| format!("failed to inspect {}", path.display()))?;
	let name = relative_name(root, path);
	let modified_ns = metadata
		.modified()
		.ok()
		.and_then(|value| value.duration_since(UNIX_EPOCH).ok())
		.map(|value| value.as_nanos())
		.unwrap_or_default();

	if metadata.is_dir() {
		entries.insert(
			name,
			Entry {
				kind: "directory",
				bytes: 0,
				content_hash: String::new(),
				modified_ns,
			},
		);
		let mut children = fs::read_dir(path)
			.with_context(|| format!("failed to read {}", path.display()))?
			.collect::<std::io::Result<Vec<_>>>()?;
		children.sort_by_key(|entry| entry.file_name());
		for child in children {
			capture_entry(root, &child.path(), entries)?;
		}
	} else if metadata.is_file() {
		entries.insert(
			name,
			Entry {
				kind: "file",
				bytes: metadata.len(),
				content_hash: hash_file(path)?,
				modified_ns,
			},
		);
	} else if metadata.file_type().is_symlink() {
		let target = fs::read_link(path).with_context(|| format!("failed to read symlink {}", path.display()))?;
		let bytes = target.as_os_str().as_encoded_bytes();
		entries.insert(
			name,
			Entry {
				kind: "symlink",
				bytes: bytes.len() as u64,
				content_hash: blake3::hash(bytes).to_hex().to_string(),
				modified_ns,
			},
		);
	} else {
		bail!("unsupported filesystem entry {}", path.display());
	}
	Ok(())
}

fn relative_name(root: &Path, path: &Path) -> String {
	if root == path {
		return ".".to_owned();
	}
	path.strip_prefix(root)
		.unwrap_or(path)
		.components()
		.map(|value| value.as_os_str().to_string_lossy())
		.collect::<Vec<_>>()
		.join("/")
}

fn hash_file(path: &Path) -> Result<String> {
	let mut input = BufReader::new(File::open(path).with_context(|| format!("failed to open {}", path.display()))?);
	let mut hasher = Hasher::new();
	let mut buffer = [0_u8; 64 * 1024];
	loop {
		let read = input.read(&mut buffer)?;
		if read == 0 {
			break;
		}
		hasher.update(&buffer[..read]);
	}
	Ok(hasher.finalize().to_hex().to_string())
}

fn digest_entries(entries: &BTreeMap<String, Entry>, include_mtime: bool) -> String {
	let mut hasher = Hasher::new();
	for (name, entry) in entries {
		hasher.update(name.as_bytes());
		hasher.update(&[0]);
		hasher.update(entry.kind.as_bytes());
		hasher.update(&entry.bytes.to_le_bytes());
		hasher.update(entry.content_hash.as_bytes());
		if include_mtime {
			hasher.update(&entry.modified_ns.to_le_bytes());
		}
	}
	hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::{io::Write, path::PathBuf};

	fn temporary_directory() -> PathBuf {
		let path = std::env::temp_dir().join(format!("carbon-qualify-snapshot-{}", uuid::Uuid::new_v4()));
		fs::create_dir_all(&path).unwrap();
		path
	}

	#[test]
	fn detects_content_changes_without_relying_on_mtime() {
		let root = temporary_directory();
		let path = root.join("value.txt");
		File::create(&path).unwrap().write_all(b"one").unwrap();
		let snapshot = PathSnapshot::capture(&root).unwrap();
		File::create(&path).unwrap().write_all(b"two").unwrap();
		assert!(snapshot
			.compare(&root, false)
			.unwrap_err()
			.to_string()
			.contains("content changed"));
		fs::remove_dir_all(root).unwrap();
	}
}
