use anyhow::{Context, Result};
use rbx_binary::{DecodeSink, Deserializer, InstanceSource};
use rbx_dom_weak::{
	types::{CFrame, ContentType, Ref, Variant},
	Ustr, UstrMap,
};
use rbx_reflection::ClassTag;
use serde::Serialize;
use std::{
	collections::{BTreeSet, HashMap, HashSet, VecDeque},
	fs::File,
	io::BufReader,
	path::{Path, PathBuf},
};

use crate::{artifact_store::MANIFEST_IDENTITY_ATTRIBUTE, util};

const NON_GAMEPLAY_STUDIO_ROOTS: &[&str] = &[
	"CoreGui",
	"MemStorageService",
	"PluginConnectionService",
	"PluginGuiService",
	"VisualizationModeService",
];

const WORKTREE_ATTRIBUTES: &[&str] = &[
	"__StudioWorktree_CarbonEndpoint",
	"__StudioWorktree_CarbonProject",
	"__StudioWorktree_CarbonGeneration",
	"__StudioWorktree_Identity",
	"__StudioWorktree_Session",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Impact {
	PotentialGameplay,
	NonGameplay,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Difference {
	pub impact: Impact,
	pub kind: String,
	pub path: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub property: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub before: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub after: Option<String>,
	pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffReport {
	pub before: PathBuf,
	pub after: PathBuf,
	pub before_instances: usize,
	pub after_instances: usize,
	pub matched_instances: usize,
	pub added_instances: usize,
	pub removed_instances: usize,
	pub blocking_differences: usize,
	pub non_gameplay_differences: usize,
	pub details_truncated: bool,
	pub differences: Vec<Difference>,
}

impl DiffReport {
	pub fn has_blockers(&self) -> bool {
		self.blocking_differences > 0
	}
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
struct Fingerprint {
	bytes: [u8; 32],
	count: u32,
}

impl Fingerprint {
	fn add(&mut self, digest: [u8; 32]) {
		for (target, byte) in self.bytes.iter_mut().zip(digest) {
			*target ^= byte;
		}
		self.count = self.count.wrapping_add(1);
	}

	fn merge(&mut self, other: Self) {
		for (target, byte) in self.bytes.iter_mut().zip(other.bytes) {
			*target ^= byte;
		}
		self.count = self.count.wrapping_add(other.count);
	}
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct NodeFingerprints {
	local: Fingerprint,
	references: Fingerprint,
	subtree: Fingerprint,
	structural_locator: [u8; 32],
	gameplay: Fingerprint,
	full: Fingerprint,
}

struct Node {
	referent: Ref,
	parent: Option<usize>,
	children: Vec<usize>,
	class: Ustr,
	name: String,
	locator: [u8; 32],
	root_child: usize,
}

struct PlaceStructure {
	path: PathBuf,
	nodes: Vec<Node>,
	by_ref: HashMap<Ref, usize>,
	nodes_by_class: HashMap<Ustr, Vec<usize>>,
	metadata: std::collections::BTreeMap<String, String>,
	managed_worktree: bool,
	studio_runtime_roots: HashSet<usize>,
}

impl PlaceStructure {
	fn load(path: &Path) -> Result<Self> {
		let source = Deserializer::new(util::get_reflection_database()).deserialize_structure(BufReader::new(
			File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
		))?;
		let metadata = source.metadata().clone();
		let root_ref = source.root_ref();
		let root = source.get_by_ref(root_ref).context("binary root is missing")?;
		let root_locator = *blake3::hash(b"carbon-place-diff-root-v1").as_bytes();
		let mut nodes = vec![Node {
			referent: root_ref,
			parent: None,
			children: Vec::new(),
			class: root.class,
			name: root.name.to_owned(),
			locator: root_locator,
			root_child: 0,
		}];
		let mut by_ref = HashMap::from([(root_ref, 0)]);
		let mut queue = VecDeque::from([0]);
		while let Some(parent_index) = queue.pop_front() {
			let parent_ref = nodes[parent_index].referent;
			let parent = source.get_by_ref(parent_ref).context("binary parent is missing")?;
			let mut occurrences: HashMap<(Ustr, &str), u32> = HashMap::new();
			let mut children = Vec::with_capacity(parent.children.len());
			for child_ref in parent.children.iter().copied() {
				let child = source.get_by_ref(child_ref).context("binary child is missing")?;
				let occurrence = occurrences.entry((child.class, child.name)).or_default();
				let mut hasher = blake3::Hasher::new();
				hasher.update(b"carbon-place-diff-locator-v1\0");
				hasher.update(&nodes[parent_index].locator);
				hasher.update(child.class.as_str().as_bytes());
				hasher.update(&[0]);
				hasher.update(child.name.as_bytes());
				hasher.update(&occurrence.to_le_bytes());
				let locator = *hasher.finalize().as_bytes();
				let index = nodes.len();
				let root_child = if parent_index == 0 {
					index
				} else {
					nodes[parent_index].root_child
				};
				nodes.push(Node {
					referent: child_ref,
					parent: Some(parent_index),
					children: Vec::new(),
					class: child.class,
					name: child.name.to_owned(),
					locator,
					root_child,
				});
				by_ref.insert(child_ref, index);
				children.push(index);
				queue.push_back(index);
				*occurrence += 1;
			}
			nodes[parent_index].children = children;
		}
		let mut nodes_by_class: HashMap<Ustr, Vec<usize>> = HashMap::new();
		for (index, node) in nodes.iter().enumerate() {
			nodes_by_class.entry(node.class).or_default().push(index);
		}
		Ok(Self {
			path: path.to_owned(),
			nodes,
			by_ref,
			nodes_by_class,
			metadata,
			managed_worktree: false,
			studio_runtime_roots: HashSet::new(),
		})
	}

	fn display_path(&self, mut index: usize) -> String {
		if index == 0 {
			return "game".to_owned();
		}
		let mut segments = Vec::new();
		while index != 0 {
			let node = &self.nodes[index];
			segments.push(format!("{}[{:?}]", node.class, node.name));
			index = node.parent.unwrap_or(0);
		}
		segments.reverse();
		format!("game/{}", segments.join("/"))
	}

	fn subtree_size(&self, root: usize) -> usize {
		let mut count = 0;
		let mut stack = vec![root];
		while let Some(index) = stack.pop() {
			count += 1;
			stack.extend(self.nodes[index].children.iter().copied());
		}
		count
	}

	fn is_studio_runtime_subtree(&self, index: usize) -> bool {
		let root_index = self.nodes[index].root_child;
		let root = &self.nodes[root_index];
		if root.name == root.class.as_str() && NON_GAMEPLAY_STUDIO_ROOTS.contains(&root.class.as_str()) {
			return true;
		}
		let mut current = Some(index);
		while let Some(candidate) = current {
			if self.studio_runtime_roots.contains(&candidate) {
				return true;
			}
			current = self.nodes[candidate].parent;
		}
		false
	}
}

struct FingerprintResult {
	nodes: Vec<NodeFingerprints>,
	properties_by_class: HashMap<Ustr, HashSet<Ustr>>,
	unique_ids: Fingerprint,
	edit_cameras: HashSet<usize>,
	references: Vec<ReferenceProperty>,
	studio_runtime_candidates: HashMap<usize, StudioRuntimeCandidate>,
}

struct StudioRuntimeCandidate {
	kind: StudioRuntimeCandidateKind,
	saw_archivable_false: bool,
	saw_manifest_identity: bool,
	all_other_properties_are_defaults: bool,
}

#[derive(Clone, Copy)]
enum StudioRuntimeCandidateKind {
	FilteredSelection,
	InsertionHash,
}

#[derive(Clone, Copy)]
enum ReferenceKind {
	Ref,
	ContentObject,
}

#[derive(Clone, Copy)]
struct ReferenceProperty {
	owner: usize,
	name: Ustr,
	target: Ref,
	kind: ReferenceKind,
}

struct FingerprintSink<'a> {
	structure: &'a PlaceStructure,
	result: &'a mut FingerprintResult,
	deferred_attributes: &'a mut Vec<(usize, Ustr, Variant)>,
}

impl DecodeSink for FingerprintSink<'_> {
	fn property(&mut self, referent: Ref, name: Ustr, value: Variant) -> std::result::Result<(), String> {
		let Some(&index) = self.structure.by_ref.get(&referent) else {
			return Ok(());
		};
		let node = &self.structure.nodes[index];
		self.result
			.properties_by_class
			.entry(node.class)
			.or_default()
			.insert(name);
		if matches!(value, Variant::Attributes(_)) {
			self.deferred_attributes.push((index, name, value));
			return Ok(());
		}
		add_property_fingerprint(self.structure, self.result, index, name, &value);
		Ok(())
	}
}

fn add_property_fingerprint(
	structure: &PlaceStructure,
	result: &mut FingerprintResult,
	index: usize,
	name: Ustr,
	value: &Variant,
) {
	let node = &structure.nodes[index];
	if let Some(candidate) = result.studio_runtime_candidates.get_mut(&index) {
		if name.as_str() == "Archivable" {
			candidate.saw_archivable_false = matches!(value, Variant::Bool(false));
		} else if !is_unique_property(name.as_str(), value) {
			let is_exact_name = name.as_str() == "Name"
				&& match (candidate.kind, value) {
					(StudioRuntimeCandidateKind::FilteredSelection, Variant::String(value)) => {
						value == "FilteredSelection"
					}
					(StudioRuntimeCandidateKind::InsertionHash, Variant::String(value)) => value == "InsertionHash",
					_ => false,
				};
			let is_insertion_hash = matches!(candidate.kind, StudioRuntimeCandidateKind::InsertionHash)
				&& name.as_str() == "Value"
				&& matches!(value, Variant::String(value) if is_braced_uuid(value));
			if name.as_str() == "Attributes" {
				candidate.saw_manifest_identity = matches!(
					value,
					Variant::Attributes(attributes)
						if attributes.get(MANIFEST_IDENTITY_ATTRIBUTE).is_some()
				);
			}
			let is_engine_default = util::get_reflection_database()
				.classes
				.get("Instance")
				.and_then(|class| util::get_reflection_database().find_default_property(class, name.as_str()))
				== Some(value);
			candidate.all_other_properties_are_defaults &= is_exact_name
				|| is_insertion_hash
				|| is_engine_default
				|| is_forward_instance_default(name.as_str(), value);
		}
	}
	if is_unique_property(name.as_str(), value) {
		result.unique_ids.add(entry_digest(
			name.as_str(),
			&canonical_value(value, structure, false, None, None),
		));
		return;
	}
	if node.class.as_str() == "Workspace" && name.as_str() == "CurrentCamera" {
		if let Variant::Ref(target) = value {
			if let Some(&target_index) = structure.by_ref.get(target) {
				result.edit_cameras.insert(target_index);
			}
		}
	}
	let reference = match value {
		Variant::Ref(target) => Some((*target, ReferenceKind::Ref)),
		Variant::Content(content) => match content.value() {
			ContentType::Object(target) => Some((*target, ReferenceKind::ContentObject)),
			_ => None,
		},
		_ => None,
	};
	if let Some((target, kind)) = reference {
		result.references.push(ReferenceProperty {
			owner: index,
			name,
			target,
			kind,
		});
	}
	let full = canonical_value(value, structure, false, None, None);
	let gameplay = canonical_value(value, structure, true, Some(node), None);
	let full_digest = entry_digest(name.as_str(), &full);
	let gameplay_digest = entry_digest(name.as_str(), &gameplay);
	let fingerprints = &mut result.nodes[index];
	fingerprints.full.add(full_digest);
	fingerprints.gameplay.add(gameplay_digest);
	if !is_reference_value(value) {
		fingerprints.local.add(gameplay_digest);
	}
}

fn fingerprint(structure: &mut PlaceStructure) -> Result<FingerprintResult> {
	let studio_runtime_candidates = structure
		.nodes
		.iter()
		.enumerate()
		.filter_map(|(index, node)| {
			let kind = if index == node.root_child
				&& node.class.as_str() == "Instance"
				&& node.name == "FilteredSelection"
				&& node.children.is_empty()
			{
				Some(StudioRuntimeCandidateKind::FilteredSelection)
			} else if node.class.as_str() == "StringValue"
				&& node.name == "InsertionHash"
				&& node.children.is_empty()
				&& node
					.parent
					.is_some_and(|parent| structure.nodes[parent].class.as_str() == "InsertService")
			{
				Some(StudioRuntimeCandidateKind::InsertionHash)
			} else {
				None
			};
			kind.map(|kind| {
				(
					index,
					StudioRuntimeCandidate {
						kind,
						saw_archivable_false: false,
						saw_manifest_identity: false,
						all_other_properties_are_defaults: true,
					},
				)
			})
		})
		.collect();
	let mut result = FingerprintResult {
		nodes: vec![NodeFingerprints::default(); structure.nodes.len()],
		properties_by_class: HashMap::new(),
		unique_ids: Fingerprint::default(),
		edit_cameras: HashSet::new(),
		references: Vec::new(),
		studio_runtime_candidates,
	};
	let mut deferred_attributes = Vec::new();
	Deserializer::new(util::get_reflection_database()).deserialize_properties_with_sink(
		BufReader::new(File::open(&structure.path)?),
		&mut FingerprintSink {
			structure,
			result: &mut result,
			deferred_attributes: &mut deferred_attributes,
		},
	)?;
	structure.managed_worktree = deferred_attributes.iter().any(|(index, _, value)| {
		structure.nodes[*index].class.as_str() == "Workspace"
			&& matches!(value, Variant::Attributes(attributes) if has_managed_worktree_contract(attributes))
	});
	for (index, name, value) in deferred_attributes {
		add_property_fingerprint(structure, &mut result, index, name, &value);
	}
	structure.studio_runtime_roots = std::mem::take(&mut result.studio_runtime_candidates)
		.into_iter()
		.filter_map(|(index, candidate)| {
			let proven_runtime = !candidate.saw_manifest_identity
				&& candidate.all_other_properties_are_defaults
				&& match candidate.kind {
					StudioRuntimeCandidateKind::FilteredSelection => {
						candidate.saw_archivable_false || structure.managed_worktree
					}
					StudioRuntimeCandidateKind::InsertionHash => structure.managed_worktree,
				};
			proven_runtime.then_some(index)
		})
		.collect();
	Ok(result)
}

fn is_braced_uuid(value: &str) -> bool {
	value
		.strip_prefix('{')
		.and_then(|value| value.strip_suffix('}'))
		.is_some_and(|value| uuid::Uuid::parse_str(value).is_ok())
}

fn is_forward_instance_default(name: &str, value: &Variant) -> bool {
	match (name, value) {
		("SourceAssetId", Variant::Int64(-1)) | ("Sandboxed" | "Disabled", Variant::Bool(false)) => true,
		("Capabilities", Variant::SecurityCapabilities(value)) => value.bits() == 0,
		("LinkedSource", Variant::ContentId(value)) => value.as_str().is_empty(),
		("RunContext", Variant::Enum(value)) => value.to_u32() == 0,
		("Tags", Variant::Tags(value)) => value.is_empty(),
		("Attributes", Variant::Attributes(value)) => value.is_empty(),
		_ => false,
	}
}

fn fill_missing_defaults(
	structure: &PlaceStructure,
	result: &mut FingerprintResult,
	other_properties: &HashMap<Ustr, HashSet<Ustr>>,
) {
	let database = util::get_reflection_database();
	for (class, properties) in other_properties {
		let Some(nodes) = structure.nodes_by_class.get(class) else {
			continue;
		};
		let Some(descriptor) = database.classes.get(class.as_str()) else {
			continue;
		};
		for property in properties {
			if result
				.properties_by_class
				.get(class)
				.is_some_and(|own| own.contains(property))
				|| absence_changes_engine_load(class.as_str(), property.as_str())
			{
				continue;
			}
			let Some(value) = database.find_default_property(descriptor, property.as_str()) else {
				continue;
			};
			if is_unique_property(property.as_str(), value) {
				continue;
			}
			for &index in nodes {
				let node = &structure.nodes[index];
				let full = canonical_value(value, structure, false, None, None);
				let gameplay = canonical_value(value, structure, true, Some(node), None);
				let full_digest = entry_digest(property.as_str(), &full);
				let gameplay_digest = entry_digest(property.as_str(), &gameplay);
				result.nodes[index].full.add(full_digest);
				result.nodes[index].gameplay.add(gameplay_digest);
				if !is_reference_value(value) {
					result.nodes[index].local.add(gameplay_digest);
				}
			}
		}
	}
}

fn fill_subtree_fingerprints(structure: &PlaceStructure, result: &mut FingerprintResult, include_references: bool) {
	for index in (0..structure.nodes.len()).rev() {
		let mut subtree = result.nodes[index].local;
		if include_references {
			subtree.merge(result.nodes[index].references);
		}
		for &child_index in &structure.nodes[index].children {
			let child = &structure.nodes[child_index];
			let child_subtree = result.nodes[child_index].subtree;
			let mut hasher = blake3::Hasher::new();
			hasher.update(b"carbon-place-diff-subtree-v1\0");
			hasher.update(child.class.as_str().as_bytes());
			hasher.update(&[0]);
			hasher.update(child.name.as_bytes());
			hasher.update(&[0]);
			hasher.update(&child_subtree.count.to_le_bytes());
			hasher.update(&child_subtree.bytes);
			subtree.add(*hasher.finalize().as_bytes());
		}
		result.nodes[index].subtree = subtree;
	}
}

fn fill_reference_fingerprints(
	structure: &PlaceStructure,
	result: &mut FingerprintResult,
	pair_ids: Option<&[Option<usize>]>,
) {
	for node in &mut result.nodes {
		node.references = Fingerprint::default();
	}
	for reference in &result.references {
		let prefix = match reference.kind {
			ReferenceKind::Ref => b"Ref".as_slice(),
			ReferenceKind::ContentObject => b"ContentObject".as_slice(),
		};
		let mut value = prefix.to_vec();
		if reference.target.is_none() {
			value.extend_from_slice(b":none");
		} else if let Some(&target_index) = structure.by_ref.get(&reference.target) {
			if let Some(pair) = pair_ids.and_then(|pairs| pairs[target_index]) {
				value.extend_from_slice(b":pair:");
				value.extend_from_slice(&(pair as u64).to_le_bytes());
			} else {
				value.extend_from_slice(b":target:");
				value.extend_from_slice(&result.nodes[target_index].structural_locator);
			}
		} else {
			value.extend_from_slice(b":external");
		}
		result.nodes[reference.owner]
			.references
			.add(entry_digest(reference.name.as_str(), &value));
	}
}

fn fill_structural_locators(structure: &PlaceStructure, result: &mut FingerprintResult) {
	for index in 0..structure.nodes.len() {
		let node = &structure.nodes[index];
		let mut hasher = blake3::Hasher::new();
		hasher.update(b"carbon-place-diff-structural-locator-v1\0");
		if let Some(parent) = node.parent {
			hasher.update(&result.nodes[parent].structural_locator);
		}
		hasher.update(node.class.as_str().as_bytes());
		hasher.update(&[0]);
		hasher.update(node.name.as_bytes());
		hasher.update(&[0]);
		hasher.update(&result.nodes[index].subtree.count.to_le_bytes());
		hasher.update(&result.nodes[index].subtree.bytes);
		result.nodes[index].structural_locator = *hasher.finalize().as_bytes();
	}
}

fn entry_digest(name: &str, value: &[u8]) -> [u8; 32] {
	let mut hasher = blake3::Hasher::new();
	hasher.update(name.as_bytes());
	hasher.update(&[0]);
	hasher.update(value);
	*hasher.finalize().as_bytes()
}

fn canonical_value(
	value: &Variant,
	structure: &PlaceStructure,
	gameplay: bool,
	node: Option<&Node>,
	pair_ids: Option<&[Option<usize>]>,
) -> Vec<u8> {
	match value {
		Variant::Ref(target) => canonical_reference(*target, structure, pair_ids, b"Ref"),
		Variant::Content(content) => match content.value() {
			ContentType::Object(target) => canonical_reference(*target, structure, pair_ids, b"ContentObject"),
			_ => rmp_serde::to_vec(value).unwrap_or_else(|_| format!("{value:?}").into_bytes()),
		},
		Variant::Tags(tags) if gameplay => {
			let mut tags: Vec<_> = tags.iter().collect();
			tags.sort_unstable();
			rmp_serde::to_vec(&tags).unwrap_or_default()
		}
		Variant::Attributes(attributes) if gameplay => {
			let filtered: std::collections::BTreeMap<_, _> = attributes
				.iter()
				.filter(|(name, value)| !is_transport_attribute(structure, node, name, value, attributes))
				.collect();
			rmp_serde::to_vec(&filtered).unwrap_or_default()
		}
		_ => rmp_serde::to_vec(value).unwrap_or_else(|_| format!("{value:?}").into_bytes()),
	}
}

fn canonical_reference(
	target: Ref,
	structure: &PlaceStructure,
	pair_ids: Option<&[Option<usize>]>,
	prefix: &[u8],
) -> Vec<u8> {
	let mut result = prefix.to_vec();
	if target.is_none() {
		result.extend_from_slice(b":none");
	} else if let Some(&index) = structure.by_ref.get(&target) {
		if let Some(pair) = pair_ids.and_then(|pairs| pairs[index]) {
			result.extend_from_slice(b":pair:");
			result.extend_from_slice(&(pair as u64).to_le_bytes());
		} else {
			result.extend_from_slice(b":locator:");
			result.extend_from_slice(&structure.nodes[index].locator);
		}
	} else {
		result.extend_from_slice(b":external");
	}
	result
}

fn is_reference_value(value: &Variant) -> bool {
	matches!(value, Variant::Ref(_))
		|| matches!(value, Variant::Content(content) if matches!(content.value(), ContentType::Object(_)))
}

fn is_unique_property(name: &str, value: &Variant) -> bool {
	matches!(name, "UniqueId" | "HistoryId") || matches!(value, Variant::UniqueId(_))
}

fn absence_changes_engine_load(class: &str, property: &str) -> bool {
	(class == "Lighting" && property == "LightingStyle") || (class == "Model" && property == "NeedsPivotMigration")
}

fn attribute_text(value: &Variant) -> Option<&str> {
	match value {
		Variant::String(value) => Some(value),
		Variant::BinaryString(value) => std::str::from_utf8(value.as_ref()).ok(),
		_ => None,
	}
}

fn has_managed_worktree_contract(attributes: &rbx_dom_weak::types::Attributes) -> bool {
	if !WORKTREE_ATTRIBUTES.iter().all(|name| {
		attributes
			.get(*name)
			.and_then(attribute_text)
			.is_some_and(|value| !value.is_empty())
	}) {
		return false;
	}
	let endpoint = attributes
		.get("__StudioWorktree_CarbonEndpoint")
		.and_then(attribute_text)
		.unwrap_or_default();
	let generation = attributes
		.get("__StudioWorktree_CarbonGeneration")
		.and_then(attribute_text)
		.unwrap_or_default();
	(endpoint.starts_with("http://127.0.0.1:") || endpoint.starts_with("http://localhost:"))
		&& generation.len() == 64
		&& generation.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_transport_attribute(
	structure: &PlaceStructure,
	node: Option<&Node>,
	name: &str,
	value: &Variant,
	attributes: &rbx_dom_weak::types::Attributes,
) -> bool {
	let Some(node) = node else { return false };
	(node.class.as_str() == "Workspace"
		&& WORKTREE_ATTRIBUTES.contains(&name)
		&& has_managed_worktree_contract(attributes))
		|| (structure.managed_worktree
			&& name == MANIFEST_IDENTITY_ATTRIBUTE
			&& attribute_text(value).is_some_and(is_manifest_identity))
		|| (structure.managed_worktree
			&& node.class.as_str() == "ServerStorage"
			&& name == "__MCPPlaceId"
			&& attribute_text(value).is_some_and(|value| uuid::Uuid::parse_str(value).is_ok()))
}

fn is_manifest_identity(value: &str) -> bool {
	value.len() == 32
		&& value.bytes().all(|byte| byte.is_ascii_hexdigit())
		&& value != "00000000000000000000000000000000"
}

#[derive(Clone)]
struct Matching {
	pairs: Vec<(usize, usize)>,
	left_pair: Vec<Option<usize>>,
	right_pair: Vec<Option<usize>>,
	left_unmatched: Vec<usize>,
	right_unmatched: Vec<usize>,
	order_changes: Vec<(usize, usize)>,
}

fn mapping_digest(mapping: &[Option<usize>]) -> [u8; 32] {
	let mut hasher = blake3::Hasher::new();
	hasher.update(b"carbon-place-diff-mapping-v1\0");
	for pair in mapping {
		hasher.update(&pair.unwrap_or(usize::MAX).to_le_bytes());
	}
	*hasher.finalize().as_bytes()
}

fn reference_disagreements(
	matching: &Matching,
	left_fingerprints: &[NodeFingerprints],
	right_fingerprints: &[NodeFingerprints],
) -> usize {
	matching
		.pairs
		.iter()
		.filter(|(left, right)| left_fingerprints[*left].references != right_fingerprints[*right].references)
		.count()
}

fn match_hierarchy(
	left: &PlaceStructure,
	right: &PlaceStructure,
	left_fingerprints: &[NodeFingerprints],
	right_fingerprints: &[NodeFingerprints],
) -> Matching {
	let mut matching = Matching {
		pairs: vec![(0, 0)],
		left_pair: vec![None; left.nodes.len()],
		right_pair: vec![None; right.nodes.len()],
		left_unmatched: Vec::new(),
		right_unmatched: Vec::new(),
		order_changes: Vec::new(),
	};
	matching.left_pair[0] = Some(0);
	matching.right_pair[0] = Some(0);
	let mut queue = VecDeque::from([(0, 0)]);
	while let Some((left_parent, right_parent)) = queue.pop_front() {
		let mut left_children = left.nodes[left_parent].children.clone();
		let mut right_children = right.nodes[right_parent].children.clone();
		left_children.sort_by(|a, b| node_key(&left.nodes[*a]).cmp(&node_key(&left.nodes[*b])));
		right_children.sort_by(|a, b| node_key(&right.nodes[*a]).cmp(&node_key(&right.nodes[*b])));
		let mut left_cursor = 0;
		let mut right_cursor = 0;
		while left_cursor < left_children.len() || right_cursor < right_children.len() {
			match (left_children.get(left_cursor), right_children.get(right_cursor)) {
				(Some(&left_index), Some(&right_index)) => {
					let left_key = node_key(&left.nodes[left_index]);
					let right_key = node_key(&right.nodes[right_index]);
					match left_key.cmp(&right_key) {
						std::cmp::Ordering::Less => {
							matching.left_unmatched.push(left_index);
							left_cursor += 1;
						}
						std::cmp::Ordering::Greater => {
							matching.right_unmatched.push(right_index);
							right_cursor += 1;
						}
						std::cmp::Ordering::Equal => {
							let left_end = group_end(&left_children, left_cursor, left, left_key);
							let right_end = group_end(&right_children, right_cursor, right, right_key);
							match_group(
								&left_children[left_cursor..left_end],
								&right_children[right_cursor..right_end],
								left_fingerprints,
								right_fingerprints,
								&mut matching,
								&mut queue,
							);
							left_cursor = left_end;
							right_cursor = right_end;
						}
					}
				}
				(Some(&index), None) => {
					matching.left_unmatched.push(index);
					left_cursor += 1;
				}
				(None, Some(&index)) => {
					matching.right_unmatched.push(index);
					right_cursor += 1;
				}
				(None, None) => break,
			}
		}
		let left_sequence: Vec<_> = left.nodes[left_parent]
			.children
			.iter()
			.filter_map(|index| matching.left_pair[*index])
			.collect();
		let right_sequence: Vec<_> = right.nodes[right_parent]
			.children
			.iter()
			.filter_map(|index| matching.right_pair[*index])
			.collect();
		if left_sequence != right_sequence {
			matching.order_changes.push((left_parent, right_parent));
		}
	}
	matching
}

fn node_key(node: &Node) -> (Ustr, &str) {
	(node.class, node.name.as_str())
}

fn group_end(children: &[usize], start: usize, structure: &PlaceStructure, key: (Ustr, &str)) -> usize {
	let mut end = start + 1;
	while end < children.len() && node_key(&structure.nodes[children[end]]) == key {
		end += 1;
	}
	end
}

fn match_group(
	left: &[usize],
	right: &[usize],
	left_fingerprints: &[NodeFingerprints],
	right_fingerprints: &[NodeFingerprints],
	matching: &mut Matching,
	queue: &mut VecDeque<(usize, usize)>,
) {
	let mut right_by_fingerprint: HashMap<Fingerprint, VecDeque<usize>> = HashMap::new();
	for &index in right {
		right_by_fingerprint
			.entry(right_fingerprints[index].subtree)
			.or_default()
			.push_back(index);
	}
	let mut remaining_left = Vec::new();
	let mut used_right = HashSet::new();
	for &left_index in left {
		let candidate = right_by_fingerprint
			.get_mut(&left_fingerprints[left_index].subtree)
			.and_then(VecDeque::pop_front);
		if let Some(right_index) = candidate {
			used_right.insert(right_index);
			add_pair(left_index, right_index, matching, queue);
		} else {
			remaining_left.push(left_index);
		}
	}
	let mut remaining_right = right.iter().copied().filter(|index| !used_right.contains(index));
	for left_index in remaining_left {
		if let Some(right_index) = remaining_right.next() {
			add_pair(left_index, right_index, matching, queue);
		} else {
			matching.left_unmatched.push(left_index);
		}
	}
	matching.right_unmatched.extend(remaining_right);
}

fn add_pair(left: usize, right: usize, matching: &mut Matching, queue: &mut VecDeque<(usize, usize)>) {
	let pair = matching.pairs.len();
	matching.pairs.push((left, right));
	matching.left_pair[left] = Some(pair);
	matching.right_pair[right] = Some(pair);
	queue.push_back((left, right));
}

struct CollectSink<'a> {
	targets: &'a HashSet<Ref>,
	properties: HashMap<Ref, UstrMap<Variant>>,
}

impl DecodeSink for CollectSink<'_> {
	fn property(&mut self, referent: Ref, name: Ustr, value: Variant) -> std::result::Result<(), String> {
		if self.targets.contains(&referent) {
			self.properties.entry(referent).or_default().insert(name, value);
		}
		Ok(())
	}
}

fn collect_properties(path: &Path, targets: HashSet<Ref>) -> Result<HashMap<Ref, UstrMap<Variant>>> {
	let mut sink = CollectSink {
		targets: &targets,
		properties: HashMap::with_capacity(targets.len()),
	};
	Deserializer::new(util::get_reflection_database())
		.deserialize_properties_with_sink(BufReader::new(File::open(path)?), &mut sink)?;
	Ok(sink.properties)
}

fn is_direct_service(structure: &PlaceStructure, index: usize) -> bool {
	let node = &structure.nodes[index];
	node.parent == Some(0)
		&& util::get_reflection_database()
			.classes
			.get(node.class.as_str())
			.is_some_and(|descriptor| descriptor.tags.contains(&ClassTag::Service))
}

fn is_engine_default_service_shell(
	structure: &PlaceStructure,
	index: usize,
	properties: &HashMap<Ref, UstrMap<Variant>>,
) -> bool {
	if !is_direct_service(structure, index) {
		return false;
	}
	let node = &structure.nodes[index];
	if node
		.children
		.iter()
		.any(|child| !structure.is_studio_runtime_subtree(*child))
	{
		return false;
	}
	let database = util::get_reflection_database();
	let descriptor = database
		.classes
		.get(node.class.as_str())
		.expect("direct service descriptor was checked above");
	properties
		.get(&node.referent)
		.into_iter()
		.flatten()
		.all(|(name, value)| {
			if is_unique_property(name.as_str(), value) || is_forward_instance_default(name.as_str(), value) {
				return true;
			}
			if is_default_hydrated_lighting_property(node.class.as_str(), name.as_str(), value) {
				return true;
			}
			if is_default_hydrated_service_property(node.class.as_str(), name.as_str(), value) {
				return true;
			}
			let Some(default) = database.find_default_property(descriptor, name.as_str()) else {
				return matches!(value, Variant::Ref(target) if target.is_none());
			};
			canonical_value(value, structure, true, Some(node), None)
				== canonical_value(default, structure, true, Some(node), None)
		})
}

fn is_default_hydrated_service_property(class: &str, property: &str, value: &Variant) -> bool {
	match (class, property, value) {
		("AssetService", "AllowInsertFreeAssets", Variant::Bool(false))
		| ("Debris", "MaxItems", Variant::Int32(1000))
		| ("Players", "BanningEnabled" | "CharacterAutoLoads", Variant::Bool(true))
		| ("Players", "UseStrafingAnimations", Variant::Bool(false))
		| ("Players", "PreferredPlayers", Variant::Int32(0))
		| ("Players", "MaxPlayers", Variant::Int32(12))
		| ("Players", "RespawnTime", Variant::Float32(5.0))
		| ("StarterGui", "ResetPlayerGuiOnSpawn" | "ShowDevelopmentGui", Variant::Bool(true)) => true,
		("PlayerEmulatorService", "SerializedEmulatedPolicyInfo", Variant::BinaryString(value)) => {
			let bytes: &[u8] = value.as_ref();
			bytes.is_empty()
		}
		("ServiceVisibilityService", "VisibleServices" | "HiddenServices", Variant::BinaryString(value)) => {
			let bytes: &[u8] = value.as_ref();
			bytes == [0, 0, 0, 0]
		}
		("SoundService", "ListenerCFrame", Variant::CFrame(value)) => value == &CFrame::identity(),
		("StarterGui", "RtlTextSupport", Variant::Enum(value)) => value.to_u32() == 0,
		("StarterGui", "ClipsDescendantsSupportsRotation", Variant::Enum(value)) => value.to_u32() == 0,
		("StarterGui", "ScreenOrientation", Variant::Enum(value)) => value.to_u32() == 2,
		("StarterGui", "VirtualCursorMode", Variant::Enum(value)) => value.to_u32() == 0,
		_ => false,
	}
}

fn is_default_hydrated_lighting_property(class: &str, property: &str, value: &Variant) -> bool {
	if class != "Lighting" {
		return false;
	}
	match (property, value) {
		("Brightness", Variant::Float32(value)) => *value == 1.0,
		("Technology", Variant::Enum(value)) => value.to_u32() == 2,
		("Attributes", Variant::Attributes(attributes)) => {
			attributes.len() == 1
				&& matches!(
					attributes.get("RBX_LightingTechnologyUnifiedMigration"),
					Some(Variant::Bool(true))
				)
		}
		_ => false,
	}
}

struct ReportBuilder {
	report: DiffReport,
	max_details: usize,
}

impl ReportBuilder {
	fn push(&mut self, difference: Difference) {
		match difference.impact {
			Impact::PotentialGameplay => self.report.blocking_differences += 1,
			Impact::NonGameplay => self.report.non_gameplay_differences += 1,
		}
		if self.report.differences.len() < self.max_details {
			self.report.differences.push(difference);
		} else {
			self.report.details_truncated = true;
		}
	}
}

pub fn compare(before: &Path, after: &Path, max_details: usize) -> Result<DiffReport> {
	let mut left = PlaceStructure::load(before)?;
	let mut right = PlaceStructure::load(after)?;
	let mut left_fp = fingerprint(&mut left)?;
	let mut right_fp = fingerprint(&mut right)?;
	fill_missing_defaults(&left, &mut left_fp, &right_fp.properties_by_class);
	fill_missing_defaults(&right, &mut right_fp, &left_fp.properties_by_class);
	fill_subtree_fingerprints(&left, &mut left_fp, false);
	fill_subtree_fingerprints(&right, &mut right_fp, false);
	fill_structural_locators(&left, &mut left_fp);
	fill_structural_locators(&right, &mut right_fp);
	fill_reference_fingerprints(&left, &mut left_fp, None);
	fill_reference_fingerprints(&right, &mut right_fp, None);
	fill_subtree_fingerprints(&left, &mut left_fp, true);
	fill_subtree_fingerprints(&right, &mut right_fp, true);
	let mut matching = match_hierarchy(&left, &right, &left_fp.nodes, &right_fp.nodes);
	// Structural identity deliberately ignores sibling position. For otherwise
	// symmetric duplicates, refine reference labels through the current cross-file
	// correspondence and rematch entirely in memory; no additional RBXL pass is
	// needed. Continue to a fixed point so arbitrarily deep reference chains remain
	// order-independent. A mapping-cycle guard retains the correspondence with the
	// fewest unexplained reference edges for genuinely changed/cyclic graphs.
	let mut seen_mappings = HashSet::from([mapping_digest(&matching.right_pair)]);
	let mut best_matching = matching.clone();
	let mut best_disagreements = usize::MAX;
	loop {
		fill_reference_fingerprints(&left, &mut left_fp, Some(&matching.left_pair));
		fill_reference_fingerprints(&right, &mut right_fp, Some(&matching.right_pair));
		fill_subtree_fingerprints(&left, &mut left_fp, true);
		fill_subtree_fingerprints(&right, &mut right_fp, true);
		let disagreements = reference_disagreements(&matching, &left_fp.nodes, &right_fp.nodes);
		if disagreements < best_disagreements {
			best_disagreements = disagreements;
			best_matching = matching.clone();
		}
		if disagreements == 0 {
			break;
		}
		let refined = match_hierarchy(&left, &right, &left_fp.nodes, &right_fp.nodes);
		let stable = refined.right_pair == matching.right_pair;
		matching = refined;
		if stable {
			break;
		}
		if !seen_mappings.insert(mapping_digest(&matching.right_pair)) {
			matching = best_matching;
			break;
		}
	}
	let mut builder = ReportBuilder {
		report: DiffReport {
			before: before.to_owned(),
			after: after.to_owned(),
			before_instances: left.nodes.len().saturating_sub(1),
			after_instances: right.nodes.len().saturating_sub(1),
			matched_instances: matching.pairs.len().saturating_sub(1),
			added_instances: matching
				.right_unmatched
				.iter()
				.map(|index| right.subtree_size(*index))
				.sum(),
			removed_instances: matching
				.left_unmatched
				.iter()
				.map(|index| left.subtree_size(*index))
				.sum(),
			blocking_differences: 0,
			non_gameplay_differences: 0,
			details_truncated: false,
			differences: Vec::new(),
		},
		max_details,
	};
	let left_unmatched_service_properties = collect_properties(
		before,
		matching
			.left_unmatched
			.iter()
			.copied()
			.filter(|index| is_direct_service(&left, *index))
			.map(|index| left.nodes[index].referent)
			.collect(),
	)?;
	let right_unmatched_service_properties = collect_properties(
		after,
		matching
			.right_unmatched
			.iter()
			.copied()
			.filter(|index| is_direct_service(&right, *index))
			.map(|index| right.nodes[index].referent)
			.collect(),
	)?;

	for key in left
		.metadata
		.keys()
		.chain(right.metadata.keys())
		.collect::<BTreeSet<_>>()
	{
		if left.metadata.get(key) != right.metadata.get(key) {
			builder.push(Difference {
				impact: Impact::NonGameplay,
				kind: "file_metadata".to_owned(),
				path: "<rbxl META>".to_owned(),
				property: Some((*key).clone()),
				before: left.metadata.get(key).cloned(),
				after: right.metadata.get(key).cloned(),
				reason: "binary file provenance does not affect gameplay".to_owned(),
			});
		}
	}
	if left_fp.unique_ids != right_fp.unique_ids {
		builder.push(Difference {
			impact: Impact::NonGameplay,
			kind: "unique_id".to_owned(),
			path: "game/**".to_owned(),
			property: Some("UniqueId/HistoryId".to_owned()),
			before: Some(format!("{} serialized values", left_fp.unique_ids.count)),
			after: Some(format!("{} serialized values", right_fp.unique_ids.count)),
			reason: "UniqueId representation is explicitly non-gameplay".to_owned(),
		});
	}
	for &(left_parent, _) in &matching.order_changes {
		builder.push(Difference {
			impact: Impact::NonGameplay,
			kind: "instance_order".to_owned(),
			path: left.display_path(left_parent),
			property: None,
			before: None,
			after: None,
			reason: "sibling ordering is explicitly non-gameplay".to_owned(),
		});
	}
	for &index in &matching.left_unmatched {
		let default_service = is_engine_default_service_shell(&left, index, &left_unmatched_service_properties);
		let non_gameplay =
			default_service || left.is_studio_runtime_subtree(index) || left_fp.edit_cameras.contains(&index);
		builder.push(Difference {
			impact: if non_gameplay {
				Impact::NonGameplay
			} else {
				Impact::PotentialGameplay
			},
			kind: "instance_removed".to_owned(),
			path: left.display_path(index),
			property: None,
			before: Some(format!("{} instance subtree", left.subtree_size(index))),
			after: None,
			reason: if default_service {
				"an empty engine-default service shell is recreated by Studio"
			} else if non_gameplay {
				"Studio runtime/edit-camera state does not affect gameplay"
			} else {
				"authored instance removal can affect gameplay"
			}
			.to_owned(),
		});
	}
	for &index in &matching.right_unmatched {
		let default_service = is_engine_default_service_shell(&right, index, &right_unmatched_service_properties);
		let non_gameplay =
			default_service || right.is_studio_runtime_subtree(index) || right_fp.edit_cameras.contains(&index);
		builder.push(Difference {
			impact: if non_gameplay {
				Impact::NonGameplay
			} else {
				Impact::PotentialGameplay
			},
			kind: "instance_added".to_owned(),
			path: right.display_path(index),
			property: None,
			before: None,
			after: Some(format!("{} instance subtree", right.subtree_size(index))),
			reason: if default_service {
				"an empty engine-default service shell is recreated by Studio"
			} else if non_gameplay {
				"Studio runtime/edit-camera state does not affect gameplay"
			} else {
				"authored instance addition can affect gameplay"
			}
			.to_owned(),
		});
	}

	let mismatched_pairs: Vec<_> = matching
		.pairs
		.iter()
		.enumerate()
		.filter(|(_, (left_index, right_index))| left_fp.nodes[*left_index].full != right_fp.nodes[*right_index].full)
		.map(|(pair, indexes)| (pair, *indexes))
		.collect();
	if !mismatched_pairs.is_empty() {
		let left_targets = mismatched_pairs
			.iter()
			.map(|(_, (index, _))| left.nodes[*index].referent)
			.collect();
		let right_targets = mismatched_pairs
			.iter()
			.map(|(_, (_, index))| right.nodes[*index].referent)
			.collect();
		let left_properties = collect_properties(before, left_targets)?;
		let right_properties = collect_properties(after, right_targets)?;
		for (_, (left_index, right_index)) in mismatched_pairs {
			compare_node_properties(
				&left,
				&right,
				left_index,
				right_index,
				left_properties.get(&left.nodes[left_index].referent),
				right_properties.get(&right.nodes[right_index].referent),
				&matching,
				&left_fp.edit_cameras,
				&right_fp.edit_cameras,
				&mut builder,
			);
		}
	}
	Ok(builder.report)
}

#[allow(clippy::too_many_arguments)]
fn compare_node_properties(
	left: &PlaceStructure,
	right: &PlaceStructure,
	left_index: usize,
	right_index: usize,
	left_properties: Option<&UstrMap<Variant>>,
	right_properties: Option<&UstrMap<Variant>>,
	matching: &Matching,
	left_edit_cameras: &HashSet<usize>,
	right_edit_cameras: &HashSet<usize>,
	builder: &mut ReportBuilder,
) {
	let empty = UstrMap::default();
	let left_properties = left_properties.unwrap_or(&empty);
	let right_properties = right_properties.unwrap_or(&empty);
	let properties: BTreeSet<_> = left_properties.keys().chain(right_properties.keys()).copied().collect();
	let left_node = &left.nodes[left_index];
	let right_node = &right.nodes[right_index];
	let database = util::get_reflection_database();
	let left_descriptor = database.classes.get(left_node.class.as_str());
	let right_descriptor = database.classes.get(right_node.class.as_str());
	for property in properties {
		let allow_default = !absence_changes_engine_load(left_node.class.as_str(), property.as_str());
		let left_value = left_properties.get(&property).or_else(|| {
			allow_default
				.then(|| left_descriptor.and_then(|class| database.find_default_property(class, property.as_str())))?
		});
		let right_value = right_properties.get(&property).or_else(|| {
			allow_default
				.then(|| right_descriptor.and_then(|class| database.find_default_property(class, property.as_str())))?
		});
		let left_full = optional_value(left_value, left, false, Some(left_node), Some(&matching.left_pair));
		let right_full = optional_value(right_value, right, false, Some(right_node), Some(&matching.right_pair));
		if left_full == right_full {
			continue;
		}
		let left_gameplay =
			optional_gameplay_value(left_value, left, left_node, &matching.left_pair, property.as_str());
		let right_gameplay =
			optional_gameplay_value(right_value, right, right_node, &matching.right_pair, property.as_str());
		let node_non_gameplay = left.is_studio_runtime_subtree(left_index)
			|| right.is_studio_runtime_subtree(right_index)
			|| left_edit_cameras.contains(&left_index)
			|| right_edit_cameras.contains(&right_index);
		let engine_equivalent = engine_serialization_equivalent(
			left,
			right,
			left_node,
			property.as_str(),
			left_value,
			right_value,
			left_properties,
			right_properties,
		);
		let accepted = node_non_gameplay
			|| (left_node.class.as_str() == "Workspace" && property.as_str() == "CurrentCamera")
			|| engine_equivalent
			|| left_gameplay == right_gameplay;
		builder.push(Difference {
			impact: if accepted {
				Impact::NonGameplay
			} else {
				Impact::PotentialGameplay
			},
			kind: "property_changed".to_owned(),
			path: left.display_path(left_index),
			property: Some(property.to_string()),
			before: Some(value_summary(left_value, left, &matching.left_pair)),
			after: Some(value_summary(right_value, right, &matching.right_pair)),
			reason: if accepted {
				accepted_property_reason(left_node, property.as_str(), node_non_gameplay, engine_equivalent)
			} else {
				"authored property difference can affect gameplay".to_owned()
			},
		});
	}
}

#[allow(clippy::too_many_arguments)]
fn engine_serialization_equivalent(
	left_structure: &PlaceStructure,
	right_structure: &PlaceStructure,
	node: &Node,
	property: &str,
	left_value: Option<&Variant>,
	right_value: Option<&Variant>,
	left_properties: &UstrMap<Variant>,
	right_properties: &UstrMap<Variant>,
) -> bool {
	if matches!(node.class.as_str(), "Script" | "LocalScript" | "ModuleScript") && property == "ScriptGuid" {
		return true;
	}
	if matches!(node.class.as_str(), "Script" | "LocalScript" | "ModuleScript") {
		let serialized_default = match (left_value, right_value) {
			(Some(value), None) | (None, Some(value)) => is_forward_instance_default(property, value),
			_ => false,
		};
		if serialized_default {
			return true;
		}
	}
	if matches!(
		node.class.as_str(),
		"Folder" | "Script" | "LocalScript" | "ModuleScript"
	) && property == "SourceAssetId"
		&& matches!(
			(left_value, right_value),
			(Some(Variant::Int64(-1)), Some(Variant::Int64(0))) | (Some(Variant::Int64(0)), Some(Variant::Int64(-1)))
		) {
		return true;
	}
	if node.class.as_str() != "Lighting" {
		return false;
	}
	if property == "ClockTime" {
		return lighting_clock_time_equivalent(left_value, left_properties)
			&& lighting_clock_time_equivalent(right_value, right_properties);
	}
	if property != "Technology" {
		return false;
	}
	let Some(original) = lighting_original_technology(left_properties) else {
		return false;
	};
	if lighting_original_technology(right_properties) != Some(original) {
		return false;
	}
	let left = match left_value {
		Some(Variant::Enum(value)) => Some(value.to_u32()),
		_ => None,
	};
	let right = match right_value {
		Some(Variant::Enum(value)) => Some(value.to_u32()),
		_ => None,
	};
	match (left_structure.managed_worktree, right_structure.managed_worktree) {
		(true, false) => left != Some(original) && right == Some(original),
		(false, true) => left == Some(original) && right != Some(original),
		_ => false,
	}
}

fn lighting_original_technology(properties: &UstrMap<Variant>) -> Option<u32> {
	let Variant::Attributes(attributes) = properties.get(&Ustr::from("Attributes"))? else {
		return None;
	};
	if !matches!(
		attributes.get("RBX_LightingTechnologyUnifiedMigration"),
		Some(Variant::Bool(true))
	) {
		return None;
	}
	match attributes.get("RBX_OriginalTechnologyOnFileLoad") {
		Some(Variant::Int32(value)) if *value >= 0 => Some(*value as u32),
		_ => None,
	}
}

fn lighting_clock_time_equivalent(value: Option<&Variant>, properties: &UstrMap<Variant>) -> bool {
	let Some(Variant::String(time_of_day)) = properties.get(&Ustr::from("TimeOfDay")) else {
		return false;
	};
	let Some(expected) = parse_time_of_day(time_of_day) else {
		return false;
	};
	match value {
		None => true,
		Some(Variant::Float32(actual)) => (*actual - expected).abs() <= 0.000_1,
		_ => false,
	}
}

fn parse_time_of_day(value: &str) -> Option<f32> {
	let mut parts = value.split(':');
	let hours: f32 = parts.next()?.parse().ok()?;
	let minutes: f32 = parts.next()?.parse().ok()?;
	let seconds: f32 = parts.next()?.parse().ok()?;
	if parts.next().is_some() || hours >= 24.0 || minutes >= 60.0 || seconds >= 60.0 {
		return None;
	}
	Some(hours + minutes / 60.0 + seconds / 3600.0)
}

fn optional_value(
	value: Option<&Variant>,
	structure: &PlaceStructure,
	gameplay: bool,
	node: Option<&Node>,
	pair_ids: Option<&[Option<usize>]>,
) -> Vec<u8> {
	value.map_or_else(
		|| b"<absent>".to_vec(),
		|value| canonical_value(value, structure, gameplay, node, pair_ids),
	)
}

fn optional_gameplay_value(
	value: Option<&Variant>,
	structure: &PlaceStructure,
	node: &Node,
	pair_ids: &[Option<usize>],
	property: &str,
) -> Option<Vec<u8>> {
	let value = value?;
	if is_unique_property(property, value) {
		return None;
	}
	Some(canonical_value(value, structure, true, Some(node), Some(pair_ids)))
}

fn accepted_property_reason(node: &Node, property: &str, node_non_gameplay: bool, engine_equivalent: bool) -> String {
	if node_non_gameplay {
		"Studio runtime/edit-camera state does not affect gameplay".to_owned()
	} else if engine_equivalent {
		"Studio's local serializer uses an engine-equivalent representation".to_owned()
	} else if matches!(property, "UniqueId" | "HistoryId") {
		"UniqueId representation is explicitly non-gameplay".to_owned()
	} else if property == "Attributes" {
		"managed worktree/MCP transport attributes do not affect gameplay".to_owned()
	} else if property == "Tags" {
		"tag ordering is set-like and does not affect gameplay".to_owned()
	} else if node.class.as_str() == "Workspace" && property == "CurrentCamera" {
		"the edit camera is dynamic Studio state".to_owned()
	} else {
		"canonical values are gameplay-equivalent".to_owned()
	}
}

fn value_summary(value: Option<&Variant>, structure: &PlaceStructure, pair_ids: &[Option<usize>]) -> String {
	let Some(value) = value else {
		return "<absent>".to_owned();
	};
	let summary = match value {
		Variant::Ref(target) => reference_summary(*target, structure, pair_ids),
		Variant::Content(content) => match content.value() {
			ContentType::Object(target) => {
				format!("Content.Object({})", reference_summary(*target, structure, pair_ids))
			}
			_ => format!("{value:?}"),
		},
		_ => format!("{value:?}"),
	};
	truncate(&summary, 240)
}

fn reference_summary(target: Ref, structure: &PlaceStructure, pair_ids: &[Option<usize>]) -> String {
	if target.is_none() {
		"nil".to_owned()
	} else if let Some(&index) = structure.by_ref.get(&target) {
		if let Some(pair) = pair_ids[index] {
			format!("pair:{pair} {}", structure.display_path(index))
		} else {
			structure.display_path(index)
		}
	} else {
		"<external>".to_owned()
	}
}

fn truncate(value: &str, max: usize) -> String {
	if value.len() <= max {
		return value.to_owned();
	}
	let mut boundary = max;
	while !value.is_char_boundary(boundary) {
		boundary -= 1;
	}
	format!("{}…", &value[..boundary])
}

#[cfg(test)]
mod tests {
	use super::*;
	use rbx_dom_weak::{
		types::{Attributes, BinaryString, Enum, UniqueId},
		InstanceBuilder, WeakDom,
	};
	use std::fs;

	fn write_fixture(name: &str, root: InstanceBuilder) -> PathBuf {
		let path = std::env::temp_dir().join(format!("carbon-place-diff-{name}-{}.rbxl", uuid::Uuid::new_v4()));
		let dom = WeakDom::new(root);
		rbx_binary::to_writer_with_database(
			File::create(&path).unwrap(),
			&dom,
			dom.root().children(),
			crate::util::get_reflection_database(),
		)
		.unwrap();
		path
	}

	#[test]
	fn gameplay_property_changes_block_while_unique_ids_and_order_do_not() {
		let left = write_fixture(
			"left",
			InstanceBuilder::new("DataModel")
				.with_child(
					InstanceBuilder::new("StringValue")
						.with_name("A")
						.with_property("Value", "before")
						.with_property("UniqueId", UniqueId::new(1, 2, 3)),
				)
				.with_child(InstanceBuilder::new("Folder").with_name("B")),
		);
		let right = write_fixture(
			"right",
			InstanceBuilder::new("DataModel")
				.with_child(InstanceBuilder::new("Folder").with_name("B"))
				.with_child(
					InstanceBuilder::new("StringValue")
						.with_name("A")
						.with_property("Value", "after")
						.with_property("UniqueId", UniqueId::new(4, 5, 6)),
				),
		);
		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 1);
		assert!(report.non_gameplay_differences >= 2);
		assert!(report
			.differences
			.iter()
			.any(|difference| difference.property.as_deref() == Some("Value")
				&& difference.impact == Impact::PotentialGameplay));
		assert!(report
			.differences
			.iter()
			.any(|difference| difference.kind == "unique_id"));
		assert!(report
			.differences
			.iter()
			.any(|difference| difference.kind == "instance_order"));
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}

	#[test]
	fn identical_places_have_no_differences() {
		let path = write_fixture(
			"identical",
			InstanceBuilder::new("DataModel").with_child(InstanceBuilder::new("Folder").with_name("Same")),
		);
		let report = compare(&path, &path, 100).unwrap();
		assert_eq!(report.blocking_differences, 0);
		assert_eq!(report.non_gameplay_differences, 0);
		let _ = fs::remove_file(path);
	}

	#[test]
	fn hydrated_service_defaults_require_exact_class_property_and_value() {
		assert!(is_default_hydrated_service_property(
			"Players",
			"UseStrafingAnimations",
			&Variant::Bool(false),
		));
		assert!(!is_default_hydrated_service_property(
			"Players",
			"UseStrafingAnimations",
			&Variant::Bool(true),
		));
		assert!(is_default_hydrated_service_property(
			"Players",
			"PreferredPlayers",
			&Variant::Int32(0),
		));
		assert!(!is_default_hydrated_service_property(
			"Players",
			"PreferredPlayers",
			&Variant::Int32(1),
		));
		assert!(is_default_hydrated_service_property(
			"StarterGui",
			"ShowDevelopmentGui",
			&Variant::Bool(true),
		));
		assert!(!is_default_hydrated_service_property(
			"StarterGui",
			"ShowDevelopmentGui",
			&Variant::Bool(false),
		));
		assert!(is_default_hydrated_service_property(
			"StarterGui",
			"ClipsDescendantsSupportsRotation",
			&Variant::Enum(rbx_dom_weak::types::Enum::from_u32(0)),
		));
		assert!(!is_default_hydrated_service_property(
			"StarterGui",
			"ClipsDescendantsSupportsRotation",
			&Variant::Enum(rbx_dom_weak::types::Enum::from_u32(1)),
		));
		assert!(!is_default_hydrated_service_property(
			"Folder",
			"ShowDevelopmentGui",
			&Variant::Bool(true),
		));
	}

	#[test]
	fn engine_default_service_shell_presence_is_non_gameplay() {
		let with_shell = write_fixture(
			"default-service-shell-present",
			InstanceBuilder::new("DataModel")
				.with_child(InstanceBuilder::new("HttpService").with_name("HttpService"))
				.with_child(
					InstanceBuilder::new("SoundService")
						.with_name("SoundService")
						.with_property("ListenerObject", Variant::Ref(Ref::none())),
				),
		);
		let without_shell = write_fixture("default-service-shell-absent", InstanceBuilder::new("DataModel"));

		let report = compare(&with_shell, &without_shell, 100).unwrap();
		assert_eq!(report.blocking_differences, 0);
		assert_eq!(report.non_gameplay_differences, 2);
		assert!(report.differences.iter().any(|difference| {
			difference.kind == "instance_removed"
				&& difference.path.contains("HttpService")
				&& difference.impact == Impact::NonGameplay
		}));

		let _ = fs::remove_file(with_shell);
		let _ = fs::remove_file(without_shell);
	}

	#[test]
	fn default_hydrated_lighting_shell_is_non_gameplay_but_authored_values_block() {
		fn lighting(brightness: f32) -> InstanceBuilder {
			InstanceBuilder::new("Lighting")
				.with_name("Lighting")
				.with_property("Brightness", brightness)
				.with_property("Technology", Enum::from_u32(2))
				.with_property(
					"Attributes",
					Attributes::new().with("RBX_LightingTechnologyUnifiedMigration", Variant::Bool(true)),
				)
		}

		let default_shell = write_fixture(
			"default-hydrated-lighting-present",
			InstanceBuilder::new("DataModel").with_child(lighting(1.0)),
		);
		let modified_shell = write_fixture(
			"modified-lighting-present",
			InstanceBuilder::new("DataModel").with_child(lighting(2.0)),
		);
		let absent = write_fixture("default-hydrated-lighting-absent", InstanceBuilder::new("DataModel"));

		let default_report = compare(&default_shell, &absent, 100).unwrap();
		assert_eq!(default_report.blocking_differences, 0);
		assert_eq!(default_report.non_gameplay_differences, 1);
		let modified_report = compare(&modified_shell, &absent, 100).unwrap();
		assert_eq!(modified_report.blocking_differences, 1);

		let _ = fs::remove_file(default_shell);
		let _ = fs::remove_file(modified_shell);
		let _ = fs::remove_file(absent);
	}

	#[test]
	fn reordered_duplicate_siblings_match_by_subtree_without_blocking() {
		fn duplicate_with_child(child_name: &str) -> InstanceBuilder {
			InstanceBuilder::new("Folder")
				.with_name("Duplicate")
				.with_child(InstanceBuilder::new("Folder").with_name(child_name))
		}

		let left = write_fixture(
			"duplicate-order-left",
			InstanceBuilder::new("DataModel")
				.with_child(duplicate_with_child("A"))
				.with_child(duplicate_with_child("B")),
		);
		let right = write_fixture(
			"duplicate-order-right",
			InstanceBuilder::new("DataModel")
				.with_child(duplicate_with_child("B"))
				.with_child(duplicate_with_child("A")),
		);

		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 0);
		assert_eq!(report.added_instances, 0);
		assert_eq!(report.removed_instances, 0);
		assert!(report
			.differences
			.iter()
			.any(|difference| difference.kind == "instance_order"));
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}

	#[test]
	fn reordered_duplicate_reference_holders_match_by_target_identity() {
		fn reference_fixture(reversed: bool) -> InstanceBuilder {
			let target_a = InstanceBuilder::new("Folder").with_name("Target");
			let target_b = InstanceBuilder::new("Folder").with_name("Target");
			let value_a = InstanceBuilder::new("ObjectValue")
				.with_name("Duplicate")
				.with_property("Value", Variant::Ref(target_a.referent()));
			let value_b = InstanceBuilder::new("ObjectValue")
				.with_name("Duplicate")
				.with_property("Value", Variant::Ref(target_b.referent()));
			let root = InstanceBuilder::new("DataModel")
				.with_child(target_a)
				.with_child(target_b);
			if reversed {
				root.with_child(value_b).with_child(value_a)
			} else {
				root.with_child(value_a).with_child(value_b)
			}
		}

		let left = write_fixture("duplicate-reference-order-left", reference_fixture(false));
		let right = write_fixture("duplicate-reference-order-right", reference_fixture(true));

		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 0);
		assert_eq!(report.added_instances, 0);
		assert_eq!(report.removed_instances, 0);
		assert!(report
			.differences
			.iter()
			.any(|difference| difference.kind == "instance_order"));
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}

	#[test]
	fn reordered_reference_correspondence_converges_through_deep_chains() {
		fn deep_reference_fixture(reverse_first_level: bool) -> InstanceBuilder {
			let target_a = InstanceBuilder::new("Folder").with_name("Target");
			let target_b = InstanceBuilder::new("Folder").with_name("Target");
			let mut previous = (target_a.referent(), target_b.referent());
			let mut children = vec![target_a, target_b];
			for level in 1..=6 {
				let name = format!("Level{level}");
				let value_a = InstanceBuilder::new("ObjectValue")
					.with_name(&name)
					.with_property("Value", Variant::Ref(previous.0));
				let value_b = InstanceBuilder::new("ObjectValue")
					.with_name(&name)
					.with_property("Value", Variant::Ref(previous.1));
				previous = (value_a.referent(), value_b.referent());
				if level == 1 && reverse_first_level {
					children.push(value_b);
					children.push(value_a);
				} else {
					children.push(value_a);
					children.push(value_b);
				}
			}
			children
				.into_iter()
				.fold(InstanceBuilder::new("DataModel"), InstanceBuilder::with_child)
		}

		let left = write_fixture("deep-reference-order-left", deep_reference_fixture(false));
		let right = write_fixture("deep-reference-order-right", deep_reference_fixture(true));
		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 0);
		assert_eq!(report.added_instances, 0);
		assert_eq!(report.removed_instances, 0);
		assert!(report
			.differences
			.iter()
			.any(|difference| difference.kind == "instance_order"));
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}

	#[test]
	fn authored_styling_service_content_is_gameplay_affecting() {
		let left = write_fixture(
			"styling-left",
			InstanceBuilder::new("DataModel")
				.with_child(InstanceBuilder::new("StylingService").with_name("StylingService")),
		);
		let right = write_fixture(
			"styling-right",
			InstanceBuilder::new("DataModel").with_child(
				InstanceBuilder::new("StylingService")
					.with_name("StylingService")
					.with_child(InstanceBuilder::new("StyleSheet").with_name("GameplayTheme")),
			),
		);

		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 1);
		assert!(report.differences.iter().any(|difference| {
			difference.path.contains("GameplayTheme") && difference.impact == Impact::PotentialGameplay
		}));
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}

	#[test]
	fn authored_filtered_selection_collision_is_gameplay_affecting() {
		let left = write_fixture("filtered-selection-left", InstanceBuilder::new("DataModel"));
		let right = write_fixture(
			"filtered-selection-right",
			InstanceBuilder::new("DataModel").with_child(
				InstanceBuilder::new("Instance")
					.with_name("FilteredSelection")
					.with_child(InstanceBuilder::new("StringValue").with_name("AuthoredValue")),
			),
		);

		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 1);
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}

	#[test]
	fn proven_non_archivable_filtered_selection_is_non_gameplay() {
		let left = write_fixture("runtime-filtered-selection-left", InstanceBuilder::new("DataModel"));
		let right = write_fixture(
			"runtime-filtered-selection-right",
			InstanceBuilder::new("DataModel").with_child(
				InstanceBuilder::new("Instance")
					.with_name("FilteredSelection")
					.with_property("Archivable", false),
			),
		);

		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 0);
		assert_eq!(report.non_gameplay_differences, 1);
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}

	#[test]
	fn non_archivable_filtered_selection_with_authored_children_blocks() {
		let left = write_fixture("runtime-name-authored-child-left", InstanceBuilder::new("DataModel"));
		let right = write_fixture(
			"runtime-name-authored-child-right",
			InstanceBuilder::new("DataModel").with_child(
				InstanceBuilder::new("Instance")
					.with_name("FilteredSelection")
					.with_property("Archivable", false)
					.with_child(InstanceBuilder::new("StringValue").with_name("AuthoredValue")),
			),
		);

		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 1);
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}

	#[test]
	fn non_archivable_filtered_selection_with_authored_attributes_blocks() {
		let left = write_fixture(
			"runtime-name-authored-attributes-left",
			InstanceBuilder::new("DataModel"),
		);
		let right = write_fixture(
			"runtime-name-authored-attributes-right",
			InstanceBuilder::new("DataModel").with_child(
				InstanceBuilder::new("Instance")
					.with_name("FilteredSelection")
					.with_property("Archivable", false)
					.with_property(
						"Attributes",
						Attributes::new().with("GameplayFlag", Variant::Bool(true)),
					),
			),
		);

		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 1);
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}

	#[test]
	fn isolated_tool_name_attribute_collisions_remain_blocking() {
		let left = write_fixture(
			"authored-attribute-left",
			InstanceBuilder::new("DataModel").with_child(
				InstanceBuilder::new("Workspace").with_name("Workspace").with_property(
					"Attributes",
					Attributes::new().with(
						"__StudioWorktree_CarbonEndpoint",
						BinaryString::from(b"authored-one".as_slice()),
					),
				),
			),
		);
		let right = write_fixture(
			"authored-attribute-right",
			InstanceBuilder::new("DataModel").with_child(
				InstanceBuilder::new("Workspace").with_name("Workspace").with_property(
					"Attributes",
					Attributes::new().with(
						"__StudioWorktree_CarbonEndpoint",
						BinaryString::from(b"authored-two".as_slice()),
					),
				),
			),
		);

		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 1);
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}

	fn managed_attributes(generation: &str) -> Attributes {
		Attributes::new()
			.with(
				"__StudioWorktree_CarbonEndpoint",
				BinaryString::from(b"http://127.0.0.1:48123".as_slice()),
			)
			.with(
				"__StudioWorktree_CarbonProject",
				BinaryString::from(b"Project".as_slice()),
			)
			.with(
				"__StudioWorktree_CarbonGeneration",
				BinaryString::from(generation.as_bytes()),
			)
			.with("__StudioWorktree_Identity", BinaryString::from(b"worktree".as_slice()))
			.with("__StudioWorktree_Session", BinaryString::from(b"session".as_slice()))
	}

	fn mcp_attributes(id: &str) -> Attributes {
		Attributes::new().with("__MCPPlaceId", BinaryString::from(id.as_bytes()))
	}

	#[test]
	fn authored_uuid_shaped_mcp_attribute_remains_blocking() {
		let left = write_fixture(
			"authored-mcp-left",
			InstanceBuilder::new("DataModel").with_child(
				InstanceBuilder::new("ServerStorage")
					.with_name("ServerStorage")
					.with_property("Attributes", mcp_attributes("11111111-1111-4111-8111-111111111111")),
			),
		);
		let right = write_fixture(
			"authored-mcp-right",
			InstanceBuilder::new("DataModel").with_child(
				InstanceBuilder::new("ServerStorage")
					.with_name("ServerStorage")
					.with_property("Attributes", mcp_attributes("22222222-2222-4222-8222-222222222222")),
			),
		);

		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 1);
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}

	#[test]
	fn managed_uuid_shaped_mcp_attribute_is_non_gameplay_transport() {
		fn managed_place(generation: &str, mcp_id: &str) -> InstanceBuilder {
			InstanceBuilder::new("DataModel")
				.with_child(
					InstanceBuilder::new("Workspace")
						.with_name("Workspace")
						.with_property("Attributes", managed_attributes(generation)),
				)
				.with_child(
					InstanceBuilder::new("ServerStorage")
						.with_name("ServerStorage")
						.with_property("Attributes", mcp_attributes(mcp_id)),
				)
		}

		let left = write_fixture(
			"managed-mcp-left",
			managed_place(&"a".repeat(64), "11111111-1111-4111-8111-111111111111"),
		);
		let right = write_fixture(
			"managed-mcp-right",
			managed_place(&"b".repeat(64), "22222222-2222-4222-8222-222222222222"),
		);

		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 0);
		assert_eq!(report.non_gameplay_differences, 2);
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}

	#[test]
	fn complete_managed_worktree_contract_changes_are_non_gameplay() {
		let left = write_fixture(
			"managed-attribute-left",
			InstanceBuilder::new("DataModel").with_child(
				InstanceBuilder::new("Workspace")
					.with_name("Workspace")
					.with_property("Attributes", managed_attributes(&"a".repeat(64))),
			),
		);
		let right = write_fixture(
			"managed-attribute-right",
			InstanceBuilder::new("DataModel").with_child(
				InstanceBuilder::new("Workspace")
					.with_name("Workspace")
					.with_property("Attributes", managed_attributes(&"b".repeat(64))),
			),
		);

		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 0);
		assert_eq!(report.non_gameplay_differences, 1);
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}

	#[test]
	fn managed_serializer_identity_and_runtime_artifacts_are_non_gameplay() {
		fn managed_place(include_runtime: bool) -> InstanceBuilder {
			let mut insert_service = InstanceBuilder::new("InsertService").with_name("InsertService");
			if include_runtime {
				insert_service = insert_service.with_child(
					InstanceBuilder::new("StringValue")
						.with_name("InsertionHash")
						.with_property("Value", "{8018D819-6E9D-4CEA-8B62-F052D50E8091}"),
				);
			}
			let mut root = InstanceBuilder::new("DataModel")
				.with_child(
					InstanceBuilder::new("Workspace")
						.with_name("Workspace")
						.with_property("Attributes", managed_attributes(&"a".repeat(64))),
				)
				.with_child(InstanceBuilder::new("Folder").with_name("Authored").with_property(
					"Attributes",
					Attributes::new().with(
						MANIFEST_IDENTITY_ATTRIBUTE,
						BinaryString::from(b"0123456789abcdef0123456789abcdef".as_slice()),
					),
				))
				.with_child(insert_service);
			if include_runtime {
				root = root.with_child(InstanceBuilder::new("Instance").with_name("FilteredSelection"));
			}
			root
		}

		let left = write_fixture("managed-runtime-artifacts-left", managed_place(true));
		let right = write_fixture("managed-runtime-artifacts-right", managed_place(false));
		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 0);
		assert_eq!(report.removed_instances, 2);
		assert!(report.non_gameplay_differences >= 2);
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}

	#[test]
	fn managed_authored_filtered_selection_identity_still_blocks() {
		let left = write_fixture(
			"managed-authored-filtered-left",
			InstanceBuilder::new("DataModel")
				.with_child(
					InstanceBuilder::new("Workspace")
						.with_name("Workspace")
						.with_property("Attributes", managed_attributes(&"a".repeat(64))),
				)
				.with_child(
					InstanceBuilder::new("Instance")
						.with_name("FilteredSelection")
						.with_property(
							"Attributes",
							Attributes::new().with(
								MANIFEST_IDENTITY_ATTRIBUTE,
								BinaryString::from(b"0123456789abcdef0123456789abcdef".as_slice()),
							),
						),
				),
		);
		let right = write_fixture(
			"managed-authored-filtered-right",
			InstanceBuilder::new("DataModel").with_child(
				InstanceBuilder::new("Workspace")
					.with_name("Workspace")
					.with_property("Attributes", managed_attributes(&"a".repeat(64))),
			),
		);
		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 1);
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}

	fn migrated_lighting(technology: u32, clock_time: Option<f32>, managed: bool) -> InstanceBuilder {
		let attributes = Attributes::new()
			.with("RBX_LightingTechnologyUnifiedMigration", Variant::Bool(true))
			.with("RBX_OriginalTechnologyOnFileLoad", Variant::Int32(2));
		let mut lighting = InstanceBuilder::new("Lighting")
			.with_name("Lighting")
			.with_property("Attributes", attributes)
			.with_property("Technology", Enum::from_u32(technology))
			.with_property("TimeOfDay", "14:00:00");
		if let Some(clock_time) = clock_time {
			lighting = lighting.with_property("ClockTime", clock_time);
		}
		let workspace = if managed {
			InstanceBuilder::new("Workspace")
				.with_name("Workspace")
				.with_property("Attributes", managed_attributes(&"a".repeat(64)))
		} else {
			InstanceBuilder::new("Workspace").with_name("Workspace")
		};
		InstanceBuilder::new("DataModel")
			.with_child(workspace)
			.with_child(lighting)
	}

	#[test]
	fn lighting_migration_file_forms_are_engine_equivalent() {
		let left = write_fixture("lighting-live-form", migrated_lighting(1, None, true));
		let right = write_fixture("lighting-on-disk-form", migrated_lighting(2, Some(14.0), false));
		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 0);
		assert!(report.non_gameplay_differences >= 2);
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}

	#[test]
	fn lighting_migration_does_not_hide_unrelated_values() {
		let technology_left = write_fixture("lighting-wrong-tech-left", migrated_lighting(0, None, true));
		let technology_right = write_fixture("lighting-wrong-tech-right", migrated_lighting(1, None, false));
		assert_eq!(
			compare(&technology_left, &technology_right, 100)
				.unwrap()
				.blocking_differences,
			1
		);

		let clock_left = write_fixture("lighting-wrong-clock-left", migrated_lighting(1, None, true));
		let clock_right = write_fixture("lighting-wrong-clock-right", migrated_lighting(1, Some(15.0), false));
		assert_eq!(compare(&clock_left, &clock_right, 100).unwrap().blocking_differences, 1);
		for path in [technology_left, technology_right, clock_left, clock_right] {
			let _ = fs::remove_file(path);
		}
	}

	#[test]
	fn studio_generated_script_guid_is_non_gameplay() {
		fn place(guid: &str) -> InstanceBuilder {
			InstanceBuilder::new("DataModel").with_child(
				InstanceBuilder::new("Script")
					.with_name("Server")
					.with_property("Source", "return true")
					.with_property("ScriptGuid", guid),
			)
		}
		let left = write_fixture("script-guid-left", place("{5C4C1694-1B60-46EF-9D95-45CD972BFD8A}"));
		let right = write_fixture("script-guid-right", place(""));
		let report = compare(&left, &right, 100).unwrap();
		assert_eq!(report.blocking_differences, 0);
		assert_eq!(report.non_gameplay_differences, 1);
		let _ = fs::remove_file(left);
		let _ = fs::remove_file(right);
	}
}
