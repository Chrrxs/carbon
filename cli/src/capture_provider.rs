//! Native manifest-capture provider seam and RML adapter.
//!
//! Policy, validation, identity reconciliation, and commit stay outside this
//! module. A provider owns only the exclusive native snapshot lease and its
//! bounded raw artifacts.

use anyhow::{bail, ensure, Context, Result};
use bytes::{BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
	borrow::Cow,
	io::{Cursor, Read, Write},
	time::Duration,
};

use crate::privileged_bridge::Bridge;
use rbx_dom_weak::{
	types::{Attributes, Ref},
	Ustr,
};

const ENVELOPE_MAGIC: &[u8; 9] = b"CARBONCP4";
pub const CAPTURE_ENVELOPE_VERSION: u16 = 10;
const FLAG_AUTHORITATIVE_IDENTITIES: u16 = 1;
const MAX_CAPTURE_NODES: usize = 20_000_000;
const NO_PARENT: u32 = u32::MAX;
const NULL_REFERENCE: u32 = u32::MAX;
const MAPPED_REFERENCE: u32 = u32::MAX - 1;
const SYNTHETIC_NODE: u32 = u32::MAX;
const IDENTITY_REMAP_CHUNK_PAIRS: usize = 4096;
/// Bump whenever capture normalization, canonical artifact semantics, or the
/// interpretation of fingerprinted native evidence changes.
pub(crate) const CAPTURE_SEMANTICS_VERSION: u32 = 4;
pub(crate) const CAPTURE_TRANSIENT_ATTRIBUTE_NAMES: &[&str] = &[
	"__StudioWorktree_CarbonEndpoint",
	"__StudioWorktree_CarbonProject",
	"__StudioWorktree_CarbonGeneration",
	"__StudioWorktree_CarbonManifestId",
	"__StudioWorktree_Identity",
	"__StudioWorktree_Session",
	"__MCPPlaceId",
];
pub(crate) const CAPTURE_HIERARCHY_FLAG_SERIALIZED: u32 = 1 << 0;
pub(crate) const CAPTURE_HIERARCHY_FLAG_SERVICE_SHELL: u32 = 1 << 1;
pub(crate) const CAPTURE_HIERARCHY_FLAG_DEFAULT_HYDRATED_SERVICE: u32 = 1 << 2;
const CAPTURE_HIERARCHY_FLAGS: u32 = CAPTURE_HIERARCHY_FLAG_SERIALIZED
	| CAPTURE_HIERARCHY_FLAG_SERVICE_SHELL
	| CAPTURE_HIERARCHY_FLAG_DEFAULT_HYDRATED_SERVICE;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureRequest {
	pub capture_id: String,
	pub studio_session_id: String,
	pub instance_id: String,
	pub engine_generation: u64,
	pub source_generation: String,
	pub managed_contract_id: String,
	pub reflection_schema_hash: String,
	pub manifest_identities_authoritative: bool,
	pub allow_page_reuse: bool,
	pub mapped_root_source_ids: Vec<String>,
	pub shell_classes: Vec<CaptureShellClassRequest>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureShellClassRequest {
	pub class_name: String,
	pub properties: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestIdentityRemap {
	pub captured_id: Ref,
	pub manifest_id: Ref,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CaptureLeaseStatus {
	pub lease_id: String,
	pub capture_id: String,
	pub state: String,
	pub cancel_requested: bool,
	pub serializer_settled: bool,
	pub model_bytes: Option<u64>,
	pub envelope_bytes: Option<u64>,
	pub total_chunks: Option<u32>,
	#[serde(default)]
	pub completed_chunks: u32,
	#[serde(default)]
	pub serialized_bytes: u64,
	#[serde(default)]
	pub committed_model_bytes: u64,
	pub digest_algorithm: String,
	pub model_digest: Option<String>,
	pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureLeaseDeleteResult {
	pub status: CaptureLeaseStatus,
	pub released: bool,
}

/// External snapshot seam used by the Rust capture coordinator.
pub trait CaptureProvider: Send + Sync {
	fn start(&self, request: &CaptureRequest) -> Result<CaptureLeaseStatus>;
	fn status(&self, lease_id: &str) -> Result<CaptureLeaseStatus>;
	fn copy_envelope(&self, lease_id: &str, output: &mut dyn Write) -> Result<u64>;
	fn copy_payload(&self, lease_id: &str, output: &mut dyn Write) -> Result<u64>;
	fn acknowledge(&self, lease_id: &str) -> Result<()>;
	fn supports_progressive_payload(&self) -> bool {
		false
	}
	fn copy_payload_range(&self, _lease_id: &str, _offset: u64, _length: u64, _output: &mut dyn Write) -> Result<u64> {
		bail!("capture provider does not support progressive payload ranges")
	}
	fn cancel(&self, lease_id: &str) -> Result<CaptureLeaseDeleteResult>;
	fn release(&self, lease_id: &str) -> Result<CaptureLeaseDeleteResult>;
}

pub struct RmlCaptureProvider {
	bridge: Bridge,
}

pub(crate) struct CapturePayloadSpool<W: Write> {
	output: W,
	hasher: Sha256,
	bytes: u64,
}

impl<W: Write> CapturePayloadSpool<W> {
	pub(crate) fn new(output: W) -> Self {
		Self {
			output,
			hasher: Sha256::new(),
			bytes: 0,
		}
	}

	#[cfg(test)]
	fn bytes(&self) -> u64 {
		self.bytes
	}

	pub(crate) fn finish(mut self, envelope: &CaptureEnvelope) -> Result<u64> {
		self.output.flush()?;
		ensure!(
			self.bytes == envelope.model_bytes,
			"capture payload length does not match its envelope"
		);
		let digest: [u8; 32] = self.hasher.finalize().into();
		ensure!(
			digest == envelope.model_digest,
			"capture payload digest does not match its envelope"
		);
		Ok(self.bytes)
	}
}

impl<W: Write> Write for CapturePayloadSpool<W> {
	fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
		let written = self.output.write(buffer)?;
		self.hasher.update(&buffer[..written]);
		self.bytes = self
			.bytes
			.checked_add(written as u64)
			.ok_or_else(|| std::io::Error::other("capture payload length overflow"))?;
		Ok(written)
	}

	fn flush(&mut self) -> std::io::Result<()> {
		self.output.flush()
	}
}

impl RmlCaptureProvider {
	pub fn new(bridge: Bridge) -> Self {
		Self { bridge }
	}

	pub fn discover(bridge_id: &str) -> Result<Self> {
		Ok(Self::new(Bridge::discover(bridge_id)?))
	}

	pub(crate) fn bridge(&self) -> &Bridge {
		&self.bridge
	}

	pub(crate) fn finalize_manifest_identities(
		&self,
		capture_id: &str,
		mappings: &[ManifestIdentityRemap],
	) -> Result<()> {
		ensure!(
			capture_id.len() == 32 && capture_id.chars().all(|character| character.is_ascii_hexdigit()),
			"capture identity is not a 128-bit hexadecimal value"
		);
		let total = mappings.len();
		if total == 0 {
			bail!("manifest identity remap is empty");
		}
		for (chunk_index, chunk) in mappings.chunks(IDENTITY_REMAP_CHUNK_PAIRS).enumerate() {
			let offset = chunk_index * IDENTITY_REMAP_CHUNK_PAIRS;
			let body = encode_identity_remap_chunk(capture_id, total, offset, chunk)?;
			let response: serde_json::Value = self.bridge.post_bytes("v1/manifest-identities/remap-chunk", body)?;
			let complete = offset + chunk.len() == total;
			ensure!(
				response.get("complete").and_then(serde_json::Value::as_bool) == Some(complete),
				"RML acknowledged the wrong manifest identity remap offset"
			);
			if complete {
				ensure!(
					response.get("authoritative").and_then(serde_json::Value::as_bool) == Some(true),
					"RML did not make the captured manifest identities authoritative"
				);
			}
		}
		Ok(())
	}
}

fn encode_identity_remap_chunk(
	capture_id: &str,
	total: usize,
	offset: usize,
	mappings: &[ManifestIdentityRemap],
) -> Result<bytes::Bytes> {
	ensure!(!mappings.is_empty(), "manifest identity remap chunk is empty");
	ensure!(
		mappings.len() <= IDENTITY_REMAP_CHUNK_PAIRS,
		"manifest identity remap chunk exceeds the pair limit"
	);
	ensure!(
		offset + mappings.len() <= total,
		"manifest identity remap chunk range is invalid"
	);
	let mut body = BytesMut::with_capacity(32 + mappings.len() * 32);
	body.extend_from_slice(&identity_bytes(capture_id)?);
	body.put_u64_le(total as u64);
	body.put_u64_le(offset as u64);
	for mapping in mappings {
		body.extend_from_slice(&identity_bytes(&mapping.captured_id.to_string())?);
		body.extend_from_slice(&identity_bytes(&mapping.manifest_id.to_string())?);
	}
	Ok(body.freeze())
}

fn identity_bytes(value: &str) -> Result<[u8; 16]> {
	let parsed = u128::from_str_radix(value, 16).context("identity is not 128-bit hexadecimal")?;
	ensure!(parsed != 0, "identity is zero");
	Ok(parsed.to_be_bytes())
}

impl CaptureProvider for RmlCaptureProvider {
	fn start(&self, request: &CaptureRequest) -> Result<CaptureLeaseStatus> {
		self.bridge.post("v2/capture-leases", request)
	}

	fn status(&self, lease_id: &str) -> Result<CaptureLeaseStatus> {
		self.bridge.get(&format!("v2/capture-leases/{lease_id}"))
	}

	fn copy_envelope(&self, lease_id: &str, output: &mut dyn Write) -> Result<u64> {
		self.bridge
			.get_to_writer(&format!("v2/capture-leases/{lease_id}/envelope"), output)
	}

	fn copy_payload(&self, lease_id: &str, output: &mut dyn Write) -> Result<u64> {
		self.bridge
			.get_to_writer(&format!("v2/capture-leases/{lease_id}/payload"), output)
	}

	fn acknowledge(&self, lease_id: &str) -> Result<()> {
		let response: serde_json::Value = self
			.bridge
			.post(&format!("v2/capture-leases/{lease_id}/commit"), &serde_json::json!({}))?;
		ensure!(
			response.get("acknowledged").and_then(serde_json::Value::as_bool) == Some(true),
			"RML did not acknowledge the committed capture page table"
		);
		Ok(())
	}

	fn supports_progressive_payload(&self) -> bool {
		true
	}

	fn copy_payload_range(&self, lease_id: &str, offset: u64, length: u64, output: &mut dyn Write) -> Result<u64> {
		self.bridge
			.get_range_to_writer(&format!("v2/capture-leases/{lease_id}/payload"), offset, length, output)
	}

	fn cancel(&self, lease_id: &str) -> Result<CaptureLeaseDeleteResult> {
		self.bridge.delete(&format!("v2/capture-leases/{lease_id}"))
	}

	fn release(&self, lease_id: &str) -> Result<CaptureLeaseDeleteResult> {
		self.bridge.delete(&format!("v2/capture-leases/{lease_id}"))
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureHierarchyNode {
	pub parent_ordinal: u32,
	pub class_name: Ustr,
	pub name: String,
	pub flags: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureServiceRoot {
	pub hierarchy_ordinal: u32,
	pub class_name: String,
	pub name: String,
	pub first_serialized_root: u32,
	pub serialized_root_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureMappedBinding {
	pub source_id: String,
	pub hierarchy_ordinal: u32,
	pub parent_ordinal: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureReferenceTarget {
	Null,
	Ordinal(u32),
	Mapped(Ref),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureExternalReference {
	pub owner_ordinal: u32,
	pub property: String,
	pub target: CaptureReferenceTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureShellProperty {
	pub owner_ordinal: u32,
	pub property: String,
	pub type_name: String,
	pub value: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureShellCarrier {
	pub owner_ordinal: u32,
	pub property: String,
	pub type_name: String,
	pub carrier_class: String,
	pub serialized_root_index: u32,
}

fn semantic_shell_property_value(property: &CaptureShellProperty) -> Cow<'_, [u8]> {
	if property.property != "Attributes" {
		return Cow::Borrowed(&property.value);
	}
	let Ok(mut attributes) = Attributes::from_reader(property.value.as_slice()) else {
		return Cow::Borrowed(&property.value);
	};
	for name in CAPTURE_TRANSIENT_ATTRIBUTE_NAMES {
		attributes.remove(*name);
	}
	let mut normalized = Vec::new();
	if attributes.to_writer(&mut normalized).is_err() {
		return Cow::Borrowed(&property.value);
	}
	Cow::Owned(normalized)
}

#[derive(Clone, Debug)]
pub struct CaptureEnvelope {
	pub capture_id: String,
	pub engine_generation: u64,
	pub hierarchy_sequence_before: u64,
	pub hierarchy_sequence_after: u64,
	pub change_sequence_before: u64,
	pub change_sequence_after: u64,
	pub model_bytes: u64,
	pub model_digest: [u8; 32],
	pub studio_session_id: String,
	pub instance_id: String,
	pub managed_contract_id: String,
	pub reflection_schema_hash: String,
	pub source_generation: String,
	pub digest_algorithm: String,
	pub manifest_identities_authoritative: bool,
	/// One nonzero 128-bit Carbon identity per native hierarchy ordinal.
	pub manifest_identities: Vec<Ref>,
	pub nodes: Vec<CaptureHierarchyNode>,
	pub roots: Vec<CaptureServiceRoot>,
	pub mapped_bindings: Vec<CaptureMappedBinding>,
	pub external_references: Vec<CaptureExternalReference>,
	pub shell_properties: Vec<CaptureShellProperty>,
	pub shell_carriers: Vec<CaptureShellCarrier>,
	/// One native hierarchy ordinal per RBXM serializer input root, in input
	/// order. Synthetic property-carrier roots use the sentinel value. This is
	/// never zipped to RBXM INST rows, which are grouped by class.
	pub serialized_root_ordinals: Vec<u32>,
}

impl CaptureEnvelope {
	pub fn decode(bytes: &[u8]) -> Result<Self> {
		let mut reader = Cursor::new(bytes);
		let mut magic = [0; ENVELOPE_MAGIC.len()];
		reader.read_exact(&mut magic)?;
		ensure!(&magic == ENVELOPE_MAGIC, "capture envelope magic is invalid");
		ensure!(
			read_u16(&mut reader)? == CAPTURE_ENVELOPE_VERSION,
			"unsupported capture envelope version"
		);
		let flags = read_u16(&mut reader)?;
		ensure!(
			flags & !FLAG_AUTHORITATIVE_IDENTITIES == 0,
			"capture envelope has unsupported flags"
		);
		let manifest_identities_authoritative = flags & FLAG_AUTHORITATIVE_IDENTITIES != 0;
		let capture_id = read_identity(&mut reader)?;
		let engine_generation = read_i64(&mut reader)?;
		ensure!(engine_generation >= 0, "capture engine generation is negative");
		let hierarchy_sequence_before = nonnegative(read_i64(&mut reader)?, "hierarchy sequence")?;
		let hierarchy_sequence_after = nonnegative(read_i64(&mut reader)?, "hierarchy sequence")?;
		let change_sequence_before = nonnegative(read_i64(&mut reader)?, "change sequence")?;
		let change_sequence_after = nonnegative(read_i64(&mut reader)?, "change sequence")?;
		let model_bytes = read_u64(&mut reader)?;
		let mut model_digest = [0; 32];
		reader.read_exact(&mut model_digest)?;

		let string_count = bounded_count(read_u32(&mut reader)?, "string")?;
		let node_count = bounded_count(read_u32(&mut reader)?, "node")?;
		let root_count = bounded_count(read_u32(&mut reader)?, "root")?;
		let mapped_count = bounded_count(read_u32(&mut reader)?, "mapped binding")?;
		let reference_count = bounded_count(read_u32(&mut reader)?, "external reference")?;
		let shell_property_count = bounded_count(read_u32(&mut reader)?, "shell property")?;
		let shell_carrier_count = bounded_count(read_u32(&mut reader)?, "shell carrier")?;
		let serialized_count = bounded_count(read_u32(&mut reader)?, "serialized root ordinal")?;
		ensure!(node_count > 0, "capture envelope hierarchy is empty");

		let studio_session_index = read_u32(&mut reader)?;
		let instance_index = read_u32(&mut reader)?;
		let managed_contract_index = read_u32(&mut reader)?;
		let reflection_schema_index = read_u32(&mut reader)?;
		let source_generation_index = read_u32(&mut reader)?;
		let digest_algorithm_index = read_u32(&mut reader)?;

		let mut strings = Vec::with_capacity(string_count);
		for _ in 0..string_count {
			let length = usize::try_from(read_u32(&mut reader)?)?;
			ensure!(length <= bytes.len(), "capture envelope string exceeds the artifact");
			let mut value = vec![0; length];
			reader.read_exact(&mut value)?;
			strings.push(String::from_utf8(value).context("capture envelope string is not UTF-8")?);
		}
		let string = |index: u32| -> Result<String> {
			strings
				.get(usize::try_from(index)?)
				.cloned()
				.context("capture envelope string index is invalid")
		};

		let mut nodes = Vec::with_capacity(node_count);
		for ordinal in 0..node_count {
			let parent_ordinal = read_u32(&mut reader)?;
			if ordinal == 0 {
				ensure!(parent_ordinal == NO_PARENT, "capture hierarchy root has a parent");
			} else {
				ensure!(
					usize::try_from(parent_ordinal)? < ordinal,
					"capture hierarchy parent is not earlier"
				);
			}
			let class_name = Ustr::from(&string(read_u32(&mut reader)?)?);
			let name_length = usize::try_from(read_u32(&mut reader)?)?;
			ensure!(
				name_length <= bytes.len().saturating_sub(reader.position() as usize),
				"capture hierarchy name exceeds the artifact"
			);
			let mut name = vec![0; name_length];
			reader.read_exact(&mut name)?;
			let flags = read_u32(&mut reader)?;
			ensure!(
				flags & !CAPTURE_HIERARCHY_FLAGS == 0,
				"capture hierarchy node has unsupported flags"
			);
			ensure!(
				flags & CAPTURE_HIERARCHY_FLAG_DEFAULT_HYDRATED_SERVICE == 0
					|| flags & CAPTURE_HIERARCHY_FLAG_SERVICE_SHELL != 0,
				"capture default-hydrated node is not a service shell"
			);
			nodes.push(CaptureHierarchyNode {
				parent_ordinal,
				class_name,
				name: String::from_utf8(name).context("capture hierarchy name is not UTF-8")?,
				flags,
			});
		}
		let mut manifest_identities = Vec::with_capacity(node_count);
		for _ in 0..node_count {
			let mut identity = [0_u8; 16];
			reader.read_exact(&mut identity)?;
			ensure!(identity != [0; 16], "capture manifest identity is zero");
			manifest_identities.push(Ref::some(u128::from_be_bytes(identity)));
		}
		let mut roots = Vec::with_capacity(root_count);
		for _ in 0..root_count {
			let hierarchy_ordinal = read_u32(&mut reader)?;
			let class_name = string(read_u32(&mut reader)?)?;
			let name = string(read_u32(&mut reader)?)?;
			let first_serialized_root = read_u32(&mut reader)?;
			let serialized_root_count = read_u32(&mut reader)?;
			ensure!(
				usize::try_from(hierarchy_ordinal)? < nodes.len(),
				"capture service ordinal is invalid"
			);
			let end = u64::from(first_serialized_root) + u64::from(serialized_root_count);
			ensure!(
				end <= serialized_count as u64,
				"capture service serialized range is invalid"
			);
			let shell = &nodes[hierarchy_ordinal as usize];
			ensure!(
				shell.class_name == class_name && shell.name == name,
				"capture service identity disagrees with its hierarchy node"
			);
			roots.push(CaptureServiceRoot {
				hierarchy_ordinal,
				class_name,
				name,
				first_serialized_root,
				serialized_root_count,
			});
		}
		let mut mapped_bindings = Vec::with_capacity(mapped_count);
		for _ in 0..mapped_count {
			let source_id = read_identity(&mut reader)?;
			let hierarchy_ordinal = read_u32(&mut reader)?;
			let parent_ordinal = read_u32(&mut reader)?;
			ensure!(
				hierarchy_ordinal == SYNTHETIC_NODE && usize::try_from(parent_ordinal)? < nodes.len(),
				"capture mapped binding graft anchor is invalid"
			);
			mapped_bindings.push(CaptureMappedBinding {
				source_id,
				hierarchy_ordinal,
				parent_ordinal,
			});
		}
		let mut external_references = Vec::with_capacity(reference_count);
		for _ in 0..reference_count {
			let owner_ordinal = read_u32(&mut reader)?;
			let property = string(read_u32(&mut reader)?)?;
			let target_ordinal = read_u32(&mut reader)?;
			ensure!(
				usize::try_from(owner_ordinal)? < nodes.len(),
				"capture external-reference owner ordinal is invalid"
			);
			ensure!(!property.is_empty(), "capture external-reference property is empty");
			let target = if target_ordinal == NULL_REFERENCE {
				CaptureReferenceTarget::Null
			} else if target_ordinal == MAPPED_REFERENCE {
				let target: Ref = read_identity(&mut reader)?
					.parse()
					.context("capture mapped-reference identity is invalid")?;
				ensure!(target.is_some(), "capture mapped-reference identity is zero");
				CaptureReferenceTarget::Mapped(target)
			} else {
				ensure!(
					usize::try_from(target_ordinal)? < nodes.len(),
					"capture external-reference target ordinal is invalid"
				);
				CaptureReferenceTarget::Ordinal(target_ordinal)
			};
			external_references.push(CaptureExternalReference {
				owner_ordinal,
				property,
				target,
			});
		}
		let mut shell_properties = Vec::with_capacity(shell_property_count);
		for _ in 0..shell_property_count {
			let owner_ordinal = read_u32(&mut reader)?;
			let property = string(read_u32(&mut reader)?)?;
			let type_name = string(read_u32(&mut reader)?)?;
			let value_length = usize::try_from(read_u32(&mut reader)?)?;
			ensure!(
				usize::try_from(owner_ordinal)? < nodes.len(),
				"capture shell-property owner ordinal is invalid"
			);
			ensure!(
				!property.is_empty() && !type_name.is_empty(),
				"capture shell-property identity is incomplete"
			);
			ensure!(
				value_length <= bytes.len().saturating_sub(reader.position() as usize),
				"capture shell-property value exceeds the artifact"
			);
			let mut value = vec![0; value_length];
			reader.read_exact(&mut value)?;
			shell_properties.push(CaptureShellProperty {
				owner_ordinal,
				property,
				type_name,
				value,
			});
		}
		let mut shell_carriers = Vec::with_capacity(shell_carrier_count);
		for _ in 0..shell_carrier_count {
			let owner_ordinal = read_u32(&mut reader)?;
			let property = string(read_u32(&mut reader)?)?;
			let type_name = string(read_u32(&mut reader)?)?;
			let carrier_class = string(read_u32(&mut reader)?)?;
			let serialized_root_index = read_u32(&mut reader)?;
			ensure!(
				usize::try_from(owner_ordinal)? < nodes.len(),
				"capture shell-carrier owner ordinal is invalid"
			);
			ensure!(
				!property.is_empty() && !type_name.is_empty() && !carrier_class.is_empty(),
				"capture shell-carrier identity is incomplete"
			);
			ensure!(
				usize::try_from(serialized_root_index)? < serialized_count,
				"capture shell-carrier serialized root is invalid"
			);
			shell_carriers.push(CaptureShellCarrier {
				owner_ordinal,
				property,
				type_name,
				carrier_class,
				serialized_root_index,
			});
		}
		let mut serialized_root_ordinals = Vec::with_capacity(serialized_count);
		for _ in 0..serialized_count {
			let ordinal = read_u32(&mut reader)?;
			ensure!(
				ordinal == SYNTHETIC_NODE || (ordinal != 0 && usize::try_from(ordinal)? < nodes.len()),
				"capture serialized root ordinal is invalid"
			);
			serialized_root_ordinals.push(ordinal);
		}
		let persistent_component_count = serialized_root_ordinals
			.iter()
			.position(|ordinal| *ordinal == SYNTHETIC_NODE)
			.unwrap_or(serialized_root_ordinals.len());
		ensure!(
			serialized_root_ordinals[persistent_component_count..]
				.iter()
				.all(|ordinal| *ordinal == SYNTHETIC_NODE),
			"capture persistent component roots and carriers are not dense ranges"
		);
		let direct_root_count = roots.iter().try_fold(0_usize, |total, root| {
			total
				.checked_add(usize::try_from(root.serialized_root_count)?)
				.context("capture direct root count overflows usize")
		})?;
		ensure!(
			persistent_component_count >= direct_root_count,
			"capture component root prefix omits a direct service root"
		);
		let mut direct_coverage = vec![false; direct_root_count];
		for root in &roots {
			let start = usize::try_from(root.first_serialized_root)?;
			let end = start
				.checked_add(usize::try_from(root.serialized_root_count)?)
				.context("capture service serialized range overflows usize")?;
			ensure!(end <= direct_root_count, "capture service serialized range is invalid");
			for index in start..end {
				ensure!(
					!std::mem::replace(&mut direct_coverage[index], true),
					"capture service serialized ranges overlap"
				);
				let ordinal = usize::try_from(serialized_root_ordinals[index])?;
				ensure!(
					nodes[ordinal].parent_ordinal == root.hierarchy_ordinal,
					"capture direct root is not parented to its service shell"
				);
			}
		}
		ensure!(
			direct_coverage.into_iter().all(|covered| covered),
			"capture service serialized ranges omit a direct root"
		);
		// Hierarchy ordinals are dense, so a byte-per-node uniqueness table is
		// substantially smaller and faster than a million-entry hash table.
		let mut persistent_ordinals = vec![false; nodes.len()];
		for &ordinal in &serialized_root_ordinals[..persistent_component_count] {
			let ordinal = usize::try_from(ordinal)?;
			ensure!(
				ordinal != 0 && ordinal < nodes.len() && !std::mem::replace(&mut persistent_ordinals[ordinal], true),
				"capture persistent component root is invalid or duplicated"
			);
		}
		let mut carrier_root_indexes = shell_carriers
			.iter()
			.map(|carrier| carrier.serialized_root_index)
			.collect::<Vec<_>>();
		carrier_root_indexes.sort_unstable();
		carrier_root_indexes.dedup();
		for (offset, &index) in carrier_root_indexes.iter().enumerate() {
			ensure!(
				usize::try_from(index)? == persistent_component_count + offset,
				"capture shell carrier roots are not a dense suffix"
			);
		}
		ensure!(
			serialized_count == persistent_component_count + carrier_root_indexes.len(),
			"capture serialized root ordinal count is invalid"
		);
		ensure!(
			reader.position() == bytes.len() as u64,
			"capture envelope has trailing bytes"
		);

		let digest_algorithm = string(digest_algorithm_index)?;
		ensure!(
			digest_algorithm == "sha256",
			"unsupported capture payload digest algorithm"
		);
		Ok(Self {
			capture_id,
			engine_generation: engine_generation as u64,
			hierarchy_sequence_before,
			hierarchy_sequence_after,
			change_sequence_before,
			change_sequence_after,
			model_bytes,
			model_digest,
			studio_session_id: string(studio_session_index)?,
			instance_id: string(instance_index)?,
			managed_contract_id: string(managed_contract_index)?,
			reflection_schema_hash: string(reflection_schema_index)?,
			source_generation: string(source_generation_index)?,
			digest_algorithm,
			manifest_identities_authoritative,
			manifest_identities,
			nodes,
			roots,
			mapped_bindings,
			external_references,
			shell_properties,
			shell_carriers,
			serialized_root_ordinals,
		})
	}

	/// Stable evidence for an exact repeated native snapshot. Volatile lease,
	/// Studio-route, contract-identity, epoch, and composed-source-generation
	/// fields are excluded. The latter would be self-referential because the
	/// fingerprint itself is stored in the manifest; mapped bindings and their
	/// source identities still cover the ownership seam.
	pub(crate) fn semantic_fingerprint(&self) -> String {
		fn bytes(hasher: &mut blake3::Hasher, value: &[u8]) {
			hasher.update(&(value.len() as u64).to_le_bytes());
			hasher.update(value);
		}
		fn text(hasher: &mut blake3::Hasher, value: &str) {
			bytes(hasher, value.as_bytes());
		}
		fn referent(hasher: &mut blake3::Hasher, value: Ref) {
			struct HasherWriter<'a>(&'a mut blake3::Hasher);
			impl std::fmt::Write for HasherWriter<'_> {
				fn write_str(&mut self, value: &str) -> std::fmt::Result {
					self.0.update(value.as_bytes());
					Ok(())
				}
			}
			// Ref exposes no numeric representation. Stream its canonical 32-byte
			// hexadecimal display directly into BLAKE3 without a million temporary
			// String allocations.
			hasher.update(&32_u64.to_le_bytes());
			std::fmt::write(&mut HasherWriter(hasher), format_args!("{value}")).expect("hash writer is infallible");
		}
		fn count(hasher: &mut blake3::Hasher, value: usize) {
			hasher.update(&(value as u64).to_le_bytes());
		}

		let mut hasher = blake3::Hasher::new();
		hasher.update(b"carbon-capture-semantic-fingerprint-v2\0");
		hasher.update(&CAPTURE_SEMANTICS_VERSION.to_le_bytes());
		hasher.update(&CAPTURE_ENVELOPE_VERSION.to_le_bytes());
		hasher.update(&self.model_bytes.to_le_bytes());
		hasher.update(&self.model_digest);
		text(&mut hasher, &self.reflection_schema_hash);
		text(&mut hasher, &self.digest_algorithm);
		hasher.update(&[u8::from(self.manifest_identities_authoritative)]);

		count(&mut hasher, self.nodes.len());
		for node in &self.nodes {
			hasher.update(&node.parent_ordinal.to_le_bytes());
			text(&mut hasher, node.class_name.as_str());
			text(&mut hasher, &node.name);
			hasher.update(&node.flags.to_le_bytes());
		}
		count(&mut hasher, self.manifest_identities.len());
		for &identity in &self.manifest_identities {
			referent(&mut hasher, identity);
		}
		count(&mut hasher, self.roots.len());
		for root in &self.roots {
			hasher.update(&root.hierarchy_ordinal.to_le_bytes());
			text(&mut hasher, &root.class_name);
			text(&mut hasher, &root.name);
			hasher.update(&root.first_serialized_root.to_le_bytes());
			hasher.update(&root.serialized_root_count.to_le_bytes());
		}
		count(&mut hasher, self.mapped_bindings.len());
		for binding in &self.mapped_bindings {
			text(&mut hasher, &binding.source_id);
			hasher.update(&binding.hierarchy_ordinal.to_le_bytes());
			hasher.update(&binding.parent_ordinal.to_le_bytes());
		}
		count(&mut hasher, self.external_references.len());
		for reference in &self.external_references {
			hasher.update(&reference.owner_ordinal.to_le_bytes());
			text(&mut hasher, &reference.property);
			match reference.target {
				CaptureReferenceTarget::Null => {
					hasher.update(&[0]);
				}
				CaptureReferenceTarget::Ordinal(ordinal) => {
					hasher.update(&[1]);
					hasher.update(&ordinal.to_le_bytes());
				}
				CaptureReferenceTarget::Mapped(target) => {
					hasher.update(&[2]);
					referent(&mut hasher, target);
				}
			};
		}
		count(&mut hasher, self.shell_properties.len());
		for property in &self.shell_properties {
			hasher.update(&property.owner_ordinal.to_le_bytes());
			text(&mut hasher, &property.property);
			text(&mut hasher, &property.type_name);
			bytes(&mut hasher, &semantic_shell_property_value(property));
		}
		count(&mut hasher, self.shell_carriers.len());
		for carrier in &self.shell_carriers {
			hasher.update(&carrier.owner_ordinal.to_le_bytes());
			text(&mut hasher, &carrier.property);
			text(&mut hasher, &carrier.type_name);
			text(&mut hasher, &carrier.carrier_class);
			hasher.update(&carrier.serialized_root_index.to_le_bytes());
		}
		count(&mut hasher, self.serialized_root_ordinals.len());
		for ordinal in &self.serialized_root_ordinals {
			hasher.update(&ordinal.to_le_bytes());
		}
		hasher.finalize().to_hex().to_string()
	}

	pub fn validate_request(&self, request: &CaptureRequest) -> Result<()> {
		ensure!(
			self.capture_id == request.capture_id,
			"capture envelope request identity mismatch"
		);
		ensure!(
			self.engine_generation == request.engine_generation,
			"capture envelope engine generation mismatch"
		);
		ensure!(
			self.studio_session_id == request.studio_session_id,
			"capture envelope Studio session mismatch"
		);
		ensure!(
			self.instance_id == request.instance_id,
			"capture envelope Studio instance mismatch"
		);
		ensure!(
			self.source_generation == request.source_generation,
			"capture envelope source generation mismatch"
		);
		ensure!(
			self.managed_contract_id == request.managed_contract_id,
			"capture envelope managed contract mismatch"
		);
		ensure!(
			self.reflection_schema_hash == request.reflection_schema_hash,
			"capture envelope reflection schema mismatch"
		);
		ensure!(
			self.manifest_identities_authoritative == request.manifest_identities_authoritative,
			"capture envelope manifest identity mode mismatch"
		);
		let requested_mapped_roots = request
			.mapped_root_source_ids
			.iter()
			.collect::<std::collections::HashSet<_>>();
		ensure!(
			requested_mapped_roots.len() == request.mapped_root_source_ids.len(),
			"capture request repeats a mapped root identity"
		);
		let captured_mapped_roots = self
			.mapped_bindings
			.iter()
			.map(|binding| &binding.source_id)
			.collect::<std::collections::HashSet<_>>();
		ensure!(
			captured_mapped_roots.len() == self.mapped_bindings.len()
				&& captured_mapped_roots == requested_mapped_roots,
			"capture envelope mapped roots do not match the exact request"
		);
		ensure!(
			self.hierarchy_sequence_before == self.hierarchy_sequence_after
				&& self.change_sequence_before == self.change_sequence_after,
			"Studio changed while the native capture snapshot was serialized"
		);
		Ok(())
	}

	pub fn validate_payload<R: Read>(&self, mut payload: R) -> Result<u64> {
		let mut hasher = Sha256::new();
		let copied = std::io::copy(&mut payload, &mut hasher)?;
		ensure!(
			copied == self.model_bytes,
			"capture payload length does not match its envelope"
		);
		let digest: [u8; 32] = hasher.finalize().into();
		ensure!(
			digest == self.model_digest,
			"capture payload digest does not match its envelope"
		);
		Ok(copied)
	}
}

pub fn wait_until_ready(
	provider: &dyn CaptureProvider,
	lease_id: &str,
	timeout: Duration,
	cancelled: impl FnMut() -> bool,
) -> Result<CaptureLeaseStatus> {
	wait_until_ready_with_progress(provider, lease_id, timeout, cancelled, |_| Ok(()))
}

pub fn wait_until_ready_with_progress(
	provider: &dyn CaptureProvider,
	lease_id: &str,
	timeout: Duration,
	mut cancelled: impl FnMut() -> bool,
	mut progress: impl FnMut(&CaptureLeaseStatus) -> Result<()>,
) -> Result<CaptureLeaseStatus> {
	let deadline = std::time::Instant::now() + timeout;
	loop {
		if cancelled() {
			let result = provider.cancel(lease_id)?;
			bail!(
				"Capture Manifest cancellation was accepted; native serializer state is {} and RML owns cleanup until settlement",
				result.status.state
			);
		}
		let status = provider.status(lease_id)?;
		progress(&status)?;
		match status.state.as_str() {
			"ready" => return Ok(status),
			"failed" => {
				let message = status.error.clone().unwrap_or_else(|| "unknown error".into());
				// The coordinator owns the single terminal delete. Releasing here and
				// again from its common failure path turns the useful snapshot error
				// into a misleading 404 from the second delete.
				bail!("native snapshot failed: {message}")
			}
			"cancelled" => {
				bail!("native snapshot was cancelled")
			}
			"preparing" | "serializing" | "spooling" | "cancelling" => {}
			state => bail!("native snapshot returned unknown lease state '{state}'"),
		}
		if std::time::Instant::now() >= deadline {
			let cancellation = provider.cancel(lease_id)?;
			bail!(
				"native snapshot lease timed out; cancellation was accepted in state {} and RML owns cleanup until settlement",
				cancellation.status.state
			)
		}
		std::thread::sleep(Duration::from_millis(25));
	}
}

fn bounded_count(value: u32, kind: &str) -> Result<usize> {
	let value = usize::try_from(value)?;
	ensure!(
		value <= MAX_CAPTURE_NODES,
		"capture envelope {kind} count exceeds the protocol limit"
	);
	Ok(value)
}

fn nonnegative(value: i64, kind: &str) -> Result<u64> {
	ensure!(value >= 0, "capture {kind} is negative");
	Ok(value as u64)
}

fn read_identity(reader: &mut Cursor<&[u8]>) -> Result<String> {
	let mut bytes = [0; 16];
	reader.read_exact(&mut bytes)?;
	Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn read_u16(reader: &mut Cursor<&[u8]>) -> Result<u16> {
	let mut bytes = [0; 2];
	reader.read_exact(&mut bytes)?;
	Ok(u16::from_le_bytes(bytes))
}

fn read_u32(reader: &mut Cursor<&[u8]>) -> Result<u32> {
	let mut bytes = [0; 4];
	reader.read_exact(&mut bytes)?;
	Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut Cursor<&[u8]>) -> Result<u64> {
	let mut bytes = [0; 8];
	reader.read_exact(&mut bytes)?;
	Ok(u64::from_le_bytes(bytes))
}

fn read_i64(reader: &mut Cursor<&[u8]>) -> Result<i64> {
	let mut bytes = [0; 8];
	reader.read_exact(&mut bytes)?;
	Ok(i64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::atomic::AtomicUsize;

	fn fixture(model: &[u8], mapped_reference: bool) -> Vec<u8> {
		let strings = [
			"session",
			"instance",
			"contract",
			"schema",
			"generation",
			"sha256",
			"DataModel",
			"game",
			"Workspace",
			"Gravity",
			"Float64",
		];
		let mut output = Vec::new();
		output.extend_from_slice(ENVELOPE_MAGIC);
		output.extend_from_slice(&CAPTURE_ENVELOPE_VERSION.to_le_bytes());
		output.extend_from_slice(&FLAG_AUTHORITATIVE_IDENTITIES.to_le_bytes());
		output.extend_from_slice(&[0; 16]);
		for value in [7_i64, 11, 11, 13, 13] {
			output.extend_from_slice(&value.to_le_bytes());
		}
		output.extend_from_slice(&(model.len() as u64).to_le_bytes());
		output.extend_from_slice(&Sha256::digest(model));
		for value in [strings.len() as u32, 2, 1, 1, 1, 1, 0, 0] {
			output.extend_from_slice(&value.to_le_bytes());
		}
		for value in [0_u32, 1, 2, 3, 4, 5] {
			output.extend_from_slice(&value.to_le_bytes());
		}
		for value in strings {
			output.extend_from_slice(&(value.len() as u32).to_le_bytes());
			output.extend_from_slice(value.as_bytes());
		}
		for (parent, class, name, flags) in [(NO_PARENT, 6_u32, "game", 1_u32), (0_u32, 8_u32, "Workspace", 1_u32)] {
			output.extend_from_slice(&parent.to_le_bytes());
			output.extend_from_slice(&class.to_le_bytes());
			output.extend_from_slice(&(name.len() as u32).to_le_bytes());
			output.extend_from_slice(name.as_bytes());
			output.extend_from_slice(&flags.to_le_bytes());
		}
		output.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
		output.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
		for value in [1_u32, 8, 8, 0, 0] {
			output.extend_from_slice(&value.to_le_bytes());
		}
		output.extend_from_slice(&[
			0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89,
		]);
		for value in [SYNTHETIC_NODE, 1] {
			output.extend_from_slice(&value.to_le_bytes());
		}
		for value in [
			1_u32,
			9,
			if mapped_reference {
				MAPPED_REFERENCE
			} else {
				NULL_REFERENCE
			},
		] {
			output.extend_from_slice(&value.to_le_bytes());
		}
		if mapped_reference {
			output.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]);
		}
		for value in [1_u32, 9, 10, 8] {
			output.extend_from_slice(&value.to_le_bytes());
		}
		output.extend_from_slice(&196.2_f64.to_le_bytes());
		output
	}

	#[test]
	fn envelope_decode_attests_request_and_payload() {
		let model = b"native rbxm";
		let envelope = CaptureEnvelope::decode(&fixture(model, false)).unwrap();
		envelope
			.validate_request(&CaptureRequest {
				capture_id: "00000000000000000000000000000000".to_owned(),
				studio_session_id: "session".to_owned(),
				instance_id: "instance".to_owned(),
				engine_generation: 7,
				source_generation: "generation".to_owned(),
				managed_contract_id: "contract".to_owned(),
				reflection_schema_hash: "schema".to_owned(),
				manifest_identities_authoritative: true,
				allow_page_reuse: true,
				mapped_root_source_ids: vec!["abcdef0123456789abcdef0123456789".to_owned()],
				shell_classes: vec![CaptureShellClassRequest {
					class_name: "Workspace".to_owned(),
					properties: vec!["Gravity".to_owned()],
				}],
			})
			.unwrap();
		assert_eq!(envelope.validate_payload(model.as_slice()).unwrap(), model.len() as u64);
		assert_eq!(envelope.roots[0].class_name, "Workspace");
		assert_eq!(envelope.mapped_bindings[0].parent_ordinal, 1);
		assert_eq!(envelope.external_references[0].target, CaptureReferenceTarget::Null);
		assert_eq!(envelope.shell_properties[0].value, 196.2_f64.to_le_bytes());
		assert!(envelope.manifest_identities_authoritative);
		assert_eq!(
			envelope.manifest_identities[1].to_string(),
			"00000000000000000000000000000002"
		);
	}

	#[test]
	fn envelope_decodes_a_mapped_reference_identity() {
		let envelope = CaptureEnvelope::decode(&fixture(b"native rbxm", true)).unwrap();
		assert_eq!(
			envelope.external_references[0].target,
			CaptureReferenceTarget::Mapped(Ref::some(3))
		);
	}

	#[test]
	fn capture_request_serializes_exact_mapped_root_source_ids() {
		let request = CaptureRequest {
			capture_id: "00000000000000000000000000000000".to_owned(),
			studio_session_id: "session".to_owned(),
			instance_id: "instance".to_owned(),
			engine_generation: 7,
			source_generation: "generation".to_owned(),
			managed_contract_id: "contract".to_owned(),
			reflection_schema_hash: "schema".to_owned(),
			manifest_identities_authoritative: true,
			allow_page_reuse: true,
			mapped_root_source_ids: vec!["abcdef0123456789abcdef0123456789".to_owned()],
			shell_classes: Vec::new(),
		};
		let value = serde_json::to_value(request).unwrap();
		assert_eq!(
			value["mappedRootSourceIds"],
			serde_json::json!(["abcdef0123456789abcdef0123456789"])
		);
		assert_eq!(value["manifestIdentitiesAuthoritative"], true);
		assert_eq!(value["allowPageReuse"], true);
	}

	#[test]
	fn envelope_rejects_payload_corruption_and_trailing_bytes() {
		let model = b"native rbxm";
		let envelope = CaptureEnvelope::decode(&fixture(model, false)).unwrap();
		assert!(envelope.validate_payload(b"native rbxl".as_slice()).is_err());
		let mut encoded = fixture(model, false);
		encoded.push(0);
		assert!(CaptureEnvelope::decode(&encoded).is_err());
	}

	#[test]
	fn payload_spool_hashes_streamed_ranges_without_a_validation_reread() {
		let model = b"native rbxm";
		let envelope = CaptureEnvelope::decode(&fixture(model, false)).unwrap();
		let mut spool = CapturePayloadSpool::new(Vec::new());
		spool.write_all(&model[..4]).unwrap();
		spool.write_all(&model[4..]).unwrap();
		assert_eq!(spool.bytes(), model.len() as u64);
		assert_eq!(spool.finish(&envelope).unwrap(), model.len() as u64);

		let mut corrupted = CapturePayloadSpool::new(Vec::new());
		corrupted.write_all(b"native rbxl").unwrap();
		assert!(corrupted.finish(&envelope).is_err());
	}

	#[test]
	fn semantic_fingerprint_ignores_lease_fields_but_covers_manifest_evidence() {
		let original = CaptureEnvelope::decode(&fixture(b"native rbxm", false)).unwrap();
		let mut envelope = original.clone();
		let fingerprint = envelope.semantic_fingerprint();

		envelope.capture_id = "f".repeat(32);
		envelope.studio_session_id = "another-session".to_owned();
		envelope.instance_id = "another-instance".to_owned();
		envelope.managed_contract_id = "another-contract".to_owned();
		envelope.source_generation = "another-generation".to_owned();
		envelope.engine_generation += 1;
		envelope.hierarchy_sequence_before += 10;
		envelope.hierarchy_sequence_after += 10;
		envelope.change_sequence_before += 10;
		envelope.change_sequence_after += 10;
		assert_eq!(fingerprint, envelope.semantic_fingerprint());

		macro_rules! assert_evidence_changes_fingerprint {
			($mutation:expr) => {{
				let mut changed = original.clone();
				$mutation(&mut changed);
				assert_ne!(fingerprint, changed.semantic_fingerprint());
			}};
		}
		assert_evidence_changes_fingerprint!(|value: &mut CaptureEnvelope| value.model_digest[0] ^= 1);
		assert_evidence_changes_fingerprint!(|value: &mut CaptureEnvelope| value.reflection_schema_hash.push('2'));
		assert_evidence_changes_fingerprint!(
			|value: &mut CaptureEnvelope| value.manifest_identities_authoritative = false
		);
		assert_evidence_changes_fingerprint!(|value: &mut CaptureEnvelope| value.nodes[1].name = "Renamed".to_owned());
		assert_evidence_changes_fingerprint!(|value: &mut CaptureEnvelope| value.manifest_identities[1] = Ref::some(99));
		assert_evidence_changes_fingerprint!(|value: &mut CaptureEnvelope| value.roots[0].name.push('2'));
		assert_evidence_changes_fingerprint!(|value: &mut CaptureEnvelope| value.mapped_bindings[0]
			.source_id
			.push('2'));
		assert_evidence_changes_fingerprint!(
			|value: &mut CaptureEnvelope| value.external_references[0].target = CaptureReferenceTarget::Ordinal(0)
		);
		assert_evidence_changes_fingerprint!(|value: &mut CaptureEnvelope| value.shell_properties[0].value[0] ^= 1);
		assert_evidence_changes_fingerprint!(|value: &mut CaptureEnvelope| value.shell_carriers.push(
			CaptureShellCarrier {
				owner_ordinal: 1,
				property: "Value".to_owned(),
				type_name: "Object".to_owned(),
				carrier_class: "ObjectValue".to_owned(),
				serialized_root_index: 0,
			}
		));
		assert_evidence_changes_fingerprint!(|value: &mut CaptureEnvelope| value.serialized_root_ordinals.push(1));
	}

	#[test]
	fn semantic_fingerprint_ignores_transport_attributes_but_covers_authored_attributes() {
		fn encoded_attributes(project: &str, authored: &str) -> Vec<u8> {
			let attributes = rbx_dom_weak::types::Attributes::new()
				.with("__StudioWorktree_CarbonProject", project)
				.with("__StudioWorktree_CarbonGeneration", format!("generation-{project}"))
				.with("__MCPPlaceId", format!("mcp-{project}"))
				.with("Authored", authored);
			let mut bytes = Vec::new();
			attributes.to_writer(&mut bytes).unwrap();
			bytes
		}

		let mut original = CaptureEnvelope::decode(&fixture(b"native rbxm", false)).unwrap();
		original.shell_properties = vec![CaptureShellProperty {
			owner_ordinal: 1,
			property: "Attributes".to_owned(),
			type_name: "BinaryString".to_owned(),
			value: encoded_attributes("before", "keep"),
		}];
		let fingerprint = original.semantic_fingerprint();

		let mut transport_changed = original.clone();
		transport_changed.shell_properties[0].value = encoded_attributes("after", "keep");
		assert_eq!(fingerprint, transport_changed.semantic_fingerprint());

		let mut authored_changed = original;
		authored_changed.shell_properties[0].value = encoded_attributes("before", "changed");
		assert_ne!(fingerprint, authored_changed.semantic_fingerprint());
	}

	#[test]
	#[ignore = "million-node semantic-fingerprint acceptance probe"]
	fn million_identity_semantic_fingerprint_is_linear() {
		const NODE_COUNT: usize = 1_000_000;
		let mut envelope = CaptureEnvelope::decode(&fixture(b"native rbxm", false)).unwrap();
		envelope.nodes = (0..NODE_COUNT)
			.map(|ordinal| CaptureHierarchyNode {
				parent_ordinal: if ordinal == 0 { NO_PARENT } else { 0 },
				class_name: Ustr::from(if ordinal == 0 { "DataModel" } else { "Folder" }),
				name: if ordinal == 0 {
					"Game".to_owned()
				} else {
					"Repeated".to_owned()
				},
				flags: if ordinal == 0 {
					CAPTURE_HIERARCHY_FLAG_SERVICE_SHELL
				} else {
					CAPTURE_HIERARCHY_FLAG_SERIALIZED
				},
			})
			.collect();
		envelope.manifest_identities = (1..=NODE_COUNT).map(|ordinal| Ref::some(ordinal as u128)).collect();
		envelope.roots.clear();
		envelope.mapped_bindings.clear();
		envelope.external_references.clear();
		envelope.shell_properties.clear();
		envelope.shell_carriers.clear();
		envelope.serialized_root_ordinals.clear();

		let started = std::time::Instant::now();
		let fingerprint = envelope.semantic_fingerprint();
		let elapsed = started.elapsed();

		assert_eq!(fingerprint.len(), 64);
		assert!(
			elapsed < Duration::from_secs(5),
			"million-node semantic fingerprint took {:.3}s",
			elapsed.as_secs_f64()
		);
	}

	struct AutoCleanupProvider {
		cancellations: AtomicUsize,
		releases: AtomicUsize,
		state: &'static str,
	}

	impl AutoCleanupProvider {
		fn status(&self, state: &str, settled: bool) -> CaptureLeaseStatus {
			CaptureLeaseStatus {
				lease_id: "lease".to_owned(),
				capture_id: "0".repeat(32),
				state: state.to_owned(),
				cancel_requested: state != "serializing",
				serializer_settled: settled,
				model_bytes: None,
				envelope_bytes: None,
				total_chunks: Some(1),
				completed_chunks: u32::from(settled),
				serialized_bytes: u64::from(settled),
				committed_model_bytes: u64::from(settled),
				digest_algorithm: "sha256".to_owned(),
				model_digest: None,
				error: (state == "failed").then(|| "serializer generation changed".to_owned()),
			}
		}
	}

	impl CaptureProvider for AutoCleanupProvider {
		fn start(&self, _: &CaptureRequest) -> Result<CaptureLeaseStatus> {
			Ok(self.status("serializing", false))
		}

		fn status(&self, _: &str) -> Result<CaptureLeaseStatus> {
			Ok(self.status(self.state, self.state != "serializing"))
		}

		fn copy_envelope(&self, _: &str, _: &mut dyn Write) -> Result<u64> {
			unreachable!()
		}

		fn copy_payload(&self, _: &str, _: &mut dyn Write) -> Result<u64> {
			unreachable!()
		}

		fn acknowledge(&self, _: &str) -> Result<()> {
			unreachable!()
		}

		fn cancel(&self, _: &str) -> Result<CaptureLeaseDeleteResult> {
			self.cancellations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
			Ok(CaptureLeaseDeleteResult {
				status: self.status("cancelling", false),
				released: false,
			})
		}

		fn release(&self, _: &str) -> Result<CaptureLeaseDeleteResult> {
			self.releases.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
			Ok(CaptureLeaseDeleteResult {
				status: self.status("cancelled", true),
				released: true,
			})
		}
	}

	#[test]
	fn cancellation_is_lease_owned_and_needs_no_second_client_delete() {
		let provider = AutoCleanupProvider {
			cancellations: AtomicUsize::new(0),
			releases: AtomicUsize::new(0),
			state: "serializing",
		};
		let error = wait_until_ready(&provider, "lease", Duration::from_secs(1), || true)
			.unwrap_err()
			.to_string();
		assert!(error.contains("RML owns cleanup until settlement"));
		assert_eq!(provider.cancellations.load(std::sync::atomic::Ordering::Relaxed), 1);
		assert_eq!(provider.releases.load(std::sync::atomic::Ordering::Relaxed), 0);
	}

	#[test]
	fn failed_wait_preserves_the_primary_error_for_coordinator_cleanup() {
		let provider = AutoCleanupProvider {
			cancellations: AtomicUsize::new(0),
			releases: AtomicUsize::new(0),
			state: "failed",
		};
		let error = wait_until_ready(&provider, "lease", Duration::from_secs(1), || false)
			.unwrap_err()
			.to_string();
		assert!(error.contains("serializer generation changed"));
		assert_eq!(provider.cancellations.load(std::sync::atomic::Ordering::Relaxed), 0);
		assert_eq!(provider.releases.load(std::sync::atomic::Ordering::Relaxed), 0);
	}

	#[test]
	fn adoption_remap_chunk_is_bounded_packed_and_big_endian_for_identities() {
		let mappings = [
			ManifestIdentityRemap {
				captured_id: Ref::some(1),
				manifest_id: Ref::some(2),
			},
			ManifestIdentityRemap {
				captured_id: Ref::some(3),
				manifest_id: Ref::some(4),
			},
		];
		let body = encode_identity_remap_chunk("00112233445566778899aabbccddeeff", 10, 4, &mappings).unwrap();

		assert_eq!(body.len(), 32 + mappings.len() * 32);
		assert_eq!(
			&body[..16],
			&u128::from_str_radix("00112233445566778899aabbccddeeff", 16)
				.unwrap()
				.to_be_bytes()
		);
		assert_eq!(u64::from_le_bytes(body[16..24].try_into().unwrap()), 10);
		assert_eq!(u64::from_le_bytes(body[24..32].try_into().unwrap()), 4);
		assert_eq!(&body[32..48], &1_u128.to_be_bytes());
		assert_eq!(&body[48..64], &2_u128.to_be_bytes());
	}
}
