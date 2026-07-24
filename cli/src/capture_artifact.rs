//! Framing for the bounded RBXM chunks produced by a native capture lease.
//!
//! This module deliberately understands only the outer artifact. RBXM decode,
//! hierarchy alignment, reference repair, and capture policy live above it.

use anyhow::{ensure, Context, Result};
use std::{
	fs::File,
	io::{Read, Seek, SeekFrom, Take},
	path::Path,
};

const MAGIC: &[u8; 9] = b"CARBONCM2";
const MAX_CHUNKS: usize = 1_000_000;
const MAX_ROOTS: usize = 20_000_000;
const SYNTHETIC_ROOT: u32 = u32::MAX;
pub(crate) const REFERENCE_DEPENDENCY_FLAG: u32 = 1 << 31;

pub(crate) fn reference_dependency_ordinal(encoded: u32) -> Option<u32> {
	(encoded != SYNTHETIC_ROOT && encoded & REFERENCE_DEPENDENCY_FLAG != 0)
		.then_some(encoded & !REFERENCE_DEPENDENCY_FLAG)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CaptureModelChunk {
	pub root_ordinals: Vec<u32>,
	pub payload_offset: u64,
	pub payload_length: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CaptureModelArtifact {
	pub chunks: Vec<CaptureModelChunk>,
}

impl CaptureModelArtifact {
	pub(crate) fn open(path: &Path) -> Result<Self> {
		let mut input = File::open(path).context("open Capture Manifest model artifact")?;
		let artifact_length = input.metadata()?.len();

		let mut magic = [0_u8; MAGIC.len()];
		input
			.read_exact(&mut magic)
			.context("capture model artifact is truncated before its magic")?;
		ensure!(
			&magic == MAGIC,
			"capture model artifact magic is invalid; CARBONCM2 is required"
		);

		let chunk_count = usize::try_from(read_u32(&mut input)?)?;
		ensure!(
			chunk_count <= MAX_CHUNKS,
			"capture model chunk count exceeds the protocol limit"
		);

		let mut chunks = Vec::with_capacity(chunk_count);
		let mut total_roots = 0_usize;
		let mut real_roots = Vec::new();
		for chunk_index in 0..chunk_count {
			let root_count = usize::try_from(read_u32(&mut input)?)?;
			ensure!(root_count > 0, "capture model chunk {chunk_index} has no roots");
			total_roots = total_roots
				.checked_add(root_count)
				.context("capture model root count overflows usize")?;
			ensure!(
				total_roots <= MAX_ROOTS,
				"capture model root count exceeds the protocol limit"
			);

			let mut root_ordinals = Vec::with_capacity(root_count);
			for _ in 0..root_count {
				let ordinal = read_u32(&mut input)?;
				if let Some(target) = reference_dependency_ordinal(ordinal) {
					ensure!(target != 0, "capture model reference dependency target is invalid");
				} else if ordinal != SYNTHETIC_ROOT {
					real_roots.push(ordinal);
				}
				root_ordinals.push(ordinal);
			}

			let payload_length = read_u64(&mut input)?;
			ensure!(payload_length > 0, "capture model chunk {chunk_index} payload is empty");
			let payload_offset = input.stream_position()?;
			let payload_end = payload_offset
				.checked_add(payload_length)
				.context("capture model chunk length overflows u64")?;
			ensure!(
				payload_end <= artifact_length,
				"capture model chunk {chunk_index} payload exceeds the artifact"
			);
			input.seek(SeekFrom::Start(payload_end))?;
			chunks.push(CaptureModelChunk {
				root_ordinals,
				payload_offset,
				payload_length,
			});
		}

		ensure!(
			input.stream_position()? == artifact_length,
			"capture model artifact has trailing bytes"
		);
		real_roots.sort_unstable();
		if let Some(pair) = real_roots.windows(2).find(|pair| pair[0] == pair[1]) {
			anyhow::bail!("capture model repeats hierarchy root ordinal {}", pair[0]);
		}
		Ok(Self { chunks })
	}

	/// Proves that framing and the attested envelope describe the same ordered
	/// persistent serializer inputs. Reference dependencies are isolated,
	/// framed roots that are intentionally absent from the envelope; real
	/// hierarchy ordinals may occur only once, while the carrier sentinel may
	/// occur more than once.
	pub(crate) fn validate_root_order(&self, expected: &[u32]) -> Result<()> {
		let observed = self
			.chunks
			.iter()
			.flat_map(|chunk| chunk.root_ordinals.iter().copied())
			.filter(|ordinal| reference_dependency_ordinal(*ordinal).is_none());
		ensure!(
			observed.eq(expected.iter().copied()),
			"capture model chunk roots disagree with the envelope"
		);
		Ok(())
	}

	pub(crate) fn open_chunk(&self, path: &Path, index: usize) -> Result<Take<File>> {
		let chunk = self
			.chunks
			.get(index)
			.with_context(|| format!("capture model chunk {index} is absent"))?;
		let mut input = File::open(path).context("open Capture Manifest model artifact")?;
		input.seek(SeekFrom::Start(chunk.payload_offset))?;
		Ok(input.take(chunk.payload_length))
	}
}

fn read_u32(input: &mut impl Read) -> Result<u32> {
	let mut value = [0_u8; 4];
	input
		.read_exact(&mut value)
		.context("capture model artifact is truncated")?;
	Ok(u32::from_le_bytes(value))
}

fn read_u64(input: &mut impl Read) -> Result<u64> {
	let mut value = [0_u8; 8];
	input
		.read_exact(&mut value)
		.context("capture model artifact is truncated")?;
	Ok(u64::from_le_bytes(value))
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::{fs, io::Read};

	fn artifact(chunks: &[(Vec<u32>, &[u8])]) -> Vec<u8> {
		let mut bytes = MAGIC.to_vec();
		bytes.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
		for (roots, payload) in chunks {
			bytes.extend_from_slice(&(roots.len() as u32).to_le_bytes());
			for root in roots {
				bytes.extend_from_slice(&root.to_le_bytes());
			}
			bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
			bytes.extend_from_slice(payload);
		}
		bytes
	}

	fn fixture(bytes: &[u8]) -> std::path::PathBuf {
		let path = std::env::temp_dir().join(format!("carbon-capture-artifact-{}.bin", uuid::Uuid::new_v4().simple()));
		fs::write(&path, bytes).unwrap();
		path
	}

	#[test]
	fn parses_multi_root_chunks_and_opens_bounded_payloads() {
		let path = fixture(&artifact(&[
			(vec![3, 4, SYNTHETIC_ROOT], b"first"),
			(vec![9], b"second"),
		]));
		let parsed = CaptureModelArtifact::open(&path).unwrap();
		assert_eq!(parsed.chunks.len(), 2);
		assert_eq!(parsed.chunks[0].root_ordinals, [3, 4, SYNTHETIC_ROOT]);
		assert_eq!(parsed.chunks[1].root_ordinals, [9]);
		parsed.validate_root_order(&[3, 4, SYNTHETIC_ROOT, 9]).unwrap();

		let mut payload = Vec::new();
		parsed.open_chunk(&path, 1).unwrap().read_to_end(&mut payload).unwrap();
		assert_eq!(payload, b"second");
		fs::remove_file(path).unwrap();
	}

	#[test]
	fn reference_dependencies_are_attested_but_excluded_from_persistent_root_order() {
		let dependency = REFERENCE_DEPENDENCY_FLAG | 9;
		let path = fixture(&artifact(&[(vec![3, dependency], b"first"), (vec![9], b"second")]));
		let parsed = CaptureModelArtifact::open(&path).unwrap();
		assert_eq!(reference_dependency_ordinal(dependency), Some(9));
		parsed.validate_root_order(&[3, 9]).unwrap();
		fs::remove_file(path).unwrap();
	}

	#[test]
	fn rejects_wrong_magic_and_legacy_raw_rbxm() {
		for bytes in [b"<roblox!".as_slice(), b"CARBONCM0".as_slice()] {
			let path = fixture(bytes);
			assert!(CaptureModelArtifact::open(&path)
				.unwrap_err()
				.to_string()
				.contains("magic"));
			fs::remove_file(path).unwrap();
		}
	}

	#[test]
	fn accepts_empty_artifact_and_rejects_unbounded_counts() {
		let mut empty = MAGIC.to_vec();
		empty.extend_from_slice(&0_u32.to_le_bytes());
		let empty_path = fixture(&empty);
		let parsed = CaptureModelArtifact::open(&empty_path).unwrap();
		assert!(parsed.chunks.is_empty());
		parsed.validate_root_order(&[]).unwrap();
		assert!(parsed.validate_root_order(&[1]).is_err());
		fs::remove_file(empty_path).unwrap();

		let mut huge = MAGIC.to_vec();
		huge.extend_from_slice(&u32::MAX.to_le_bytes());
		let huge_path = fixture(&huge);
		assert!(CaptureModelArtifact::open(&huge_path)
			.unwrap_err()
			.to_string()
			.contains("protocol limit"));
		fs::remove_file(huge_path).unwrap();
	}

	#[test]
	fn rejects_empty_roots_duplicate_ordinals_and_wrong_order() {
		let empty_roots = fixture(&artifact(&[(vec![], b"payload")]));
		assert!(CaptureModelArtifact::open(&empty_roots)
			.unwrap_err()
			.to_string()
			.contains("has no roots"));
		fs::remove_file(empty_roots).unwrap();

		let duplicate = fixture(&artifact(&[(vec![7], b"a"), (vec![7], b"b")]));
		assert!(CaptureModelArtifact::open(&duplicate)
			.unwrap_err()
			.to_string()
			.contains("repeats hierarchy root ordinal 7"));
		fs::remove_file(duplicate).unwrap();

		let ordered = fixture(&artifact(&[(vec![1, 2], b"payload")]));
		let parsed = CaptureModelArtifact::open(&ordered).unwrap();
		assert!(parsed.validate_root_order(&[2, 1]).is_err());
		fs::remove_file(ordered).unwrap();
	}

	#[test]
	fn rejects_empty_truncated_and_trailing_payloads() {
		let empty = fixture(&artifact(&[(vec![1], b"")]));
		assert!(CaptureModelArtifact::open(&empty)
			.unwrap_err()
			.to_string()
			.contains("payload is empty"));
		fs::remove_file(empty).unwrap();

		let mut truncated = artifact(&[(vec![1], b"abc")]);
		truncated.pop();
		let truncated_path = fixture(&truncated);
		assert!(CaptureModelArtifact::open(&truncated_path)
			.unwrap_err()
			.to_string()
			.contains("exceeds the artifact"));
		fs::remove_file(truncated_path).unwrap();

		let mut trailing = artifact(&[(vec![1], b"abc")]);
		trailing.push(0xff);
		let trailing_path = fixture(&trailing);
		assert!(CaptureModelArtifact::open(&trailing_path)
			.unwrap_err()
			.to_string()
			.contains("trailing bytes"));
		fs::remove_file(trailing_path).unwrap();
	}
}
