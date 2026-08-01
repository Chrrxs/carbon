//! Canonical Carbon artifact backed by normalized Roblox binary.
//!
//! The module is the persistence seam for the Studio-owned complement. The
//! repository representation is one strict, versioned envelope plus optional
//! content-addressed blobs; callers never depend on the physical encoding.

use anyhow::{bail, ensure, Context, Result};
use rbx_binary::{Deserializer, InstanceSource, InstanceView, Serializer};
use rbx_dom_weak::{
	types::{Attributes, BinaryString, Content, ContentType, Ref, SharedString, Variant},
	InstanceBuilder, Ustr, UstrMap, WeakDom,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
#[cfg(test)]
use std::cell::Cell;
use std::{
	collections::{BTreeMap, HashMap, HashSet},
	fs::{self, File, OpenOptions},
	io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
	path::{Path, PathBuf},
	sync::{
		atomic::{AtomicBool, Ordering},
		Arc, Mutex, RwLock,
	},
};
use uuid::Uuid;

use crate::{
	core::{changes::Changes, snapshot::Snapshot, tree::Tree},
	ext::PathExt,
	manifest_identity::{IdentityColumn, ManifestIdentityAllocator},
	resolution::UnresolvedValue,
	util,
};

use crate::source_wire::{adapt_lighting_output_properties, normalize_wire_attributes};
pub(crate) use crate::source_wire::{canonical_property_name, WireProperties};

pub const MANIFEST_IDENTITY_ATTRIBUTE: &str = "__StudioWorktree_CarbonManifestId";
pub(crate) const CAPTURE_FINGERPRINT_METADATA_KEY: &str = "CarbonCaptureFingerprintV2";
pub(crate) const CAPTURE_PROJECT_GENERATION_METADATA_KEY: &str = "CarbonCaptureProjectGenerationV1";
const LEGACY_CAPTURE_FINGERPRINT_METADATA_KEY: &str = "CarbonCaptureFingerprintV1";

const MAGIC: &[u8; 8] = b"CARBONRB";
const VERSION: u32 = 1;
const FLAGS: u32 = 0;
const FIXED_HEADER_BYTES: u64 = 96;
const BLOB_THRESHOLD: usize = 16 * 1024;

#[cfg(test)]
thread_local! {
	static TEST_TREE_LOADS: Cell<usize> = const { Cell::new(0) };
	static TEST_CANONICAL_HIERARCHY_SORTS: Cell<usize> = const { Cell::new(0) };
	static TEST_ARTIFACT_PAYLOAD_PASSES: Cell<usize> = const { Cell::new(0) };
	static TEST_ARTIFACT_WRITES: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn count_tree_loads<T>(action: impl FnOnce() -> T) -> (T, usize) {
	TEST_TREE_LOADS.with(|loads| loads.set(0));
	let result = action();
	let loads = TEST_TREE_LOADS.with(Cell::get);
	(result, loads)
}

#[cfg(test)]
pub(crate) fn count_artifact_payload_passes<T>(action: impl FnOnce() -> T) -> (T, usize) {
	TEST_ARTIFACT_PAYLOAD_PASSES.with(|passes| passes.set(0));
	let result = action();
	let passes = TEST_ARTIFACT_PAYLOAD_PASSES.with(Cell::get);
	(result, passes)
}

#[cfg(test)]
pub(crate) fn count_artifact_writes<T>(action: impl FnOnce() -> T) -> (T, usize) {
	TEST_ARTIFACT_WRITES.with(|writes| writes.set(0));
	let result = action();
	let writes = TEST_ARTIFACT_WRITES.with(Cell::get);
	(result, writes)
}

#[cfg(test)]
fn count_canonical_hierarchy_sorts<T>(action: impl FnOnce() -> T) -> (T, usize) {
	TEST_CANONICAL_HIERARCHY_SORTS.with(|sorts| sorts.set(0));
	let result = action();
	let sorts = TEST_CANONICAL_HIERARCHY_SORTS.with(Cell::get);
	(result, sorts)
}

#[derive(Clone, Debug)]
pub struct WorktreeContract {
	pub endpoint: String,
	pub project: String,
	pub worktree_id: String,
	pub session_token: String,
	pub identity_exclusions: HashSet<Ref>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
enum ExternalKind {
	BinaryString,
	SharedString,
	String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
enum ExternalReferenceKind {
	Ref,
	Content,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExternalReference {
	owner: u32,
	property: String,
	kind: ExternalReferenceKind,
	target: [u8; 16],
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExternalValue {
	owner: u32,
	property: String,
	kind: ExternalKind,
	hash: [u8; 32],
	bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Sideband {
	format: String,
	name: String,
	root: [u8; 16],
	root_name: String,
	root_class: String,
	metadata: BTreeMap<String, String>,
	instances: u64,
	properties: u64,
	identities: IdentityColumn,
	#[serde(skip)]
	ids: Vec<[u8; 16]>,
	external: Vec<ExternalValue>,
	references: Vec<ExternalReference>,
}

#[derive(Clone, Debug)]
struct Artifact {
	path: PathBuf,
	payload_offset: u64,
	payload_len: u64,
	sideband: Sideband,
}

/// Cryptographic evidence for one fully validated canonical artifact.
///
/// The receipt is deliberately content-derived. Callers may share it across
/// capture phases, but any source transition or uncertain filesystem event must
/// discard it and validate the artifact again.
#[derive(Clone, Debug)]
pub(crate) struct ValidatedArtifactReceipt {
	generation: String,
	build_generation: String,
	name: String,
	metadata: BTreeMap<String, String>,
	blobs: BTreeMap<[u8; 32], u64>,
}

impl ValidatedArtifactReceipt {
	pub(crate) fn generation(&self) -> &str {
		&self.generation
	}

	/// Stable authored input for build caching. Capture attestation is carried
	/// by the artifact but is not part of the place payload Carbon builds.
	pub(crate) fn build_generation(&self) -> &str {
		&self.build_generation
	}

	pub(crate) fn name(&self) -> &str {
		&self.name
	}

	#[cfg(test)]
	pub(crate) fn capture_fingerprint(&self) -> Option<&str> {
		self.metadata.get(CAPTURE_FINGERPRINT_METADATA_KEY).map(String::as_str)
	}

	#[cfg(test)]
	pub(crate) fn project_generation(&self) -> Option<&str> {
		self.metadata
			.get(CAPTURE_PROJECT_GENERATION_METADATA_KEY)
			.map(String::as_str)
	}

	pub(crate) fn metadata(&self) -> &BTreeMap<String, String> {
		&self.metadata
	}
}

#[derive(Clone, Debug)]
pub struct Inspection {
	pub name: String,
	root_class: String,
	pub instances: u64,
	pub properties: u64,
	pub blobs: u64,
}

impl Inspection {
	pub fn is_place(&self) -> bool {
		self.root_class == "DataModel"
	}
}

#[derive(Clone, Debug)]
pub struct ExtractReport {
	pub instances: u64,
	pub properties: u64,
	pub blobs: u64,
	/// Artifact files are atomic, so a successful extraction always writes one.
	pub artifacts: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompileReport {
	pub instances: u64,
	pub properties: u64,
	/// Artifact files are atomic, so a successful build reads one.
	pub artifacts: usize,
}

fn ref_bytes(referent: Ref) -> Result<[u8; 16]> {
	let value = u128::from_str_radix(&referent.to_string(), 16)
		.with_context(|| format!("referent {referent} is not hexadecimal"))?;
	Ok(value.to_be_bytes())
}

fn ref_from_bytes(bytes: [u8; 16]) -> Ref {
	format!("{:032x}", u128::from_be_bytes(bytes))
		.parse()
		.expect("a 128-bit value is always a Roblox referent")
}

fn binary_index(referent: Ref) -> Result<usize> {
	let value = u128::from_str_radix(&referent.to_string(), 16)
		.with_context(|| format!("binary referent {referent} is not hexadecimal"))?;
	usize::try_from(value.checked_sub(1).context("binary referent is zero")?)
		.context("binary referent exceeds host address space")
}

fn stable_binary_ref(referent: Ref, ids: &[[u8; 16]]) -> Result<Ref> {
	if referent.is_none() {
		return Ok(Ref::none());
	}
	let index = binary_index(referent)?;
	Ok(ref_from_bytes(
		*ids.get(index).context("binary referent is outside identity sideband")?,
	))
}

fn read_u32(reader: &mut impl Read) -> Result<u32> {
	let mut bytes = [0; 4];
	reader.read_exact(&mut bytes)?;
	Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> Result<u64> {
	let mut bytes = [0; 8];
	reader.read_exact(&mut bytes)?;
	Ok(u64::from_le_bytes(bytes))
}

struct DigestingWriter<'a, W> {
	inner: &'a mut W,
	hasher: blake3::Hasher,
	bytes: u64,
}

impl<'a, W> DigestingWriter<'a, W> {
	fn new(inner: &'a mut W) -> Self {
		Self {
			inner,
			hasher: blake3::Hasher::new(),
			bytes: 0,
		}
	}

	fn evidence(self) -> (u64, [u8; 32]) {
		(self.bytes, *self.hasher.finalize().as_bytes())
	}
}

impl<W: Write> Write for DigestingWriter<'_, W> {
	fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
		let written = self.inner.write(buffer)?;
		self.hasher.update(&buffer[..written]);
		self.bytes = self
			.bytes
			.checked_add(written as u64)
			.ok_or_else(|| std::io::Error::other("Carbon artifact payload length overflow"))?;
		Ok(written)
	}

	fn flush(&mut self) -> std::io::Result<()> {
		self.inner.flush()
	}
}

#[cfg(unix)]
fn replace_file_atomic(source: &Path, destination: &Path) -> Result<()> {
	fs::rename(source, destination)?;
	Ok(())
}

#[cfg(windows)]
fn replace_file_atomic(source: &Path, destination: &Path) -> Result<()> {
	use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};

	let source_wide = windows_api_path(source)?;
	let destination_wide = windows_api_path(destination)?;
	let result = unsafe {
		MoveFileExW(
			source_wide.as_ptr(),
			destination_wide.as_ptr(),
			MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
		)
	};
	if result == 0 {
		return Err(std::io::Error::last_os_error()).with_context(|| {
			format!(
				"failed to atomically replace Carbon artifact {} with {}",
				destination.display(),
				source.display()
			)
		});
	}
	Ok(())
}

#[cfg(windows)]
fn windows_api_path(path: &Path) -> Result<Vec<u16>> {
	use std::os::windows::ffi::OsStrExt;

	const SEPARATOR: u16 = b'\\' as u16;
	const VERBATIM_PREFIX: [u16; 4] = [SEPARATOR, SEPARATOR, b'?' as u16, SEPARATOR];
	const DEVICE_PREFIX: [u16; 4] = [SEPARATOR, SEPARATOR, b'.' as u16, SEPARATOR];
	const UNC_PREFIX: [u16; 8] = [
		SEPARATOR,
		SEPARATOR,
		b'?' as u16,
		SEPARATOR,
		b'U' as u16,
		b'N' as u16,
		b'C' as u16,
		SEPARATOR,
	];

	let absolute = std::path::absolute(path)?;
	let wide = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
	let mut encoded = Vec::with_capacity(wide.len() + UNC_PREFIX.len() + 1);
	if wide.starts_with(&VERBATIM_PREFIX) || wide.starts_with(&DEVICE_PREFIX) {
		encoded.extend_from_slice(&wide);
	} else if wide.starts_with(&[SEPARATOR, SEPARATOR]) {
		encoded.extend_from_slice(&UNC_PREFIX);
		encoded.extend_from_slice(&wide[2..]);
	} else {
		encoded.extend_from_slice(&VERBATIM_PREFIX);
		encoded.extend_from_slice(&wide);
	}
	encoded.push(0);
	Ok(encoded)
}

#[cfg(not(any(unix, windows)))]
fn replace_file_atomic(source: &Path, destination: &Path) -> Result<()> {
	if destination.exists() {
		fs::remove_file(destination)?;
	}
	fs::rename(source, destination)?;
	Ok(())
}

pub(crate) fn install_artifact_file(source: &Path, destination: &Path) -> Result<()> {
	replace_file_atomic(source, destination)
}

fn sync_directory(_path: &Path) -> Result<()> {
	#[cfg(unix)]
	File::open(_path)?.sync_all()?;
	Ok(())
}

impl Artifact {
	fn open(path: &Path) -> Result<Self> {
		let bytes = fs::read(path).with_context(|| format!("failed to open Carbon artifact {}", path.display()))?;
		Self::from_bytes(path, &bytes)
	}

	fn from_bytes(path: &Path, bytes: &[u8]) -> Result<Self> {
		let length = bytes.len() as u64;
		ensure!(length >= FIXED_HEADER_BYTES, "Carbon artifact is truncated");
		let mut file = std::io::Cursor::new(bytes);
		let mut magic = [0_u8; 8];
		file.read_exact(&mut magic)?;
		ensure!(magic == *MAGIC, "unsupported Carbon artifact magic");
		ensure!(read_u32(&mut file)? == VERSION, "unsupported Carbon artifact version");
		ensure!(read_u32(&mut file)? == FLAGS, "unsupported Carbon artifact flags");
		let payload_len = read_u64(&mut file)?;
		let sideband_len = read_u64(&mut file)?;
		let mut payload_hash = [0; 32];
		file.read_exact(&mut payload_hash)?;
		let mut sideband_hash = [0; 32];
		file.read_exact(&mut sideband_hash)?;
		let expected = FIXED_HEADER_BYTES
			.checked_add(payload_len)
			.and_then(|value| value.checked_add(sideband_len))
			.context("Carbon artifact length overflows")?;
		ensure!(length == expected, "Carbon artifact length does not match its header");
		let payload_start = FIXED_HEADER_BYTES as usize;
		let payload_end = payload_start
			.checked_add(usize::try_from(payload_len).context("payload is too large")?)
			.context("Carbon artifact payload range overflows")?;
		ensure!(
			*blake3::hash(&bytes[payload_start..payload_end]).as_bytes() == payload_hash,
			"Carbon artifact RBXL payload checksum mismatch"
		);
		ensure!(
			*blake3::hash(&bytes[payload_end..]).as_bytes() == sideband_hash,
			"Carbon artifact identity sideband checksum mismatch"
		);
		let mut sideband: Sideband =
			rmp_serde::from_slice(&bytes[payload_end..]).context("invalid Carbon artifact sideband")?;
		ensure!(
			sideband.format == "carbon-rbxl-v1",
			"unsupported Carbon sideband format"
		);
		sideband.ids = sideband
			.identities
			.decode(usize::try_from(sideband.instances).context("identity count is too large")?)?;
		ensure!(
			sideband.instances == sideband.ids.len() as u64,
			"identity count mismatch"
		);
		let mut unique = HashSet::with_capacity(sideband.ids.len());
		ensure!(
			sideband.ids.iter().all(|id| unique.insert(*id)),
			"duplicate Carbon identity"
		);
		ensure!(
			sideband.ids.contains(&sideband.root),
			"root identity is absent from the RBXL payload"
		);
		ensure!(
			sideband.external.windows(2).all(|pair| pair[0] < pair[1]),
			"external value index is not canonical"
		);
		ensure!(
			sideband
				.external
				.iter()
				.all(|value| value.owner < sideband.ids.len() as u32),
			"external value owner is outside identity sideband"
		);
		ensure!(
			sideband.references.windows(2).all(|pair| pair[0] < pair[1]),
			"external reference index is not canonical"
		);
		ensure!(
			sideband
				.references
				.iter()
				.all(|value| value.owner < sideband.ids.len() as u32),
			"external reference owner is outside identity sideband"
		);
		let mut external_properties = HashSet::with_capacity(sideband.external.len() + sideband.references.len());
		ensure!(
			sideband
				.external
				.iter()
				.all(|value| external_properties.insert((value.owner, value.property.as_str()))),
			"duplicate external value property"
		);
		ensure!(
			sideband
				.references
				.iter()
				.all(|value| external_properties.insert((value.owner, value.property.as_str()))),
			"external value and reference properties overlap"
		);
		Ok(Self {
			path: path.to_owned(),
			payload_offset: FIXED_HEADER_BYTES,
			payload_len,
			sideband,
		})
	}

	fn payload(&self) -> Result<impl Read> {
		#[cfg(test)]
		TEST_ARTIFACT_PAYLOAD_PASSES.with(|passes| passes.set(passes.get() + 1));
		let mut file = File::open(&self.path)?;
		file.seek(SeekFrom::Start(self.payload_offset))?;
		Ok(file.take(self.payload_len))
	}

	fn inspection(&self) -> Inspection {
		Inspection {
			name: self.sideband.name.clone(),
			root_class: self.sideband.root_class.clone(),
			instances: self.sideband.instances,
			properties: self.sideband.properties,
			blobs: self
				.sideband
				.external
				.iter()
				.map(|value| value.hash)
				.collect::<HashSet<_>>()
				.len() as u64,
		}
	}
}

fn postorder(source: &dyn InstanceSource, root: Ref) -> Result<Vec<Ref>> {
	source.get_by_ref(root).context("artifact source root is missing")?;
	let mut stack = vec![(root, false)];
	let mut result = Vec::new();
	while let Some((id, expanded)) = stack.pop() {
		let instance = source.get_by_ref(id).context("artifact source instance is missing")?;
		if expanded {
			result.push(id);
			continue;
		}
		stack.push((id, true));
		stack.extend(instance.children.iter().rev().map(|child| (*child, false)));
	}
	Ok(result)
}

struct CanonicalHierarchySource<'a> {
	base: &'a dyn InstanceSource,
	children: HashMap<Ref, Vec<Ref>>,
}

impl<'a> CanonicalHierarchySource<'a> {
	fn new(base: &'a dyn InstanceSource, root: Ref) -> Result<Self> {
		let mut canonical = HashMap::new();
		let mut stack = vec![root];
		while let Some(id) = stack.pop() {
			let instance = base.get_by_ref(id).context("artifact source instance is missing")?;
			stack.extend(instance.children.iter().copied());
			if instance.children.len() < 2 {
				continue;
			}
			let mut identities = instance.children.iter().copied();
			let mut previous = ref_bytes(identities.next().expect("two or more children"))?;
			let mut already_canonical = true;
			for child in identities {
				let current = ref_bytes(child)?;
				if previous > current {
					already_canonical = false;
					break;
				}
				previous = current;
			}
			if already_canonical {
				continue;
			}
			let mut children = instance
				.children
				.iter()
				.copied()
				.map(|child| Ok((ref_bytes(child)?, child)))
				.collect::<Result<Vec<_>>>()?;
			#[cfg(test)]
			TEST_CANONICAL_HIERARCHY_SORTS.with(|sorts| sorts.set(sorts.get() + 1));
			children.sort_unstable_by_key(|(identity, _)| *identity);
			let children = children.into_iter().map(|(_, child)| child).collect::<Vec<_>>();
			canonical.insert(id, children);
		}
		Ok(Self {
			base,
			children: canonical,
		})
	}
}

impl InstanceSource for CanonicalHierarchySource<'_> {
	fn get_by_ref<'a>(&'a self, referent: Ref) -> Option<InstanceView<'a>> {
		let mut instance = self.base.get_by_ref(referent)?;
		if let Some(children) = self.children.get(&referent) {
			instance.children = children;
		}
		Some(instance)
	}
}

struct NormalizedSource<'a> {
	base: &'a dyn InstanceSource,
	overrides: HashMap<Ref, UstrMap<Variant>>,
}

impl InstanceSource for NormalizedSource<'_> {
	fn get_by_ref<'a>(&'a self, referent: Ref) -> Option<InstanceView<'a>> {
		let mut instance = self.base.get_by_ref(referent)?;
		if let Some(properties) = self.overrides.get(&referent) {
			instance.properties = properties;
		}
		Some(instance)
	}
}

struct FilteredSource<'a, T: InstanceSource + ?Sized> {
	base: &'a T,
	excluded: &'a HashSet<Ref>,
	children: HashMap<Ref, Vec<Ref>>,
}

impl<'a, T: InstanceSource + ?Sized> FilteredSource<'a, T> {
	fn new(base: &'a T, root: Ref, excluded: &'a HashSet<Ref>) -> Result<Self> {
		ensure!(
			!excluded.contains(&root),
			"filesystem mapping may not exclude the artifact root"
		);
		let mut removed_by_parent = HashMap::<Ref, HashSet<Ref>>::new();
		for &id in excluded {
			let instance = base
				.get_by_ref(id)
				.with_context(|| format!("filesystem mapping identity {id} is absent from composite artifact"))?;
			if !excluded.contains(&instance.parent) {
				removed_by_parent.entry(instance.parent).or_default().insert(id);
			}
		}
		let mut children = HashMap::with_capacity(removed_by_parent.len());
		for (parent, removed) in removed_by_parent {
			let instance = base
				.get_by_ref(parent)
				.context("filesystem mapping parent is absent from composite artifact")?;
			let retained = instance
				.children
				.iter()
				.copied()
				.filter(|child| !removed.contains(child))
				.collect::<Vec<_>>();
			ensure!(
				instance.children.len() - retained.len() == removed.len(),
				"filesystem mapping roots do not match their composite parent"
			);
			children.insert(parent, retained);
		}
		Ok(Self {
			base,
			excluded,
			children,
		})
	}
}

impl<T: InstanceSource + ?Sized> InstanceSource for FilteredSource<'_, T> {
	fn get_by_ref<'a>(&'a self, referent: Ref) -> Option<InstanceView<'a>> {
		if self.excluded.contains(&referent) {
			return None;
		}
		let mut instance = self.base.get_by_ref(referent)?;
		if let Some(children) = self.children.get(&referent) {
			instance.children = children;
		}
		Some(instance)
	}
}

fn external_bytes(value: &Variant) -> Option<(ExternalKind, &[u8])> {
	match value {
		Variant::String(value) if value.len() >= BLOB_THRESHOLD => Some((ExternalKind::String, value.as_bytes())),
		Variant::BinaryString(value) if <BinaryString as AsRef<[u8]>>::as_ref(value).len() >= BLOB_THRESHOLD => {
			Some((ExternalKind::BinaryString, <BinaryString as AsRef<[u8]>>::as_ref(value)))
		}
		Variant::SharedString(value) if value.data().len() >= BLOB_THRESHOLD => {
			Some((ExternalKind::SharedString, value.data()))
		}
		_ => None,
	}
}

fn external_placeholder(kind: ExternalKind) -> Variant {
	match kind {
		ExternalKind::String => Variant::String(String::new()),
		ExternalKind::BinaryString => Variant::BinaryString(BinaryString::new()),
		ExternalKind::SharedString => Variant::SharedString(SharedString::new(Vec::new())),
	}
}

fn blob_path(artifact_path: &Path, hash: [u8; 32]) -> PathBuf {
	artifact_path
		.parent()
		.unwrap_or_else(|| Path::new("."))
		.join("blobs")
		.join(format!("{}.zst", blake3::Hash::from_bytes(hash).to_hex()))
}

fn validate_blob_file(path: &Path, hash: [u8; 32], expected_bytes: u64) -> Result<()> {
	let compressed = File::open(path).with_context(|| format!("missing Carbon blob {}", path.display()))?;
	let mut decoder = zstd::Decoder::new(BufReader::new(compressed))?;
	let mut hasher = blake3::Hasher::new();
	let mut bytes = 0_u64;
	let mut buffer = [0_u8; 64 * 1024];
	loop {
		let read = decoder.read(&mut buffer)?;
		if read == 0 {
			break;
		}
		bytes = bytes.checked_add(read as u64).context("Carbon blob length overflows")?;
		hasher.update(&buffer[..read]);
	}
	ensure!(bytes == expected_bytes, "Carbon blob length mismatch");
	ensure!(*hasher.finalize().as_bytes() == hash, "Carbon blob checksum mismatch");
	Ok(())
}

fn validate_artifact_blobs(artifact: &Artifact, anchor: &Path) -> Result<()> {
	let blobs = artifact_blob_receipts(artifact)?;
	validate_receipt_blobs(&blobs, anchor)
}

fn artifact_blob_receipts(artifact: &Artifact) -> Result<BTreeMap<[u8; 32], u64>> {
	let mut blobs = BTreeMap::new();
	for external in &artifact.sideband.external {
		if let Some(bytes) = blobs.insert(external.hash, external.bytes) {
			ensure!(bytes == external.bytes, "Carbon blob descriptors disagree on length");
		}
	}
	Ok(blobs)
}

fn validate_receipt_blobs(blobs: &BTreeMap<[u8; 32], u64>, anchor: &Path) -> Result<()> {
	for (&hash, &bytes) in blobs {
		validate_blob_file(&blob_path(anchor, hash), hash, bytes)?;
	}
	Ok(())
}

fn write_blob(artifact_path: &Path, hash: [u8; 32], bytes: &[u8]) -> Result<()> {
	let path = blob_path(artifact_path, hash);
	if path.is_file() {
		validate_blob_file(&path, hash, bytes.len() as u64)?;
		return Ok(());
	}
	let parent = path.parent().context("blob path has no parent")?;
	fs::create_dir_all(parent)?;
	let temporary = parent.join(format!(".blob-{}.tmp", Uuid::new_v4().simple()));
	let result = (|| {
		let file = File::create(&temporary)?;
		let mut encoder = zstd::Encoder::new(BufWriter::new(file), 3)?;
		encoder.include_checksum(true)?;
		encoder.write_all(bytes)?;
		let mut writer = encoder.finish()?;
		writer.flush()?;
		writer.get_ref().sync_all()?;
		fs::rename(&temporary, &path)?;
		sync_directory(parent)?;
		Ok(())
	})();
	if result.is_err() {
		let _ = fs::remove_file(&temporary);
	}
	result
}

fn prepare_source<'a>(
	base: &'a dyn InstanceSource,
	ids: &[Ref],
	artifact_path: &Path,
) -> Result<(NormalizedSource<'a>, Vec<ExternalValue>, Vec<ExternalReference>)> {
	let mut overrides = HashMap::new();
	let mut external = Vec::new();
	let mut references = Vec::new();
	let serialized = ids.iter().copied().collect::<HashSet<_>>();
	for (owner, id) in ids.iter().copied().enumerate() {
		let instance = base.get_by_ref(id).context("artifact source instance is missing")?;
		let mut replacement = None;
		for (property, value) in instance.properties {
			if let Some((kind, bytes)) = external_bytes(value) {
				let hash = *blake3::hash(bytes).as_bytes();
				write_blob(artifact_path, hash, bytes)?;
				replacement
					.get_or_insert_with(|| instance.properties.clone())
					.insert(*property, external_placeholder(kind));
				external.push(ExternalValue {
					owner: u32::try_from(owner).context("artifact has too many instances")?,
					property: property.to_string(),
					kind,
					hash,
					bytes: bytes.len() as u64,
				});
			}
			let external_reference = match value {
				Variant::Ref(target) if target.is_some() && !serialized.contains(target) => {
					Some((ExternalReferenceKind::Ref, *target, Variant::Ref(Ref::none())))
				}
				Variant::Content(content) => match content.value() {
					ContentType::Object(target) if target.is_some() && !serialized.contains(target) => Some((
						ExternalReferenceKind::Content,
						*target,
						Variant::Content(Content::from_referent(Ref::none())),
					)),
					_ => None,
				},
				_ => None,
			};
			if let Some((kind, target, placeholder)) = external_reference {
				replacement
					.get_or_insert_with(|| instance.properties.clone())
					.insert(*property, placeholder);
				references.push(ExternalReference {
					owner: u32::try_from(owner).context("artifact has too many instances")?,
					property: property.to_string(),
					kind,
					target: ref_bytes(target)?,
				});
			}
		}
		if let Some(replacement) = replacement {
			overrides.insert(id, replacement);
		}
	}
	external.sort();
	references.sort();
	Ok((NormalizedSource { base, overrides }, external, references))
}

fn write_artifact(
	source: &dyn InstanceSource,
	root: Ref,
	name: String,
	metadata: BTreeMap<String, String>,
	output: &Path,
) -> Result<ExtractReport> {
	#[cfg(test)]
	TEST_ARTIFACT_WRITES.with(|writes| writes.set(writes.get() + 1));
	let canonical = CanonicalHierarchySource::new(source, root)?;
	let source = &canonical as &dyn InstanceSource;
	let root_view = source.get_by_ref(root).context("artifact source root is missing")?;
	ensure!(root.is_some(), "artifact root has no stable Carbon identity");
	let ids = postorder(source, root)?;
	ensure!(ids.len() < u32::MAX as usize, "artifact has too many instances");
	let (normalized, external, references) = prepare_source(source, &ids, output)?;
	let parent = output.parent().unwrap_or_else(|| Path::new("."));
	fs::create_dir_all(parent)?;
	let artifact_path = parent.join(format!(".artifact-{}.tmp", Uuid::new_v4().simple()));
	let result = (|| {
		let mut writer = BufWriter::new(File::create(&artifact_path)?);
		writer.write_all(&[0_u8; FIXED_HEADER_BYTES as usize])?;
		let (report, payload_len, payload_hash) = {
			let mut payload = DigestingWriter::new(&mut writer);
			let report = Serializer::new(util::get_reflection_database())
				.metadata(metadata.clone())
				.serialize_source_with_report(&mut payload, &normalized, &[root])?;
			payload.flush()?;
			let (payload_len, payload_hash) = payload.evidence();
			(report, payload_len, payload_hash)
		};
		let columns = report.property_columns.into_iter().collect::<HashSet<_>>();
		let missing_descriptors = external
			.iter()
			.map(|value| (value.owner, value.property.as_str()))
			.chain(references.iter().map(|value| (value.owner, value.property.as_str())))
			.try_fold(0_u64, |count, (owner, property)| {
				let id = ids[owner as usize];
				let instance = source.get_by_ref(id).context("artifact descriptor owner is missing")?;
				let canonical = canonical_property_name(instance.class.as_str(), property).unwrap_or(property);
				Ok::<_, anyhow::Error>(count + u64::from(!columns.contains(&(instance.class, Ustr::from(canonical)))))
			})?;
		let properties = report
			.properties
			.checked_add(missing_descriptors)
			.context("artifact property count overflows")?;
		let stable_ids = ids.iter().copied().map(ref_bytes).collect::<Result<Vec<_>>>()?;
		let sideband = Sideband {
			format: "carbon-rbxl-v1".to_owned(),
			name,
			root: ref_bytes(root)?,
			root_name: root_view.name.to_owned(),
			root_class: root_view.class.to_string(),
			metadata,
			instances: ids.len() as u64,
			properties,
			identities: IdentityColumn::encode(&stable_ids)?,
			ids: stable_ids,
			external,
			references,
		};
		let sideband_bytes = rmp_serde::to_vec_named(&sideband)?;
		let sideband_hash = *blake3::hash(&sideband_bytes).as_bytes();
		writer.write_all(&sideband_bytes)?;
		writer.seek(SeekFrom::Start(0))?;
		writer.write_all(MAGIC)?;
		writer.write_all(&VERSION.to_le_bytes())?;
		writer.write_all(&FLAGS.to_le_bytes())?;
		writer.write_all(&payload_len.to_le_bytes())?;
		writer.write_all(&(sideband_bytes.len() as u64).to_le_bytes())?;
		writer.write_all(&payload_hash)?;
		writer.write_all(&sideband_hash)?;
		ensure!(
			writer.stream_position()? == FIXED_HEADER_BYTES,
			"Carbon artifact header width changed"
		);
		writer.flush()?;
		writer.get_ref().sync_all()?;
		drop(writer);
		replace_file_atomic(&artifact_path, output)?;
		sync_directory(parent)?;
		let referenced = sideband.external.iter().map(|value| value.hash).collect::<HashSet<_>>();
		let blobs = parent.join("blobs");
		let _ = (|| -> Result<()> {
			if blobs.is_dir() {
				for entry in fs::read_dir(&blobs)? {
					let entry = entry?;
					let name = entry.file_name();
					let Some(name) = name.to_str().and_then(|name| name.strip_suffix(".zst")) else {
						continue;
					};
					let Ok(hash) = blake3::Hash::from_hex(name) else {
						continue;
					};
					if !referenced.contains(hash.as_bytes()) {
						fs::remove_file(entry.path())?;
					}
				}
				if fs::read_dir(&blobs)?.next().is_none() {
					fs::remove_dir(&blobs)?;
				}
			}
			Ok(())
		})();
		Ok(ExtractReport {
			instances: sideband.instances,
			properties: sideband.properties,
			blobs: referenced.len() as u64,
			artifacts: 1,
		})
	})();
	let _ = fs::remove_file(&artifact_path);
	result
}

fn remap_variant(value: &mut Variant, ids: &[[u8; 16]]) -> Result<()> {
	match value {
		Variant::Ref(target) => *target = stable_binary_ref(*target, ids)?,
		Variant::Content(content) => {
			if let ContentType::Object(target) = content.value() {
				*content = Content::from_referent(stable_binary_ref(*target, ids)?);
			}
		}
		_ => {}
	}
	Ok(())
}

fn remap_properties(properties: &mut UstrMap<Variant>, ids: &[[u8; 16]]) -> Result<()> {
	for value in properties.values_mut() {
		remap_variant(value, ids)?;
	}
	Ok(())
}

fn hydrate_external(tree: &mut Tree, artifact: &Artifact, blob_anchor: &Path, hydrate_values: bool) -> Result<()> {
	if hydrate_values {
		let mut hydrated = HashMap::<[u8; 32], Vec<u8>>::new();
		for external in &artifact.sideband.external {
			let id = ref_from_bytes(artifact.sideband.ids[external.owner as usize]);
			if let std::collections::hash_map::Entry::Vacant(entry) = hydrated.entry(external.hash) {
				let path = blob_path(blob_anchor, external.hash);
				let compressed =
					File::open(&path).with_context(|| format!("missing Carbon blob {}", path.display()))?;
				let mut bytes = Vec::with_capacity(usize::try_from(external.bytes).context("blob is too large")?);
				zstd::Decoder::new(BufReader::new(compressed))?.read_to_end(&mut bytes)?;
				ensure!(bytes.len() as u64 == external.bytes, "Carbon blob length mismatch");
				ensure!(
					*blake3::hash(&bytes).as_bytes() == external.hash,
					"Carbon blob checksum mismatch"
				);
				entry.insert(bytes);
			}
			let bytes = hydrated[&external.hash].clone();
			let value = match external.kind {
				ExternalKind::String => {
					Variant::String(String::from_utf8(bytes).context("external string is not UTF-8")?)
				}
				ExternalKind::BinaryString => Variant::BinaryString(BinaryString::from(bytes)),
				ExternalKind::SharedString => Variant::SharedString(SharedString::new(bytes)),
			};
			tree.get_instance_mut(id)
				.context("external value owner is missing")?
				.properties
				.insert(Ustr::from(&external.property), value);
		}
	}
	for reference in &artifact.sideband.references {
		let id = ref_from_bytes(artifact.sideband.ids[reference.owner as usize]);
		let target = ref_from_bytes(reference.target);
		let value = match reference.kind {
			ExternalReferenceKind::Ref => Variant::Ref(target),
			ExternalReferenceKind::Content => Variant::Content(Content::from_referent(target)),
		};
		tree.get_instance_mut(id)
			.context("external reference owner is missing")?
			.properties
			.insert(Ustr::from(&reference.property), value);
	}
	Ok(())
}

fn load_artifact_structure(artifact: &Artifact) -> Result<Tree> {
	let decoder = Deserializer::new(util::get_reflection_database()).strict(true);
	let structure = decoder.deserialize_structure(BufReader::new(artifact.payload()?))?;
	ensure!(
		structure.metadata() == &artifact.sideband.metadata,
		"Carbon artifact metadata sideband does not match RBXL payload"
	);
	let synthetic_root = structure
		.get_by_ref(structure.root_ref())
		.context("RBXL synthetic root is missing")?;
	ensure!(
		synthetic_root.children.len() == 1,
		"Carbon artifact RBXL payload must have one root"
	);
	let binary_root = synthetic_root.children[0];
	let root_instance = structure
		.get_by_ref(binary_root)
		.context("Carbon artifact RBXL root is missing")?;
	let root = stable_binary_ref(binary_root, &artifact.sideband.ids)?;
	ensure!(
		root == ref_from_bytes(artifact.sideband.root),
		"root identity sideband mismatch"
	);
	ensure!(
		root_instance.name == artifact.sideband.root_name,
		"root name sideband mismatch"
	);
	ensure!(
		root_instance.class.as_str() == artifact.sideband.root_class,
		"root class sideband mismatch"
	);
	let mut tree = Tree::new_detached(
		Snapshot::new()
			.with_id(root)
			.with_name(root_instance.name)
			.with_class(root_instance.class.as_str()),
		artifact.sideband.ids.len(),
	)?;
	let mut stack = root_instance
		.children
		.iter()
		.rev()
		.map(|child| (*child, root))
		.collect::<Vec<_>>();
	let mut observed = 1_usize;
	while let Some((binary, parent)) = stack.pop() {
		let instance = structure
			.get_by_ref(binary)
			.context("RBXL structure instance is missing")?;
		let id = stable_binary_ref(binary, &artifact.sideband.ids)?;
		tree.insert_detached(
			Snapshot::new()
				.with_id(id)
				.with_name(instance.name)
				.with_class(instance.class.as_str()),
			parent,
		)?;
		observed += 1;
		stack.extend(instance.children.iter().rev().map(|child| (*child, id)));
	}
	ensure!(
		observed == artifact.sideband.ids.len(),
		"RBXL instance count does not match identity sideband"
	);
	tree.finish_detached()?;
	Ok(tree)
}

fn load_artifact_tree_from(artifact: &Artifact, blob_anchor: &Path, hydrate_values: bool) -> Result<Tree> {
	#[cfg(test)]
	TEST_TREE_LOADS.with(|loads| loads.set(loads.get() + 1));
	let decoder = Deserializer::new(util::get_reflection_database()).strict(true);
	let arena = decoder.deserialize_compact_source(BufReader::new(artifact.payload()?))?;
	ensure!(
		arena.metadata() == &artifact.sideband.metadata,
		"Carbon artifact metadata sideband does not match RBXL payload"
	);
	let synthetic_root = arena
		.get_by_ref(arena.root_ref())
		.context("RBXL synthetic root is missing")?;
	ensure!(
		synthetic_root.children.len() == 1,
		"Carbon artifact RBXL payload must have one root"
	);
	let synthetic_root_ref = arena.root_ref();
	let binary_root = synthetic_root.children[0];
	let root_instance = arena
		.get_by_ref(binary_root)
		.context("Carbon artifact RBXL root is missing")?;
	let root = stable_binary_ref(binary_root, &artifact.sideband.ids)?;
	ensure!(
		root == ref_from_bytes(artifact.sideband.root),
		"root identity sideband mismatch"
	);
	ensure!(
		root_instance.name == artifact.sideband.root_name,
		"root name sideband mismatch"
	);
	ensure!(
		root_instance.class.as_str() == artifact.sideband.root_class,
		"root class sideband mismatch"
	);
	let mut root_instance = None;
	let mut instances = Vec::with_capacity(artifact.sideband.ids.len().saturating_sub(1));
	for instance in arena.into_instances() {
		if instance.referent == synthetic_root_ref {
			continue;
		}
		if instance.referent == binary_root {
			root_instance = Some(instance);
		} else {
			instances.push(instance);
		}
	}
	let mut root_instance = root_instance.context("Carbon artifact RBXL root is missing")?;
	remap_properties(&mut root_instance.properties, &artifact.sideband.ids)?;
	let mut tree = Tree::new_detached(
		Snapshot {
			id: root,
			name: root_instance.name,
			raw_name: None,
			class: root_instance.class,
			properties: root_instance.properties,
			children: Vec::new(),
		},
		artifact.sideband.ids.len(),
	)?;
	let observed = 1_usize
		.checked_add(instances.len())
		.context("RBXL instance count overflows")?;
	ensure!(
		observed == artifact.sideband.ids.len(),
		"RBXL instance count does not match identity sideband"
	);
	for mut instance in instances {
		let id = stable_binary_ref(instance.referent, &artifact.sideband.ids)?;
		let parent = stable_binary_ref(instance.parent, &artifact.sideband.ids)?;
		remap_properties(&mut instance.properties, &artifact.sideband.ids)?;
		tree.insert_detached(
			Snapshot {
				id,
				name: instance.name,
				raw_name: None,
				class: instance.class,
				properties: instance.properties,
				children: Vec::new(),
			},
			parent,
		)?;
	}
	tree.finish_detached()?;
	hydrate_external(&mut tree, artifact, blob_anchor, hydrate_values)?;
	let properties = tree
		.subtree_refs(tree.root_ref())?
		.into_iter()
		.try_fold(0_u64, |count, id| {
			count
				.checked_add(tree.get_instance(id).map_or(0, |node| node.properties.len()) as u64)
				.context("artifact property count overflows")
		})?;
	let property_distribution = if properties == artifact.sideband.properties {
		None
	} else {
		let mut counts = BTreeMap::<String, u64>::new();
		for id in tree.subtree_refs(tree.root_ref())? {
			for name in tree
				.get_instance(id)
				.context("artifact property owner is missing")?
				.properties
				.keys()
			{
				*counts.entry(name.to_string()).or_default() += 1;
			}
		}
		Some(counts)
	};
	ensure!(
		properties == artifact.sideband.properties,
		"RBXL property count {properties} does not match identity sideband {}; decoded properties: {property_distribution:?}",
		artifact.sideband.properties,
	);
	Ok(tree)
}

fn load_artifact_tree(artifact: &Artifact) -> Result<Tree> {
	load_artifact_tree_from(artifact, &artifact.path, true)
}

fn tree_snapshot(tree: &Tree, id: Ref) -> Result<Snapshot> {
	let node = tree.get_instance(id).context("artifact tree instance is missing")?;
	let mut properties = node.properties.clone();
	let raw_name = properties
		.remove(&Ustr::from("__CarbonRawName"))
		.and_then(|value| match value {
			Variant::BinaryString(value) => Some(serde_bytes::ByteBuf::from(value.into_vec())),
			_ => None,
		});
	Ok(Snapshot {
		id,
		name: node.name.clone(),
		raw_name,
		class: node.class,
		properties,
		children: node
			.children()
			.iter()
			.copied()
			.map(|child| tree_snapshot(tree, child))
			.collect::<Result<_>>()?,
	})
}

pub fn inspect(path: &Path) -> Result<Inspection> {
	Ok(Artifact::open(path)?.inspection())
}

pub fn initialize(path: &Path, name: String, is_place: bool) -> Result<ExtractReport> {
	ensure!(is_artifact_path(path), "output must be a .carbon artifact");
	let root = if is_place {
		InstanceBuilder::new("DataModel")
			.with_referent(Ref::new())
			.with_name("DataModel")
			.with_child(
				InstanceBuilder::new("Workspace")
					.with_referent(Ref::new())
					.with_name("Workspace"),
			)
			.with_child(
				InstanceBuilder::new("ServerStorage")
					.with_referent(Ref::new())
					.with_name("ServerStorage"),
			)
	} else {
		InstanceBuilder::new("Model").with_referent(Ref::new()).with_name(&name)
	};
	let dom = WeakDom::new(root);
	write_artifact(&dom, dom.root_ref(), name, BTreeMap::new(), path)
}

pub fn is_artifact_path(path: &Path) -> bool {
	path.extension().and_then(|value| value.to_str()) == Some("carbon")
}

pub fn extract_snapshot(snapshot: Snapshot, name: String, output: &Path) -> Result<ExtractReport> {
	extract_snapshot_with_metadata(snapshot, name, BTreeMap::new(), output)
}

pub(crate) fn extract_tree(tree: &Tree, name: String, output: &Path) -> Result<ExtractReport> {
	write_artifact(tree, tree.root_ref(), name, BTreeMap::new(), output)
}

pub fn extract_snapshot_with_metadata(
	snapshot: Snapshot,
	name: String,
	metadata: BTreeMap<String, String>,
	output: &Path,
) -> Result<ExtractReport> {
	let tree = Tree::new(snapshot);
	write_artifact(&tree, tree.root_ref(), name, metadata, output)
}

pub fn extract_binary(input: &Path, output: &Path) -> Result<ExtractReport> {
	let arena =
		Deserializer::new(util::get_reflection_database()).deserialize_source(BufReader::new(File::open(input)?))?;
	let name = output.get_stem();
	fn convert(
		source: &dyn InstanceSource,
		id: Ref,
		allocator: &mut ManifestIdentityAllocator,
		remap: &mut HashMap<Ref, Ref>,
	) -> Result<Snapshot> {
		let instance = source.get_by_ref(id).context("binary instance is missing")?;
		let stable = Ref::some(allocator.next());
		remap.insert(id, stable);
		let mut children = Vec::with_capacity(instance.children.len());
		for child in instance.children {
			children.push(convert(source, *child, allocator, remap)?);
		}
		Ok(Snapshot::new()
			.with_id(stable)
			.with_name(instance.name)
			.with_class(instance.class.as_str())
			.with_properties(instance.properties.clone())
			.with_children(children))
	}
	fn remap_references(snapshot: &mut Snapshot, remap: &HashMap<Ref, Ref>) -> Result<()> {
		for value in snapshot.properties.values_mut() {
			match value {
				Variant::Ref(target) if target.is_some() => {
					*target = *remap
						.get(target)
						.context("binary reference target is outside the extracted root")?;
				}
				Variant::Content(content) => {
					if let ContentType::Object(target) = content.value() {
						if target.is_some() {
							*content = Content::from_referent(
								*remap
									.get(target)
									.context("binary content target is outside the extracted root")?,
							);
						}
					}
				}
				_ => {}
			}
		}
		for child in &mut snapshot.children {
			remap_references(child, remap)?;
		}
		Ok(())
	}
	let mut allocator = ManifestIdentityAllocator::new();
	let mut remap = HashMap::new();
	let mut root = convert(&arena, arena.root_ref(), &mut allocator, &mut remap)?;
	remap_references(&mut root, &remap)?;
	let root_id = root.id;
	write_artifact(
		&Tree::new(root),
		root_id,
		name.to_owned(),
		arena.metadata().clone(),
		output,
	)
}

fn serialize_tree(
	tree: &mut Tree,
	output: &Path,
	worktree: Option<&WorktreeContract>,
	source_generation: Option<&str>,
	indexed_refs: Option<&HashSet<Ref>>,
) -> Result<(
	rbx_binary::SerializationReport,
	HashMap<Ref, rbx_binary::SerializedInstancePosition>,
)> {
	tree.canonicalize_output_order();
	let ids = tree.subtree_refs(tree.root_ref())?;
	let root = tree.root_ref();
	for id in ids {
		let node = tree.get_instance_mut(id).context("compiled instance is missing")?;
		adapt_lighting_output_properties(node.class.as_str(), &mut node.properties)?;
		let Some(contract) = worktree else { continue };
		if id != root && !contract.identity_exclusions.contains(&id) {
			let mut attributes = match node.properties.remove(&Ustr::from("Attributes")) {
				Some(Variant::Attributes(attributes)) => attributes,
				Some(_) => bail!("{}.Attributes has an unexpected type", node.class),
				None => Attributes::new(),
			};
			attributes.insert(MANIFEST_IDENTITY_ATTRIBUTE.to_owned(), id.to_string().into());
			node.properties
				.insert(Ustr::from("Attributes"), Variant::Attributes(attributes));
		}
		if node.class.as_str() == "Workspace" {
			let mut attributes = match node.properties.remove(&Ustr::from("Attributes")) {
				Some(Variant::Attributes(attributes)) => attributes,
				Some(_) => bail!("Workspace.Attributes has an unexpected type"),
				None => Attributes::new(),
			};
			attributes.insert(
				"__StudioWorktree_CarbonEndpoint".to_owned(),
				contract.endpoint.clone().into(),
			);
			attributes.insert(
				"__StudioWorktree_CarbonProject".to_owned(),
				contract.project.clone().into(),
			);
			attributes.insert(
				"__StudioWorktree_Identity".to_owned(),
				contract.worktree_id.clone().into(),
			);
			attributes.insert(
				"__StudioWorktree_Session".to_owned(),
				contract.session_token.clone().into(),
			);
			attributes.insert(
				"__StudioWorktree_CarbonGeneration".to_owned(),
				source_generation
					.context("managed build source generation is missing")?
					.into(),
			);
			node.properties
				.insert(Ustr::from("Attributes"), Variant::Attributes(attributes));
		}
	}
	let writer = BufWriter::new(File::create(output)?);
	let serializer = Serializer::new(util::get_reflection_database());
	let result = match indexed_refs {
		Some(indexed_refs) => serializer
			.serialize_source_with_report_and_index(writer, tree, tree.place_root_refs(), indexed_refs)
			.map_err(Into::into),
		None => Ok((
			serializer.serialize_source_with_report(writer, tree, tree.place_root_refs())?,
			HashMap::new(),
		)),
	};
	result
}

fn compile_impl(path: &Path, output: &Path, worktree: Option<&WorktreeContract>) -> Result<CompileReport> {
	let source_generation = worktree.map(|_| canonical_source_generation(path)).transpose()?;
	let artifact = Artifact::open(path)?;
	let mut tree = load_artifact_tree(&artifact)?;
	let temporary = output.with_file_name(format!(".{}.tmp-{}", output.get_name(), Uuid::new_v4().simple()));
	if let Some(parent) = output.parent() {
		fs::create_dir_all(parent)?;
	}
	let result = serialize_tree(&mut tree, &temporary, worktree, source_generation.as_deref(), None);
	match result {
		Ok(_) => replace_file_atomic(&temporary, output)?,
		Err(error) => {
			let _ = fs::remove_file(&temporary);
			return Err(error);
		}
	}
	Ok(CompileReport {
		instances: artifact.sideband.instances,
		properties: artifact.sideband.properties,
		artifacts: 1,
	})
}

/// Compile a composed tree which was derived from a strictly validated Studio
/// artifact and mapped source. The caller owns the tree so output-only
/// normalization can happen in place without cloning the complete hierarchy.
pub(crate) fn compile_tree(
	tree: &mut Tree,
	output: &Path,
	worktree: Option<&WorktreeContract>,
	source_generation: Option<&str>,
	indexed_refs: &HashSet<Ref>,
) -> Result<(CompileReport, HashMap<Ref, rbx_binary::SerializedInstancePosition>)> {
	let refs = tree.subtree_refs(tree.root_ref())?;
	let instances = refs.len() as u64;
	let properties = refs.into_iter().try_fold(0_u64, |count, id| {
		count
			.checked_add(tree.get_instance(id).map_or(0, |node| node.properties.len()) as u64)
			.context("compiled property count overflows")
	})?;
	let temporary = output.with_file_name(format!(".{}.tmp-{}", output.get_name(), Uuid::new_v4().simple()));
	if let Some(parent) = output.parent() {
		fs::create_dir_all(parent)?;
	}
	let result = serialize_tree(tree, &temporary, worktree, source_generation, Some(indexed_refs));
	let positions = match result {
		Ok((_, positions)) => {
			replace_file_atomic(&temporary, output)?;
			positions
		}
		Err(error) => {
			let _ = fs::remove_file(&temporary);
			return Err(error);
		}
	};
	Ok((
		CompileReport {
			instances,
			properties,
			artifacts: 1,
		},
		positions,
	))
}

/// Replace the per-launch transport contract in an already compiled place.
/// The binary rewriter touches only Workspace.Attributes, so a cache hit does
/// not deserialize or reserialize the rest of a large place.
pub(crate) fn rewrite_worktree_contract(path: &Path, contract: &WorktreeContract) -> Result<()> {
	let temporary = path.with_file_name(format!(".{}.contract-{}", path.get_name(), Uuid::new_v4().simple()));
	let result = (|| -> Result<()> {
		let input = BufReader::new(File::open(path)?);
		let mut output = BufWriter::new(File::create(&temporary)?);
		let rewritten = rbx_binary::rewrite_workspace_attributes(input, &mut output, |attributes| {
			for (name, value) in [
				("__StudioWorktree_CarbonEndpoint", contract.endpoint.as_str()),
				("__StudioWorktree_CarbonProject", contract.project.as_str()),
				("__StudioWorktree_Identity", contract.worktree_id.as_str()),
				("__StudioWorktree_Session", contract.session_token.as_str()),
			] {
				attributes.insert(name.to_owned(), value.into());
			}
		})?;
		ensure!(rewritten, "compiled place has no Workspace Attributes transport column");
		output.flush()?;
		replace_file_atomic(&temporary, path)
	})();
	if result.is_err() {
		let _ = fs::remove_file(&temporary);
	}
	result
}

/// Replace selected mapped script sources in an already compiled place.
pub(crate) fn rewrite_script_sources(path: &Path, patches: &[rbx_binary::ScriptSourcePatch]) -> Result<()> {
	let temporary = path.with_file_name(format!(".{}.sources-{}", path.get_name(), Uuid::new_v4().simple()));
	let result = (|| -> Result<()> {
		let input = BufReader::new(File::open(path)?);
		let mut output = BufWriter::new(File::create(&temporary)?);
		let rewritten = rbx_binary::rewrite_script_sources(input, &mut output, patches)?;
		ensure!(
			rewritten == patches.len(),
			"compiled place rewrote {rewritten} of {} mapped script sources",
			patches.len()
		);
		output.flush()?;
		replace_file_atomic(&temporary, path)
	})();
	if result.is_err() {
		let _ = fs::remove_file(&temporary);
	}
	result
}

pub fn compile(path: &Path, output: &Path) -> Result<CompileReport> {
	compile_impl(path, output, None)
}

pub fn compile_worktree(path: &Path, output: &Path, contract: &WorktreeContract) -> Result<CompileReport> {
	compile_impl(path, output, Some(contract))
}

pub struct LoadedTree {
	pub name: String,
	pub root_class: String,
	pub tree: Tree,
	pub store: ArtifactStore,
	pub live_mask: Vec<Vec<bool>>,
	pub script_sources: Arc<RwLock<HashMap<PathBuf, Ref>>>,
	pub script_roots: Vec<PathBuf>,
}

pub struct ArtifactStore {
	artifact_path: PathBuf,
	name: String,
	metadata: BTreeMap<String, String>,
	projected: bool,
	script_sources: Arc<RwLock<HashMap<PathBuf, Ref>>>,
}

#[derive(Default)]
pub struct ArtifactTransaction {
	replacement_roots: HashSet<Ref>,
	pages: Vec<Changes>,
}

fn source_ids(artifact: &Artifact) -> Vec<Ref> {
	artifact.sideband.ids.iter().copied().map(ref_from_bytes).collect()
}

fn loaded(path: &Path, selected: Option<&HashSet<Ref>>, properties: bool) -> Result<LoadedTree> {
	let artifact = Artifact::open(path)?;
	let full = if properties {
		load_artifact_tree(&artifact)?
	} else {
		load_artifact_structure(&artifact)?
	};
	let all_ids = source_ids(&artifact);
	let (tree, ids) = if let Some(selected) = selected {
		let root = full.root_ref();
		ensure!(selected.contains(&root), "projected artifact selection omits the root");
		let root_snapshot = tree_snapshot(&full, root)?;
		let mut projected = Tree::new_detached(
			Snapshot {
				children: Vec::new(),
				..root_snapshot
			},
			selected.len(),
		)?;
		let mut ids = vec![root];
		for id in full.subtree_refs(root)?.into_iter().skip(1) {
			if !selected.contains(&id) {
				continue;
			}
			let node = full.get_instance(id).context("selected artifact instance is missing")?;
			ensure!(
				selected.contains(&node.parent()),
				"projected artifact selection omits parent {} of {id}",
				node.parent()
			);
			let snapshot = tree_snapshot(&full, id)?;
			projected.insert_detached(
				Snapshot {
					children: Vec::new(),
					..snapshot
				},
				node.parent(),
			)?;
			ids.push(id);
		}
		projected.finish_detached()?;
		(projected, ids)
	} else {
		(full, all_ids)
	};
	let script_sources = Arc::new(RwLock::new(HashMap::new()));
	Ok(LoadedTree {
		name: artifact.sideband.name.clone(),
		root_class: artifact.sideband.root_class.clone(),
		live_mask: vec![vec![true; ids.len()]],
		store: ArtifactStore {
			artifact_path: path.to_owned(),
			name: artifact.sideband.name,
			metadata: artifact.sideband.metadata,
			projected: selected.is_some(),
			script_sources: script_sources.clone(),
		},
		tree,
		script_sources,
		script_roots: Vec::new(),
	})
}

pub fn load_tree(path: &Path) -> Result<LoadedTree> {
	loaded(path, None, true)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticConflict {
	pub identity: String,
	pub field: String,
	pub base: String,
	pub current: String,
	pub incoming: String,
}

#[derive(Clone, Debug)]
pub struct MergeReport {
	pub instances: u64,
	pub properties: u64,
}

#[derive(Clone, Debug)]
pub enum MergeOutcome {
	Merged(MergeReport),
	Conflicted(Vec<SemanticConflict>),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ResolutionSide {
	Base,
	Current,
	Incoming,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MergeSubjectView {
	pub path: Vec<String>,
	pub class: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MergeContextView {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub base: Option<MergeSubjectView>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub current: Option<MergeSubjectView>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub incoming: Option<MergeSubjectView>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MergeFieldView {
	pub kind: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MergeValueView {
	pub state: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub value: Option<JsonValue>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PlannedConflict {
	pub identity: String,
	pub field: MergeFieldView,
	pub context: MergeContextView,
	pub base: MergeValueView,
	pub current: MergeValueView,
	pub incoming: MergeValueView,
	pub allowed: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExternalValueKey {
	kind: ExternalKind,
	hash: [u8; 32],
	bytes: u64,
}

#[derive(Clone)]
struct SemanticProperty {
	value: Variant,
	external: Option<ExternalValueKey>,
}

impl std::fmt::Debug for SemanticProperty {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if let Some(external) = &self.external {
			return formatter
				.debug_struct("ExternalValue")
				.field("kind", &external.kind)
				.field("hash", &blake3::Hash::from_bytes(external.hash).to_hex())
				.field("bytes", &external.bytes)
				.finish();
		}
		self.value.fmt(formatter)
	}
}

impl PartialEq for SemanticProperty {
	fn eq(&self, other: &Self) -> bool {
		match (&self.external, &other.external) {
			(Some(left), Some(right)) => left == right,
			(None, None) => self.value == other.value,
			_ => false,
		}
	}
}

#[derive(Clone, Debug, PartialEq)]
struct SemanticNode {
	parent: Ref,
	name: String,
	class: Ustr,
	properties: UstrMap<SemanticProperty>,
}

#[derive(Clone, Debug)]
enum ConflictTarget {
	Node(Ref),
	Parent(Ref),
	Name(Ref),
	Class(Ref),
	Property(Ref, Ustr),
	ArtifactName,
	Metadata(String),
}

#[derive(Clone, Debug)]
enum MergeValue {
	Node(Option<SemanticNode>),
	Parent(Ref),
	Text(String),
	Class(Ustr),
	Property(Option<SemanticProperty>),
	Metadata(Option<String>),
}

#[derive(Clone, Debug)]
struct ArtifactConflict {
	identity: Ref,
	field: String,
	target: ConflictTarget,
	base: MergeValue,
	current: MergeValue,
	incoming: MergeValue,
	resolved: bool,
}

pub(crate) struct ArtifactMergePlan {
	root: Ref,
	base_nodes: HashMap<Ref, SemanticNode>,
	current_nodes: HashMap<Ref, SemanticNode>,
	incoming_nodes: HashMap<Ref, SemanticNode>,
	merged: HashMap<Ref, SemanticNode>,
	name: String,
	metadata: BTreeMap<String, String>,
	conflicts: Vec<ArtifactConflict>,
}

fn semantic_nodes(tree: &Tree, artifact: &Artifact) -> Result<HashMap<Ref, SemanticNode>> {
	let external = artifact
		.sideband
		.external
		.iter()
		.map(|value| {
			(
				(
					ref_from_bytes(artifact.sideband.ids[value.owner as usize]),
					Ustr::from(&value.property),
				),
				ExternalValueKey {
					kind: value.kind,
					hash: value.hash,
					bytes: value.bytes,
				},
			)
		})
		.collect::<HashMap<_, _>>();
	let mut nodes = HashMap::new();
	for id in tree.subtree_refs(tree.root_ref())? {
		let node = tree.get_instance(id).context("semantic merge instance is missing")?;
		nodes.insert(
			id,
			SemanticNode {
				parent: node.parent(),
				name: node.name.clone(),
				class: node.class,
				properties: node
					.properties
					.iter()
					.map(|(name, value)| {
						(
							*name,
							SemanticProperty {
								value: value.clone(),
								external: external.get(&(id, *name)).cloned(),
							},
						)
					})
					.collect(),
			},
		);
	}
	Ok(nodes)
}

fn merge_value<T: Clone + PartialEq>(base: &T, current: &T, incoming: &T) -> (T, bool) {
	if current == incoming {
		return (current.clone(), false);
	}
	if current == base {
		return (incoming.clone(), false);
	}
	if incoming == base {
		return (current.clone(), false);
	}
	(current.clone(), true)
}

fn push_conflict(
	conflicts: &mut Vec<ArtifactConflict>,
	identity: Ref,
	field: impl Into<String>,
	target: ConflictTarget,
	base: MergeValue,
	current: MergeValue,
	incoming: MergeValue,
) {
	conflicts.push(ArtifactConflict {
		identity,
		field: field.into(),
		target,
		base,
		current,
		incoming,
		resolved: false,
	});
}

fn merge_node(
	id: Ref,
	base: &SemanticNode,
	current: &SemanticNode,
	incoming: &SemanticNode,
	conflicts: &mut Vec<ArtifactConflict>,
) -> SemanticNode {
	let mut names = HashSet::new();
	names.extend(base.properties.keys().copied());
	names.extend(current.properties.keys().copied());
	names.extend(incoming.properties.keys().copied());
	let mut names = names.into_iter().collect::<Vec<_>>();
	names.sort_unstable_by_key(|name| name.to_string());
	let mut properties = UstrMap::default();
	for name in names {
		let base_value = base.properties.get(&name).cloned();
		let current_value = current.properties.get(&name).cloned();
		let incoming_value = incoming.properties.get(&name).cloned();
		let (value, conflicted) = merge_value(&base_value, &current_value, &incoming_value);
		if conflicted {
			push_conflict(
				conflicts,
				id,
				format!("property.{name}"),
				ConflictTarget::Property(id, name),
				MergeValue::Property(base_value),
				MergeValue::Property(current_value),
				MergeValue::Property(incoming_value),
			);
		}
		if let Some(value) = value {
			properties.insert(name, value);
		}
	}
	let (parent, parent_conflict) = merge_value(&base.parent, &current.parent, &incoming.parent);
	if parent_conflict {
		push_conflict(
			conflicts,
			id,
			"parent",
			ConflictTarget::Parent(id),
			MergeValue::Parent(base.parent),
			MergeValue::Parent(current.parent),
			MergeValue::Parent(incoming.parent),
		);
	}
	let (name, name_conflict) = merge_value(&base.name, &current.name, &incoming.name);
	if name_conflict {
		push_conflict(
			conflicts,
			id,
			"name",
			ConflictTarget::Name(id),
			MergeValue::Text(base.name.clone()),
			MergeValue::Text(current.name.clone()),
			MergeValue::Text(incoming.name.clone()),
		);
	}
	let (class, class_conflict) = merge_value(&base.class, &current.class, &incoming.class);
	if class_conflict {
		push_conflict(
			conflicts,
			id,
			"class",
			ConflictTarget::Class(id),
			MergeValue::Class(base.class),
			MergeValue::Class(current.class),
			MergeValue::Class(incoming.class),
		);
	}
	SemanticNode {
		parent,
		name,
		class,
		properties,
	}
}

fn subject_view(id: Ref, nodes: &HashMap<Ref, SemanticNode>, root: Ref) -> Option<MergeSubjectView> {
	let node = nodes.get(&id)?;
	let mut path = Vec::new();
	let mut current = id;
	let mut visited = HashSet::new();
	while current != root {
		if !visited.insert(current) {
			return None;
		}
		let current_node = nodes.get(&current)?;
		path.push(current_node.name.clone());
		current = current_node.parent;
	}
	path.reverse();
	if path.is_empty() {
		path.push(node.name.clone());
	}
	Some(MergeSubjectView {
		path,
		class: node.class.to_string(),
	})
}

fn external_preview(value: &Variant) -> Option<String> {
	const LIMIT: usize = 160;
	match value {
		Variant::String(value) => Some(value.chars().take(LIMIT).collect()),
		Variant::BinaryString(value) => std::str::from_utf8(value.as_ref())
			.ok()
			.map(|value| value.chars().take(LIMIT).collect()),
		Variant::SharedString(value) => std::str::from_utf8(value.data())
			.ok()
			.map(|value| value.chars().take(LIMIT).collect()),
		_ => None,
	}
}

fn canonical_property_json(value: &SemanticProperty, class: &str, property: &str) -> Result<JsonValue> {
	if let Some(external) = &value.external {
		return Ok(json!({
			"$type": "ExternalValue",
			"kind": external.kind,
			"blake3": blake3::Hash::from_bytes(external.hash).to_hex().to_string(),
			"bytes": external.bytes,
			"preview": external_preview(&value.value),
		}));
	}
	let resolved = UnresolvedValue::from_variant(value.value.clone(), class, property);
	let canonical = match resolved {
		UnresolvedValue::SpecialFloat(_) => resolved,
		_ => UnresolvedValue::FullyQualified(value.value.clone()),
	};
	Ok(serde_json::to_value(canonical)?)
}

fn field_view(field: &str) -> MergeFieldView {
	if let Some(name) = field.strip_prefix("property.") {
		return MergeFieldView {
			kind: "property".to_owned(),
			name: Some(name.to_owned()),
		};
	}
	if let Some(name) = field.strip_prefix("metadata.") {
		return MergeFieldView {
			kind: "metadata".to_owned(),
			name: Some(name.to_owned()),
		};
	}
	if field == "artifact.name" {
		return MergeFieldView {
			kind: "artifactName".to_owned(),
			name: None,
		};
	}
	MergeFieldView {
		kind: field.to_owned(),
		name: None,
	}
}

fn value_view(
	value: &MergeValue,
	field: &MergeFieldView,
	subject: Option<&MergeSubjectView>,
) -> Result<MergeValueView> {
	let absent = || MergeValueView {
		state: "absent".to_owned(),
		value: None,
	};
	Ok(match value {
		MergeValue::Node(None) => absent(),
		MergeValue::Node(Some(node)) => MergeValueView {
			state: "present".to_owned(),
			value: Some(json!({
				"parent": node.parent.is_some().then(|| node.parent.to_string()),
				"name": node.name,
				"class": node.class.as_str(),
				"properties": node.properties.len(),
			})),
		},
		MergeValue::Parent(parent) => MergeValueView {
			state: "value".to_owned(),
			value: Some(
				parent
					.is_some()
					.then(|| parent.to_string())
					.map_or(JsonValue::Null, JsonValue::String),
			),
		},
		MergeValue::Text(value) => MergeValueView {
			state: "value".to_owned(),
			value: Some(JsonValue::String(value.clone())),
		},
		MergeValue::Class(value) => MergeValueView {
			state: "value".to_owned(),
			value: Some(JsonValue::String(value.to_string())),
		},
		MergeValue::Property(None) | MergeValue::Metadata(None) => absent(),
		MergeValue::Property(Some(value)) => {
			let property = field
				.name
				.as_deref()
				.context("property conflict has no property name")?;
			let class = subject.map(|subject| subject.class.as_str()).unwrap_or("");
			MergeValueView {
				state: "value".to_owned(),
				value: Some(canonical_property_json(value, class, property)?),
			}
		}
		MergeValue::Metadata(Some(value)) => MergeValueView {
			state: "value".to_owned(),
			value: Some(JsonValue::String(value.clone())),
		},
	})
}

impl ArtifactConflict {
	fn side(&self, side: ResolutionSide) -> &MergeValue {
		match side {
			ResolutionSide::Base => &self.base,
			ResolutionSide::Current => &self.current,
			ResolutionSide::Incoming => &self.incoming,
		}
	}

	fn rank(&self) -> u8 {
		match self.target {
			ConflictTarget::Node(_) => 0,
			ConflictTarget::Parent(_) => 1,
			ConflictTarget::Class(_) | ConflictTarget::Name(_) => 2,
			ConflictTarget::ArtifactName | ConflictTarget::Metadata(_) => 3,
			ConflictTarget::Property(_, _) => 4,
		}
	}
}

impl ArtifactMergePlan {
	pub(crate) fn conflict_count(&self) -> usize {
		self.conflicts.len()
	}

	pub(crate) fn conflict_rank(&self, index: usize) -> Result<u8> {
		Ok(self
			.conflicts
			.get(index)
			.context("merge conflict index is out of range")?
			.rank())
	}

	pub(crate) fn conflicts(&self) -> Result<Vec<PlannedConflict>> {
		self.conflicts
			.iter()
			.map(|conflict| {
				let context = MergeContextView {
					base: subject_view(conflict.identity, &self.base_nodes, self.root),
					current: subject_view(conflict.identity, &self.current_nodes, self.root),
					incoming: subject_view(conflict.identity, &self.incoming_nodes, self.root),
				};
				let field = field_view(&conflict.field);
				let mut allowed = vec!["base".to_owned(), "current".to_owned(), "incoming".to_owned()];
				if !matches!(conflict.target, ConflictTarget::Node(_)) {
					allowed.push("custom".to_owned());
				}
				if matches!(
					conflict.target,
					ConflictTarget::Property(_, _) | ConflictTarget::Metadata(_)
				) {
					allowed.push("remove".to_owned());
				}
				Ok(PlannedConflict {
					identity: conflict.identity.to_string(),
					field: field.clone(),
					base: value_view(&conflict.base, &field, context.base.as_ref())?,
					current: value_view(&conflict.current, &field, context.current.as_ref())?,
					incoming: value_view(&conflict.incoming, &field, context.incoming.as_ref())?,
					context,
					allowed,
				})
			})
			.collect()
	}

	fn apply_value(&mut self, target: &ConflictTarget, value: MergeValue) -> Result<()> {
		match (target, value) {
			(ConflictTarget::Node(id), MergeValue::Node(node)) => {
				if let Some(node) = node {
					self.merged.insert(*id, node);
				} else {
					self.merged.remove(id);
				}
			}
			(ConflictTarget::Parent(id), MergeValue::Parent(parent)) => {
				self.merged
					.get_mut(id)
					.context("resolved parent owner is absent")?
					.parent = parent;
			}
			(ConflictTarget::Name(id), MergeValue::Text(name)) => {
				self.merged.get_mut(id).context("resolved name owner is absent")?.name = name;
			}
			(ConflictTarget::Class(id), MergeValue::Class(class)) => {
				self.merged.get_mut(id).context("resolved class owner is absent")?.class = class;
			}
			(ConflictTarget::Property(id, name), MergeValue::Property(value)) => {
				let properties = &mut self
					.merged
					.get_mut(id)
					.context("resolved property owner is absent")?
					.properties;
				if let Some(value) = value {
					properties.insert(*name, value);
				} else {
					properties.remove(name);
				}
			}
			(ConflictTarget::ArtifactName, MergeValue::Text(name)) => self.name = name,
			(ConflictTarget::Metadata(key), MergeValue::Metadata(value)) => {
				if let Some(value) = value {
					self.metadata.insert(key.clone(), value);
				} else {
					self.metadata.remove(key);
				}
			}
			_ => bail!("resolution value does not match its semantic field"),
		}
		Ok(())
	}

	fn restore_implicit_ancestors(&mut self, node: &SemanticNode, side: ResolutionSide) -> Result<()> {
		let source = match side {
			ResolutionSide::Base => &self.base_nodes,
			ResolutionSide::Current => &self.current_nodes,
			ResolutionSide::Incoming => &self.incoming_nodes,
		};
		let mut ancestors = Vec::new();
		let mut parent = node.parent;
		while parent.is_some() && !self.merged.contains_key(&parent) {
			if self
				.conflicts
				.iter()
				.any(|conflict| matches!(conflict.target, ConflictTarget::Node(id) if id == parent))
			{
				break;
			}
			let ancestor = source
				.get(&parent)
				.with_context(|| format!("selected semantic merge node requires missing ancestor {parent}"))?;
			ancestors.push((parent, ancestor.clone()));
			parent = ancestor.parent;
		}
		for (id, ancestor) in ancestors.into_iter().rev() {
			self.merged.insert(id, ancestor);
		}
		Ok(())
	}

	pub(crate) fn take(&mut self, index: usize, side: ResolutionSide) -> Result<()> {
		let conflict = self
			.conflicts
			.get(index)
			.context("merge conflict index is out of range")?
			.clone();
		if let MergeValue::Node(Some(node)) = conflict.side(side) {
			self.restore_implicit_ancestors(node, side)?;
		}
		self.apply_value(&conflict.target, conflict.side(side).clone())?;
		self.conflicts[index].resolved = true;
		Ok(())
	}

	pub(crate) fn set(&mut self, index: usize, value: JsonValue) -> Result<()> {
		let conflict = self
			.conflicts
			.get(index)
			.context("merge conflict index is out of range")?
			.clone();
		let resolved = match &conflict.target {
			ConflictTarget::Parent(_) => MergeValue::Parent(match value {
				JsonValue::Null => Ref::none(),
				JsonValue::String(value) => value.parse().context("custom parent is not a manifest identity")?,
				_ => bail!("custom parent must be a manifest identity string or null"),
			}),
			ConflictTarget::Name(_) | ConflictTarget::ArtifactName => {
				MergeValue::Text(value.as_str().context("custom name must be a string")?.to_owned())
			}
			ConflictTarget::Class(_) => {
				MergeValue::Class(Ustr::from(value.as_str().context("custom class must be a string")?))
			}
			ConflictTarget::Metadata(_) => MergeValue::Metadata(Some(
				value.as_str().context("custom metadata must be a string")?.to_owned(),
			)),
			ConflictTarget::Property(id, name) => {
				let class = self.merged.get(id).context("custom property owner is absent")?.class;
				let unresolved: UnresolvedValue = serde_json::from_value(value)?;
				let value = unresolved.resolve(class.as_str(), name.as_str())?;
				MergeValue::Property(Some(SemanticProperty { value, external: None }))
			}
			ConflictTarget::Node(_) => bail!("add/delete conflicts must select base, current, or incoming"),
		};
		self.apply_value(&conflict.target, resolved)?;
		self.conflicts[index].resolved = true;
		Ok(())
	}

	pub(crate) fn remove(&mut self, index: usize) -> Result<()> {
		let conflict = self
			.conflicts
			.get(index)
			.context("merge conflict index is out of range")?
			.clone();
		let value = match conflict.target {
			ConflictTarget::Property(_, _) => MergeValue::Property(None),
			ConflictTarget::Metadata(_) => MergeValue::Metadata(None),
			_ => bail!("only property and metadata conflicts may be removed"),
		};
		self.apply_value(&conflict.target, value)?;
		self.conflicts[index].resolved = true;
		Ok(())
	}

	pub(crate) fn finish(&self, output: &Path) -> Result<MergeReport> {
		ensure!(
			self.conflicts.iter().all(|conflict| conflict.resolved),
			"semantic merge still has unresolved conflicts"
		);
		ensure!(
			self.merged.contains_key(&self.root),
			"semantic merge deleted the artifact root"
		);
		ensure!(
			self.merged[&self.root].parent.is_none(),
			"semantic merge reparented the artifact root"
		);
		let mut children = HashMap::<Ref, Vec<Ref>>::new();
		for (id, node) in &self.merged {
			if *id == self.root {
				continue;
			}
			ensure!(node.parent.is_some(), "semantic merge detached identity {id}");
			ensure!(
				self.merged.contains_key(&node.parent),
				"semantic merge parent {} of {id} is missing",
				node.parent
			);
			children.entry(node.parent).or_default().push(*id);
		}
		for values in children.values_mut() {
			values.sort_unstable_by_key(ToString::to_string);
		}
		let mut visiting = HashSet::new();
		let mut visited = HashSet::new();
		let snapshot = node_snapshot(self.root, &self.merged, &children, &mut visiting, &mut visited)?;
		ensure!(
			visited.len() == self.merged.len(),
			"semantic merge produced instances unreachable from the root"
		);
		let report = extract_snapshot_with_metadata(snapshot, self.name.clone(), self.metadata.clone(), output)?;
		Ok(MergeReport {
			instances: report.instances,
			properties: report.properties,
		})
	}
}

fn node_snapshot(
	id: Ref,
	nodes: &HashMap<Ref, SemanticNode>,
	children: &HashMap<Ref, Vec<Ref>>,
	visiting: &mut HashSet<Ref>,
	visited: &mut HashSet<Ref>,
) -> Result<Snapshot> {
	ensure!(visiting.insert(id), "semantic merge produced a parent cycle at {id}");
	ensure!(
		visited.insert(id),
		"semantic merge reached identity {id} more than once"
	);
	let node = nodes.get(&id).context("semantic merge instance disappeared")?;
	let snapshots = children
		.get(&id)
		.into_iter()
		.flatten()
		.copied()
		.map(|child| node_snapshot(child, nodes, children, visiting, visited))
		.collect::<Result<Vec<_>>>()?;
	visiting.remove(&id);
	Ok(Snapshot::new()
		.with_id(id)
		.with_name(&node.name)
		.with_class(node.class.as_str())
		.with_properties(
			node.properties
				.iter()
				.map(|(name, property)| (*name, property.value.clone()))
				.collect(),
		)
		.with_children(snapshots))
}

fn plan_artifact_merge_impl(
	base: &Path,
	base_blob_anchor: &Path,
	hydrate_base: bool,
	current: &Path,
	current_blob_anchor: &Path,
	incoming: &Path,
	incoming_blob_anchor: &Path,
) -> Result<ArtifactMergePlan> {
	let base_artifact = Artifact::open(base)?;
	let current_artifact = Artifact::open(current)?;
	let incoming_artifact = Artifact::open(incoming)?;
	ensure!(
		base_artifact.sideband.root == current_artifact.sideband.root
			&& base_artifact.sideband.root == incoming_artifact.sideband.root,
		"Carbon artifacts do not share a stable root identity"
	);
	let root = ref_from_bytes(base_artifact.sideband.root);
	let base_tree = load_artifact_tree_from(&base_artifact, base_blob_anchor, hydrate_base)?;
	let current_tree = load_artifact_tree_from(&current_artifact, current_blob_anchor, true)?;
	let incoming_tree = load_artifact_tree_from(&incoming_artifact, incoming_blob_anchor, true)?;
	let base_nodes = semantic_nodes(&base_tree, &base_artifact)?;
	let current_nodes = semantic_nodes(&current_tree, &current_artifact)?;
	let incoming_nodes = semantic_nodes(&incoming_tree, &incoming_artifact)?;
	let mut ids = HashSet::new();
	ids.extend(base_nodes.keys().copied());
	ids.extend(current_nodes.keys().copied());
	ids.extend(incoming_nodes.keys().copied());
	let mut ids = ids.into_iter().collect::<Vec<_>>();
	ids.sort_unstable_by_key(ToString::to_string);
	let mut conflicts = Vec::new();
	let mut merged = HashMap::new();
	for id in ids {
		let base = base_nodes.get(&id);
		let current = current_nodes.get(&id);
		let incoming = incoming_nodes.get(&id);
		let node = match (base, current, incoming) {
			(None, Some(current), None) => Some(current.clone()),
			(None, None, Some(incoming)) => Some(incoming.clone()),
			(None, Some(current), Some(incoming)) if current == incoming => Some(current.clone()),
			(None, Some(current), Some(incoming)) => {
				push_conflict(
					&mut conflicts,
					id,
					"addition",
					ConflictTarget::Node(id),
					MergeValue::Node(None),
					MergeValue::Node(Some(current.clone())),
					MergeValue::Node(Some(incoming.clone())),
				);
				Some(current.clone())
			}
			(Some(_), None, None) => None,
			(Some(base), None, Some(incoming)) if base == incoming => None,
			(Some(base), Some(current), None) if base == current => None,
			(Some(base), None, Some(incoming)) => {
				push_conflict(
					&mut conflicts,
					id,
					"existence",
					ConflictTarget::Node(id),
					MergeValue::Node(Some(base.clone())),
					MergeValue::Node(None),
					MergeValue::Node(Some(incoming.clone())),
				);
				None
			}
			(Some(base), Some(current), None) => {
				push_conflict(
					&mut conflicts,
					id,
					"existence",
					ConflictTarget::Node(id),
					MergeValue::Node(Some(base.clone())),
					MergeValue::Node(Some(current.clone())),
					MergeValue::Node(None),
				);
				Some(current.clone())
			}
			(Some(base), Some(current), Some(incoming)) => {
				Some(merge_node(id, base, current, incoming, &mut conflicts))
			}
			(None, None, None) => unreachable!(),
		};
		if let Some(node) = node {
			merged.insert(id, node);
		}
	}
	let (name, name_conflict) = merge_value(
		&base_artifact.sideband.name,
		&current_artifact.sideband.name,
		&incoming_artifact.sideband.name,
	);
	if name_conflict {
		push_conflict(
			&mut conflicts,
			root,
			"artifact.name",
			ConflictTarget::ArtifactName,
			MergeValue::Text(base_artifact.sideband.name.clone()),
			MergeValue::Text(current_artifact.sideband.name.clone()),
			MergeValue::Text(incoming_artifact.sideband.name.clone()),
		);
	}
	let mut metadata_keys = HashSet::new();
	metadata_keys.extend(base_artifact.sideband.metadata.keys().cloned());
	metadata_keys.extend(current_artifact.sideband.metadata.keys().cloned());
	metadata_keys.extend(incoming_artifact.sideband.metadata.keys().cloned());
	let mut metadata_keys = metadata_keys.into_iter().collect::<Vec<_>>();
	metadata_keys.retain(|key| {
		!matches!(
			key.as_str(),
			CAPTURE_FINGERPRINT_METADATA_KEY
				| CAPTURE_PROJECT_GENERATION_METADATA_KEY
				| LEGACY_CAPTURE_FINGERPRINT_METADATA_KEY
		)
	});
	metadata_keys.sort();
	let mut metadata = BTreeMap::new();
	for key in metadata_keys {
		let base_value = base_artifact.sideband.metadata.get(&key).cloned();
		let current_value = current_artifact.sideband.metadata.get(&key).cloned();
		let incoming_value = incoming_artifact.sideband.metadata.get(&key).cloned();
		let (value, conflicted) = merge_value(&base_value, &current_value, &incoming_value);
		if conflicted {
			push_conflict(
				&mut conflicts,
				root,
				format!("metadata.{key}"),
				ConflictTarget::Metadata(key.clone()),
				MergeValue::Metadata(base_value),
				MergeValue::Metadata(current_value),
				MergeValue::Metadata(incoming_value),
			);
		}
		if let Some(value) = value {
			metadata.insert(key, value);
		}
	}
	Ok(ArtifactMergePlan {
		root,
		base_nodes,
		current_nodes,
		incoming_nodes,
		merged,
		name,
		metadata,
		conflicts,
	})
}

pub(crate) fn plan_artifact_merge(
	base: &Path,
	base_blob_anchor: &Path,
	current: &Path,
	current_blob_anchor: &Path,
	incoming: &Path,
	incoming_blob_anchor: &Path,
) -> Result<ArtifactMergePlan> {
	plan_artifact_merge_impl(
		base,
		base_blob_anchor,
		true,
		current,
		current_blob_anchor,
		incoming,
		incoming_blob_anchor,
	)
}

pub(crate) fn artifact_blob_names(path: &Path) -> Result<Vec<String>> {
	let artifact = Artifact::open(path)?;
	let mut names = artifact
		.sideband
		.external
		.iter()
		.map(|value| format!("{}.zst", blake3::Hash::from_bytes(value.hash).to_hex()))
		.collect::<Vec<_>>();
	names.sort();
	names.dedup();
	Ok(names)
}

fn planned_conflicts(plan: &ArtifactMergePlan) -> Result<Vec<SemanticConflict>> {
	plan.conflicts()?
		.into_iter()
		.map(|conflict| {
			Ok(SemanticConflict {
				identity: conflict.identity,
				field: match conflict.field.name {
					Some(name) => format!("{}.{}", conflict.field.kind, name),
					None => conflict.field.kind,
				},
				base: serde_json::to_string(&conflict.base)?,
				current: serde_json::to_string(&conflict.current)?,
				incoming: serde_json::to_string(&conflict.incoming)?,
			})
		})
		.collect()
}

fn install_git_merge_candidate(candidate: &Path, current: &Path) -> Result<()> {
	load_tree(candidate).context("Git merge candidate failed blob validation")?;
	let current_parent = current.parent().unwrap_or_else(|| Path::new("."));
	let replacement = current_parent.join(format!(".carbon-merge-result-{}.tmp", Uuid::new_v4().simple()));
	let result = (|| {
		fs::copy(candidate, &replacement)?;
		OpenOptions::new().write(true).open(&replacement)?.sync_all()?;
		replace_file_atomic(&replacement, current)?;
		sync_directory(current_parent)?;
		Ok(())
	})();
	let _ = fs::remove_file(&replacement);
	result
}

/// Merge hydrated copies of Git's temporary artifact files and atomically
/// replace the merge driver's `%A` file with the validated result. Git itself
/// installs the immutable blob-path union after the driver succeeds.
pub(crate) fn merge_git_artifacts(
	base: &Path,
	current: &Path,
	incoming: &Path,
	merge_output: &Path,
	worktree: &Path,
) -> Result<MergeOutcome> {
	let plan = plan_artifact_merge_impl(base, base, false, current, current, incoming, incoming)?;
	if plan.conflict_count() != 0 {
		return Ok(MergeOutcome::Conflicted(planned_conflicts(&plan)?));
	}
	let parent = worktree.parent().context("worktree artifact has no parent")?;
	let stage = parent.join(format!(".carbon-merge-{}", Uuid::new_v4().simple()));
	let candidate = stage.join("state.carbon");
	let result = (|| {
		let report = plan.finish(&candidate)?;
		install_git_merge_candidate(&candidate, merge_output)?;
		Ok(MergeOutcome::Merged(report))
	})();
	let _ = fs::remove_dir_all(&stage);
	result
}

/// Merge two Carbon artifacts against their common ancestor. The current
/// artifact is replaced atomically only when every semantic field converges.
pub fn merge_artifacts(base: &Path, current: &Path, incoming: &Path) -> Result<MergeOutcome> {
	let plan = plan_artifact_merge_impl(base, current, false, current, current, incoming, current)?;
	if plan.conflict_count() != 0 {
		return Ok(MergeOutcome::Conflicted(planned_conflicts(&plan)?));
	}
	Ok(MergeOutcome::Merged(plan.finish(current)?))
}

pub fn load_live(path: &Path) -> Result<LoadedTree> {
	loaded(path, None, false)
}

pub(crate) fn load_projected_live(
	path: &Path,
	mapped_refs: &HashSet<Ref>,
	routing_refs: &HashSet<Ref>,
) -> Result<LoadedTree> {
	let artifact = Artifact::open(path)?;
	let root = ref_from_bytes(artifact.sideband.root);
	let mut selected = mapped_refs.union(routing_refs).copied().collect::<HashSet<_>>();
	selected.insert(root);
	loaded(path, Some(&selected), true)
}

fn normalize_snapshot(snapshot: &mut Snapshot) -> Result<()> {
	normalize_wire_attributes(snapshot.class.as_str(), &mut snapshot.properties)?;
	for child in &mut snapshot.children {
		normalize_snapshot(child)?;
	}
	Ok(())
}

fn apply_changes(tree: &mut Tree, mut changes: Changes, replacements: &HashSet<Ref>) -> Result<()> {
	for root in replacements {
		if tree.exists(*root) {
			tree.remove_instance(*root);
		}
	}
	for addition in &mut changes.additions {
		normalize_wire_attributes(addition.class.as_str(), &mut addition.properties)?;
		for child in &mut addition.children {
			normalize_snapshot(child)?;
		}
		ensure!(
			tree.exists(addition.parent),
			"addition parent {} is missing",
			addition.parent
		);
		ensure!(!tree.exists(addition.id), "addition {} already exists", addition.id);
		tree.insert_instance_recursive(Snapshot::from(addition.clone()), addition.parent);
	}
	for update in changes.updates {
		let class = tree
			.get_instance(update.id)
			.context("updated instance is missing")?
			.class
			.to_string();
		let mut update = update;
		if let Some(properties) = &mut update.properties {
			normalize_wire_attributes(&class, properties)?;
		}
		tree.apply_update(update)?;
	}
	let mut removed = HashSet::new();
	for root in changes.removals {
		if replacements.contains(&root) || !tree.exists(root) {
			continue;
		}
		removed.extend(tree.subtree_refs(root)?);
		tree.remove_instance(root);
	}
	if !removed.is_empty() {
		for id in tree.subtree_refs(tree.root_ref())? {
			let node = tree.get_instance_mut(id).context("artifact instance disappeared")?;
			for value in node.properties.values_mut() {
				match value {
					Variant::Ref(target) if removed.contains(target) => *target = Ref::none(),
					Variant::Content(content) => {
						if let ContentType::Object(target) = content.value() {
							if removed.contains(target) {
								*content = Content::from_referent(Ref::none());
							}
						}
					}
					_ => {}
				}
			}
		}
	}
	Ok(())
}

impl ArtifactStore {
	pub fn artifact_path(&self) -> &Path {
		&self.artifact_path
	}

	pub(crate) fn is_projected(&self) -> bool {
		self.projected
	}

	pub(crate) fn generation(&self) -> Result<String> {
		canonical_source_generation(&self.artifact_path)
	}

	pub(crate) fn script_sources(&self) -> Arc<RwLock<HashMap<PathBuf, Ref>>> {
		self.script_sources.clone()
	}

	pub(crate) fn use_script_sources(&mut self, shared: Arc<RwLock<HashMap<PathBuf, Ref>>>) {
		self.script_sources = shared;
	}

	pub(crate) fn reload_projected(&mut self, _tree: &Tree) -> Result<()> {
		let artifact = Artifact::open(&self.artifact_path)?;
		self.name = artifact.sideband.name;
		self.metadata = artifact.sideband.metadata;
		self.projected = true;
		Ok(())
	}

	pub(crate) fn install_projected_receipt(&mut self, receipt: &ValidatedArtifactReceipt) {
		self.name = receipt.name().to_owned();
		self.metadata = receipt.metadata().clone();
		self.projected = true;
	}

	pub fn apply(&mut self, tree: &mut Tree, changes: Changes) -> Result<()> {
		let addition_ids = changes.additions.iter().map(|value| value.id).collect::<HashSet<_>>();
		let removals = changes.removals.iter().copied().collect::<HashSet<_>>();
		let replacements = addition_ids.intersection(&removals).copied().collect::<HashSet<_>>();
		let mut transaction = self.begin_transaction();
		self.prepare_replacements(tree, replacements, &mut transaction)?;
		self.apply_page(tree, changes, &mut transaction)?;
		self.commit_transaction(tree, transaction)
	}

	pub fn begin_transaction(&self) -> ArtifactTransaction {
		ArtifactTransaction::default()
	}

	pub(crate) fn prepare_replacements(
		&mut self,
		tree: &mut Tree,
		roots: impl IntoIterator<Item = Ref>,
		transaction: &mut ArtifactTransaction,
	) -> Result<()> {
		for root in roots {
			if transaction.replacement_roots.insert(root) && tree.exists(root) {
				tree.remove_instance(root);
			}
		}
		Ok(())
	}

	pub fn apply_page(
		&mut self,
		tree: &mut Tree,
		changes: Changes,
		transaction: &mut ArtifactTransaction,
	) -> Result<()> {
		apply_changes(tree, changes.clone(), &transaction.replacement_roots)?;
		transaction.pages.push(changes);
		Ok(())
	}

	pub fn commit_transaction(&mut self, tree: &mut Tree, transaction: ArtifactTransaction) -> Result<()> {
		if transaction.pages.is_empty() && transaction.replacement_roots.is_empty() {
			return Ok(());
		}
		let mut complete = if self.projected {
			load_artifact_tree(&Artifact::open(&self.artifact_path)?)?
		} else {
			tree.clone()
		};
		if self.projected {
			for root in &transaction.replacement_roots {
				if complete.exists(*root) {
					complete.remove_instance(*root);
				}
			}
			for page in transaction.pages {
				apply_changes(&mut complete, page, &HashSet::new())?;
			}
		}
		self.metadata.remove(CAPTURE_FINGERPRINT_METADATA_KEY);
		write_artifact(
			&complete,
			complete.root_ref(),
			self.name.clone(),
			self.metadata.clone(),
			&self.artifact_path,
		)?;
		Ok(())
	}
}

pub(crate) struct CanonicalSourceSnapshot {
	pub generation: String,
	pub scripts: HashMap<PathBuf, Vec<u8>>,
}

pub(crate) fn canonical_source_snapshot_for_paths(
	artifact_path: &Path,
	paths: impl IntoIterator<Item = PathBuf>,
) -> Result<CanonicalSourceSnapshot> {
	let bytes = fs::read(artifact_path)?;
	Artifact::from_bytes(artifact_path, &bytes)?;
	let base = artifact_path.parent().unwrap_or_else(|| Path::new("."));
	let mut paths = paths.into_iter().collect::<Vec<_>>();
	paths.sort();
	paths.dedup();
	let mut hasher = blake3::Hasher::new();
	hasher.update(b"carbon-canonical-artifact-v1\0");
	hasher.update(&(bytes.len() as u64).to_le_bytes());
	hasher.update(&bytes);
	let mut scripts = HashMap::new();
	for path in paths {
		let relative = path.strip_prefix(base).with_context(|| {
			format!(
				"mapped script source {} is outside artifact root {}",
				path.display(),
				base.display()
			)
		})?;
		ensure!(
			relative
				.components()
				.all(|value| matches!(value, std::path::Component::Normal(_))),
			"mapped source path contains unsafe components"
		);
		let contents = fs::read(&path).with_context(|| format!("failed to read mapped script {}", path.display()))?;
		let display = relative.to_string_lossy();
		hasher.update(&(display.len() as u64).to_le_bytes());
		hasher.update(display.as_bytes());
		hasher.update(&(contents.len() as u64).to_le_bytes());
		hasher.update(&contents);
		scripts.insert(path, contents);
	}
	Ok(CanonicalSourceSnapshot {
		generation: hasher.finalize().to_hex().to_string(),
		scripts,
	})
}

pub(crate) fn canonical_source_snapshot(path: &Path) -> Result<CanonicalSourceSnapshot> {
	canonical_source_snapshot_for_paths(path, Vec::new())
}

pub(crate) fn canonical_source_generation(path: &Path) -> Result<String> {
	Ok(canonical_source_snapshot(path)?.generation)
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceCursor {
	pub generation: String,
	pub index: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcePage {
	pub instances: Vec<PropertySnapshot>,
	pub cursor: Option<SourceCursor>,
	pub done: bool,
	pub encoded_bytes: usize,
}

pub struct PropertySnapshot {
	pub id: Ref,
	pub properties: UstrMap<Variant>,
}

impl Serialize for PropertySnapshot {
	fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		use serde::ser::SerializeStruct;
		let mut snapshot = serializer.serialize_struct("PropertySnapshot", 2)?;
		snapshot.serialize_field("id", &self.id)?;
		snapshot.serialize_field("properties", &WireProperties(&self.properties))?;
		snapshot.end()
	}
}

pub(crate) struct MappingSourcePage<'a>(pub(crate) &'a SourcePage);

impl Serialize for MappingSourcePage<'_> {
	fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		use serde::ser::SerializeStruct;
		let mut page = serializer.serialize_struct("SourcePage", 4)?;
		page.serialize_field("instances", &MappingPropertySnapshots(&self.0.instances))?;
		page.serialize_field("cursor", &self.0.cursor)?;
		page.serialize_field("done", &self.0.done)?;
		page.serialize_field("encodedBytes", &self.0.encoded_bytes)?;
		page.end()
	}
}

struct MappingPropertySnapshots<'a>(&'a [PropertySnapshot]);

impl Serialize for MappingPropertySnapshots<'_> {
	fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		use serde::ser::SerializeSeq;
		let mut instances = serializer.serialize_seq(Some(self.0.len()))?;
		for snapshot in self.0 {
			instances.serialize_element(&MappingPropertySnapshot(snapshot))?;
		}
		instances.end()
	}
}

struct MappingPropertySnapshot<'a>(&'a PropertySnapshot);

impl Serialize for MappingPropertySnapshot<'_> {
	fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		use serde::ser::SerializeStruct;
		let mut snapshot = serializer.serialize_struct("PropertySnapshot", 2)?;
		snapshot.serialize_field("id", &self.0.id)?;
		snapshot.serialize_field("properties", &MappingWireProperties(&self.0.properties))?;
		snapshot.end()
	}
}

struct MappingWireProperties<'a>(&'a UstrMap<Variant>);

impl Serialize for MappingWireProperties<'_> {
	fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		use serde::ser::SerializeMap;
		let serialized_len = self.0.keys().filter(|name| name.as_str() != "__CarbonRawName").count();
		let mut properties = serializer.serialize_map(Some(serialized_len))?;
		for (name, value) in self.0 {
			if name.as_str() != "__CarbonRawName" {
				properties.serialize_entry(name, &MappingWireVariant(value))?;
			}
		}
		properties.end()
	}
}

struct MappingWireAttributes<'a>(&'a Attributes);

impl Serialize for MappingWireAttributes<'_> {
	fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		use serde::ser::SerializeMap;
		let mut map = serializer.serialize_map(Some(self.0.len()))?;
		for (name, value) in self.0 {
			map.serialize_entry(name, &MappingWireVariant(value))?;
		}
		map.end()
	}
}

struct MappingWireVariant<'a>(&'a Variant);

impl Serialize for MappingWireVariant<'_> {
	fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		use serde::ser::SerializeMap;
		match self.0 {
			Variant::Attributes(value) => {
				let mut variant = serializer.serialize_map(Some(1))?;
				variant.serialize_entry("Attributes", &MappingWireAttributes(value))?;
				variant.end()
			}
			Variant::BinaryString(value) => {
				let mut variant = serializer.serialize_map(Some(1))?;
				variant.serialize_entry("BinaryString", &serde_bytes::Bytes::new(value.as_ref()))?;
				variant.end()
			}
			Variant::SharedString(value) => {
				let mut variant = serializer.serialize_map(Some(1))?;
				variant.serialize_entry("SharedString", &serde_bytes::Bytes::new(value.data()))?;
				variant.end()
			}
			value => value.serialize(serializer),
		}
	}
}

struct ReaderCache {
	generation: String,
	tree: Tree,
	ids: Vec<Ref>,
}

pub struct SourceReader {
	artifact_path: PathBuf,
	selected: Mutex<Option<HashSet<Ref>>>,
	generation_override: RwLock<Option<String>>,
	cache: Mutex<ReaderCache>,
}

impl SourceReader {
	fn construct(path: &Path, selected: Option<HashSet<Ref>>) -> Result<Self> {
		let artifact = Artifact::open(path)?;
		let tree = load_artifact_tree(&artifact)?;
		let ids = source_ids(&artifact)
			.into_iter()
			.filter(|id| selected.as_ref().is_none_or(|selected| selected.contains(id)))
			.collect();
		let generation = canonical_source_generation(path)?;
		Ok(Self {
			artifact_path: path.to_owned(),
			selected: Mutex::new(selected),
			generation_override: RwLock::new(None),
			cache: Mutex::new(ReaderCache { generation, tree, ids }),
		})
	}

	pub fn new(path: &Path, live_mask: Vec<Vec<bool>>) -> Result<Self> {
		let artifact = Artifact::open(path)?;
		ensure!(
			live_mask.len() == 1 && live_mask[0].len() == artifact.sideband.ids.len(),
			"live mask does not match Carbon artifact"
		);
		Self::construct(path, None)
	}

	pub fn new_projected(path: &Path, selected: HashSet<Ref>) -> Result<Self> {
		Self::construct(path, Some(selected))
	}

	fn refresh(&self) -> Result<()> {
		let generation = self
			.generation_override
			.read()
			.unwrap()
			.clone()
			.unwrap_or(canonical_source_generation(&self.artifact_path)?);
		if self.cache.lock().unwrap().generation == generation {
			return Ok(());
		}
		let artifact = Artifact::open(&self.artifact_path)?;
		let tree = load_artifact_tree(&artifact)?;
		let selected = self.selected.lock().unwrap().clone();
		let ids = source_ids(&artifact)
			.into_iter()
			.filter(|id| selected.as_ref().is_none_or(|selected| selected.contains(id)))
			.collect();
		*self.cache.lock().unwrap() = ReaderCache { generation, tree, ids };
		Ok(())
	}

	pub fn page(&self, cursor: Option<SourceCursor>, max_instances: usize, max_bytes: usize) -> Result<SourcePage> {
		self.read_page(cursor, max_instances, max_bytes, false)
	}

	pub fn metadata_page(
		&self,
		cursor: Option<SourceCursor>,
		max_instances: usize,
		max_bytes: usize,
	) -> Result<SourcePage> {
		self.read_page(cursor, max_instances, max_bytes, true)
	}

	pub fn generation(&self) -> String {
		self.cache.lock().unwrap().generation.clone()
	}

	fn read_page(
		&self,
		cursor: Option<SourceCursor>,
		max_instances: usize,
		max_bytes: usize,
		metadata_only: bool,
	) -> Result<SourcePage> {
		ensure!(
			max_instances > 0 && max_bytes > 0,
			"source page limits must be positive"
		);
		self.refresh()?;
		let cache = self.cache.lock().unwrap();
		let mut cursor = cursor.unwrap_or_else(|| SourceCursor {
			generation: cache.generation.clone(),
			..SourceCursor::default()
		});
		ensure!(
			cursor.generation == cache.generation,
			"source cursor generation changed"
		);
		ensure!(
			cursor.index <= cache.ids.len(),
			"source cursor is outside Carbon artifact"
		);
		let mut instances = Vec::new();
		let mut encoded_bytes = 0;
		while cursor.index < cache.ids.len() && instances.len() < max_instances {
			let id = cache.ids[cursor.index];
			let node = cache.tree.get_instance(id).context("source page instance is missing")?;
			let properties = if metadata_only {
				node.properties
					.iter()
					.filter(|(name, value)| {
						matches!(name.as_str(), "Attributes" | "Tags")
							|| matches!(value, Variant::Ref(_))
							|| matches!(value, Variant::Content(content) if matches!(content.value(), ContentType::Object(_)))
					})
					.map(|(name, value)| (*name, value.clone()))
					.collect()
			} else {
				node.properties.clone()
			};
			let snapshot = PropertySnapshot { id, properties };
			let bytes = rmp_serde::to_vec_named(&snapshot)?.len();
			if !instances.is_empty() && encoded_bytes + bytes > max_bytes {
				break;
			}
			encoded_bytes += bytes;
			instances.push(snapshot);
			cursor.index += 1;
		}
		let done = cursor.index == cache.ids.len();
		Ok(SourcePage {
			instances,
			cursor: (!done).then_some(cursor),
			done,
			encoded_bytes,
		})
	}

	pub(crate) fn set_projected_source_ids(&self, selected: HashSet<Ref>, generation: String) -> Result<()> {
		*self.selected.lock().unwrap() = Some(selected);
		*self.generation_override.write().unwrap() = Some(generation);
		self.refresh()
	}

	pub(crate) fn install_projected_state(&self, selected: HashSet<Ref>, generation: String) -> Result<()> {
		// The projected tree is the processor's bounded topology state. The
		// committed artifact retains the complete filesystem-backed source bytes.
		self.set_projected_source_ids(selected, generation)
	}

	pub(crate) fn set_generation(&self, generation: String) {
		*self.generation_override.write().unwrap() = Some(generation);
	}
}

pub(crate) fn validated_artifact_receipt(path: &Path) -> Result<ValidatedArtifactReceipt> {
	let bytes = fs::read(path)?;
	let artifact = Artifact::from_bytes(path, &bytes)?;
	validate_artifact_blobs(&artifact, path)?;
	let blobs = artifact_blob_receipts(&artifact)?;
	let mut hasher = blake3::Hasher::new();
	hasher.update(b"carbon-canonical-artifact-v1\0");
	hasher.update(&(bytes.len() as u64).to_le_bytes());
	hasher.update(&bytes);
	let generation = hasher.finalize().to_hex().to_string();
	let build_generation = artifact_build_generation(&artifact, &bytes)?;
	Ok(ValidatedArtifactReceipt {
		generation,
		build_generation,
		name: artifact.sideband.name,
		metadata: artifact.sideband.metadata,
		blobs,
	})
}

fn artifact_build_generation(artifact: &Artifact, bytes: &[u8]) -> Result<String> {
	const RBXL_HEADER_BYTES: usize = 32;
	const CHUNK_HEADER_BYTES: usize = 16;
	let payload_start = usize::try_from(artifact.payload_offset).context("artifact payload offset is too large")?;
	let payload_end = payload_start
		.checked_add(usize::try_from(artifact.payload_len).context("artifact payload is too large")?)
		.context("artifact payload range overflows")?;
	let payload = bytes
		.get(payload_start..payload_end)
		.context("artifact payload range is invalid")?;
	ensure!(
		payload.len() >= RBXL_HEADER_BYTES + CHUNK_HEADER_BYTES,
		"Carbon artifact RBXL payload is truncated"
	);
	ensure!(
		&payload[..8] == b"<roblox!" && &payload[8..14] == b"\x89\xff\r\n\x1a\n",
		"Carbon artifact RBXL payload header is invalid"
	);
	let mut chunk = std::io::Cursor::new(&payload[RBXL_HEADER_BYTES..]);
	let mut chunk_name = [0_u8; 4];
	chunk.read_exact(&mut chunk_name)?;
	let metadata_end = if chunk_name == *b"META" {
		let compressed_len = read_u32(&mut chunk)?;
		let uncompressed_len = read_u32(&mut chunk)?;
		ensure!(
			read_u32(&mut chunk)? == 0,
			"Carbon artifact RBXL metadata chunk is invalid"
		);
		let stored_len = if compressed_len == 0 {
			uncompressed_len
		} else {
			compressed_len
		};
		let end = RBXL_HEADER_BYTES
			.checked_add(CHUNK_HEADER_BYTES)
			.and_then(|value| value.checked_add(usize::try_from(stored_len).ok()?))
			.context("Carbon artifact RBXL metadata range overflows")?;
		ensure!(end <= payload.len(), "Carbon artifact RBXL metadata chunk is truncated");
		end
	} else {
		ensure!(
			artifact.sideband.metadata.is_empty(),
			"Carbon artifact RBXL metadata chunk is missing"
		);
		RBXL_HEADER_BYTES
	};

	let mut sideband = artifact.sideband.clone();
	sideband.metadata.remove(CAPTURE_FINGERPRINT_METADATA_KEY);
	sideband.metadata.remove(CAPTURE_PROJECT_GENERATION_METADATA_KEY);
	sideband.metadata.remove(LEGACY_CAPTURE_FINGERPRINT_METADATA_KEY);
	let normalized_sideband = rmp_serde::to_vec_named(&sideband)?;
	let mut hasher = blake3::Hasher::new();
	hasher.update(b"carbon-authored-build-input-v1\0");
	for field in [
		&payload[..RBXL_HEADER_BYTES],
		&payload[metadata_end..],
		normalized_sideband.as_slice(),
	] {
		hasher.update(&(field.len() as u64).to_le_bytes());
		hasher.update(field);
	}
	Ok(hasher.finalize().to_hex().to_string())
}

#[derive(Debug)]
pub(crate) struct StagedCompiledCapture {
	root: PathBuf,
	artifact: PathBuf,
	active_artifact: PathBuf,
	receipt: ValidatedArtifactReceipt,
	studio_artifact: Option<PathBuf>,
	studio_receipt: Option<ValidatedArtifactReceipt>,
	studio_excluded: HashSet<Ref>,
	cleanup_on_drop: AtomicBool,
}

impl StagedCompiledCapture {
	pub(crate) fn artifact(&self) -> &Path {
		&self.artifact
	}

	pub(crate) fn staged_data_dir(&self) -> Result<PathBuf> {
		Ok(self
			.artifact
			.parent()
			.context("staged artifact has no data directory")?
			.to_owned())
	}

	pub(crate) fn active_data_dir(&self) -> Result<PathBuf> {
		Ok(self
			.active_artifact
			.parent()
			.context("active artifact has no data directory")?
			.to_owned())
	}

	pub(crate) fn is_noop(&self) -> Result<bool> {
		if self.artifact == self.active_artifact {
			ensure!(self.active_artifact.is_file(), "active capture artifact is missing");
			validate_receipt_blobs(&self.receipt.blobs, &self.active_artifact)?;
			return Ok(true);
		}
		if !self.active_artifact.exists() {
			return Ok(false);
		}
		let active = validated_artifact_receipt(&self.active_artifact)?;
		Ok(active.build_generation() == self.receipt.build_generation())
	}

	pub(crate) fn receipt(&self) -> &ValidatedArtifactReceipt {
		&self.receipt
	}

	pub(crate) fn studio_domain_artifact(
		&self,
		excluded: &HashSet<Ref>,
	) -> Result<Option<(&Path, &ValidatedArtifactReceipt)>> {
		if excluded.is_empty() {
			return Ok(Some((&self.artifact, &self.receipt)));
		}
		if self.studio_excluded.is_empty() {
			return Ok(None);
		}
		ensure!(
			self.studio_excluded == *excluded,
			"captured Studio-domain exclusion set changed before staging"
		);
		Ok(Some((
			self.studio_artifact
				.as_deref()
				.context("captured Studio-domain artifact is missing")?,
			self.studio_receipt
				.as_ref()
				.context("captured Studio-domain receipt is missing")?,
		)))
	}

	pub(crate) fn preserve_for_recovery(&self) {
		self.cleanup_on_drop.store(false, Ordering::Release);
	}
}

impl Drop for StagedCompiledCapture {
	fn drop(&mut self) {
		if self.cleanup_on_drop.load(Ordering::Acquire) && self.root.exists() {
			let _ = fs::remove_dir_all(&self.root);
		}
	}
}

fn source_tree(source: &dyn InstanceSource, root: Ref, cancelled: &dyn Fn() -> bool) -> Result<Tree> {
	let root_view = source.get_by_ref(root).context("compiled capture root is missing")?;
	let root_snapshot = Snapshot::new()
		.with_id(root)
		.with_name(root_view.name)
		.with_class(root_view.class.as_str())
		.with_properties(root_view.properties.clone());
	let mut tree = Tree::new_detached(root_snapshot, 1024)?;
	let mut stack = root_view
		.children
		.iter()
		.rev()
		.map(|child| (*child, root))
		.collect::<Vec<_>>();
	while let Some((id, parent)) = stack.pop() {
		ensure!(
			!cancelled(),
			"Capture Manifest was cancelled during artifact structure staging"
		);
		let instance = source.get_by_ref(id).context("compiled capture instance is missing")?;
		ensure!(id.is_some(), "compiled capture instance has no stable Carbon identity");
		tree.insert_detached(
			Snapshot::new()
				.with_id(id)
				.with_name(instance.name)
				.with_class(instance.class.as_str())
				.with_properties(instance.properties.clone()),
			parent,
		)?;
		stack.extend(instance.children.iter().rev().map(|child| (*child, id)));
	}
	tree.finish_detached()?;
	Ok(tree)
}

pub(crate) fn stage_compiled_capture(
	source: &dyn InstanceSource,
	root_ref: Ref,
	name: String,
	mut metadata: BTreeMap<String, String>,
	preserve_records: &HashSet<Ref>,
	output: &Path,
	cancelled: &dyn Fn() -> bool,
) -> Result<StagedCompiledCapture> {
	let started = std::time::Instant::now();
	let tree = source_tree(source, root_ref, cancelled)?;
	let ids = tree.subtree_refs(tree.root_ref())?;
	let active_data = output.parent().unwrap_or_else(|| Path::new("."));
	let parent = active_data.parent().unwrap_or_else(|| Path::new("."));
	let root = parent.join(format!(
		".{}.carbon-capture-stage-{}",
		active_data.get_name(),
		Uuid::new_v4().simple()
	));
	fs::create_dir_all(&root)?;
	let staged_data = root.join(
		active_data
			.file_name()
			.context("capture artifact has no data directory name")?,
	);
	fs::create_dir_all(&staged_data)?;
	let staged = staged_data.join(output.file_name().context("capture artifact has no file name")?);
	metadata.remove(CAPTURE_FINGERPRINT_METADATA_KEY);
	metadata.remove(LEGACY_CAPTURE_FINGERPRINT_METADATA_KEY);
	let staged_studio = if preserve_records.is_empty() {
		None
	} else {
		let directory = root.join("studio-domain");
		fs::create_dir_all(&directory)?;
		Some(directory.join(output.file_name().context("capture artifact has no file name")?))
	};
	let result = match &staged_studio {
		Some(studio) => {
			let filtered = FilteredSource::new(&tree, tree.root_ref(), preserve_records)?;
			std::thread::scope(|scope| -> Result<()> {
				let studio_write =
					scope.spawn(|| write_artifact(&filtered, tree.root_ref(), name.clone(), metadata.clone(), studio));
				let composite_write = write_artifact(&tree, tree.root_ref(), name.clone(), metadata.clone(), &staged);
				let studio_write = studio_write
					.join()
					.map_err(|_| std::io::Error::other("Studio-domain artifact writer panicked"))?;
				composite_write?;
				studio_write?;
				Ok(())
			})
		}
		None => write_artifact(&tree, tree.root_ref(), name, metadata, &staged).map(|_| ()),
	};
	if let Err(error) = result {
		let _ = fs::remove_dir_all(&root);
		return Err(error);
	}
	crate::carbon_info!(
		"Capture Manifest artifact stage: instances={}, total={:.1}ms",
		ids.len(),
		started.elapsed().as_secs_f64() * 1_000.0,
	);
	let (receipt, studio_receipt) = match &staged_studio {
		Some(studio) => std::thread::scope(|scope| -> Result<_> {
			let studio_receipt = scope.spawn(|| validated_artifact_receipt(studio));
			let receipt = validated_artifact_receipt(&staged)?;
			let studio_receipt = studio_receipt
				.join()
				.map_err(|_| std::io::Error::other("Studio-domain artifact validator panicked"))??;
			Ok((receipt, Some(studio_receipt)))
		})?,
		None => (validated_artifact_receipt(&staged)?, None),
	};
	Ok(StagedCompiledCapture {
		root,
		artifact: staged,
		active_artifact: output.to_owned(),
		receipt,
		studio_artifact: staged_studio,
		studio_receipt,
		studio_excluded: preserve_records.clone(),
		cleanup_on_drop: AtomicBool::new(true),
	})
}

#[cfg(test)]
pub(crate) fn stage_snapshot_capture(
	snapshot: &Snapshot,
	name: String,
	preserve_records: &HashSet<Ref>,
	output: &Path,
	cancelled: &dyn Fn() -> bool,
) -> Result<StagedCompiledCapture> {
	let tree = Tree::new(snapshot.clone());
	stage_compiled_capture(
		&tree,
		tree.root_ref(),
		name,
		BTreeMap::new(),
		preserve_records,
		output,
		cancelled,
	)
}

fn link_or_copy(source: &Path, output: &Path) -> Result<()> {
	match fs::hard_link(source, output) {
		Ok(()) => Ok(()),
		Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
			fs::copy(source, output)?;
			Ok(())
		}
		Err(error) => Err(error.into()),
	}
}

fn files_equal(left: &Path, right: &Path) -> Result<bool> {
	if left == right {
		return Ok(true);
	}
	let length = fs::metadata(left)?.len();
	if length != fs::metadata(right)?.len() {
		return Ok(false);
	}
	let mut left = BufReader::with_capacity(1024 * 1024, File::open(left)?);
	let mut right = BufReader::with_capacity(1024 * 1024, File::open(right)?);
	let mut left_buffer = [0_u8; 64 * 1024];
	let mut right_buffer = [0_u8; 64 * 1024];
	let mut remaining = length;
	while remaining != 0 {
		let chunk = usize::try_from(remaining.min(left_buffer.len() as u64))?;
		left.read_exact(&mut left_buffer[..chunk])?;
		right.read_exact(&mut right_buffer[..chunk])?;
		if left_buffer[..chunk] != right_buffer[..chunk] {
			return Ok(false);
		}
		remaining -= chunk as u64;
	}
	Ok(true)
}

pub(crate) fn stage_validated_unfiltered_artifact(
	composite: &Path,
	receipt: &ValidatedArtifactReceipt,
	active: &Path,
	output: &Path,
	cancelled: &dyn Fn() -> bool,
) -> Result<bool> {
	ensure!(
		!cancelled(),
		"Capture Manifest was cancelled during Studio-domain staging"
	);
	if active.exists() && files_equal(active, composite)? {
		validate_receipt_blobs(&receipt.blobs, active)?;
		return Ok(false);
	}
	let output_root = output.parent().context("staged artifact has no data directory")?;
	fs::create_dir_all(output_root)?;
	link_or_copy(composite, output)?;
	for &hash in receipt.blobs.keys() {
		ensure!(
			!cancelled(),
			"Capture Manifest was cancelled during Studio-domain staging"
		);
		let source = blob_path(composite, hash);
		let target = blob_path(output, hash);
		fs::create_dir_all(target.parent().unwrap())?;
		link_or_copy(&source, &target)?;
	}
	Ok(true)
}

pub(crate) fn stage_unfiltered_artifact(
	composite: &Path,
	active: &Path,
	output: &Path,
	cancelled: &dyn Fn() -> bool,
) -> Result<bool> {
	let receipt = validated_artifact_receipt(composite)?;
	stage_validated_unfiltered_artifact(composite, &receipt, active, output, cancelled)
}

pub(crate) fn write_filtered_artifact(
	composite: &Path,
	excluded: &HashSet<Ref>,
	output: &Path,
	cancelled: &dyn Fn() -> bool,
) -> Result<()> {
	let artifact = Artifact::open(composite)?;
	let root = ref_from_bytes(artifact.sideband.root);
	ensure!(
		!excluded.contains(&root),
		"filesystem mapping may not exclude the artifact root"
	);
	let mut tree = load_artifact_tree(&artifact)?;
	let mut observed = HashSet::new();
	for id in excluded {
		ensure!(
			!cancelled(),
			"Capture Manifest was cancelled during Studio-domain staging"
		);
		if tree.exists(*id) {
			observed.insert(*id);
		}
	}
	ensure!(
		observed == *excluded,
		"filesystem mapping set does not match composite artifact"
	);
	let mut roots = excluded
		.iter()
		.copied()
		.filter(|id| {
			tree.get_instance(*id)
				.is_some_and(|node| !excluded.contains(&node.parent()))
		})
		.collect::<Vec<_>>();
	roots.sort_unstable_by_key(ToString::to_string);
	for root in roots {
		tree.remove_instance(root);
	}
	write_artifact(
		&tree,
		tree.root_ref(),
		artifact.sideband.name,
		artifact.sideband.metadata,
		output,
	)?;
	Ok(())
}

/// Write the Studio-owned complement from a composed tree already held by the
/// caller. This is equivalent to `write_filtered_artifact`, but avoids loading
/// the same validated composite payload again after a build.
pub(crate) fn write_filtered_tree(
	tree: &Tree,
	excluded: &HashSet<Ref>,
	name: String,
	metadata: BTreeMap<String, String>,
	output: &Path,
	cancelled: &dyn Fn() -> bool,
) -> Result<()> {
	let root = tree.root_ref();
	ensure!(
		!cancelled(),
		"Capture Manifest was cancelled during Studio-domain staging"
	);
	let filtered = FilteredSource::new(tree, root, excluded)?;
	ensure!(
		!cancelled(),
		"Capture Manifest was cancelled during Studio-domain staging"
	);
	write_artifact(&filtered, root, name, metadata, output)?;
	Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SourcemapNode {
	name: String,
	class_name: String,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	file_paths: Vec<PathBuf>,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	children: Vec<SourcemapNode>,
}

pub fn write_sourcemap(path: &Path, output: &Path) -> Result<u64> {
	let loaded = load_tree(path)?;
	let mut included = HashSet::new();
	for id in loaded.tree.subtree_refs(loaded.tree.root_ref())? {
		let node = loaded.tree.get_instance(id).unwrap();
		if !matches!(node.class.as_str(), "Script" | "LocalScript" | "ModuleScript") {
			continue;
		}
		let mut current = id;
		loop {
			if !included.insert(current) {
				break;
			}
			let parent = loaded.tree.get_instance(current).unwrap().parent();
			if parent.is_none() {
				break;
			}
			current = parent;
		}
	}
	if included.is_empty() {
		included.insert(loaded.tree.root_ref());
	}
	fn node(tree: &Tree, id: Ref, included: &HashSet<Ref>, root_file: &Path) -> SourcemapNode {
		let value = tree.get_instance(id).unwrap();
		SourcemapNode {
			name: value.name.clone(),
			class_name: value.class.to_string(),
			file_paths: (id == tree.root_ref())
				.then(|| root_file.to_owned())
				.into_iter()
				.collect(),
			children: value
				.children()
				.iter()
				.copied()
				.filter(|child| included.contains(child))
				.map(|child| node(tree, child, included, root_file))
				.collect(),
		}
	}
	let root_file = path.file_name().map(PathBuf::from).unwrap_or_else(|| path.to_owned());
	let mut value = node(&loaded.tree, loaded.tree.root_ref(), &included, &root_file);
	value.name = loaded.name;
	if let Some(parent) = output.parent() {
		fs::create_dir_all(parent)?;
	}
	let mut writer = BufWriter::new(File::create(output)?);
	serde_json::to_writer(&mut writer, &value)?;
	writer.write_all(b"\n")?;
	writer.flush()?;
	Ok(included.len() as u64)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::{SystemTime, UNIX_EPOCH};

	fn temp(name: &str) -> PathBuf {
		let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
		std::env::temp_dir().join(format!("carbon-artifact-{name}-{unique}"))
	}

	#[derive(Deserialize)]
	struct WireSourcePage {
		instances: Vec<WirePropertySnapshot>,
	}

	#[derive(Deserialize)]
	struct WirePropertySnapshot {
		properties: BTreeMap<String, WireVariant>,
	}

	#[derive(Deserialize)]
	enum WireVariant {
		Attributes(BTreeMap<String, WireVariant>),
		BinaryString(serde_bytes::ByteBuf),
	}
	#[test]
	fn mapping_source_page_encodes_nested_binary_attributes_as_msgpack_bytes() {
		let payload = b"captured-binary-attribute".to_vec();
		let nested = Attributes::new().with("Binary", BinaryString::from(payload.clone()));
		let attributes = Attributes::new().with("Nested", Variant::Attributes(nested));
		let page = SourcePage {
			instances: vec![PropertySnapshot {
				id: Ref::new(),
				properties: UstrMap::from_iter([(Ustr::from("Attributes"), Variant::Attributes(attributes))]),
			}],
			cursor: None,
			done: true,
			encoded_bytes: 0,
		};

		let encoded = rmp_serde::to_vec_named(&MappingSourcePage(&page)).unwrap();
		let decoded: WireSourcePage = rmp_serde::from_slice(&encoded).unwrap();
		let WireVariant::Attributes(attributes) = &decoded.instances[0].properties["Attributes"] else {
			panic!("top-level Attributes lost its tagged map representation");
		};
		let WireVariant::Attributes(nested) = &attributes["Nested"] else {
			panic!("nested Attributes lost its tagged map representation");
		};
		let WireVariant::BinaryString(bytes) = &nested["Binary"] else {
			panic!("nested BinaryString lost its tagged binary representation");
		};
		assert_eq!(bytes.as_ref(), payload);
	}

	fn fixture(root: Ref, child: Ref, properties: UstrMap<Variant>) -> Snapshot {
		Snapshot::new()
			.with_id(root)
			.with_name("Game")
			.with_class("DataModel")
			.with_children(vec![Snapshot::new()
				.with_id(child)
				.with_name("Shared")
				.with_class("Folder")
				.with_properties(properties)])
	}

	fn write(path: &Path, snapshot: Snapshot) {
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		extract_snapshot(snapshot, "Game".to_owned(), path).unwrap();
	}

	#[test]
	fn fresh_capture_stage_never_reads_the_active_artifact() {
		let directory = temp("fresh-stage-ignores-active");
		let artifact = directory.join("data/state.carbon");
		fs::create_dir_all(artifact.parent().unwrap()).unwrap();
		fs::write(&artifact, b"corrupt active artifact must not be reused").unwrap();
		let root = Ref::new();
		let child = Ref::new();
		let snapshot = fixture(
			root,
			child,
			UstrMap::from_iter([(Ustr::from("Archivable"), Variant::Bool(false))]),
		);

		let staged =
			stage_snapshot_capture(&snapshot, "Fresh".to_owned(), &HashSet::new(), &artifact, &|| false).unwrap();
		let loaded = load_tree(staged.artifact()).unwrap();
		assert_eq!(loaded.name, "Fresh");
		assert_eq!(
			loaded
				.tree
				.get_instance(child)
				.unwrap()
				.properties
				.get(&Ustr::from("Archivable")),
			Some(&Variant::Bool(false))
		);
		assert_eq!(
			fs::read(&artifact).unwrap(),
			b"corrupt active artifact must not be reused"
		);
		drop(staged);
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn strict_reader_rejects_the_removed_json_store() {
		let directory = temp("reject-json-v5");
		fs::create_dir_all(&directory).unwrap();
		let path = directory.join("state.carbon");
		let mut legacy = br#"{"format":"carbon-studio-state","version":5,"name":"Game"}"#.to_vec();
		legacy.resize(FIXED_HEADER_BYTES as usize + 1, b' ');
		fs::write(&path, legacy).unwrap();
		assert!(inspect(&path)
			.unwrap_err()
			.to_string()
			.contains("unsupported Carbon artifact magic"));
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn strict_reader_rejects_plain_rbxl_as_canonical_state() {
		let directory = temp("reject-plain-rbxl");
		fs::create_dir_all(&directory).unwrap();
		let path = directory.join("state.carbon");
		let dom = WeakDom::new(
			InstanceBuilder::new("DataModel")
				.with_referent(Ref::new())
				.with_name("DataModel"),
		);
		let mut bytes = Vec::new();
		Serializer::new(util::get_reflection_database())
			.serialize_source(&mut bytes, &dom, &[dom.root_ref()])
			.unwrap();
		bytes.resize(bytes.len().max(FIXED_HEADER_BYTES as usize), 0);
		fs::write(&path, bytes).unwrap();
		assert!(inspect(&path)
			.unwrap_err()
			.to_string()
			.contains("unsupported Carbon artifact magic"));
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn strict_tree_load_decodes_the_artifact_payload_once() {
		let directory = temp("single-pass-load");
		let artifact = directory.join("state.carbon");
		let root = Ref::new();
		let child = Ref::new();
		write(
			&artifact,
			fixture(
				root,
				child,
				UstrMap::from_iter([(Ustr::from("Archivable"), Variant::Bool(false))]),
			),
		);

		let (loaded, payload_passes) = count_artifact_payload_passes(|| load_tree(&artifact));
		let loaded = loaded.unwrap();
		assert_eq!(
			loaded
				.tree
				.get_instance(child)
				.unwrap()
				.properties
				.get(&Ustr::from("Archivable")),
			Some(&Variant::Bool(false))
		);
		assert_eq!(
			payload_passes, 1,
			"strict artifact loading must decode one payload pass"
		);
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn managed_build_embeds_the_exact_artifact_generation() {
		let directory = temp("managed-generation");
		let artifact = directory.join("state.carbon");
		let output = directory.join("managed.rbxl");
		write(
			&artifact,
			Snapshot::new()
				.with_id(Ref::new())
				.with_name("Game")
				.with_class("DataModel")
				.with_children(vec![Snapshot::new()
					.with_id(Ref::new())
					.with_name("Workspace")
					.with_class("Workspace")]),
		);
		let expected = canonical_source_generation(&artifact).unwrap();
		compile_worktree(
			&artifact,
			&output,
			&WorktreeContract {
				endpoint: "http://127.0.0.1:8000".to_owned(),
				project: "Game".to_owned(),
				worktree_id: "worktree".to_owned(),
				session_token: "session".to_owned(),
				identity_exclusions: HashSet::new(),
			},
		)
		.unwrap();
		let arena = Deserializer::new(util::get_reflection_database())
			.deserialize_source(BufReader::new(File::open(&output).unwrap()))
			.unwrap();
		let root = arena.get_by_ref(arena.root_ref()).unwrap();
		let workspace = root
			.children
			.iter()
			.map(|id| arena.get_by_ref(*id).unwrap())
			.find(|instance| instance.class.as_str() == "Workspace")
			.unwrap();
		let Some(Variant::Attributes(attributes)) = workspace.properties.get(&Ustr::from("Attributes")) else {
			panic!("managed Workspace attributes are missing")
		};
		assert_eq!(
			attributes.get("__StudioWorktree_CarbonGeneration"),
			Some(&Variant::BinaryString(BinaryString::from(expected.into_bytes())))
		);
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn artifact_bytes_are_deterministic_and_stable_references_round_trip() {
		let directory = temp("deterministic");
		let root = Ref::new();
		let target = Ref::new();
		let holder = Ref::new();
		let snapshot = Snapshot::new()
			.with_id(root)
			.with_name("Game")
			.with_class("DataModel")
			.with_children(vec![
				Snapshot::new().with_id(target).with_name("Target").with_class("Folder"),
				Snapshot::new()
					.with_id(holder)
					.with_name("Holder")
					.with_class("ObjectValue")
					.with_properties(UstrMap::from_iter([(Ustr::from("Value"), Variant::Ref(target))])),
			]);
		let first = directory.join("first/state.carbon");
		let second = directory.join("second/state.carbon");
		write(&first, snapshot.clone());
		write(&second, snapshot);
		assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
		let loaded = load_tree(&first).unwrap();
		assert_eq!(
			loaded.tree.get_instance(holder).unwrap().properties[&Ustr::from("Value")],
			Variant::Ref(target)
		);
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn already_canonical_artifact_hierarchy_skips_redundant_sibling_sort() {
		let directory = temp("canonical-hierarchy-fast-path");
		let artifact = directory.join("state.carbon");
		let children = (1..=8192)
			.map(|ordinal| {
				Snapshot::new()
					.with_id(Ref::some(ordinal))
					.with_name("Child")
					.with_class("Folder")
			})
			.collect();
		let snapshot = Snapshot::new()
			.with_id(Ref::some(8193))
			.with_name("Game")
			.with_class("DataModel")
			.with_children(children);

		let (result, sorts) =
			count_canonical_hierarchy_sorts(|| extract_snapshot(snapshot, "Game".to_owned(), &artifact));
		result.unwrap();
		assert_eq!(sorts, 0, "an already canonical hierarchy must not be sorted again");
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn binary_import_allocates_structured_ids_and_remaps_all_reference_variants() {
		let directory = temp("binary-import-identities");
		fs::create_dir_all(&directory).unwrap();
		let input = directory.join("input.rbxl");
		let artifact = directory.join("state.carbon");
		let target = InstanceBuilder::new("Folder").with_name("Target");
		let source_target = target.referent();
		let dom = WeakDom::new(
			InstanceBuilder::new("DataModel")
				.with_name("Game")
				.with_child(target)
				.with_child(
					InstanceBuilder::new("ObjectValue")
						.with_name("RefHolder")
						.with_property("Value", Variant::Ref(source_target)),
				)
				.with_child(
					InstanceBuilder::new("AdGui")
						.with_name("ContentHolder")
						.with_property("FallbackImageContent", Content::from_referent(source_target)),
				),
		);
		Serializer::new(util::get_reflection_database())
			.serialize_source(BufWriter::new(File::create(&input).unwrap()), &dom, &[dom.root_ref()])
			.unwrap();

		extract_binary(&input, &artifact).unwrap();
		let loaded = load_tree(&artifact).unwrap();
		let by_name = loaded
			.tree
			.subtree_refs(loaded.tree.root_ref())
			.unwrap()
			.into_iter()
			.map(|id| (loaded.tree.get_instance(id).unwrap().name.clone(), id))
			.collect::<HashMap<_, _>>();
		let target = by_name["Target"];
		let ref_holder = loaded.tree.get_instance(by_name["RefHolder"]).unwrap();
		let content_holder = loaded.tree.get_instance(by_name["ContentHolder"]).unwrap();
		assert_ne!(target, source_target);
		assert_eq!(ref_holder.properties[&Ustr::from("Value")], Variant::Ref(target));
		assert_eq!(
			content_holder.properties[&Ustr::from("FallbackImageContent")],
			Variant::Content(Content::from_referent(target))
		);
		let prefixes = by_name
			.values()
			.map(|id| ref_bytes(*id).unwrap()[..crate::manifest_identity::PREFIX_BYTES].to_vec())
			.collect::<HashSet<_>>();
		assert_eq!(prefixes.len(), 1);
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn root_properties_are_stored_in_the_payload_and_round_trip() {
		let directory = temp("root-properties");
		let path = directory.join("state.carbon");
		let root = Ref::new();
		let tags = vec![b'x'; BLOB_THRESHOLD + 1];
		write(
			&path,
			Snapshot::new()
				.with_id(root)
				.with_name("Game")
				.with_class("DataModel")
				.with_properties(UstrMap::from_iter([
					(Ustr::from("Archivable"), Variant::Bool(false)),
					(
						Ustr::from("Tags"),
						Variant::BinaryString(BinaryString::from(tags.clone())),
					),
				])),
		);
		let loaded = load_tree(&path).unwrap();
		let properties = &loaded.tree.get_instance(root).unwrap().properties;
		assert_eq!(properties[&Ustr::from("Archivable")], Variant::Bool(false));
		assert_eq!(
			properties[&Ustr::from("Tags")],
			Variant::BinaryString(BinaryString::from(tags))
		);
		assert_eq!(inspect(&path).unwrap().instances, 1);
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn raw_names_round_trip_through_the_normalized_payload() {
		let directory = temp("raw-name");
		let path = directory.join("state.carbon");
		let root = Ref::new();
		let child = Ref::new();
		let raw = serde_bytes::ByteBuf::from(vec![b'N', 0xff, b'm', b'e']);
		let mut named = Snapshot::new().with_id(child).with_name("N�me").with_class("Folder");
		named.raw_name = Some(raw.clone());
		write(
			&path,
			Snapshot::new()
				.with_id(root)
				.with_name("Game")
				.with_class("DataModel")
				.with_children(vec![named]),
		);
		let loaded = load_tree(&path).unwrap();
		assert_eq!(tree_snapshot(&loaded.tree, child).unwrap().raw_name, Some(raw));
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn large_values_are_content_addressed_and_integrity_checked() {
		let directory = temp("blob");
		let path = directory.join("state.carbon");
		let root = Ref::new();
		let script = Ref::new();
		let source = "x".repeat(BLOB_THRESHOLD + 1);
		write(
			&path,
			fixture(
				root,
				script,
				UstrMap::from_iter([(Ustr::from("Source"), Variant::String(source.clone()))]),
			),
		);
		assert_eq!(inspect(&path).unwrap().blobs, 1);
		assert_eq!(
			load_tree(&path).unwrap().tree.get_instance(script).unwrap().properties[&Ustr::from("Source")],
			Variant::String(source)
		);
		let blob = fs::read_dir(directory.join("blobs"))
			.unwrap()
			.next()
			.unwrap()
			.unwrap()
			.path();
		let mut bytes = fs::read(&blob).unwrap();
		let middle = bytes.len() / 2;
		bytes[middle] ^= 1;
		fs::write(&blob, bytes).unwrap();
		assert!(load_tree(&path).err().unwrap().to_string().contains("checksum"));
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn capture_local_refresh_preserves_authoritative_source_properties() {
		let directory = temp("capture-source-refresh");
		let path = directory.join("state.carbon");
		let root = Ref::new();
		let script = Ref::new();
		let previous_source = "return nil".to_owned();
		let source = "return function() end".to_owned();
		write(
			&path,
			Snapshot::new()
				.with_id(root)
				.with_name("Game")
				.with_class("DataModel")
				.with_children(vec![Snapshot::new()
					.with_id(script)
					.with_name("Generator")
					.with_class("ModuleScript")
					.with_properties(UstrMap::from_iter([(
						Ustr::from("Source"),
						Variant::String(previous_source.clone()),
					)]))]),
		);
		let selected = HashSet::from([root, script]);
		let reader = SourceReader::new_projected(&path, selected.clone()).unwrap();
		let unchanged_generation = canonical_source_generation(&path).unwrap();
		reader
			.install_projected_state(selected.clone(), unchanged_generation.clone())
			.unwrap();
		reader
			.install_projected_state(selected.clone(), unchanged_generation)
			.unwrap();
		let page = reader.page(None, 16, 1024 * 1024).unwrap();
		let generator = page.instances.iter().find(|instance| instance.id == script).unwrap();
		assert_eq!(
			generator.properties.get(&Ustr::from("Source")),
			Some(&Variant::String(previous_source))
		);

		write(
			&path,
			Snapshot::new()
				.with_id(root)
				.with_name("Game")
				.with_class("DataModel")
				.with_children(vec![Snapshot::new()
					.with_id(script)
					.with_name("Generator")
					.with_class("ModuleScript")
					.with_properties(UstrMap::from_iter([(
						Ustr::from("Source"),
						Variant::String(source.clone()),
					)]))]),
		);

		reader
			.install_projected_state(selected, canonical_source_generation(&path).unwrap())
			.unwrap();
		let page = reader.page(None, 16, 1024 * 1024).unwrap();
		let generator = page.instances.iter().find(|instance| instance.id == script).unwrap();

		assert_eq!(
			generator.properties.get(&Ustr::from("Source")),
			Some(&Variant::String(source))
		);
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn filtered_artifact_preserves_a_reference_to_filesystem_owned_identity() {
		let directory = temp("external-ref");
		fs::create_dir_all(&directory).unwrap();
		let composite = directory.join("composite.carbon");
		let filtered = directory.join("filtered.carbon");
		let direct = directory.join("direct/filtered.carbon");
		let from_tree = directory.join("tree/filtered.carbon");
		let root = Ref::new();
		let mapped = Ref::new();
		let holder = Ref::new();
		write(
			&composite,
			Snapshot::new()
				.with_id(root)
				.with_name("Game")
				.with_class("DataModel")
				.with_children(vec![
					Snapshot::new().with_id(mapped).with_name("Mapped").with_class("Folder"),
					Snapshot::new()
						.with_id(holder)
						.with_name("Holder")
						.with_class("ObjectValue")
						.with_properties(UstrMap::from_iter([(Ustr::from("Value"), Variant::Ref(mapped))])),
				]),
		);
		write_filtered_artifact(&composite, &HashSet::from([mapped]), &filtered, &|| false).unwrap();
		let composite_tree = load_tree(&composite).unwrap();
		let exclusions = HashSet::from([mapped]);
		let source = FilteredSource::new(&composite_tree.tree, composite_tree.tree.root_ref(), &exclusions).unwrap();
		write_artifact(
			&source,
			composite_tree.tree.root_ref(),
			composite_tree.name.clone(),
			composite_tree.store.metadata.clone(),
			&direct,
		)
		.unwrap();
		assert_eq!(fs::read(&direct).unwrap(), fs::read(&filtered).unwrap());
		write_filtered_tree(
			&composite_tree.tree,
			&exclusions,
			composite_tree.name.clone(),
			composite_tree.store.metadata.clone(),
			&from_tree,
			&|| false,
		)
		.unwrap();
		assert_eq!(
			fs::read(&from_tree).unwrap(),
			fs::read(&filtered).unwrap(),
			"clone-free tree projection changed canonical artifact bytes"
		);
		let loaded = load_tree(&filtered).unwrap();
		assert!(!loaded.tree.exists(mapped));
		assert_eq!(
			loaded.tree.get_instance(holder).unwrap().properties[&Ustr::from("Value")],
			Variant::Ref(mapped)
		);
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn semantic_merge_combines_disjoint_property_edits() {
		let directory = temp("merge-disjoint");
		let root = Ref::new();
		let child = Ref::new();
		let base = directory.join("base.carbon");
		let current = directory.join("current.carbon");
		let other = directory.join("other.carbon");
		let original = UstrMap::from_iter([
			(Ustr::from("Archivable"), Variant::Bool(true)),
			(Ustr::from("NameTag"), Variant::String("base".to_owned())),
		]);
		let mut current_properties = original.clone();
		current_properties.insert(Ustr::from("Archivable"), Variant::Bool(false));
		let mut other_properties = original.clone();
		other_properties.insert(Ustr::from("NameTag"), Variant::String("other".to_owned()));
		write(&base, fixture(root, child, original));
		write(&current, fixture(root, child, current_properties));
		write(&other, fixture(root, child, other_properties));
		assert!(matches!(
			merge_artifacts(&base, &current, &other).unwrap(),
			MergeOutcome::Merged(_)
		));
		let merged = load_tree(&current).unwrap();
		let properties = &merged.tree.get_instance(child).unwrap().properties;
		assert_eq!(properties[&Ustr::from("Archivable")], Variant::Bool(false));
		assert_eq!(
			properties[&Ustr::from("NameTag")],
			Variant::BinaryString(BinaryString::from(b"other".as_slice()))
		);
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn merge_stress_regression_tool_owned_capture_metadata_never_conflicts() {
		let directory = temp("merge-capture-metadata");
		let root = Ref::new();
		let child = Ref::new();
		let snapshot = fixture(root, child, UstrMap::default());
		let base = directory.join("base.carbon");
		let current = directory.join("current.carbon");
		let incoming = directory.join("incoming.carbon");
		let metadata = |fingerprint: &str, generation: &str| {
			BTreeMap::from([
				(CAPTURE_FINGERPRINT_METADATA_KEY.to_owned(), fingerprint.to_owned()),
				(
					CAPTURE_PROJECT_GENERATION_METADATA_KEY.to_owned(),
					generation.to_owned(),
				),
				("AuthoredSetting".to_owned(), "preserved".to_owned()),
			])
		};
		fs::create_dir_all(&directory).unwrap();
		extract_snapshot_with_metadata(
			snapshot.clone(),
			"Game".to_owned(),
			metadata("base-fingerprint", "base-generation"),
			&base,
		)
		.unwrap();
		extract_snapshot_with_metadata(
			snapshot.clone(),
			"Game".to_owned(),
			metadata("current-fingerprint", "current-generation"),
			&current,
		)
		.unwrap();
		extract_snapshot_with_metadata(
			snapshot,
			"Game".to_owned(),
			metadata("incoming-fingerprint", "incoming-generation"),
			&incoming,
		)
		.unwrap();

		assert!(matches!(
			merge_artifacts(&base, &current, &incoming).unwrap(),
			MergeOutcome::Merged(_)
		));
		let receipt = validated_artifact_receipt(&current).unwrap();
		assert_eq!(receipt.capture_fingerprint(), None);
		assert_eq!(receipt.project_generation(), None);
		assert_eq!(
			receipt.metadata().get("AuthoredSetting").map(String::as_str),
			Some("preserved")
		);
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn post_resolution_capture_attestation_is_an_exact_noop() {
		let directory = temp("post-resolution-capture-noop");
		fs::create_dir_all(&directory).unwrap();
		let root = Ref::new();
		let child = Ref::new();
		let base = directory.join("base.carbon");
		let current = directory.join("current.carbon");
		let incoming = directory.join("incoming.carbon");
		let resolved = directory.join("resolved.carbon");
		let captured = directory.join("captured.carbon");
		let property = |value: &str| UstrMap::from_iter([(Ustr::from("NameTag"), Variant::String(value.to_owned()))]);
		let metadata = |fingerprint: &str, generation: &str| {
			BTreeMap::from([
				(CAPTURE_FINGERPRINT_METADATA_KEY.to_owned(), fingerprint.to_owned()),
				(
					CAPTURE_PROJECT_GENERATION_METADATA_KEY.to_owned(),
					generation.to_owned(),
				),
				("AuthoredSetting".to_owned(), "preserved".to_owned()),
			])
		};
		for (path, value, fingerprint, generation) in [
			(&base, "base", "base-fingerprint", "base-generation"),
			(&current, "current", "current-fingerprint", "current-generation"),
			(&incoming, "incoming", "incoming-fingerprint", "incoming-generation"),
		] {
			extract_snapshot_with_metadata(
				fixture(root, child, property(value)),
				"Game".to_owned(),
				metadata(fingerprint, generation),
				path,
			)
			.unwrap();
		}

		let mut resolution = plan_artifact_merge(&base, &base, &current, &current, &incoming, &incoming).unwrap();
		assert_eq!(resolution.conflict_count(), 1);
		resolution.take(0, ResolutionSide::Incoming).unwrap();
		resolution.finish(&resolved).unwrap();
		let resolved_receipt = validated_artifact_receipt(&resolved).unwrap();
		assert_eq!(resolved_receipt.capture_fingerprint(), None);
		assert_eq!(resolved_receipt.project_generation(), None);

		let snapshot = load_tree(&resolved).unwrap().tree.into_snapshot().unwrap();
		let mut captured_metadata = resolved_receipt.metadata().clone();
		captured_metadata.insert(
			CAPTURE_FINGERPRINT_METADATA_KEY.to_owned(),
			"fresh-capture-fingerprint".to_owned(),
		);
		captured_metadata.insert(
			CAPTURE_PROJECT_GENERATION_METADATA_KEY.to_owned(),
			"fresh-project-generation".to_owned(),
		);
		extract_snapshot_with_metadata(snapshot, "Game".to_owned(), captured_metadata, &captured).unwrap();
		assert_ne!(fs::read(&resolved).unwrap(), fs::read(&captured).unwrap());

		let staged = StagedCompiledCapture {
			root: directory.join("unused-stage-root"),
			artifact: captured.clone(),
			active_artifact: resolved.clone(),
			receipt: validated_artifact_receipt(&captured).unwrap(),
			studio_artifact: None,
			studio_receipt: None,
			studio_excluded: HashSet::new(),
			cleanup_on_drop: AtomicBool::new(true),
		};
		assert!(
			staged.is_noop().unwrap(),
			"capture-only attestation must not rewrite a staged semantic resolution"
		);
		drop(staged);
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn semantic_merge_reports_divergent_field_and_leaves_current_untouched() {
		let directory = temp("merge-conflict");
		let root = Ref::new();
		let child = Ref::new();
		let base = directory.join("base.carbon");
		let current = directory.join("current.carbon");
		let other = directory.join("other.carbon");
		let property = |value: &str| UstrMap::from_iter([(Ustr::from("NameTag"), Variant::String(value.to_owned()))]);
		write(&base, fixture(root, child, property("base")));
		write(&current, fixture(root, child, property("current")));
		write(&other, fixture(root, child, property("other")));
		let before = fs::read(&current).unwrap();
		let MergeOutcome::Conflicted(conflicts) = merge_artifacts(&base, &current, &other).unwrap() else {
			panic!("divergent property edit unexpectedly merged")
		};
		assert_eq!(conflicts.len(), 1);
		assert_eq!(conflicts[0].field, "property.NameTag");
		assert_eq!(fs::read(&current).unwrap(), before);
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn semantic_merge_combines_rename_and_reparent() {
		let directory = temp("merge-topology");
		let root = Ref::new();
		let left = Ref::new();
		let right = Ref::new();
		let child = Ref::new();
		let make = |name: &str, parent: Ref| {
			let node = Snapshot::new().with_id(child).with_name(name).with_class("Folder");
			Snapshot::new()
				.with_id(root)
				.with_name("Game")
				.with_class("DataModel")
				.with_children(vec![
					Snapshot::new()
						.with_id(left)
						.with_name("Left")
						.with_class("Folder")
						.with_children((parent == left).then_some(node.clone()).into_iter().collect()),
					Snapshot::new()
						.with_id(right)
						.with_name("Right")
						.with_class("Folder")
						.with_children((parent == right).then_some(node).into_iter().collect()),
				])
		};
		let base = directory.join("base.carbon");
		let current = directory.join("current.carbon");
		let other = directory.join("other.carbon");
		write(&base, make("Child", left));
		write(&current, make("Renamed", left));
		write(&other, make("Child", right));
		assert!(matches!(
			merge_artifacts(&base, &current, &other).unwrap(),
			MergeOutcome::Merged(_)
		));
		let merged = load_tree(&current).unwrap();
		let child = merged.tree.get_instance(child).unwrap();
		assert_eq!(child.name, "Renamed");
		assert_eq!(child.parent(), right);
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn semantic_merge_rejects_delete_versus_edit() {
		let directory = temp("merge-delete-edit");
		let root = Ref::new();
		let child = Ref::new();
		let base = directory.join("base.carbon");
		let current = directory.join("current.carbon");
		let other = directory.join("other.carbon");
		write(&base, fixture(root, child, UstrMap::default()));
		write(
			&current,
			Snapshot::new().with_id(root).with_name("Game").with_class("DataModel"),
		);
		write(
			&other,
			fixture(
				root,
				child,
				UstrMap::from_iter([(Ustr::from("Archivable"), Variant::Bool(false))]),
			),
		);
		let MergeOutcome::Conflicted(conflicts) = merge_artifacts(&base, &current, &other).unwrap() else {
			panic!("delete-versus-edit unexpectedly merged")
		};
		assert_eq!(conflicts.len(), 1);
		assert_eq!(conflicts[0].field, "existence");
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn semantic_merge_compares_base_external_values_without_the_retired_blob() {
		let directory = temp("merge-external-base");
		let root = Ref::new();
		let child = Ref::new();
		let base_source = "a".repeat(BLOB_THRESHOLD + 1);
		let next_source = "b".repeat(BLOB_THRESHOLD + 1);
		for (name, source) in [
			("base", &base_source),
			("current", &next_source),
			("other", &next_source),
		] {
			write(
				&directory.join(name).join("state.carbon"),
				fixture(
					root,
					child,
					UstrMap::from_iter([(Ustr::from("Source"), Variant::String(source.clone()))]),
				),
			);
		}
		let merge = directory.join("merge");
		fs::create_dir_all(merge.join("blobs")).unwrap();
		for name in ["base", "current", "other"] {
			fs::copy(
				directory.join(name).join("state.carbon"),
				merge.join(format!("{name}.carbon")),
			)
			.unwrap();
		}
		for entry in fs::read_dir(directory.join("current/blobs")).unwrap() {
			let entry = entry.unwrap();
			fs::copy(entry.path(), merge.join("blobs").join(entry.file_name())).unwrap();
		}
		assert!(matches!(
			merge_artifacts(
				&merge.join("base.carbon"),
				&merge.join("current.carbon"),
				&merge.join("other.carbon"),
			)
			.unwrap(),
			MergeOutcome::Merged(_)
		));
		assert_eq!(
			load_tree(&merge.join("current.carbon"))
				.unwrap()
				.tree
				.get_instance(child)
				.unwrap()
				.properties[&Ustr::from("Source")],
			Variant::String(next_source)
		);
		fs::remove_dir_all(directory).unwrap();
	}
}
