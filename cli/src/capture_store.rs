//! Compact native-capture compiler.
//!
//! A monolithic capture compiler would materialize both a `WeakDom` and a recursive
//! `Snapshot` before constructing a second, recursive `Changes` tree. This
//! module keeps the decoded place in one flat arena and assigns final source
//! identities in place. The artifact writer can then consume the arena directly.

use anyhow::{bail, ensure, Context, Result};
use rbx_binary::InstanceSource;
use rbx_dom_weak::{
	types::{Content, ContentType, Ref, Variant},
	Ustr, UstrMap,
};
use std::{
	collections::{HashMap, HashSet},
	io::{self, BufReader, Read},
	path::{Path, PathBuf},
	time::Instant,
};

struct CancellableReader<'a, R> {
	inner: R,
	cancelled: &'a dyn Fn() -> bool,
}

impl<R: Read> Read for CancellableReader<'_, R> {
	fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
		if (self.cancelled)() {
			// `read_exact` is required to retry Interrupted indefinitely. Use a
			// terminal I/O error so cancellation unwinds every decoder immediately.
			return Err(io::Error::other(
				"Capture Manifest was cancelled during native decoding",
			));
		}
		self.inner.read(buffer)
	}
}

fn capture_reader<'a, R: Read>(input: R, cancelled: &'a dyn Fn() -> bool) -> BufReader<CancellableReader<'a, R>> {
	BufReader::new(CancellableReader {
		inner: input,
		cancelled,
	})
}

use crate::{
	artifact_store::{self, CapturePropertySpools},
	capture_artifact::{reference_dependency_ordinal, CaptureModelArtifact, CaptureModelChunk},
	capture_provider::{
		CaptureEnvelope, CaptureReferenceTarget, ManifestIdentityRemap, CAPTURE_HIERARCHY_FLAG_DEFAULT_HYDRATED_SERVICE,
	},
	core::{snapshot::Snapshot, tree::Tree},
	manifest_identity::ManifestIdentityAllocator,
	project::capture_service_anchor,
	server::privileged,
	util,
};

#[cfg(test)]
use crate::project::stable_ref;

#[derive(Clone, Debug)]
pub(crate) enum ValidatedCaptureClass {
	ExactNoop(artifact_store::ValidatedArtifactReceipt),
	Rebuild(artifact_store::ValidatedArtifactReceipt),
}

pub(crate) fn classify_validated_capture(
	envelope: &CaptureEnvelope,
	project_name: &str,
	project_generation: &str,
	artifact_path: &Path,
) -> Result<ValidatedCaptureClass> {
	let receipt = artifact_store::validated_artifact_receipt(artifact_path)?;
	Ok(classify_validated_receipt(
		envelope,
		project_name,
		project_generation,
		receipt,
	))
}

fn classify_validated_receipt(
	envelope: &CaptureEnvelope,
	project_name: &str,
	project_generation: &str,
	receipt: artifact_store::ValidatedArtifactReceipt,
) -> ValidatedCaptureClass {
	let exact = envelope.manifest_identities_authoritative
		&& receipt.name() == project_name
		&& receipt.project_generation() == Some(project_generation)
		&& receipt.capture_fingerprint() == Some(envelope.semantic_fingerprint().as_str());
	if exact {
		ValidatedCaptureClass::ExactNoop(receipt)
	} else {
		ValidatedCaptureClass::Rebuild(receipt)
	}
}

#[derive(Debug)]
pub(crate) struct CompiledCapture {
	pub arena: CaptureArena,
	pub projected_tree: Tree,
	pub preserve_records: HashSet<Ref>,
	pub properties: Option<CapturePropertySpools>,
	pub canonical_cframes: Option<CapturePropertySpools>,
	pub stage_plan: artifact_store::CaptureStagePlan,
	pub semantic_noop: bool,
	pub capture_fingerprint: String,
	pub metadata: std::collections::BTreeMap<String, String>,
	pub identity_remap: Vec<ManifestIdentityRemap>,
	pub referenced_mapped_refs: HashSet<Ref>,
}

impl CompiledCapture {
	pub(crate) fn stage_composite(
		self,
		project_name: String,
		artifact_path: &Path,
		cancelled: &dyn Fn() -> bool,
	) -> Result<(Tree, artifact_store::StagedCompiledCapture, Vec<ManifestIdentityRemap>)> {
		let Self {
			arena,
			projected_tree,
			preserve_records,
			properties,
			canonical_cframes,
			stage_plan,
			semantic_noop,
			capture_fingerprint,
			metadata,
			identity_remap,
			referenced_mapped_refs: _,
		} = self;
		if semantic_noop {
			return Ok((
				projected_tree,
				artifact_store::stage_compiled_noop(artifact_path)?,
				identity_remap,
			));
		}
		let staged = artifact_store::stage_compiled_capture(
			&arena,
			arena.root_ref(),
			project_name,
			metadata,
			Some(&capture_fingerprint),
			properties.as_ref(),
			canonical_cframes.as_ref(),
			&stage_plan,
			&preserve_records,
			artifact_path,
			cancelled,
		)?;
		Ok((projected_tree, staged, identity_remap))
	}
}

#[derive(Debug)]
pub(crate) struct CaptureArena {
	nodes: Vec<CaptureNode>,
	by_ref: HashMap<Ref, usize>,
	root: Ref,
}

#[derive(Debug)]
struct CaptureNode {
	referent: Ref,
	parent: Ref,
	class: Ustr,
	name: String,
	properties: UstrMap<Variant>,
	children: Vec<Ref>,
	property_digest: DigestMultiset,
	observed_properties: Vec<ObservedProperty>,
	digest: [u8; 32],
	semantic_name: [u8; 32],
}

#[derive(Clone, Debug)]
struct ObservedProperty {
	name: Ustr,
	digest: blake3::Hash,
	reference: Option<(bool, Ref)>,
	omittable_default: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct DigestMultiset {
	lanes: [u64; 4],
	count: u64,
}

impl DigestMultiset {
	fn insert(&mut self, digest: blake3::Hash) {
		for (lane, bytes) in self.lanes.iter_mut().zip(digest.as_bytes().chunks_exact(8)) {
			*lane = lane.wrapping_add(u64::from_le_bytes(bytes.try_into().unwrap()));
		}
		self.count = self.count.wrapping_add(1);
	}

	fn finish(self, domain: &[u8]) -> [u8; 32] {
		let mut hasher = blake3::Hasher::new();
		hasher.update(domain);
		hasher.update(&self.count.to_le_bytes());
		for lane in self.lanes {
			hasher.update(&lane.to_le_bytes());
		}
		*hasher.finalize().as_bytes()
	}
}

fn semantic_name(name: &str, raw_name: Option<&[u8]>) -> [u8; 32] {
	let bytes = match raw_name {
		Some(raw) if std::str::from_utf8(raw).ok() != Some(name) => raw,
		_ => name.as_bytes(),
	};
	*blake3::hash(bytes).as_bytes()
}

impl rbx_binary::InstanceSource for CaptureArena {
	fn get_by_ref<'a>(&'a self, referent: Ref) -> Option<rbx_binary::InstanceView<'a>> {
		let node = &self.nodes[*self.by_ref.get(&referent)?];
		Some(rbx_binary::InstanceView {
			referent: node.referent,
			parent: node.parent,
			class: node.class,
			name: &node.name,
			raw_name: node
				.properties
				.get(&Ustr::from("__CarbonRawName"))
				.and_then(|value| match value {
					Variant::BinaryString(value) => Some(value.as_ref()),
					_ => None,
				}),
			properties: &node.properties,
			children: &node.children,
		})
	}
}

impl CaptureArena {
	pub(crate) fn root_ref(&self) -> Ref {
		self.root
	}

	fn from_envelope(envelope: &CaptureEnvelope) -> Result<(Self, Vec<Ref>)> {
		let referents = (0..envelope.nodes.len())
			.map(|ordinal| Ref::some(ordinal as u128 + 1))
			.collect::<Vec<_>>();
		let mut nodes = Vec::with_capacity(envelope.nodes.len());
		for (ordinal, value) in envelope.nodes.iter().enumerate() {
			let referent = referents[ordinal];
			let parent = if ordinal == 0 {
				Ref::none()
			} else {
				referents[usize::try_from(value.parent_ordinal)?]
			};
			nodes.push(CaptureNode {
				referent,
				parent,
				class: value.class_name,
				name: value.name.clone(),
				properties: UstrMap::default(),
				children: Vec::new(),
				property_digest: DigestMultiset::default(),
				observed_properties: Vec::new(),
				digest: [0; 32],
				semantic_name: semantic_name(&value.name, None),
			});
		}
		for ordinal in 1..nodes.len() {
			let child = nodes[ordinal].referent;
			let parent = usize::try_from(envelope.nodes[ordinal].parent_ordinal)?;
			nodes[parent].children.push(child);
		}
		let by_ref = nodes
			.iter()
			.enumerate()
			.map(|(index, node)| (node.referent, index))
			.collect();
		Ok((
			Self {
				root: referents[0],
				nodes,
				by_ref,
			},
			referents,
		))
	}

	fn get(&self, referent: Ref) -> Result<&CaptureNode> {
		self.by_ref
			.get(&referent)
			.and_then(|index| self.nodes.get(*index))
			.with_context(|| format!("capture arena instance {referent} is missing"))
	}

	fn get_mut(&mut self, referent: Ref) -> Result<&mut CaptureNode> {
		let index = *self
			.by_ref
			.get(&referent)
			.with_context(|| format!("capture arena instance {referent} is missing"))?;
		Ok(&mut self.nodes[index])
	}

	fn studio_path(&self, referent: Ref) -> Result<String> {
		let mut names = Vec::new();
		let mut cursor = referent;
		loop {
			let node = self.get(cursor)?;
			if cursor == self.root {
				break;
			}
			names.push(node.name.clone());
			cursor = node.parent;
		}
		names.reverse();
		Ok(std::iter::once("game".to_owned())
			.chain(names)
			.collect::<Vec<_>>()
			.join("."))
	}

	fn insert_snapshot(&mut self, parent: Ref, snapshot: &Snapshot) -> Result<()> {
		let root = snapshot.id;
		ensure!(
			!self.by_ref.contains_key(&root),
			"capture mapped source {root} was serialized unexpectedly"
		);
		let mut stack = vec![(snapshot, parent)];
		while let Some((value, value_parent)) = stack.pop() {
			ensure!(
				self.by_ref.insert(value.id, self.nodes.len()).is_none(),
				"capture mapped identity {} is duplicated",
				value.id
			);
			let mut properties = UstrMap::default();
			if let Some(raw_name) = &value.raw_name {
				properties.insert(
					Ustr::from("__CarbonRawName"),
					Variant::BinaryString(raw_name.clone().into_vec().into()),
				);
			}
			for (name, property) in &value.properties {
				if is_managed_contract_property(value.class.as_str(), &value.name, name.as_str()) {
					properties.insert(*name, property.clone());
				}
			}
			self.nodes.push(CaptureNode {
				referent: value.id,
				parent: value_parent,
				class: value.class,
				name: value.name.clone(),
				properties,
				children: {
					let mut children = value.children.iter().map(|child| child.id).collect::<Vec<_>>();
					children.sort_unstable_by_key(ToString::to_string);
					children
				},
				property_digest: DigestMultiset::default(),
				observed_properties: Vec::new(),
				digest: [0; 32],
				semantic_name: semantic_name(&value.name, value.raw_name.as_deref().map(Vec::as_slice)),
			});
			stack.extend(value.children.iter().rev().map(|child| (child, value.id)));
		}
		let children = &mut self.get_mut(parent)?.children;
		children.push(root);
		children.sort_unstable_by_key(ToString::to_string);
		Ok(())
	}

	fn rebuild_index(&mut self) -> Result<()> {
		self.by_ref.clear();
		for (index, node) in self.nodes.iter().enumerate() {
			ensure!(
				self.by_ref.insert(node.referent, index).is_none(),
				"compiled capture identity {} is duplicated",
				node.referent
			);
		}
		Ok(())
	}

	fn normalize_properties(&mut self) {
		for node in &mut self.nodes {
			let class = node.class;
			node.properties = std::mem::take(&mut node.properties)
				.into_iter()
				.filter_map(|(name, value)| normalize_property(class.as_str(), name, value))
				.collect();
		}
	}

	fn digest_inline_properties(&mut self, cancelled: &dyn Fn() -> bool) -> Result<()> {
		for node in &mut self.nodes {
			ensure!(
				!cancelled(),
				"Capture Manifest was cancelled during property digest staging"
			);
			let mut observed = Vec::with_capacity(node.properties.len());
			for (name, value) in &node.properties {
				if name.as_str() == "__CarbonRawName" {
					continue;
				}
				let property = observed_property(node.class.as_str(), *name, value)?;
				if property.omittable_default {
					continue;
				}
				node.property_digest.insert(property.digest);
				observed.push(property);
			}
			for property in observed {
				upsert_observed_property(node, property);
			}
		}
		Ok(())
	}

	#[cfg(test)]
	fn finish_digests(&mut self) -> Result<()> {
		self.finish_digests_cancellable(&|| false)
	}

	fn finish_digests_cancellable(&mut self, cancelled: &dyn Fn() -> bool) -> Result<()> {
		let mut order = Vec::with_capacity(self.nodes.len());
		let mut stack = vec![self.root];
		while let Some(id) = stack.pop() {
			ensure!(
				!cancelled(),
				"Capture Manifest was cancelled during hierarchy digest staging"
			);
			order.push(id);
			stack.extend(self.get(id)?.children.iter().copied());
		}
		for id in order.into_iter().rev() {
			ensure!(
				!cancelled(),
				"Capture Manifest was cancelled during hierarchy digest staging"
			);
			let index = self.by_ref[&id];
			let mut children = DigestMultiset::default();
			for child in &self.nodes[index].children {
				children.insert(blake3::Hash::from(self.nodes[self.by_ref[child]].digest));
			}
			let mut hasher = blake3::Hasher::new();
			hasher.update(b"carbon-capture-subtree-v2\0");
			hasher.update(self.nodes[index].class.as_bytes());
			hasher.update(&self.nodes[index].property_digest.finish(b"properties"));
			hasher.update(&children.finish(b"children"));
			self.nodes[index].digest = *hasher.finalize().as_bytes();
		}
		Ok(())
	}

	#[cfg(test)]
	fn exactly_matches_prior(
		&self,
		prior: &HashMap<Ref, artifact_store::PriorIdentityNode>,
		cancelled: &dyn Fn() -> bool,
	) -> Result<bool> {
		Ok(self.dirty_buckets_against_prior(prior, cancelled)?.is_empty())
	}

	#[cfg(test)]
	fn dirty_buckets_against_prior(
		&self,
		prior: &HashMap<Ref, artifact_store::PriorIdentityNode>,
		cancelled: &dyn Fn() -> bool,
	) -> Result<HashSet<usize>> {
		fn mark(dirty: &mut HashSet<usize>, referent: Ref) -> Result<()> {
			dirty.insert(artifact_store::partition_for_ref(referent)?);
			Ok(())
		}

		let mut dirty = HashSet::new();
		let mut current = HashSet::with_capacity(self.nodes.len());
		let mut default_keepers = HashSet::new();
		let mut stack = vec![(self.root, None)];
		while let Some((id, parent)) = stack.pop() {
			ensure!(
				!cancelled(),
				"Capture Manifest was cancelled during sparse-stage evidence staging"
			);
			let node = self.get(id)?;
			current.insert(id);
			let bucket = artifact_store::partition_for_ref(id)?;
			let Some(previous) = prior.get(&id) else {
				dirty.insert(bucket);
				if let Some(parent) = parent {
					mark(&mut dirty, parent)?;
				}
				stack.extend(node.children.iter().copied().rev().map(|child| (child, Some(id))));
				continue;
			};
			let default_keeper = default_keepers.insert((bucket, node.class));
			let current_properties = current_record_property_evidence(node, default_keeper)?;
			let prior_properties = prior_record_property_evidence(previous)?;
			let current_raw_name = node
				.properties
				.get(&Ustr::from("__CarbonRawName"))
				.and_then(|value| match value {
					Variant::BinaryString(raw) => Some(raw.as_ref()),
					_ => None,
				});
			let topology_changed = previous.parent != parent;
			let record_changed = topology_changed
				|| previous.class != node.class
				|| previous.name != node.name
				|| previous.raw_name.as_deref() != current_raw_name
				|| prior_properties != current_properties;
			if record_changed {
				dirty.insert(bucket);
			}
			if topology_changed {
				if let Some(parent) = previous.parent {
					mark(&mut dirty, parent)?;
				}
				if let Some(parent) = parent {
					mark(&mut dirty, parent)?;
				}
			}
			stack.extend(node.children.iter().copied().rev().map(|child| (child, Some(id))));
		}
		for (&id, previous) in prior {
			ensure!(
				!cancelled(),
				"Capture Manifest was cancelled during sparse-stage evidence staging"
			);
			if current.contains(&id) {
				continue;
			}
			mark(&mut dirty, id)?;
			if let Some(parent) = previous.parent {
				mark(&mut dirty, parent)?;
			}
		}
		Ok(dirty)
	}

	fn dirty_buckets_against_artifact(
		&self,
		artifact_path: &Path,
		cancelled: &dyn Fn() -> bool,
	) -> Result<HashSet<usize>> {
		fn mark(dirty: &mut HashSet<usize>, referent: Ref) -> Result<()> {
			dirty.insert(artifact_store::partition_for_ref(referent)?);
			Ok(())
		}

		let mut default_classes = HashSet::new();
		let mut default_keepers = HashSet::new();
		let mut stack = vec![self.root];
		while let Some(id) = stack.pop() {
			ensure!(
				!cancelled(),
				"Capture Manifest was cancelled during current sparse evidence staging"
			);
			let node = self.get(id)?;
			let bucket = artifact_store::partition_for_ref(id)?;
			if default_classes.insert((bucket, node.class)) {
				default_keepers.insert(id);
			}
			for child in node.children.iter().copied().rev() {
				stack.push(child);
			}
		}

		let mut dirty = HashSet::new();
		let mut seen = vec![false; self.nodes.len()];
		artifact_store::visit_prior_identity_nodes_cancellable(artifact_path, cancelled, |previous| {
			let Some(&index) = self.by_ref.get(&previous.id) else {
				mark(&mut dirty, previous.id)?;
				if let Some(parent) = previous.parent {
					mark(&mut dirty, parent)?;
				}
				return Ok(());
			};
			seen[index] = true;
			let node = &self.nodes[index];
			let parent = node.parent.is_some().then_some(node.parent);
			let current_properties = current_record_property_evidence(node, default_keepers.contains(&node.referent))?;
			let prior_properties = prior_record_property_evidence(&previous)?;
			let current_raw_name = node
				.properties
				.get(&Ustr::from("__CarbonRawName"))
				.and_then(|value| match value {
					Variant::BinaryString(raw) => Some(raw.as_ref()),
					_ => None,
				});
			let topology_changed = previous.parent != parent;
			let record_changed = topology_changed
				|| previous.class != node.class
				|| previous.name != node.name
				|| previous.raw_name.as_deref() != current_raw_name
				|| prior_properties != current_properties;
			if record_changed {
				mark(&mut dirty, node.referent)?;
			}
			if topology_changed {
				if let Some(parent) = previous.parent {
					mark(&mut dirty, parent)?;
				}
				if let Some(parent) = parent {
					mark(&mut dirty, parent)?;
				}
			}
			Ok(())
		})?;

		for (index, node) in self.nodes.iter().enumerate() {
			ensure!(
				!cancelled(),
				"Capture Manifest was cancelled during current addition evidence staging"
			);
			if seen[index] {
				continue;
			}
			mark(&mut dirty, node.referent)?;
			if node.parent.is_some() {
				mark(&mut dirty, node.parent)?;
			}
		}
		Ok(dirty)
	}

	fn projected_tree(&self, selected: &HashSet<Ref>, cancelled: &dyn Fn() -> bool) -> Result<Tree> {
		ensure!(selected.contains(&self.root), "compiled projection excludes its root");
		let snapshot = |node: &CaptureNode| {
			let mut properties = node.properties.clone();
			let raw_name = properties
				.remove(&Ustr::from("__CarbonRawName"))
				.and_then(|value| match value {
					Variant::BinaryString(raw) => Some(serde_bytes::ByteBuf::from(raw.into_vec())),
					_ => None,
				});
			let mut snapshot = Snapshot::new()
				.with_id(node.referent)
				.with_name(&node.name)
				.with_class(node.class.as_str())
				.with_properties(properties);
			snapshot.raw_name = raw_name;
			snapshot
		};
		let root = self.get(self.root)?;
		let mut tree = Tree::new_detached(snapshot(root), selected.len())?;
		let mut stack = vec![root.referent];
		while let Some(parent) = stack.pop() {
			ensure!(
				!cancelled(),
				"Capture Manifest was cancelled during projected hierarchy staging"
			);
			for child in self.get(parent)?.children.iter().copied().rev() {
				if !selected.contains(&child) {
					continue;
				}
				let node = self.get(child)?;
				tree.insert_detached(snapshot(node), parent)?;
				stack.push(child);
			}
		}
		tree.finish_detached()?;
		Ok(tree)
	}

	fn remove_leaf(&mut self, referent: Ref) -> Result<()> {
		ensure!(referent != self.root, "compiled capture cannot omit its root");
		let index = *self
			.by_ref
			.get(&referent)
			.context("compiled capture omitted service is missing")?;
		let parent = self.nodes[index].parent;
		ensure!(
			self.nodes[index].children.is_empty(),
			"compiled capture cannot omit a service with children"
		);
		self.get_mut(parent)?.children.retain(|child| *child != referent);
		self.by_ref.remove(&referent);
		self.nodes.swap_remove(index);
		if let Some(moved) = self.nodes.get(index) {
			self.by_ref.insert(moved.referent, index);
		}
		Ok(())
	}

	fn assign_stable_referents(
		&mut self,
		mut remap: HashMap<Ref, Ref>,
		cancelled: &dyn Fn() -> bool,
	) -> Result<HashMap<Ref, Ref>> {
		// A fully reconciled capture already has the complete remap. Consume and
		// reuse it instead of retaining two million-entry maps plus a collision
		// set. The set is necessary only when genuinely new identities remain.
		let mut used = (remap.len() != self.nodes.len()).then(|| remap.values().copied().collect::<HashSet<_>>());
		let mut allocator = ManifestIdentityAllocator::new();
		let mut stack = vec![self.root];
		while let Some(parent) = stack.pop() {
			ensure!(
				!cancelled(),
				"Capture Manifest was cancelled during identity assignment staging"
			);
			ensure!(remap.contains_key(&parent), "capture parent has no stable identity");
			let children = &self.get(parent)?.children;
			for child in children {
				let exact_assignment = remap.get(child).copied();
				let mut assigned = exact_assignment.unwrap_or_else(|| Ref::some(allocator.next()));
				while exact_assignment.is_none() && used.as_ref().is_some_and(|used| used.contains(&assigned)) {
					assigned = Ref::some(allocator.next());
				}
				if exact_assignment.is_none() {
					used.as_mut()
						.context("new capture identity has no collision index")?
						.insert(assigned);
					remap.insert(*child, assigned);
				}
			}
			stack.extend(children.iter().rev().copied());
		}

		for node in &mut self.nodes {
			ensure!(
				!cancelled(),
				"Capture Manifest was cancelled during identity assignment staging"
			);
			node.referent = remap[&node.referent];
			node.parent = remap.get(&node.parent).copied().unwrap_or_else(Ref::none);
			for child in &mut node.children {
				*child = remap[child];
			}
			for value in node.properties.values_mut() {
				match value {
					Variant::Ref(target) => {
						if let Some(stable) = remap.get(target) {
							*target = *stable;
						}
					}
					Variant::Content(content) => {
						if let ContentType::Object(target) = content.value() {
							if let Some(stable) = remap.get(target) {
								*content = Content::from_referent(*stable);
							}
						}
					}
					_ => {}
				}
			}
			for observed in &mut node.observed_properties {
				if let Some((_, target)) = &mut observed.reference {
					if let Some(stable) = remap.get(target) {
						*target = *stable;
					}
				}
			}
		}
		self.root = remap[&self.root];
		self.rebuild_index()?;
		Ok(remap)
	}
}

fn normalize_property(class: &str, name: Ustr, mut value: Variant) -> Option<(Ustr, Variant)> {
	if matches!(name.as_str(), "Name" | "Parent" | "HistoryId" | "UniqueId") {
		return None;
	}
	if name.as_str() == "Attributes" {
		let Variant::Attributes(attributes) = &mut value else {
			return Some((name, value));
		};
		for attribute in [
			"__StudioWorktree_CarbonEndpoint",
			"__StudioWorktree_CarbonProject",
			"__StudioWorktree_CarbonGeneration",
			artifact_store::MANIFEST_IDENTITY_ATTRIBUTE,
			"__StudioWorktree_Identity",
			"__StudioWorktree_Session",
			"__MCPPlaceId",
		] {
			attributes.remove(attribute);
		}
		if attributes.is_empty() {
			return None;
		}
	}
	if artifact_store::is_omittable_default(class, name.as_str(), &value) {
		// Identity digests treat a reflected default and an omitted column as the
		// same semantic value. The value is still spooled on the final pass so the
		// record writer can retain its one default-keeper column per partition.
	}
	Some((name, value))
}

fn is_managed_contract_property(class: &str, instance_name: &str, property: &str) -> bool {
	(class == "Workspace" && property == "CurrentCamera")
		|| (class == "Weld" && instance_name == "HeadWeld" && property == "Part1")
}

fn property_digest(name: &Ustr, value: &Variant) -> Result<blake3::Hash> {
	let mut hasher = blake3::Hasher::new();
	hasher.update(name.as_bytes());
	match value {
		Variant::Ref(_) => hasher.update(b"instance-reference"),
		Variant::Content(content) if matches!(content.value(), ContentType::Object(_)) => {
			hasher.update(b"instance-content-reference")
		}
		_ => hasher.update(&rmp_serde::to_vec_named(value)?),
	};
	Ok(hasher.finalize())
}

fn exact_property_digest(class: &str, name: &Ustr, value: &Variant) -> Result<blake3::Hash> {
	if matches!(value, Variant::Ref(_))
		|| matches!(value, Variant::Content(content) if matches!(content.value(), ContentType::Object(_)))
	{
		return property_digest(name, value);
	}
	let canonical = crate::resolution::UnresolvedValue::from_variant(value.clone(), class, name.as_str())
		.resolve(class, name.as_str())?;
	property_digest(name, &canonical)
}

fn observed_property(class: &str, name: Ustr, value: &Variant) -> Result<ObservedProperty> {
	Ok(ObservedProperty {
		name,
		digest: exact_property_digest(class, &name, value)?,
		reference: property_reference_evidence(name, value).map(|(_, content, target)| (content, target)),
		omittable_default: artifact_store::is_omittable_default(class, name.as_str(), value),
	})
}

fn upsert_observed_property(node: &mut CaptureNode, property: ObservedProperty) {
	if let Some(existing) = node
		.observed_properties
		.iter_mut()
		.find(|existing| existing.name == property.name)
	{
		*existing = property;
	} else {
		node.observed_properties.push(property);
	}
}

fn property_reference_evidence(name: Ustr, value: &Variant) -> Option<(Ustr, bool, Ref)> {
	match value {
		Variant::Ref(target) => Some((name, false, *target)),
		Variant::Content(content) => match content.value() {
			ContentType::Object(target) => Some((name, true, *target)),
			_ => None,
		},
		_ => None,
	}
}

fn property_target(value: &Variant) -> Option<Ref> {
	match value {
		Variant::Ref(target) if target.is_some() => Some(*target),
		Variant::Content(content) => match content.value() {
			ContentType::Object(target) if target.is_some() => Some(*target),
			_ => None,
		},
		_ => None,
	}
}

fn validate_canonical_domains(canonical: &Snapshot, mapped: &HashSet<Ref>) -> Result<()> {
	let mut stack = vec![(canonical, "game".to_owned())];
	let mut paths = HashMap::new();
	while let Some((node, path)) = stack.pop() {
		paths.insert(node.id, path.clone());
		stack.extend(
			node.children
				.iter()
				.rev()
				.map(|child| (child, format!("{path}.{}", child.name))),
		);
	}
	let mut stack = vec![canonical];
	while let Some(node) = stack.pop() {
		for (property, value) in &node.properties {
			let Some(target) = property_target(value) else { continue };
			if mapped.contains(&node.id) && !mapped.contains(&target) {
				bail!(
					"filesystem-owned reference blocker: {}.{} targets manifest-owned {}",
					paths.get(&node.id).map(String::as_str).unwrap_or("<missing owner>"),
					property,
					paths.get(&target).map(String::as_str).unwrap_or("<missing target>")
				);
			}
		}
		stack.extend(node.children.iter().rev());
	}
	Ok(())
}

struct CanonicalNode<'a> {
	snapshot: &'a Snapshot,
	parent: Option<Ref>,
}

fn canonical_index(root: &Snapshot) -> HashMap<Ref, CanonicalNode<'_>> {
	let mut result = HashMap::new();
	let mut stack = vec![(root, None, 0)];
	while let Some((snapshot, parent, order)) = stack.pop() {
		let _ = order;
		result.insert(snapshot.id, CanonicalNode { snapshot, parent });
		stack.extend(
			snapshot
				.children
				.iter()
				.enumerate()
				.rev()
				.map(|(order, child)| (child, Some(snapshot.id), order)),
		);
	}
	result
}

fn prior_digests(
	nodes: &HashMap<Ref, artifact_store::PriorIdentityNode>,
	cancelled: &dyn Fn() -> bool,
) -> Result<HashMap<Ref, [u8; 32]>> {
	let mut order = Vec::with_capacity(nodes.len());
	let mut stack = nodes
		.values()
		.filter(|node| node.parent.is_none_or(|parent| !nodes.contains_key(&parent)))
		.map(|node| node.id)
		.collect::<Vec<_>>();
	while let Some(id) = stack.pop() {
		ensure!(
			!cancelled(),
			"Capture Manifest was cancelled during prior identity digest staging"
		);
		order.push(id);
		stack.extend(nodes[&id].children.iter().rev().copied());
	}
	ensure!(
		order.len() == nodes.len(),
		"prior residual identity topology is cyclic or disconnected"
	);
	let mut digests = HashMap::with_capacity(nodes.len());
	for id in order.into_iter().rev() {
		ensure!(
			!cancelled(),
			"Capture Manifest was cancelled during prior identity digest staging"
		);
		let node = &nodes[&id];
		let mut properties = DigestMultiset::default();
		for (name, value) in &node.properties {
			if matches!(name.as_str(), "Name" | "Parent" | "HistoryId" | "UniqueId")
				|| artifact_store::is_omittable_default(node.class.as_str(), name.as_str(), value)
			{
				continue;
			}
			let Some((name, value)) = normalize_property(node.class.as_str(), *name, value.clone()) else {
				continue;
			};
			properties.insert(property_digest(&name, &value)?);
		}
		let mut children = DigestMultiset::default();
		for child in &node.children {
			children.insert(blake3::Hash::from(digests[child]));
		}
		let mut hasher = blake3::Hasher::new();
		hasher.update(b"carbon-capture-subtree-v2\0");
		hasher.update(node.class.as_bytes());
		hasher.update(&properties.finish(b"properties"));
		hasher.update(&children.finish(b"children"));
		digests.insert(id, *hasher.finalize().as_bytes());
	}
	Ok(digests)
}

type RecordPropertyEvidence = (Ustr, blake3::Hash, Option<(bool, Ref)>);

fn prior_record_property_evidence(node: &artifact_store::PriorIdentityNode) -> Result<Vec<RecordPropertyEvidence>> {
	let mut evidence = Vec::with_capacity(node.properties.len());
	for (name, value) in &node.properties {
		let Some((name, value)) = normalize_property(node.class.as_str(), *name, value.clone()) else {
			continue;
		};
		if name.as_str() == "__CarbonRawName" {
			continue;
		}
		evidence.push((
			name,
			exact_property_digest(node.class.as_str(), &name, &value)?,
			property_reference_evidence(name, &value).map(|(_, content, target)| (content, target)),
		));
	}
	evidence.sort_unstable_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
	Ok(evidence)
}

fn current_record_property_evidence(
	node: &CaptureNode,
	preserve_omittable_defaults: bool,
) -> Result<Vec<RecordPropertyEvidence>> {
	let mut evidence = Vec::with_capacity(node.observed_properties.len());
	for property in &node.observed_properties {
		if !preserve_omittable_defaults && property.omittable_default {
			continue;
		}
		evidence.push((property.name, property.digest, property.reference));
	}
	if preserve_omittable_defaults {
		for (name, value) in artifact_store::capture_synthesized_defaults(node.class.as_str()) {
			if node.observed_properties.iter().any(|property| property.name == name) {
				continue;
			}
			let Some((name, value)) = normalize_property(node.class.as_str(), name, value) else {
				continue;
			};
			let property = observed_property(node.class.as_str(), name, &value)?;
			evidence.push((property.name, property.digest, property.reference));
		}
	}
	evidence.sort_unstable_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
	Ok(evidence)
}

#[derive(Clone, Copy)]
struct Candidate {
	id: Ref,
	count: u32,
}

type StrongIdentityEvidence = (Ustr, [u8; 32], [u8; 32]);
type DigestIdentityEvidence = (Ustr, [u8; 32]);
type NamedIdentityEvidence = (Ustr, [u8; 32]);

struct IdentityCandidateIndexes {
	local_strong: HashMap<(Ref, StrongIdentityEvidence), Candidate>,
	global_strong: HashMap<StrongIdentityEvidence, Candidate>,
	local_digest: HashMap<(Ref, DigestIdentityEvidence), Candidate>,
	global_digest: HashMap<DigestIdentityEvidence, Candidate>,
	local_named: HashMap<(Ref, NamedIdentityEvidence), Candidate>,
	global_named: HashMap<NamedIdentityEvidence, Candidate>,
	local_class: HashMap<(Ref, Ustr), Candidate>,
	global_class: HashMap<Ustr, Candidate>,
}

impl IdentityCandidateIndexes {
	fn build(
		nodes: &HashMap<Ref, artifact_store::PriorIdentityNode>,
		digests: &HashMap<Ref, [u8; 32]>,
		claimed: &HashSet<Ref>,
		cancelled: &dyn Fn() -> bool,
	) -> Result<Self> {
		let mut result = Self {
			local_strong: HashMap::new(),
			global_strong: HashMap::new(),
			local_digest: HashMap::new(),
			global_digest: HashMap::new(),
			local_named: HashMap::new(),
			global_named: HashMap::new(),
			local_class: HashMap::new(),
			global_class: HashMap::new(),
		};
		for (&id, node) in nodes {
			ensure!(
				!cancelled(),
				"Capture Manifest was cancelled during identity reconciliation staging"
			);
			let Some(parent) = node.parent else { continue };
			if claimed.contains(&id) {
				continue;
			}
			let class = node.class;
			let digest = digests[&id];
			let name = semantic_name(&node.name, node.raw_name.as_deref().map(Vec::as_slice));
			record_candidate(&mut result.local_strong, (parent, (class, digest, name)), id);
			record_candidate(&mut result.global_strong, (class, digest, name), id);
			record_candidate(&mut result.local_digest, (parent, (class, digest)), id);
			record_candidate(&mut result.global_digest, (class, digest), id);
			record_candidate(&mut result.local_named, (parent, (class, name)), id);
			record_candidate(&mut result.global_named, (class, name), id);
			record_candidate(&mut result.local_class, (parent, class), id);
			record_candidate(&mut result.global_class, class, id);
		}
		Ok(result)
	}
}

fn record_candidate<K: std::hash::Hash + Eq>(index: &mut HashMap<K, Candidate>, key: K, id: Ref) {
	index
		.entry(key)
		.and_modify(|candidate| candidate.count = candidate.count.saturating_add(1))
		.or_insert(Candidate { id, count: 1 });
}

enum CandidateLookup {
	Missing,
	Claimed,
	Unique(Ref),
	Ambiguous(u32),
}

fn candidate<K: std::hash::Hash + Eq>(
	index: &HashMap<K, Candidate>,
	key: &K,
	claimed: &HashSet<Ref>,
) -> CandidateLookup {
	let Some(candidate) = index.get(key) else {
		return CandidateLookup::Missing;
	};
	if candidate.count > 1 {
		CandidateLookup::Ambiguous(candidate.count)
	} else if claimed.contains(&candidate.id) {
		CandidateLookup::Claimed
	} else {
		CandidateLookup::Unique(candidate.id)
	}
}

fn resolve_identity_candidate(
	name: &str,
	lookups: impl IntoIterator<Item = CandidateLookup>,
) -> Result<(Option<Ref>, u32)> {
	let mut resolved = None;
	let mut ambiguous = 0;
	for lookup in lookups {
		match lookup {
			CandidateLookup::Missing | CandidateLookup::Claimed => continue,
			CandidateLookup::Unique(id) => {
				if resolved.is_some_and(|previous| previous != id) {
					bail!(
						"identity-ambiguity blocker at {name}: independent identity evidence selects conflicting prior instances"
					);
				}
				resolved = Some(id);
			}
			CandidateLookup::Ambiguous(count) => ambiguous = ambiguous.max(count),
		}
	}
	Ok((resolved, ambiguous))
}

fn reconcile_identities(
	arena: &mut CaptureArena,
	prior_ids: &artifact_store::ArtifactReferents,
	artifact_path: &Path,
	exact: &mut HashMap<Ref, Ref>,
	cancelled: &dyn Fn() -> bool,
) -> Result<()> {
	let anchored = exact.values().copied().collect::<HashSet<_>>();
	let mut claimed = anchored;
	let mut unmatched = Vec::with_capacity(arena.nodes.len().saturating_sub(exact.len()));
	let mut stack = Vec::with_capacity(arena.nodes.len());
	stack.push(arena.root);
	while let Some(parent) = stack.pop() {
		ensure!(
			!cancelled(),
			"Capture Manifest was cancelled during identity reconciliation staging"
		);
		for child in arena.get(parent)?.children.iter().copied() {
			stack.push(child);
			if let Some(stable) = exact.get(&child).copied() {
				claimed.insert(stable);
			} else {
				unmatched.push(arena.by_ref[&child]);
			}
		}
	}
	drop(stack);

	// An unchanged semantic path is definitive identity evidence. Resolve it
	// from lightweight manifest headers before loading prior properties or
	// constructing the broad rename/reparent indexes. This is the common path
	// for large captures and keeps memory proportional to one compact index.
	exact.reserve(unmatched.len());
	let mut local_named = HashMap::with_capacity(unmatched.len());
	artifact_store::visit_prior_identity_headers_cancellable(artifact_path, cancelled, |header| {
		if !claimed.contains(&header.id) {
			if let Some(parent) = header.parent {
				record_candidate(
					&mut local_named,
					(parent, header.class, header.semantic_name),
					header.id,
				);
			}
		}
		Ok(())
	})?;
	for &index in &unmatched {
		ensure!(
			!cancelled(),
			"Capture Manifest was cancelled during exact-path identity staging"
		);
		let node = &arena.nodes[index];
		let Some(stable_parent) = exact.get(&node.parent).copied() else {
			continue;
		};
		let key = (stable_parent, node.class, node.semantic_name);
		let Some(selection) = local_named.get(&key).copied() else {
			continue;
		};
		if selection.count == 1 && !claimed.contains(&selection.id) {
			exact.insert(node.referent, selection.id);
			// Consume the candidate so duplicate observed paths cannot claim the
			// same prior identity without allocating a million-entry claimed set.
			local_named.remove(&key);
		}
	}
	drop(local_named);
	unmatched.retain(|&index| !exact.contains_key(&arena.nodes[index].referent));
	if unmatched.is_empty() {
		return Ok(());
	}
	// Broad rename/reparent reconciliation is rare. Materialize its complete claimed set only
	// after the common exact-path index has been released.
	claimed = exact.values().copied().collect();

	// Subtree digests are needed only by rename/reparent reconciliation. Avoid
	// allocating its million-node traversal buffers on the exact-path fast path.
	arena.finish_digests_cancellable(cancelled)?;
	let residual = prior_ids.residual(&claimed);
	let nodes = artifact_store::load_prior_identity_nodes(artifact_path, &residual)?;
	let digests = prior_digests(&nodes, cancelled)?;

	// Claim every whole-set exact-name correspondence before any rename or
	// reparent reconciliation is allowed to observe broad digest/class ambiguity.
	// Rebuild after progress so duplicate counts always describe the remaining
	// candidates, and so a newly resolved parent unlocks local evidence below it.
	loop {
		let indexes = IdentityCandidateIndexes::build(&nodes, &digests, &claimed, cancelled)?;
		let mut progress = false;
		for &index in &unmatched {
			ensure!(
				!cancelled(),
				"Capture Manifest was cancelled during identity reconciliation staging"
			);
			let child = arena.nodes[index].referent;
			if exact.contains_key(&child) {
				continue;
			}
			let node = &arena.nodes[index];
			let stable_parent = exact.get(&node.parent).copied();
			let strong = (node.class, node.digest, node.semantic_name);
			let named = (node.class, node.semantic_name);
			let mut lookups = Vec::with_capacity(4);
			if let Some(parent) = stable_parent {
				lookups.push(candidate(&indexes.local_strong, &(parent, strong), &claimed));
			}
			lookups.push(candidate(&indexes.global_strong, &strong, &claimed));
			if let Some(parent) = stable_parent {
				lookups.push(candidate(&indexes.local_named, &(parent, named), &claimed));
			}
			lookups.push(candidate(&indexes.global_named, &named, &claimed));
			let (resolved, _) = resolve_identity_candidate(&node.name, lookups)?;
			if let Some(id) = resolved {
				exact.insert(child, id);
				claimed.insert(id);
				progress = true;
			}
		}
		if !progress {
			break;
		}
	}

	// The heuristic reconciliation index is built only from the residual set left after all
	// exact correspondences. One renamed object among a huge unchanged sibling
	// group is therefore unique by elimination, while genuine duplicates remain
	// ambiguous and block capture.
	let indexes = IdentityCandidateIndexes::build(&nodes, &digests, &claimed, cancelled)?;
	for index in unmatched {
		ensure!(
			!cancelled(),
			"Capture Manifest was cancelled during identity reconciliation staging"
		);
		let child = arena.nodes[index].referent;
		if exact.contains_key(&child) {
			continue;
		}
		let node = &arena.nodes[index];
		// Residual parents are processed before their descendants. Resolve the
		// current stable parent here rather than freezing `None` during the direct
		// pass, so a renamed parent restores local evidence for a homogeneous
		// descendant group.
		let stable_parent = exact.get(&node.parent).copied();
		let strong = (node.class, node.digest, node.semantic_name);
		let digest = (node.class, node.digest);
		let named = (node.class, node.semantic_name);
		let mut lookups = Vec::with_capacity(8);
		if let Some(parent) = stable_parent {
			lookups.push(candidate(&indexes.local_strong, &(parent, strong), &claimed));
		}
		lookups.push(candidate(&indexes.global_strong, &strong, &claimed));
		if let Some(parent) = stable_parent {
			lookups.push(candidate(&indexes.local_digest, &(parent, digest), &claimed));
		}
		lookups.push(candidate(&indexes.global_digest, &digest, &claimed));
		if let Some(parent) = stable_parent {
			lookups.push(candidate(&indexes.local_named, &(parent, named), &claimed));
		}
		lookups.push(candidate(&indexes.global_named, &named, &claimed));
		if let Some(parent) = stable_parent {
			lookups.push(candidate(&indexes.local_class, &(parent, node.class), &claimed));
		}
		lookups.push(candidate(&indexes.global_class, &node.class, &claimed));
		let (resolved, ambiguous) = resolve_identity_candidate(&node.name, lookups)?;
		if let Some(id) = resolved {
			exact.insert(child, id);
			claimed.insert(id);
		} else if ambiguous > 0 {
			bail!(
				"identity-ambiguity blocker at {}: {ambiguous} prior instances could be the same object",
				node.name
			);
		}
	}
	Ok(())
}

fn apply_authoritative_identities(
	arena: &CaptureArena,
	ordinal_refs: &[Ref],
	manifest_identities: &[Ref],
	exact: &mut HashMap<Ref, Ref>,
) -> Result<()> {
	ensure!(
		manifest_identities.len() == ordinal_refs.len(),
		"capture manifest identity count disagrees with the native hierarchy"
	);
	ensure!(
		arena.nodes.len() >= ordinal_refs.len(),
		"capture ordinal arena omits native hierarchy nodes"
	);
	for (index, (&observed, &stable)) in ordinal_refs.iter().zip(manifest_identities).enumerate() {
		ensure!(stable.is_some(), "capture manifest identity is zero");
		ensure!(
			arena.nodes[index].referent == observed,
			"capture manifest identity ordinal is unavailable"
		);
		if let Some(previous) = exact.insert(observed, stable) {
			ensure!(
				previous == stable,
				"capture manifest identity conflicts with a canonical managed anchor"
			);
		}
	}
	Ok(())
}

struct CaptureChunkAlignment {
	persistent: Vec<(Ref, Ref)>,
	carrier_routes: HashMap<(Ref, Ustr), (Ref, Ustr)>,
}

struct CaptureTopology {
	child_counts: Vec<u32>,
	child_offsets: Vec<u32>,
	children: Vec<u32>,
	component_roots: Vec<bool>,
	service_shells: Vec<bool>,
	visited: Vec<bool>,
	ordinal_refs: Vec<Ref>,
}

impl CaptureTopology {
	fn new(envelope: &CaptureEnvelope, artifact: &CaptureModelArtifact, ordinal_refs: Vec<Ref>) -> Result<Self> {
		ensure!(
			ordinal_refs.len() == envelope.nodes.len(),
			"capture ordinal identity count changed"
		);
		let mut child_counts = vec![0_u32; envelope.nodes.len()];
		for node in envelope.nodes.iter().skip(1) {
			child_counts[node.parent_ordinal as usize] = child_counts[node.parent_ordinal as usize]
				.checked_add(1)
				.context("capture hierarchy child count exceeds u32")?;
		}
		let mut child_offsets = vec![0_u32; envelope.nodes.len()];
		let mut child_total = 0_u32;
		for (offset, count) in child_offsets.iter_mut().zip(&child_counts) {
			*offset = child_total;
			child_total = child_total
				.checked_add(*count)
				.context("capture hierarchy edge count exceeds u32")?;
		}
		ensure!(
			child_total as usize == envelope.nodes.len() - 1,
			"capture hierarchy edge count is invalid"
		);
		let mut cursor = child_offsets.clone();
		let mut children = vec![0_u32; child_total as usize];
		for (ordinal, node) in envelope.nodes.iter().enumerate().skip(1) {
			let parent = node.parent_ordinal as usize;
			children[cursor[parent] as usize] = ordinal as u32;
			cursor[parent] += 1;
		}

		let mut component_roots = vec![false; envelope.nodes.len()];
		for ordinal in artifact
			.chunks
			.iter()
			.flat_map(|chunk| chunk.root_ordinals.iter().copied())
			.filter(|ordinal| *ordinal != u32::MAX && reference_dependency_ordinal(*ordinal).is_none())
		{
			component_roots[usize::try_from(ordinal)?] = true;
		}
		let mut service_shells = vec![false; envelope.nodes.len()];
		service_shells[0] = true;
		for root in &envelope.roots {
			service_shells[usize::try_from(root.hierarchy_ordinal)?] = true;
		}
		for (ordinal, component) in component_roots.iter().copied().enumerate() {
			ensure!(
				!component || !service_shells[ordinal],
				"capture component root points at a service shell"
			);
		}

		Ok(Self {
			child_counts,
			child_offsets,
			children,
			component_roots,
			service_shells,
			visited: vec![false; envelope.nodes.len()],
			ordinal_refs,
		})
	}

	fn align_chunk(
		&mut self,
		envelope: &CaptureEnvelope,
		chunk: &CaptureModelChunk,
		serialized_start: usize,
		source: &rbx_binary::DecodedArena,
		carrier_indexes: &HashMap<u32, Vec<usize>>,
		track_coverage: bool,
	) -> Result<CaptureChunkAlignment> {
		let model_root = source
			.get_by_ref(source.root_ref())
			.context("capture RBXM synthetic root is missing")?;
		ensure!(
			model_root.children.len() == chunk.root_ordinals.len(),
			"capture RBXM chunk root count does not match its frame"
		);
		let mut persistent = Vec::new();
		let mut carrier_routes = HashMap::new();
		let mut serialized_offset = 0_usize;
		for (&ordinal, &decoded_root) in chunk.root_ordinals.iter().zip(model_root.children) {
			if let Some(target_ordinal) = reference_dependency_ordinal(ordinal) {
				let target_index = usize::try_from(target_ordinal)?;
				let expected = envelope
					.nodes
					.get(target_index)
					.context("capture reference dependency target ordinal is invalid")?;
				let decoded = source
					.get_by_ref(decoded_root)
					.context("capture reference dependency root is missing")?;
				ensure!(
					decoded.class.as_str() == expected.class_name
						&& decoded.name == expected.name
						&& decoded.children.is_empty(),
					"capture reference dependency root is not the attested isolated target"
				);
				continue;
			}
			let serialized_index = serialized_start + serialized_offset;
			serialized_offset += 1;
			if ordinal == u32::MAX {
				let indexes = carrier_indexes
					.get(&u32::try_from(serialized_index)?)
					.context("capture chunk has an unattested synthetic carrier root")?;
				let wrapper = source
					.get_by_ref(decoded_root)
					.context("capture shell carrier wrapper is missing")?;
				ensure!(
					wrapper.class.as_str() == "Folder" && wrapper.children.len() == 1,
					"capture shell carrier wrapper shape is invalid"
				);
				let carrier_ref = wrapper.children[0];
				let carrier = source
					.get_by_ref(carrier_ref)
					.context("capture shell carrier instance is missing")?;
				for &carrier_index in indexes {
					let route = &envelope.shell_carriers[carrier_index];
					ensure!(
						carrier.class.as_str() == route.carrier_class,
						"capture shell carrier class disagrees with its envelope"
					);
					let owner = self.ordinal_refs[usize::try_from(route.owner_ordinal)?];
					let class = &envelope.nodes[route.owner_ordinal as usize].class_name;
					let (name, data_type) = privileged::capture_shell_property_identity(class, &route.property)?;
					ensure!(
						privileged::capture_shell_property_type_matches(
							class,
							&route.property,
							data_type,
							&route.type_name,
						)?,
						"capture shell carrier property type disagrees"
					);
					ensure!(
						carrier_routes.insert((carrier_ref, name), (owner, name)).is_none(),
						"capture shell carrier property is duplicated"
					);
				}
				continue;
			}

			let mut stack = vec![(ordinal, decoded_root)];
			while let Some((native_ordinal, decoded_ref)) = stack.pop() {
				let native_index = usize::try_from(native_ordinal)?;
				ensure!(
					!self.service_shells[native_index],
					"capture component includes a service shell"
				);
				if track_coverage {
					ensure!(
						!std::mem::replace(&mut self.visited[native_index], true),
						"capture native hierarchy ordinal is serialized more than once"
					);
				}
				let expected = &envelope.nodes[native_index];
				let decoded = source
					.get_by_ref(decoded_ref)
					.context("capture RBXM hierarchy contains a missing referent")?;
				ensure!(
					decoded.class.as_str() == expected.class_name && decoded.name == expected.name,
					"capture RBXM hierarchy disagrees at native ordinal {native_ordinal}: expected {} '{}', received {} '{}'",
					expected.class_name,
					expected.name,
					decoded.class,
					decoded.name
				);
				let child_start = self.child_offsets[native_index] as usize;
				let child_end = child_start + self.child_counts[native_index] as usize;
				let expected_count = self.children[child_start..child_end]
					.iter()
					.filter(|child| !self.component_roots[**child as usize])
					.count();
				ensure!(
					decoded.children.len() == expected_count,
					"capture RBXM child count disagrees at {} '{}'",
					expected.class_name,
					expected.name
				);
				persistent.push((decoded_ref, self.ordinal_refs[native_index]));
				let mut decoded_index = decoded.children.len();
				for &child_ordinal in self.children[child_start..child_end].iter().rev() {
					if self.component_roots[child_ordinal as usize] {
						continue;
					}
					decoded_index -= 1;
					stack.push((child_ordinal, decoded.children[decoded_index]));
				}
			}
		}
		Ok(CaptureChunkAlignment {
			persistent,
			carrier_routes,
		})
	}

	fn validate_complete(&self) -> Result<()> {
		for ordinal in 1..self.visited.len() {
			ensure!(
				self.service_shells[ordinal] || self.visited[ordinal],
				"capture native hierarchy omitted serialized node at ordinal {ordinal}"
			);
		}
		Ok(())
	}
}

fn replay_chunk_properties(
	source: &rbx_binary::DecodedArena,
	alignment: &CaptureChunkAlignment,
	sink: &mut dyn rbx_binary::DecodeSink,
	include_carriers: bool,
) -> Result<()> {
	for &(decoded, global) in &alignment.persistent {
		let instance = source
			.get_by_ref(decoded)
			.context("capture decoded property owner is missing")?;
		for (&name, value) in instance.properties {
			sink.property(global, name, value.clone()).map_err(anyhow::Error::msg)?;
		}
	}
	if include_carriers {
		let mut visited = HashSet::new();
		for &(carrier, _) in alignment.carrier_routes.keys() {
			if !visited.insert(carrier) {
				continue;
			}
			let instance = source
				.get_by_ref(carrier)
				.context("capture decoded shell carrier is missing")?;
			for (&name, value) in instance.properties {
				sink.property(carrier, name, value.clone())
					.map_err(anyhow::Error::msg)?;
			}
		}
	}
	Ok(())
}

struct DigestSink<'a> {
	arena: &'a mut CaptureArena,
	carriers: &'a HashMap<(Ref, Ustr), (Ref, Ustr)>,
	sideband_references: &'a HashSet<(Ref, Ustr)>,
	authoritative_identities: Option<&'a [Ref]>,
	properties: Option<&'a mut CapturePropertySpools>,
	roundtrip_cframe_observed: &'a mut bool,
}

fn requires_reference_sideband(value: &Variant) -> bool {
	matches!(value, Variant::Ref(target) if target.is_some())
}

impl rbx_binary::DecodeSink for DigestSink<'_> {
	fn property(&mut self, referent: Ref, name: Ustr, value: Variant) -> std::result::Result<(), String> {
		*self.roundtrip_cframe_observed |= matches!(value, Variant::CFrame(_));
		if let Some(&(owner, target_name)) = self.carriers.get(&(referent, name)) {
			self.arena
				.get_mut(owner)
				.map_err(|error| error.to_string())?
				.properties
				.insert(target_name, value.clone());
		}
		let Some(&index) = self.arena.by_ref.get(&referent) else {
			return Ok(());
		};
		if self.sideband_references.contains(&(referent, name)) {
			return Ok(());
		}
		// A nil reference carries no identity and is safe to accept from the
		// serializer. Non-nil references must come from the native sideband so
		// cross-chunk targets cannot be silently lost or misidentified.
		if requires_reference_sideband(&value) {
			return Err(format!(
				"capture reference {}.{} is missing from the native sideband",
				self.arena.studio_path(referent).map_err(|error| error.to_string())?,
				name
			));
		}
		if matches!(value, Variant::Content(ref content) if matches!(content.value(), ContentType::Object(_))) {
			return Err(format!(
				"capture Content.Object {}.{} bypassed the native preflight blocker",
				self.arena.studio_path(referent).map_err(|error| error.to_string())?,
				name
			));
		}
		let class = self.arena.nodes[index].class;
		let Some((name, value)) = normalize_property(class.as_str(), name, value) else {
			return Ok(());
		};
		if name.as_str() == "__CarbonRawName" {
			if let Variant::BinaryString(raw) = &value {
				self.arena.nodes[index].semantic_name =
					semantic_name(&self.arena.nodes[index].name, Some(raw.as_ref()));
			}
			self.arena.nodes[index].properties.insert(name, value);
			return Ok(());
		}
		if is_managed_contract_property(class.as_str(), &self.arena.nodes[index].name, name.as_str()) {
			self.arena.nodes[index].properties.insert(name, value.clone());
			return Ok(());
		}
		artifact_store::validate_capture_property(class.as_str(), name.as_str(), &value, |target| {
			self.arena.by_ref.contains_key(&target)
		})
		.map_err(|error| error.to_string())?;
		if artifact_store::is_omittable_default(class.as_str(), name.as_str(), &value) {
			return Ok(());
		}
		if let (Some(properties), Some(identities)) = (self.properties.as_mut(), self.authoritative_identities) {
			let stable = *identities
				.get(index)
				.ok_or_else(|| "authoritative property identity is unavailable".to_owned())?;
			properties
				.append(stable, name, value)
				.map_err(|error| error.to_string())?;
			return Ok(());
		}
		let property = observed_property(class.as_str(), name, &value).map_err(|error| error.to_string())?;
		self.arena.nodes[index].property_digest.insert(property.digest);
		upsert_observed_property(&mut self.arena.nodes[index], property);
		Ok(())
	}
}

struct FinalPropertySink<'a> {
	arena: &'a CaptureArena,
	remap: &'a HashMap<Ref, Ref>,
	sideband_references: &'a HashSet<(Ref, Ustr)>,
	preserve_records: &'a HashSet<Ref>,
	dirty_buckets: &'a HashSet<usize>,
	spools: &'a mut CapturePropertySpools,
}

impl rbx_binary::DecodeSink for FinalPropertySink<'_> {
	fn property(&mut self, referent: Ref, name: Ustr, mut value: Variant) -> std::result::Result<(), String> {
		if self.sideband_references.contains(&(referent, name)) {
			return Ok(());
		}
		if requires_reference_sideband(&value) {
			return Err(format!(
				"capture reference property {name} is missing from the native sideband"
			));
		}
		if matches!(value, Variant::Content(ref content) if matches!(content.value(), ContentType::Object(_))) {
			return Err(format!(
				"capture Content.Object property {name} bypassed the native preflight blocker"
			));
		}
		let Some(stable) = self.remap.get(&referent).copied() else {
			return Ok(());
		};
		if !artifact_store::partition_for_ref(stable)
			.map(|bucket| self.dirty_buckets.contains(&bucket))
			.map_err(|error| error.to_string())?
		{
			return Ok(());
		}
		let class = self.arena.get(stable).map_err(|error| error.to_string())?.class;
		let Some((name, normalized)) = normalize_property(class.as_str(), name, value) else {
			return Ok(());
		};
		value = normalized;
		// Default columns are regenerated once per dirty partition by the canonical
		// writer. Spooling every reflected default would otherwise turn a flat
		// million-instance capture into millions of redundant records.
		if artifact_store::is_omittable_default(class.as_str(), name.as_str(), &value) {
			return Ok(());
		}
		match &mut value {
			Variant::Ref(target) => {
				if let Some(replacement) = self.remap.get(target) {
					*target = *replacement;
				}
			}
			Variant::Content(content) => {
				if let ContentType::Object(target) = content.value() {
					if let Some(replacement) = self.remap.get(target) {
						*content = Content::from_referent(*replacement);
					}
				}
			}
			_ => {}
		}
		artifact_store::validate_capture_property(class.as_str(), name.as_str(), &value, |target| {
			self.arena.by_ref.contains_key(&target)
		})
		.map_err(|error| {
			format!(
				"capture property {}.{} is not representable: {error:#}",
				self.arena
					.studio_path(stable)
					.unwrap_or_else(|_| format!("<missing {stable}>")),
				name
			)
		})?;
		let target = match &value {
			Variant::Ref(target) if target.is_some() => Some(*target),
			Variant::Content(content) => match content.value() {
				ContentType::Object(target) if target.is_some() => Some(*target),
				_ => None,
			},
			_ => None,
		};
		if let Some(target) = target {
			if self.preserve_records.contains(&stable) && !self.preserve_records.contains(&target) {
				return Err(format!(
					"filesystem-owned reference blocker: {}.{} targets manifest-owned {}",
					self.arena.studio_path(stable).map_err(|error| error.to_string())?,
					name,
					self.arena.studio_path(target).map_err(|error| error.to_string())?
				));
			}
		}
		self.spools
			.append(stable, name, value)
			.map_err(|error| error.to_string())
	}
}

#[cfg(test)]
pub(crate) fn compile(
	input: &Path,
	envelope: &CaptureEnvelope,
	canonical: &Snapshot,
	project_name: &str,
	artifact_path: &Path,
	cancelled: &dyn Fn() -> bool,
) -> Result<CompiledCapture> {
	let baseline = artifact_store::validated_artifact_receipt(artifact_path)?;
	compile_validated(
		input,
		envelope,
		canonical,
		project_name,
		artifact_path,
		&baseline,
		"test-project-generation",
		cancelled,
	)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn compile_validated(
	input: &Path,
	envelope: &CaptureEnvelope,
	canonical: &Snapshot,
	project_name: &str,
	artifact_path: &Path,
	baseline: &artifact_store::ValidatedArtifactReceipt,
	project_generation: &str,
	cancelled: &dyn Fn() -> bool,
) -> Result<CompiledCapture> {
	let total_started = Instant::now();
	ensure!(!cancelled(), "Capture Manifest was cancelled before native decoding");
	let prior_started = Instant::now();
	let capture_fingerprint = envelope.semantic_fingerprint();
	let (prior_name, prior_ids) = if envelope.manifest_identities_authoritative {
		(baseline.name().to_owned(), None)
	} else {
		let referents = artifact_store::artifact_referents_cancellable(artifact_path, cancelled)?;
		(referents.name().to_owned(), Some(referents))
	};
	let prior_elapsed = prior_started.elapsed();
	let structure_started = Instant::now();
	let artifact = CaptureModelArtifact::open(input)?;
	artifact.validate_root_order(&envelope.serialized_root_ordinals)?;
	let (mut arena, ordinal_refs) = CaptureArena::from_envelope(envelope)?;
	let mut topology = CaptureTopology::new(envelope, &artifact, ordinal_refs)?;
	let default_hydrated_services = envelope
		.roots
		.iter()
		.filter(|root| {
			envelope.nodes[root.hierarchy_ordinal as usize].flags & CAPTURE_HIERARCHY_FLAG_DEFAULT_HYDRATED_SERVICE != 0
		})
		.map(|root| topology.ordinal_refs[root.hierarchy_ordinal as usize])
		.collect::<Vec<_>>();
	let data_model = arena.root;
	let structure_elapsed = structure_started.elapsed();
	let topology_started = Instant::now();
	ensure!(!cancelled(), "Capture Manifest was cancelled during hierarchy decoding");
	{
		let root = arena.get_mut(data_model)?;
		root.class = canonical.class;
		root.name = canonical.name.clone();
	}

	let mut exact = HashMap::with_capacity(envelope.nodes.len() + envelope.mapped_bindings.len());
	exact.insert(data_model, canonical.id);
	let mut managed_records = HashSet::from([canonical.id]);
	for root in &envelope.roots {
		let ordinal = usize::try_from(root.hierarchy_ordinal)?;
		let canonical_anchor = capture_service_anchor(canonical, &root.class_name, &root.name)?;
		let anchor = if envelope.manifest_identities_authoritative
			&& !canonical.children.iter().any(|child| child.id == canonical_anchor)
		{
			// The native ledger owns the identity of a service first materialized
			// after the prior canonical snapshot. Existing service shells still use
			// their canonical identity and are checked below for ledger drift.
			*envelope
				.manifest_identities
				.get(ordinal)
				.context("capture manifest identity is missing for a new service anchor")?
		} else {
			canonical_anchor
		};
		let observed = topology.ordinal_refs[ordinal];
		ensure!(
			arena.get(observed)?.parent == data_model,
			"capture service shell is not parented to DataModel"
		);
		exact.insert(observed, anchor);
		managed_records.insert(anchor);
	}
	for root in &envelope.roots {
		let shell = topology.ordinal_refs[root.hierarchy_ordinal as usize];
		let start = usize::try_from(root.first_serialized_root)?;
		let end = start + usize::try_from(root.serialized_root_count)?;
		for index in start..end {
			let ordinal = envelope.serialized_root_ordinals[index];
			ensure!(ordinal != u32::MAX, "capture service range contains a carrier");
			let child = topology.ordinal_refs[ordinal as usize];
			ensure!(
				arena.get(child)?.parent == shell,
				"capture direct serialized root is not parented to its service shell"
			);
		}
	}

	for property in &envelope.shell_properties {
		let owner = topology.ordinal_refs[property.owner_ordinal as usize];
		let class = &envelope.nodes[property.owner_ordinal as usize].class_name;
		let (name, data_type) = privileged::capture_shell_property_identity(class, &property.property)?;
		ensure!(
			privileged::capture_shell_property_type_matches(class, &property.property, data_type, &property.type_name)?,
			"capture shell property type disagrees for {class}.{}",
			property.property
		);
		let value = privileged::decode_capture_shell_property(data_type, &property.value)?;
		arena.get_mut(owner)?.properties.insert(name, value);
	}
	let shell_ordinals = envelope
		.roots
		.iter()
		.map(|root| root.hierarchy_ordinal)
		.chain(std::iter::once(0))
		.collect::<HashSet<_>>();
	let mut sideband_reference_targets = HashMap::with_capacity(envelope.external_references.len());
	let mut referenced_mapped_refs = HashSet::new();
	let mut omitted_non_persistent_references = HashSet::<(&str, &str)>::new();
	for reference in &envelope.external_references {
		let owner = topology.ordinal_refs[reference.owner_ordinal as usize];
		let class = &envelope.nodes[reference.owner_ordinal as usize].class_name;
		let name = if shell_ordinals.contains(&reference.owner_ordinal) {
			let (name, data_type) = privileged::capture_shell_property_identity(class, &reference.property)?;
			ensure!(
				data_type == rbx_dom_weak::types::VariantType::Ref,
				"capture shell sideband is not a Ref property"
			);
			name
		} else {
			let canonical = artifact_store::canonical_property_name(class, &reference.property)
				.with_context(|| format!("capture reference descriptor {class}.{} is unknown", reference.property))?;
			ensure!(
				artifact_store::canonical_variant_type(class, canonical) == Some(rbx_dom_weak::types::VariantType::Ref),
				"capture sideband {class}.{} is not a Ref property",
				reference.property
			);
			let serializes = artifact_store::canonical_property_serializes(class, canonical)
				.with_context(|| format!("capture canonical reference descriptor {class}.{canonical} is unknown"))?;
			if !serializes {
				omitted_non_persistent_references.insert((class.as_str(), reference.property.as_str()));
				continue;
			}
			Ustr::from(canonical)
		};
		let target = match reference.target {
			CaptureReferenceTarget::Null => Ref::none(),
			CaptureReferenceTarget::Ordinal(ordinal) => topology.ordinal_refs[ordinal as usize],
			CaptureReferenceTarget::Mapped(target) => {
				referenced_mapped_refs.insert(target);
				target
			}
		};
		if let Some(existing) = sideband_reference_targets.get(&(owner, name)) {
			ensure!(
				*existing == target,
				"capture native sideband conflicts for {class}.{}",
				reference.property
			);
			continue;
		}
		sideband_reference_targets.insert((owner, name), target);
		arena.get_mut(owner)?.properties.insert(name, Variant::Ref(target));
	}
	if !omitted_non_persistent_references.is_empty() {
		let mut descriptors = omitted_non_persistent_references.iter().copied().collect::<Vec<_>>();
		descriptors.sort_unstable();
		let omitted = descriptors.len();
		let details = descriptors
			.iter()
			.take(8)
			.map(|(class, property)| format!("{class}.{property}"))
			.collect::<Vec<_>>()
			.join(", ");
		log::debug!(
			"Capture omitted {omitted} non-persistent reference descriptor{} that Studio cannot round-trip: {details}{}",
			if omitted == 1 { "" } else { "s" },
			if omitted > 8 { ", ..." } else { "" }
		);
	}
	let sideband_references = sideband_reference_targets.keys().copied().collect::<HashSet<_>>();
	let topology_elapsed = topology_started.elapsed();
	let digest_properties_started = Instant::now();
	let mut carrier_indexes = HashMap::<u32, Vec<usize>>::new();
	for (index, carrier) in envelope.shell_carriers.iter().enumerate() {
		carrier_indexes
			.entry(carrier.serialized_root_index)
			.or_default()
			.push(index);
	}
	let mut serialized_start = 0_usize;
	let mut authoritative_properties = if envelope.manifest_identities_authoritative {
		let spool_dir = input
			.parent()
			.unwrap_or_else(|| Path::new("."))
			.join(format!(".carbon-capture-properties-{}", uuid::Uuid::new_v4()));
		Some(CapturePropertySpools::new(spool_dir)?)
	} else {
		None
	};
	let mut roundtrip_cframe_observed = false;
	for (chunk_index, chunk) in artifact.chunks.iter().enumerate() {
		ensure!(!cancelled(), "Capture Manifest was cancelled during chunk decoding");
		let source = rbx_binary::Deserializer::new()
			.strict(true)
			.skip_known_non_serializing_properties(true)
			.reflection_database(util::get_reflection_database())
			.deserialize_source(capture_reader(artifact.open_chunk(input, chunk_index)?, cancelled))?;
		let alignment = topology.align_chunk(envelope, chunk, serialized_start, &source, &carrier_indexes, true)?;
		let mut sink = DigestSink {
			arena: &mut arena,
			carriers: &alignment.carrier_routes,
			sideband_references: &sideband_references,
			authoritative_identities: envelope
				.manifest_identities_authoritative
				.then_some(envelope.manifest_identities.as_slice()),
			properties: authoritative_properties.as_mut(),
			roundtrip_cframe_observed: &mut roundtrip_cframe_observed,
		};
		replay_chunk_properties(&source, &alignment, &mut sink, true)?;
		serialized_start += chunk
			.root_ordinals
			.iter()
			.filter(|ordinal| reference_dependency_ordinal(**ordinal).is_none())
			.count();
	}
	ensure!(
		serialized_start == envelope.serialized_root_ordinals.len(),
		"capture chunk root count changed during decoding"
	);
	topology.validate_complete()?;
	let digest_properties_elapsed = digest_properties_started.elapsed();
	let inline_digest_started = Instant::now();
	for carrier in &envelope.shell_carriers {
		let owner = topology.ordinal_refs[carrier.owner_ordinal as usize];
		let class = &envelope.nodes[carrier.owner_ordinal as usize].class_name;
		let (name, _) = privileged::capture_shell_property_identity(class, &carrier.property)?;
		ensure!(
			arena.get(owner)?.properties.contains_key(&name),
			"capture shell carrier omitted {class}.{}",
			carrier.property
		);
	}
	arena.normalize_properties();
	arena.digest_inline_properties(cancelled)?;
	roundtrip_cframe_observed |= arena.nodes.iter().any(|node| {
		node.properties
			.values()
			.any(|value| matches!(value, Variant::CFrame(_)))
	});
	let inline_digest_elapsed = inline_digest_started.elapsed();

	let mapped_graft_started = Instant::now();
	let canonical_nodes = (!envelope.mapped_bindings.is_empty()).then(|| canonical_index(canonical));
	for binding in &envelope.mapped_bindings {
		let source_id: Ref = binding
			.source_id
			.parse()
			.context("capture mapped identity is invalid")?;
		let source = canonical_nodes
			.as_ref()
			.expect("mapped capture has a canonical index")
			.get(&source_id)
			.with_context(|| format!("capture mapped identity {source_id} is absent"))?;
		let parent = topology.ordinal_refs[binding.parent_ordinal as usize];
		arena.insert_snapshot(parent, source.snapshot)?;
	}

	for binding in &envelope.mapped_bindings {
		let source_id: Ref = binding.source_id.parse()?;
		let canonical_nodes = canonical_nodes.as_ref().expect("mapped capture has a canonical index");
		let mut canonical_id = canonical_nodes[&source_id]
			.parent
			.context("capture mapped root has no canonical parent")?;
		let mut ordinal = binding.parent_ordinal;
		loop {
			let observed = topology.ordinal_refs[ordinal as usize];
			if let Some(previous) = exact.get(&observed) {
				ensure!(
					*previous == canonical_id,
					"capture routing identity conflicts with canonical state"
				);
				break;
			}
			let native = &envelope.nodes[ordinal as usize];
			let canonical_node = &canonical_nodes[&canonical_id];
			ensure!(
				native.class_name == canonical_node.snapshot.class.as_str()
					&& native.name == canonical_node.snapshot.name,
				"capture mapped routing ancestor changed"
			);
			exact.insert(observed, canonical_id);
			managed_records.insert(canonical_id);
			match (native.parent_ordinal, canonical_node.parent) {
				(u32::MAX, None) => break,
				(native_parent, Some(canonical_parent)) => {
					ordinal = native_parent;
					canonical_id = canonical_parent;
				}
				_ => bail!("capture mapped routing chains disagree"),
			}
		}
	}
	let mut preserve_records = HashSet::new();
	for binding in &envelope.mapped_bindings {
		let source_id: Ref = binding.source_id.parse()?;
		let mut stack = vec![source_id];
		while let Some(id) = stack.pop() {
			exact.insert(id, id);
			preserve_records.insert(id);
			managed_records.insert(id);
			stack.extend(arena.get(id)?.children.iter().rev().copied());
		}
	}
	for target in &referenced_mapped_refs {
		ensure!(
			preserve_records.contains(target),
			"capture mapped-reference identity {target} is absent from canonical mapped source"
		);
	}
	validate_canonical_domains(canonical, &preserve_records)?;
	let mapped_graft_elapsed = mapped_graft_started.elapsed();
	let reconcile_started = Instant::now();
	if envelope.manifest_identities_authoritative {
		apply_authoritative_identities(
			&arena,
			&topology.ordinal_refs,
			&envelope.manifest_identities,
			&mut exact,
		)?;
	} else {
		reconcile_identities(
			&mut arena,
			prior_ids
				.as_ref()
				.context("adoption capture identity inventory is unavailable")?,
			artifact_path,
			&mut exact,
			cancelled,
		)?;
	}
	let reconcile_elapsed = reconcile_started.elapsed();

	let finalize_started = Instant::now();
	let remap = arena.assign_stable_referents(exact, cancelled)?;
	let identity_remap = if !envelope.manifest_identities_authoritative
		&& envelope.manifest_identities.len() == topology.ordinal_refs.len()
	{
		envelope
			.manifest_identities
			.iter()
			.zip(&topology.ordinal_refs)
			.map(|(captured_id, observed)| ManifestIdentityRemap {
				captured_id: *captured_id,
				manifest_id: remap[observed],
			})
			.collect()
	} else {
		Vec::new()
	};
	let referenced = arena
		.nodes
		.iter()
		.flat_map(|node| node.properties.values())
		.filter_map(property_target)
		.collect::<HashSet<_>>();
	for observed in default_hydrated_services {
		let stable = remap[&observed];
		if arena.get(stable)?.children.is_empty() && !referenced.contains(&stable) {
			arena.remove_leaf(stable)?;
			managed_records.remove(&stable);
		}
	}
	let projected_tree = arena.projected_tree(&managed_records, cancelled)?;
	for node in &arena.nodes {
		for (name, value) in &node.properties {
			artifact_store::validate_capture_property(node.class.as_str(), name.as_str(), value, |target| {
				arena.by_ref.contains_key(&target)
			})
			.with_context(|| {
				format!(
					"capture property {}.{} is not representable",
					arena
						.studio_path(node.referent)
						.unwrap_or_else(|_| format!("<missing {}>", node.referent)),
					name
				)
			})?;
			if let Some(target) = property_target(value) {
				ensure!(
					!preserve_records.contains(&node.referent) || preserve_records.contains(&target),
					"filesystem-owned reference blocker: {}.{} targets manifest-owned {}",
					arena.studio_path(node.referent)?,
					name,
					arena.studio_path(target)?
				);
			}
		}
	}
	let finalize_elapsed = finalize_started.elapsed();
	let noop_evidence_started = Instant::now();
	let (semantic_noop, stage_plan) = if envelope.manifest_identities_authoritative {
		// The canonical representation is one atomic artifact. Once the sealed
		// fingerprint differs, derive the replacement wholly from the fresh native
		// snapshot and the currently attested mapped graft; prior artifact records
		// are neither necessary nor authoritative evidence.
		(false, artifact_store::CaptureStagePlan::fresh())
	} else {
		let mut dirty_buckets = arena.dirty_buckets_against_artifact(artifact_path, cancelled)?;
		for referent in &preserve_records {
			dirty_buckets.insert(artifact_store::partition_for_ref(*referent)?);
		}
		let unchanged = preserve_records.is_empty() && prior_name == project_name && dirty_buckets.is_empty();
		let stage_plan = if unchanged {
			artifact_store::CaptureStagePlan::semantic_noop(artifact_path)?
		} else {
			artifact_store::CaptureStagePlan::from_artifact_cancellable(artifact_path, dirty_buckets, cancelled)?
		};
		(unchanged, stage_plan)
	};
	let noop_evidence_elapsed = noop_evidence_started.elapsed();
	let final_properties_started = Instant::now();
	let mut properties = if stage_plan.dirty_buckets().is_empty() {
		None
	} else if let Some(properties) = authoritative_properties {
		Some(properties)
	} else {
		let spool_dir: PathBuf = input
			.parent()
			.unwrap_or_else(|| Path::new("."))
			.join(format!(".carbon-capture-properties-{}", uuid::Uuid::new_v4()));
		let mut properties = CapturePropertySpools::new(spool_dir)?;
		{
			let mut sink = FinalPropertySink {
				arena: &arena,
				remap: &remap,
				sideband_references: &sideband_references,
				preserve_records: &preserve_records,
				dirty_buckets: stage_plan.dirty_buckets(),
				spools: &mut properties,
			};
			let mut serialized_start = 0_usize;
			for (chunk_index, chunk) in artifact.chunks.iter().enumerate() {
				ensure!(
					!cancelled(),
					"Capture Manifest was cancelled during final chunk decoding"
				);
				let source = rbx_binary::Deserializer::new()
					.strict(true)
					.skip_known_non_serializing_properties(true)
					.reflection_database(util::get_reflection_database())
					.deserialize_source(capture_reader(artifact.open_chunk(input, chunk_index)?, cancelled))?;
				let alignment =
					topology.align_chunk(envelope, chunk, serialized_start, &source, &carrier_indexes, false)?;
				replay_chunk_properties(&source, &alignment, &mut sink, false)?;
				serialized_start += chunk
					.root_ordinals
					.iter()
					.filter(|ordinal| reference_dependency_ordinal(**ordinal).is_none())
					.count();
			}
		}
		Some(properties)
	};
	if let Some(properties) = properties.as_mut() {
		for id in &preserve_records {
			ensure!(
				!cancelled(),
				"Capture Manifest was cancelled during mapped property staging"
			);
			let snapshot = canonical_nodes.as_ref().expect("mapped capture has a canonical index")[id].snapshot;
			for (name, value) in &snapshot.properties {
				properties.append(*id, *name, value.clone())?;
			}
		}
		properties.finish()?;
	}
	let canonical_cframes = if envelope.manifest_identities_authoritative && roundtrip_cframe_observed {
		let spool_dir = input
			.parent()
			.unwrap_or_else(|| Path::new("."))
			.join(format!(".carbon-capture-canonical-cframes-{}", uuid::Uuid::new_v4()));
		Some(artifact_store::canonical_cframe_spools(
			artifact_path,
			spool_dir,
			cancelled,
		)?)
	} else {
		None
	};
	let final_properties_elapsed = final_properties_started.elapsed();
	crate::carbon_info!(
		"Capture Manifest compile internals: prior-manifest={:.1}ms, structure={:.1}ms, topology-sideband={:.1}ms, digest-property-pass={:.1}ms, inline-digest={:.1}ms, mapped-graft={:.1}ms, identity-reconcile={:.1}ms, finalize-validate={:.1}ms, sparse-stage-evidence={:.1}ms, dirty-buckets={}, final-property-pass={:.1}ms, semantic-noop={}, total={:.1}ms",
		prior_elapsed.as_secs_f64() * 1_000.0,
		structure_elapsed.as_secs_f64() * 1_000.0,
		topology_elapsed.as_secs_f64() * 1_000.0,
		digest_properties_elapsed.as_secs_f64() * 1_000.0,
		inline_digest_elapsed.as_secs_f64() * 1_000.0,
		mapped_graft_elapsed.as_secs_f64() * 1_000.0,
		reconcile_elapsed.as_secs_f64() * 1_000.0,
		finalize_elapsed.as_secs_f64() * 1_000.0,
		noop_evidence_elapsed.as_secs_f64() * 1_000.0,
		stage_plan.dirty_buckets().len(),
		final_properties_elapsed.as_secs_f64() * 1_000.0,
		semantic_noop,
		total_started.elapsed().as_secs_f64() * 1_000.0,
	);
	let mut metadata = baseline.metadata().clone();
	metadata.insert(
		artifact_store::CAPTURE_PROJECT_GENERATION_METADATA_KEY.to_owned(),
		project_generation.to_owned(),
	);
	Ok(CompiledCapture {
		arena,
		projected_tree,
		preserve_records,
		properties,
		canonical_cframes,
		stage_plan,
		semantic_noop,
		capture_fingerprint,
		metadata,
		identity_remap,
		referenced_mapped_refs,
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use sha2::Digest;
	use std::collections::BTreeMap;
	use std::fs::File;
	use std::io::Cursor;
	use std::time::Instant;

	fn folder(id: Ref, name: &str) -> Snapshot {
		Snapshot::new().with_id(id).with_name(name).with_class("Folder")
	}

	fn sequential_ref(value: u128) -> Ref {
		format!("{value:032x}").parse().unwrap()
	}

	fn framed_capture(chunks: &[(Vec<u32>, Vec<u8>)]) -> Vec<u8> {
		let mut artifact = b"CARBONCM2".to_vec();
		artifact.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
		for (roots, payload) in chunks {
			artifact.extend_from_slice(&(roots.len() as u32).to_le_bytes());
			for root in roots {
				artifact.extend_from_slice(&root.to_le_bytes());
			}
			artifact.extend_from_slice(&(payload.len() as u64).to_le_bytes());
			artifact.extend_from_slice(payload);
		}
		artifact
	}

	#[test]
	fn only_non_nil_references_require_native_sideband() {
		assert!(!requires_reference_sideband(&Variant::Ref(Ref::none())));
		assert!(requires_reference_sideband(&Variant::Ref(Ref::new())));
	}

	fn prior_artifact(snapshot: &Snapshot, label: &str) -> (PathBuf, artifact_store::ArtifactReferents) {
		let directory = std::env::temp_dir().join(format!("carbon-capture-prior-{label}-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&directory).unwrap();
		let manifest = directory.join("prior.carbon.json");
		artifact_store::extract_snapshot(snapshot.clone(), "Prior".to_owned(), &manifest).unwrap();
		let referents = artifact_store::artifact_referents(&manifest).unwrap();
		(manifest, referents)
	}

	fn exact_evidence_fixture(
		value: Variant,
	) -> (CaptureArena, HashMap<Ref, artifact_store::PriorIdentityNode>, Ref, Ref) {
		let root = stable_ref("capture-exact-evidence-root");
		let child = stable_ref("capture-exact-evidence-child");
		let property_name = Ustr::from("Value");
		let class = Ustr::from("CaptureExactEvidenceNode");
		let nodes = vec![
			CaptureNode {
				referent: root,
				parent: Ref::none(),
				class,
				name: "Root".to_owned(),
				properties: UstrMap::default(),
				children: vec![child],
				property_digest: DigestMultiset::default(),
				observed_properties: Vec::new(),
				digest: [0; 32],
				semantic_name: semantic_name("Root", None),
			},
			CaptureNode {
				referent: child,
				parent: root,
				class,
				name: "Child".to_owned(),
				properties: UstrMap::default(),
				children: Vec::new(),
				property_digest: DigestMultiset::default(),
				observed_properties: vec![observed_property(class.as_str(), property_name, &value).unwrap()],
				digest: [0; 32],
				semantic_name: semantic_name("Child", None),
			},
		];
		let arena = CaptureArena {
			by_ref: nodes
				.iter()
				.enumerate()
				.map(|(index, node)| (node.referent, index))
				.collect(),
			nodes,
			root,
		};
		let prior = HashMap::from([
			(
				root,
				artifact_store::PriorIdentityNode {
					id: root,
					parent: None,
					class,
					name: "Root".to_owned(),
					raw_name: None,
					properties: UstrMap::default(),
					children: vec![child],
				},
			),
			(
				child,
				artifact_store::PriorIdentityNode {
					id: child,
					parent: Some(root),
					class,
					name: "Child".to_owned(),
					raw_name: None,
					properties: UstrMap::from_iter([(property_name, value)]),
					children: Vec::new(),
				},
			),
		]);
		(arena, prior, root, child)
	}

	#[test]
	fn native_decode_reader_interrupts_immediately_when_capture_is_cancelled() {
		let mut reader = CancellableReader {
			inner: Cursor::new(vec![1_u8; 1024]),
			cancelled: &|| true,
		};
		let mut output = [0_u8; 32];
		let error = reader.read(&mut output).unwrap_err();
		assert_eq!(error.kind(), io::ErrorKind::Other);
		assert_eq!(output, [0_u8; 32]);
	}

	#[test]
	fn exact_noop_evidence_accepts_canonical_color_encoding() {
		use rbx_dom_weak::types::{Color3, Color3uint8};

		let name = Ustr::from("Color");
		let packed = Variant::Color3uint8(Color3uint8::new(163, 162, 165));
		let canonical = Variant::Color3(Color3::new(163.0 / 255.0, 162.0 / 255.0, 165.0 / 255.0));
		assert_eq!(
			exact_property_digest("Terrain", &name, &packed).unwrap(),
			exact_property_digest("Terrain", &name, &canonical).unwrap()
		);
	}

	#[test]
	fn exact_noop_evidence_rejects_property_reference_identity_and_topology_changes() {
		let (arena, prior, root, child) = exact_evidence_fixture(Variant::Bool(true));
		let root_bucket = artifact_store::partition_for_ref(root).unwrap();
		let child_bucket = artifact_store::partition_for_ref(child).unwrap();
		assert!(arena.exactly_matches_prior(&prior, &|| false).unwrap());

		let mut changed_property = prior.clone();
		changed_property
			.get_mut(&child)
			.unwrap()
			.properties
			.insert(Ustr::from("Value"), Variant::Bool(false));
		assert!(!arena.exactly_matches_prior(&changed_property, &|| false).unwrap());
		assert_eq!(
			arena.dirty_buckets_against_prior(&changed_property, &|| false).unwrap(),
			HashSet::from([child_bucket])
		);

		let (reference_arena, mut changed_reference, _, reference_child) = exact_evidence_fixture(Variant::Ref(child));
		assert!(reference_arena
			.exactly_matches_prior(&changed_reference, &|| false)
			.unwrap());
		changed_reference
			.get_mut(&reference_child)
			.unwrap()
			.properties
			.insert(Ustr::from("Value"), Variant::Ref(root));
		assert!(!reference_arena
			.exactly_matches_prior(&changed_reference, &|| false)
			.unwrap());
		assert_eq!(
			reference_arena
				.dirty_buckets_against_prior(&changed_reference, &|| false)
				.unwrap(),
			HashSet::from([child_bucket])
		);

		let mut changed_identity = prior.clone();
		changed_identity.get_mut(&child).unwrap().name = "Renamed".to_owned();
		assert!(!arena.exactly_matches_prior(&changed_identity, &|| false).unwrap());
		assert_eq!(
			arena.dirty_buckets_against_prior(&changed_identity, &|| false).unwrap(),
			HashSet::from([child_bucket])
		);

		let mut changed_raw_name = prior.clone();
		changed_raw_name.get_mut(&child).unwrap().raw_name = Some(serde_bytes::ByteBuf::from(b"Child\0".to_vec()));
		assert_eq!(
			arena.dirty_buckets_against_prior(&changed_raw_name, &|| false).unwrap(),
			HashSet::from([child_bucket])
		);

		let mut missing_prior_child = prior.clone();
		missing_prior_child.remove(&child);
		missing_prior_child.get_mut(&root).unwrap().children.clear();
		assert_eq!(
			arena
				.dirty_buckets_against_prior(&missing_prior_child, &|| false)
				.unwrap(),
			HashSet::from([root_bucket, child_bucket])
		);

		let (mut deletion_arena, deletion_prior, _, _) = exact_evidence_fixture(Variant::Bool(true));
		deletion_arena.get_mut(root).unwrap().children.clear();
		deletion_arena.nodes.retain(|node| node.referent != child);
		deletion_arena.rebuild_index().unwrap();
		assert_eq!(
			deletion_arena
				.dirty_buckets_against_prior(&deletion_prior, &|| false)
				.unwrap(),
			HashSet::from([root_bucket, child_bucket])
		);
	}

	#[test]
	fn sparse_stage_evidence_dirties_reparented_record_and_both_parent_buckets() {
		let root = stable_ref("sparse-reparent-root");
		let old_parent = stable_ref("sparse-reparent-old-parent");
		let new_parent = stable_ref("sparse-reparent-new-parent");
		let child = stable_ref("sparse-reparent-child");
		let class = Ustr::from("CaptureSparseTopologyNode");
		let capture_node = |referent: Ref, parent: Ref, name: &str, children: Vec<Ref>| CaptureNode {
			referent,
			parent,
			class,
			name: name.to_owned(),
			properties: UstrMap::default(),
			children,
			property_digest: DigestMultiset::default(),
			observed_properties: Vec::new(),
			digest: [0; 32],
			semantic_name: semantic_name(name, None),
		};
		let nodes = vec![
			capture_node(root, Ref::none(), "Root", vec![old_parent, new_parent]),
			capture_node(old_parent, root, "Old", Vec::new()),
			capture_node(new_parent, root, "New", vec![child]),
			capture_node(child, new_parent, "Child", Vec::new()),
		];
		let arena = CaptureArena {
			by_ref: nodes
				.iter()
				.enumerate()
				.map(|(index, node)| (node.referent, index))
				.collect(),
			nodes,
			root,
		};
		let prior_node =
			|id: Ref, parent: Option<Ref>, name: &str, children: Vec<Ref>| artifact_store::PriorIdentityNode {
				id,
				parent,
				class,
				name: name.to_owned(),
				raw_name: None,
				properties: UstrMap::default(),
				children,
			};
		let prior = HashMap::from([
			(root, prior_node(root, None, "Root", vec![old_parent, new_parent])),
			(old_parent, prior_node(old_parent, Some(root), "Old", vec![child])),
			(new_parent, prior_node(new_parent, Some(root), "New", Vec::new())),
			(child, prior_node(child, Some(old_parent), "Child", Vec::new())),
		]);
		assert_eq!(
			arena.dirty_buckets_against_prior(&prior, &|| false).unwrap(),
			HashSet::from([
				artifact_store::partition_for_ref(old_parent).unwrap(),
				artifact_store::partition_for_ref(new_parent).unwrap(),
				artifact_store::partition_for_ref(child).unwrap(),
			])
		);
	}

	fn linux_peak_rss_kib() -> Option<u64> {
		let status = std::fs::read_to_string("/proc/self/status").ok()?;
		status
			.lines()
			.find_map(|line| line.strip_prefix("VmHWM:")?.split_whitespace().next()?.parse().ok())
	}

	#[test]
	fn capture_preserves_manifest_project_name_instead_of_datamodel_instance_name() {
		use crate::capture_provider::CaptureHierarchyNode;

		let canonical = Snapshot::new()
			.with_id(stable_ref("capture-project-name-root"))
			.with_name("DataModel")
			.with_class("DataModel");
		let directory = std::env::temp_dir().join(format!("carbon-capture-project-name-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&directory).unwrap();
		let manifest = directory.join("game.carbon.json");
		artifact_store::extract_snapshot(canonical.clone(), "CarbonScale40000".to_owned(), &manifest).unwrap();

		let payload = directory.join("capture.rbxm");
		let mut bytes = b"CARBONCM2".to_vec();
		bytes.extend_from_slice(&0_u32.to_le_bytes());
		std::fs::write(&payload, bytes).unwrap();
		let envelope = CaptureEnvelope {
			capture_id: "capture-project-name".to_owned(),
			engine_generation: 1,
			hierarchy_sequence_before: 1,
			hierarchy_sequence_after: 1,
			change_sequence_before: 1,
			change_sequence_after: 1,
			model_bytes: 0,
			model_digest: [0; 32],
			studio_session_id: "studio-session".to_owned(),
			instance_id: "instance".to_owned(),
			managed_contract_id: "contract".to_owned(),
			reflection_schema_hash: "schema".to_owned(),
			source_generation: "generation".to_owned(),
			digest_algorithm: "sha256".to_owned(),
			manifest_identities_authoritative: false,
			manifest_identities: Vec::new(),
			nodes: vec![CaptureHierarchyNode {
				parent_ordinal: u32::MAX,
				class_name: Ustr::from("DataModel"),
				name: "DataModel".to_owned(),
				flags: 0,
			}],
			roots: Vec::new(),
			mapped_bindings: Vec::new(),
			external_references: Vec::new(),
			shell_properties: Vec::new(),
			shell_carriers: Vec::new(),
			serialized_root_ordinals: Vec::new(),
		};

		let compiled = compile(&payload, &envelope, &canonical, "CarbonScale40000", &manifest, &|| {
			false
		})
		.unwrap();
		assert_eq!(compiled.arena.get(compiled.arena.root_ref()).unwrap().name, "DataModel");
		let (_, staged, _) = compiled
			.stage_composite("CarbonScale40000".to_owned(), &manifest, &|| false)
			.unwrap();
		assert_eq!(
			artifact_store::inspect(staged.artifact()).unwrap().name,
			"CarbonScale40000"
		);
		drop(staged);
		std::fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn authoritative_exact_fingerprint_classifies_without_model_decode() {
		use crate::capture_provider::CaptureHierarchyNode;

		let root_id = stable_ref("authoritative-fingerprint-root");
		let canonical = Snapshot::new()
			.with_id(root_id)
			.with_name("DataModel")
			.with_class("DataModel");
		let directory = std::env::temp_dir().join(format!("carbon-authoritative-fingerprint-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&directory).unwrap();
		let manifest = directory.join("game.carbon.json");
		let artifact = framed_capture(&[]);
		let envelope = CaptureEnvelope {
			capture_id: "authoritative-fingerprint".to_owned(),
			engine_generation: 1,
			hierarchy_sequence_before: 1,
			hierarchy_sequence_after: 1,
			change_sequence_before: 1,
			change_sequence_after: 1,
			model_bytes: artifact.len() as u64,
			model_digest: sha2::Sha256::digest(&artifact).into(),
			studio_session_id: "studio-session".to_owned(),
			instance_id: "instance".to_owned(),
			managed_contract_id: "contract".to_owned(),
			reflection_schema_hash: "schema".to_owned(),
			source_generation: "generation".to_owned(),
			digest_algorithm: "sha256".to_owned(),
			manifest_identities_authoritative: true,
			manifest_identities: vec![root_id],
			nodes: vec![CaptureHierarchyNode {
				parent_ordinal: u32::MAX,
				class_name: Ustr::from("DataModel"),
				name: "DataModel".to_owned(),
				flags: crate::capture_provider::CAPTURE_HIERARCHY_FLAG_SERVICE_SHELL,
			}],
			roots: Vec::new(),
			mapped_bindings: Vec::new(),
			external_references: Vec::new(),
			shell_properties: Vec::new(),
			shell_carriers: Vec::new(),
			serialized_root_ordinals: Vec::new(),
		};
		let fingerprint = envelope.semantic_fingerprint();
		artifact_store::extract_snapshot_with_metadata(
			canonical.clone(),
			"Game".to_owned(),
			BTreeMap::from([
				(artifact_store::CAPTURE_FINGERPRINT_METADATA_KEY.to_owned(), fingerprint),
				(
					artifact_store::CAPTURE_PROJECT_GENERATION_METADATA_KEY.to_owned(),
					"project-generation".to_owned(),
				),
			]),
			&manifest,
		)
		.unwrap();
		let exact = classify_validated_capture(&envelope, "Game", "project-generation", &manifest).unwrap();
		assert!(matches!(exact, ValidatedCaptureClass::ExactNoop(_)));
		let project_changed =
			classify_validated_capture(&envelope, "Game", "new-project-generation", &manifest).unwrap();
		assert!(matches!(project_changed, ValidatedCaptureClass::Rebuild(_)));
		let project_renamed =
			classify_validated_capture(&envelope, "RenamedGame", "project-generation", &manifest).unwrap();
		assert!(matches!(project_renamed, ValidatedCaptureClass::Rebuild(_)));

		artifact_store::extract_snapshot_with_metadata(
			canonical,
			"Game".to_owned(),
			BTreeMap::from([
				("CarbonCaptureFingerprintV1".to_owned(), envelope.semantic_fingerprint()),
				(
					artifact_store::CAPTURE_PROJECT_GENERATION_METADATA_KEY.to_owned(),
					"project-generation".to_owned(),
				),
			]),
			&manifest,
		)
		.unwrap();
		let legacy = classify_validated_capture(&envelope, "Game", "project-generation", &manifest).unwrap();
		assert!(matches!(legacy, ValidatedCaptureClass::Rebuild(_)));

		std::fs::write(&manifest, b"corrupt active capture artifact").unwrap();
		assert!(classify_validated_capture(&envelope, "Game", "project-generation", &manifest).is_err());
		std::fs::remove_dir_all(directory).unwrap();
	}

	fn authoritative_timer_service_capture(default_hydrated: bool, has_child: bool) -> (CompiledCapture, PathBuf, Ref) {
		use crate::capture_provider::{CaptureHierarchyNode, CaptureServiceRoot};
		use rbx_dom_weak::{InstanceBuilder, WeakDom};

		let root_id = stable_ref("authoritative-new-service-root");
		let native_service_id = sequential_ref(101);
		let canonical = Snapshot::new()
			.with_id(root_id)
			.with_name("DataModel")
			.with_class("DataModel");
		let directory = std::env::temp_dir().join(format!("carbon-authoritative-new-service-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&directory).unwrap();
		let manifest = directory.join("game.carbon.json");
		artifact_store::extract_snapshot(canonical.clone(), "Game".to_owned(), &manifest).unwrap();

		let payload = directory.join("capture.rbxm");
		let (capture_bytes, child_node, child_identity, serialized_root_ordinals) = if has_child {
			let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
			let child = dom.insert(
				dom.root_ref(),
				InstanceBuilder::new("BoolValue")
					.with_name("Child")
					.with_property("Value", true),
			);
			let mut child_model = Vec::new();
			rbx_binary::to_writer(&mut child_model, &dom, &[child]).unwrap();
			(
				framed_capture(&[(vec![2], child_model)]),
				Some(CaptureHierarchyNode {
					parent_ordinal: 1,
					class_name: Ustr::from("BoolValue"),
					name: "Child".to_owned(),
					flags: crate::capture_provider::CAPTURE_HIERARCHY_FLAG_SERIALIZED,
				}),
				Some(sequential_ref(102)),
				vec![2],
			)
		} else {
			(framed_capture(&[]), None, None, Vec::new())
		};
		std::fs::write(&payload, capture_bytes).unwrap();
		let mut nodes = vec![
			CaptureHierarchyNode {
				parent_ordinal: u32::MAX,
				class_name: Ustr::from("DataModel"),
				name: "DataModel".to_owned(),
				flags: crate::capture_provider::CAPTURE_HIERARCHY_FLAG_SERVICE_SHELL,
			},
			CaptureHierarchyNode {
				parent_ordinal: 0,
				class_name: Ustr::from("TimerService"),
				name: "TimerService".to_owned(),
				flags: crate::capture_provider::CAPTURE_HIERARCHY_FLAG_SERVICE_SHELL
					| if default_hydrated {
						crate::capture_provider::CAPTURE_HIERARCHY_FLAG_DEFAULT_HYDRATED_SERVICE
					} else {
						0
					},
			},
		];
		if let Some(child) = child_node {
			nodes.push(child);
		}
		let mut manifest_identities = vec![root_id, native_service_id];
		if let Some(child) = child_identity {
			manifest_identities.push(child);
		}
		let envelope = CaptureEnvelope {
			capture_id: "authoritative-new-service".to_owned(),
			engine_generation: 1,
			hierarchy_sequence_before: 1,
			hierarchy_sequence_after: 1,
			change_sequence_before: 1,
			change_sequence_after: 1,
			model_bytes: 0,
			model_digest: [0; 32],
			studio_session_id: "studio-session".to_owned(),
			instance_id: "instance".to_owned(),
			managed_contract_id: "contract".to_owned(),
			reflection_schema_hash: "schema".to_owned(),
			source_generation: "generation".to_owned(),
			digest_algorithm: "sha256".to_owned(),
			manifest_identities_authoritative: true,
			manifest_identities,
			nodes,
			roots: vec![CaptureServiceRoot {
				hierarchy_ordinal: 1,
				class_name: "TimerService".to_owned(),
				name: "TimerService".to_owned(),
				first_serialized_root: 0,
				serialized_root_count: u32::from(has_child),
			}],
			mapped_bindings: Vec::new(),
			external_references: Vec::new(),
			shell_properties: Vec::new(),
			shell_carriers: Vec::new(),
			serialized_root_ordinals,
		};

		(
			compile(&payload, &envelope, &canonical, "Game", &manifest, &|| false).unwrap(),
			directory,
			native_service_id,
		)
	}

	#[test]
	fn authoritative_capture_omits_new_empty_default_service_anchor() {
		let (compiled, directory, native_service_id) = authoritative_timer_service_capture(true, false);
		assert!(compiled.projected_tree.get_instance(native_service_id).is_none());
		let capture_fingerprint = compiled.capture_fingerprint.clone();
		let manifest = directory.join("game.carbon.json");
		let (_, staged, _) = compiled
			.stage_composite("Game".to_owned(), &manifest, &|| false)
			.unwrap();
		assert!(!artifact_store::artifact_referents(staged.artifact())
			.unwrap()
			.all()
			.contains(&native_service_id));
		assert_eq!(
			artifact_store::validated_artifact_receipt(staged.artifact())
				.unwrap()
				.capture_fingerprint(),
			Some(capture_fingerprint.as_str())
		);
		drop(staged);
		std::fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn authoritative_capture_keeps_new_empty_modified_service_anchor() {
		let (compiled, directory, native_service_id) = authoritative_timer_service_capture(false, false);
		let service = compiled
			.arena
			.nodes
			.iter()
			.find(|node| node.class.as_str() == "TimerService")
			.unwrap();
		assert_eq!(service.referent, native_service_id);
		assert!(compiled.projected_tree.get_instance(native_service_id).is_some());
		std::fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn authoritative_capture_keeps_default_service_anchor_with_children() {
		let (compiled, directory, native_service_id) = authoritative_timer_service_capture(true, true);
		assert!(compiled.projected_tree.get_instance(native_service_id).is_some());
		assert!(compiled.properties.is_some());
		let manifest = directory.join("game.carbon.json");
		let (_, staged, _) = compiled
			.stage_composite("Game".to_owned(), &manifest, &|| false)
			.unwrap();
		let loaded = artifact_store::load_tree(staged.artifact()).unwrap();
		let child = loaded
			.tree
			.subtree_refs(loaded.tree.root_ref())
			.unwrap()
			.into_iter()
			.find_map(|id| {
				let instance = loaded.tree.get_instance(id)?;
				(instance.class.as_str() == "BoolValue").then_some(instance)
			})
			.unwrap();
		assert_eq!(child.properties.get(&Ustr::from("Value")), Some(&Variant::Bool(true)));
		drop(staged);
		std::fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn authoritative_capture_preserves_studio_cframe_rotation_rounding() {
		use crate::capture_provider::{CaptureHierarchyNode, CaptureServiceRoot};
		use rbx_dom_weak::{
			types::{CFrame, Matrix3, Vector3},
			InstanceBuilder, WeakDom,
		};

		let root_id = stable_ref("authoritative-cframe-root");
		let workspace_id = stable_ref("authoritative-cframe-workspace");
		let right_wrist_id = stable_ref("authoritative-cframe-right-wrist");
		let position = Vector3::new(-5.638731e-7, -1.1857359e-5, 7.4354045e-7);
		let cframe = |rows: [[f32; 3]; 3]| {
			CFrame::new(
				position,
				Matrix3::new(
					Vector3::new(rows[0][0], rows[0][1], rows[0][2]),
					Vector3::new(rows[1][0], rows[1][1], rows[1][2]),
					Vector3::new(rows[2][0], rows[2][1], rows[2][2]),
				),
			)
		};
		let canonical_transform = cframe([
			[0.86990005, -0.19090259, 0.45478573],
			[0.26061186, 0.96073776, -0.09520735],
			[-0.4187545, 0.20134343, 0.88549733],
		]);
		let studio_transform = cframe([
			[0.86990005, -0.1909026, 0.4547858],
			[0.2606119, 0.9607377, -0.095207356],
			[-0.41875455, 0.20134348, 0.8854973],
		]);
		let canonical = Snapshot::new()
			.with_id(root_id)
			.with_name("DataModel")
			.with_class("DataModel")
			.with_children(vec![Snapshot::new()
				.with_id(workspace_id)
				.with_name("Workspace")
				.with_class("Workspace")
				.with_children(vec![Snapshot::new()
					.with_id(right_wrist_id)
					.with_name("RightWrist")
					.with_class("AnimationConstraint")
					.with_properties(UstrMap::from_iter([(
						Ustr::from("Transform"),
						Variant::CFrame(canonical_transform),
					)]))])]);
		let directory = std::env::temp_dir().join(format!("carbon-authoritative-cframe-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&directory).unwrap();
		let manifest = directory.join("game.carbon.json");
		artifact_store::extract_snapshot(canonical.clone(), "Game".to_owned(), &manifest).unwrap();

		let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
		let right_wrist = dom.insert(
			dom.root_ref(),
			InstanceBuilder::new("AnimationConstraint")
				.with_name("RightWrist")
				.with_property("Transform", studio_transform),
		);
		let mut model = Vec::new();
		rbx_binary::to_writer(&mut model, &dom, &[right_wrist]).unwrap();
		let capture = framed_capture(&[(vec![2], model)]);
		let payload = directory.join("capture.rbxm");
		std::fs::write(&payload, &capture).unwrap();
		let envelope = CaptureEnvelope {
			capture_id: "authoritative-cframe".to_owned(),
			engine_generation: 1,
			hierarchy_sequence_before: 1,
			hierarchy_sequence_after: 1,
			change_sequence_before: 1,
			change_sequence_after: 1,
			model_bytes: capture.len() as u64,
			model_digest: sha2::Sha256::digest(&capture).into(),
			studio_session_id: "studio-session".to_owned(),
			instance_id: "instance".to_owned(),
			managed_contract_id: "contract".to_owned(),
			reflection_schema_hash: "schema".to_owned(),
			source_generation: "generation".to_owned(),
			digest_algorithm: "sha256".to_owned(),
			manifest_identities_authoritative: true,
			manifest_identities: vec![root_id, workspace_id, right_wrist_id],
			nodes: vec![
				CaptureHierarchyNode {
					parent_ordinal: u32::MAX,
					class_name: Ustr::from("DataModel"),
					name: "DataModel".to_owned(),
					flags: crate::capture_provider::CAPTURE_HIERARCHY_FLAG_SERVICE_SHELL,
				},
				CaptureHierarchyNode {
					parent_ordinal: 0,
					class_name: Ustr::from("Workspace"),
					name: "Workspace".to_owned(),
					flags: crate::capture_provider::CAPTURE_HIERARCHY_FLAG_SERVICE_SHELL,
				},
				CaptureHierarchyNode {
					parent_ordinal: 1,
					class_name: Ustr::from("AnimationConstraint"),
					name: "RightWrist".to_owned(),
					flags: crate::capture_provider::CAPTURE_HIERARCHY_FLAG_SERIALIZED,
				},
			],
			roots: vec![CaptureServiceRoot {
				hierarchy_ordinal: 1,
				class_name: "Workspace".to_owned(),
				name: "Workspace".to_owned(),
				first_serialized_root: 0,
				serialized_root_count: 1,
			}],
			mapped_bindings: Vec::new(),
			external_references: Vec::new(),
			shell_properties: Vec::new(),
			shell_carriers: Vec::new(),
			serialized_root_ordinals: vec![2],
		};

		let (staged, tree_loads) = artifact_store::count_tree_loads(|| {
			let compiled = compile(&payload, &envelope, &canonical, "Game", &manifest, &|| false)?;
			compiled.stage_composite("Game".to_owned(), &manifest, &|| false)
		});
		let (_, staged, _) = staged.unwrap();
		assert_eq!(tree_loads, 0, "CFrame reconciliation must stream the prior artifact");
		let loaded = artifact_store::load_tree(staged.artifact()).unwrap();
		assert_eq!(
			loaded
				.tree
				.get_instance(right_wrist_id)
				.unwrap()
				.properties
				.get(&Ustr::from("Transform")),
			Some(&Variant::CFrame(canonical_transform))
		);
		drop(staged);
		std::fs::remove_dir_all(directory).unwrap();
	}

	fn compile_cross_chunk_reference(
		owner_class: &str,
		binary_property: &str,
		sideband_properties: &[(&str, CaptureReferenceTarget)],
	) -> (Result<CompiledCapture>, PathBuf) {
		use crate::capture_provider::{CaptureExternalReference, CaptureHierarchyNode, CaptureServiceRoot};
		use rbx_dom_weak::{InstanceBuilder, WeakDom};

		let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
		let root = dom.root_ref();
		let target = dom.insert(root, InstanceBuilder::new("Part").with_name("Target"));
		let owner = dom.insert(
			root,
			InstanceBuilder::new(owner_class)
				.with_name("Owner")
				.with_property(binary_property, Variant::Ref(target)),
		);
		let mut owner_model = Vec::new();
		rbx_binary::Serializer::new()
			.reflection_database(util::get_reflection_database())
			.serialize(&mut owner_model, &dom, &[owner])
			.unwrap();
		let mut target_model = Vec::new();
		rbx_binary::Serializer::new()
			.reflection_database(util::get_reflection_database())
			.serialize(&mut target_model, &dom, &[target])
			.unwrap();
		let artifact = framed_capture(&[(vec![2], owner_model), (vec![3], target_model)]);

		let directory = std::env::temp_dir().join(format!("carbon-cross-chunk-reference-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&directory).unwrap();
		let payload = directory.join("capture.rbxm");
		std::fs::write(&payload, &artifact).unwrap();
		let manifest = directory.join("game.carbon.json");
		let canonical = Snapshot::new()
			.with_id(stable_ref("cross-chunk-root"))
			.with_name("DataModel")
			.with_class("DataModel");
		artifact_store::extract_snapshot(canonical.clone(), "Chunked".to_owned(), &manifest).unwrap();
		let envelope = CaptureEnvelope {
			capture_id: "cross-chunk-reference".to_owned(),
			engine_generation: 1,
			hierarchy_sequence_before: 1,
			hierarchy_sequence_after: 1,
			change_sequence_before: 1,
			change_sequence_after: 1,
			model_bytes: artifact.len() as u64,
			model_digest: sha2::Sha256::digest(&artifact).into(),
			studio_session_id: "studio-session".to_owned(),
			instance_id: "instance".to_owned(),
			managed_contract_id: "contract".to_owned(),
			reflection_schema_hash: "schema".to_owned(),
			source_generation: "generation".to_owned(),
			digest_algorithm: "sha256".to_owned(),
			manifest_identities_authoritative: false,
			manifest_identities: Vec::new(),
			nodes: vec![
				CaptureHierarchyNode {
					parent_ordinal: u32::MAX,
					class_name: Ustr::from("DataModel"),
					name: "DataModel".to_owned(),
					flags: 1,
				},
				CaptureHierarchyNode {
					parent_ordinal: 0,
					class_name: Ustr::from("Workspace"),
					name: "Workspace".to_owned(),
					flags: 1,
				},
				CaptureHierarchyNode {
					parent_ordinal: 1,
					class_name: Ustr::from(owner_class),
					name: "Owner".to_owned(),
					flags: 1,
				},
				CaptureHierarchyNode {
					parent_ordinal: 1,
					class_name: Ustr::from("Part"),
					name: "Target".to_owned(),
					flags: 1,
				},
			],
			roots: vec![CaptureServiceRoot {
				hierarchy_ordinal: 1,
				class_name: "Workspace".to_owned(),
				name: "Workspace".to_owned(),
				first_serialized_root: 0,
				serialized_root_count: 2,
			}],
			mapped_bindings: Vec::new(),
			external_references: sideband_properties
				.iter()
				.map(|(property, target)| CaptureExternalReference {
					owner_ordinal: 2,
					property: (*property).to_owned(),
					target: *target,
				})
				.collect(),
			shell_properties: Vec::new(),
			shell_carriers: Vec::new(),
			serialized_root_ordinals: vec![2, 3],
		};

		let compiled = compile(&payload, &envelope, &canonical, "Chunked", &manifest, &|| false);
		(compiled, directory)
	}

	#[test]
	fn chunked_capture_repairs_cross_chunk_instance_references_from_native_sideband() {
		let (compiled, directory) =
			compile_cross_chunk_reference("ObjectValue", "Value", &[("Value", CaptureReferenceTarget::Ordinal(3))]);
		let compiled = compiled.unwrap();
		let owner = compiled.arena.nodes.iter().find(|node| node.name == "Owner").unwrap();
		let target = compiled.arena.nodes.iter().find(|node| node.name == "Target").unwrap();
		assert_eq!(
			owner.properties.get(&Ustr::from("Value")),
			Some(&Variant::Ref(target.referent))
		);
		std::fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn native_capture_omits_non_round_trippable_reference() {
		let (compiled, directory) = compile_cross_chunk_reference(
			"Part",
			"AssemblyRootPart",
			&[("AssemblyRootPart", CaptureReferenceTarget::Ordinal(3))],
		);
		let compiled = compiled.unwrap();
		let owner = compiled.arena.nodes.iter().find(|node| node.name == "Owner").unwrap();
		assert!(!owner.properties.contains_key(&Ustr::from("AssemblyRootPart")));
		assert!(compiled.referenced_mapped_refs.is_empty());
		std::fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn native_capture_rejects_unknown_reference_descriptor() {
		let (compiled, directory) = compile_cross_chunk_reference(
			"Part",
			"AssemblyRootPart",
			&[("FutureAssemblyRootPart", CaptureReferenceTarget::Ordinal(3))],
		);
		assert_eq!(
			compiled.unwrap_err().to_string(),
			"capture reference descriptor Part.FutureAssemblyRootPart is unknown"
		);
		std::fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn native_capture_coalesces_identical_canonical_reference_aliases() {
		let (compiled, directory) = compile_cross_chunk_reference(
			"ManualWeld",
			"Part1",
			&[
				("Part1", CaptureReferenceTarget::Ordinal(3)),
				("part1", CaptureReferenceTarget::Ordinal(3)),
			],
		);
		let compiled = compiled.unwrap();
		let owner = compiled.arena.nodes.iter().find(|node| node.name == "Owner").unwrap();
		let target = compiled.arena.nodes.iter().find(|node| node.name == "Target").unwrap();
		assert_eq!(
			owner.properties.get(&Ustr::from("Part1")),
			Some(&Variant::Ref(target.referent))
		);
		std::fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn native_capture_rejects_conflicting_canonical_reference_aliases() {
		let (compiled, directory) = compile_cross_chunk_reference(
			"ManualWeld",
			"Part1",
			&[
				("Part1", CaptureReferenceTarget::Ordinal(3)),
				("part1", CaptureReferenceTarget::Null),
			],
		);
		let error = match compiled {
			Ok(_) => panic!("conflicting canonical aliases must fail"),
			Err(error) => error,
		};
		assert_eq!(
			error.to_string(),
			"capture native sideband conflicts for ManualWeld.part1"
		);
		std::fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn native_capture_accepts_manifest_script_below_non_script_parent() {
		use crate::capture_provider::{
			CaptureHierarchyNode, CaptureServiceRoot, CAPTURE_HIERARCHY_FLAG_SERIALIZED,
			CAPTURE_HIERARCHY_FLAG_SERVICE_SHELL,
		};
		use rbx_dom_weak::{InstanceBuilder, WeakDom};

		let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
		let root = dom.root_ref();
		let parent = dom.insert(root, InstanceBuilder::new("Part").with_name("ManifestParent"));
		dom.insert(
			parent,
			InstanceBuilder::new("ModuleScript")
				.with_name("NestedScript")
				.with_property("Source", Variant::String("return 'native capture'\n".to_owned())),
		);
		let mut model = Vec::new();
		rbx_binary::to_writer(&mut model, &dom, &[parent]).unwrap();
		let artifact = framed_capture(&[(vec![2], model)]);

		let directory = std::env::temp_dir().join(format!("carbon-native-manifest-script-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&directory).unwrap();
		let payload = directory.join("capture.rbxm");
		std::fs::write(&payload, &artifact).unwrap();
		let manifest = directory.join("game.carbon.json");
		let canonical = Snapshot::new()
			.with_id(stable_ref("native-manifest-script-root"))
			.with_name("DataModel")
			.with_class("DataModel");
		artifact_store::extract_snapshot(canonical.clone(), "Game".to_owned(), &manifest).unwrap();
		let envelope = CaptureEnvelope {
			capture_id: "native-manifest-script".to_owned(),
			engine_generation: 1,
			hierarchy_sequence_before: 1,
			hierarchy_sequence_after: 1,
			change_sequence_before: 1,
			change_sequence_after: 1,
			model_bytes: artifact.len() as u64,
			model_digest: sha2::Sha256::digest(&artifact).into(),
			studio_session_id: "studio-session".to_owned(),
			instance_id: "instance".to_owned(),
			managed_contract_id: "contract".to_owned(),
			reflection_schema_hash: "schema".to_owned(),
			source_generation: "generation".to_owned(),
			digest_algorithm: "sha256".to_owned(),
			manifest_identities_authoritative: false,
			manifest_identities: Vec::new(),
			nodes: vec![
				CaptureHierarchyNode {
					parent_ordinal: u32::MAX,
					class_name: Ustr::from("DataModel"),
					name: "DataModel".to_owned(),
					flags: CAPTURE_HIERARCHY_FLAG_SERVICE_SHELL,
				},
				CaptureHierarchyNode {
					parent_ordinal: 0,
					class_name: Ustr::from("Workspace"),
					name: "Workspace".to_owned(),
					flags: CAPTURE_HIERARCHY_FLAG_SERVICE_SHELL,
				},
				CaptureHierarchyNode {
					parent_ordinal: 1,
					class_name: Ustr::from("Part"),
					name: "ManifestParent".to_owned(),
					flags: CAPTURE_HIERARCHY_FLAG_SERIALIZED,
				},
				CaptureHierarchyNode {
					parent_ordinal: 2,
					class_name: Ustr::from("ModuleScript"),
					name: "NestedScript".to_owned(),
					flags: CAPTURE_HIERARCHY_FLAG_SERIALIZED,
				},
			],
			roots: vec![CaptureServiceRoot {
				hierarchy_ordinal: 1,
				class_name: "Workspace".to_owned(),
				name: "Workspace".to_owned(),
				first_serialized_root: 0,
				serialized_root_count: 1,
			}],
			mapped_bindings: Vec::new(),
			external_references: Vec::new(),
			shell_properties: Vec::new(),
			shell_carriers: Vec::new(),
			serialized_root_ordinals: vec![2],
		};

		let compiled = compile(&payload, &envelope, &canonical, "Game", &manifest, &|| false).unwrap();
		let script = compiled
			.arena
			.nodes
			.iter()
			.find(|node| node.class.as_str() == "ModuleScript")
			.unwrap();
		assert_eq!(compiled.arena.get(script.parent).unwrap().class.as_str(), "Part");
		let (_, staged, _) = compiled
			.stage_composite("Game".to_owned(), &manifest, &|| false)
			.unwrap();
		let loaded = artifact_store::load_tree(staged.artifact()).unwrap();
		let rebuilt_script = loaded
			.tree
			.subtree_refs(loaded.tree.root_ref())
			.unwrap()
			.into_iter()
			.find_map(|id| {
				let instance = loaded.tree.get_instance(id)?;
				(instance.class.as_str() == "ModuleScript").then_some(instance)
			})
			.unwrap();
		assert_eq!(
			rebuilt_script.properties.get(&Ustr::from("Source")),
			Some(&Variant::String("return 'native capture'\n".to_owned()))
		);
		drop(staged);
		std::fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn native_capture_restores_any_ref_property_to_a_mapped_source_identity() {
		use crate::capture_provider::{
			CaptureExternalReference, CaptureHierarchyNode, CaptureMappedBinding, CaptureReferenceTarget,
			CaptureServiceRoot,
		};
		use rbx_dom_weak::{InstanceBuilder, WeakDom};

		let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
		let owner = dom.insert(
			dom.root_ref(),
			InstanceBuilder::new("ObjectValue")
				.with_name("Owner")
				.with_property("Value", Variant::Ref(Ref::none())),
		);
		let mut model = Vec::new();
		rbx_binary::to_writer(&mut model, &dom, &[owner]).unwrap();
		let artifact = framed_capture(&[(vec![2], model)]);
		let directory = std::env::temp_dir().join(format!("carbon-mapped-reference-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&directory).unwrap();
		let payload = directory.join("capture.rbxm");
		std::fs::write(&payload, &artifact).unwrap();
		let manifest = directory.join("game.carbon.json");
		let root_id = stable_ref("mapped-reference-root");
		let workspace_id = stable_ref("mapped-reference-workspace");
		let mapped_id = stable_ref("mapped-reference-target");
		let canonical = Snapshot::new()
			.with_id(root_id)
			.with_name("DataModel")
			.with_class("DataModel")
			.with_children(vec![Snapshot::new()
				.with_id(workspace_id)
				.with_name("Workspace")
				.with_class("Workspace")
				.with_children(vec![Snapshot::new()
					.with_id(mapped_id)
					.with_name("Generator")
					.with_class("ModuleScript")
					.with_properties(UstrMap::from_iter([(
						Ustr::from("Source"),
						Variant::String("return function() end".to_owned()),
					)]))])]);
		artifact_store::extract_snapshot(canonical.clone(), "MappedReference".to_owned(), &manifest).unwrap();
		let envelope = CaptureEnvelope {
			capture_id: "mapped-reference".to_owned(),
			engine_generation: 1,
			hierarchy_sequence_before: 1,
			hierarchy_sequence_after: 1,
			change_sequence_before: 1,
			change_sequence_after: 1,
			model_bytes: artifact.len() as u64,
			model_digest: sha2::Sha256::digest(&artifact).into(),
			studio_session_id: "studio".to_owned(),
			instance_id: "instance".to_owned(),
			managed_contract_id: "contract".to_owned(),
			reflection_schema_hash: "schema".to_owned(),
			source_generation: "generation".to_owned(),
			digest_algorithm: "sha256".to_owned(),
			manifest_identities_authoritative: false,
			manifest_identities: Vec::new(),
			nodes: vec![
				CaptureHierarchyNode {
					parent_ordinal: u32::MAX,
					class_name: Ustr::from("DataModel"),
					name: "DataModel".to_owned(),
					flags: 1,
				},
				CaptureHierarchyNode {
					parent_ordinal: 0,
					class_name: Ustr::from("Workspace"),
					name: "Workspace".to_owned(),
					flags: 1,
				},
				CaptureHierarchyNode {
					parent_ordinal: 1,
					class_name: Ustr::from("ObjectValue"),
					name: "Owner".to_owned(),
					flags: 1,
				},
			],
			roots: vec![CaptureServiceRoot {
				hierarchy_ordinal: 1,
				class_name: "Workspace".to_owned(),
				name: "Workspace".to_owned(),
				first_serialized_root: 0,
				serialized_root_count: 1,
			}],
			mapped_bindings: vec![CaptureMappedBinding {
				source_id: mapped_id.to_string(),
				hierarchy_ordinal: u32::MAX,
				parent_ordinal: 1,
			}],
			external_references: vec![CaptureExternalReference {
				owner_ordinal: 2,
				property: "Value".to_owned(),
				target: CaptureReferenceTarget::Mapped(mapped_id),
			}],
			shell_properties: Vec::new(),
			shell_carriers: Vec::new(),
			serialized_root_ordinals: vec![2],
		};

		let compiled = compile(&payload, &envelope, &canonical, "MappedReference", &manifest, &|| false).unwrap();
		let owner = compiled.arena.nodes.iter().find(|node| node.name == "Owner").unwrap();
		assert_eq!(owner.properties[&Ustr::from("Value")], Variant::Ref(mapped_id));
		assert_eq!(compiled.referenced_mapped_refs, HashSet::from([mapped_id]));
		std::fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn authoritative_capture_preserves_mapped_source_properties_in_the_fresh_artifact() {
		use crate::capture_provider::{
			CaptureHierarchyNode, CaptureMappedBinding, CaptureServiceRoot, CAPTURE_HIERARCHY_FLAG_SERIALIZED,
			CAPTURE_HIERARCHY_FLAG_SERVICE_SHELL,
		};
		use rbx_dom_weak::{InstanceBuilder, WeakDom};

		let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
		let native = dom.insert(dom.root_ref(), InstanceBuilder::new("Folder").with_name("Native"));
		let mut model = Vec::new();
		rbx_binary::to_writer(&mut model, &dom, &[native]).unwrap();
		let artifact = framed_capture(&[(vec![2], model)]);
		let directory =
			std::env::temp_dir().join(format!("carbon-authoritative-mapped-source-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&directory).unwrap();
		let payload = directory.join("capture.rbxm");
		std::fs::write(&payload, &artifact).unwrap();
		let manifest = directory.join("state.carbon");
		let root_id = stable_ref("authoritative-mapped-source-root");
		let workspace_id = stable_ref("authoritative-mapped-source-workspace");
		let native_id = stable_ref("authoritative-mapped-source-native");
		let mapped_id = stable_ref("authoritative-mapped-source-script");
		let source = "return function() end".to_owned();
		let canonical = Snapshot::new()
			.with_id(root_id)
			.with_name("DataModel")
			.with_class("DataModel")
			.with_children(vec![Snapshot::new()
				.with_id(workspace_id)
				.with_name("Workspace")
				.with_class("Workspace")
				.with_children(vec![Snapshot::new()
					.with_id(mapped_id)
					.with_name("Generator")
					.with_class("ModuleScript")
					.with_properties(UstrMap::from_iter([(
						Ustr::from("Source"),
						Variant::String(source.clone()),
					)]))])]);
		artifact_store::extract_snapshot(canonical.clone(), "Game".to_owned(), &manifest).unwrap();
		let envelope = CaptureEnvelope {
			capture_id: "authoritative-mapped-source".to_owned(),
			engine_generation: 1,
			hierarchy_sequence_before: 1,
			hierarchy_sequence_after: 1,
			change_sequence_before: 1,
			change_sequence_after: 1,
			model_bytes: artifact.len() as u64,
			model_digest: sha2::Sha256::digest(&artifact).into(),
			studio_session_id: "studio".to_owned(),
			instance_id: "instance".to_owned(),
			managed_contract_id: "contract".to_owned(),
			reflection_schema_hash: "schema".to_owned(),
			source_generation: "generation".to_owned(),
			digest_algorithm: "sha256".to_owned(),
			manifest_identities_authoritative: true,
			manifest_identities: vec![root_id, workspace_id, native_id],
			nodes: vec![
				CaptureHierarchyNode {
					parent_ordinal: u32::MAX,
					class_name: Ustr::from("DataModel"),
					name: "DataModel".to_owned(),
					flags: CAPTURE_HIERARCHY_FLAG_SERVICE_SHELL,
				},
				CaptureHierarchyNode {
					parent_ordinal: 0,
					class_name: Ustr::from("Workspace"),
					name: "Workspace".to_owned(),
					flags: CAPTURE_HIERARCHY_FLAG_SERVICE_SHELL,
				},
				CaptureHierarchyNode {
					parent_ordinal: 1,
					class_name: Ustr::from("Folder"),
					name: "Native".to_owned(),
					flags: CAPTURE_HIERARCHY_FLAG_SERIALIZED,
				},
			],
			roots: vec![CaptureServiceRoot {
				hierarchy_ordinal: 1,
				class_name: "Workspace".to_owned(),
				name: "Workspace".to_owned(),
				first_serialized_root: 0,
				serialized_root_count: 1,
			}],
			mapped_bindings: vec![CaptureMappedBinding {
				source_id: mapped_id.to_string(),
				hierarchy_ordinal: u32::MAX,
				parent_ordinal: 1,
			}],
			external_references: Vec::new(),
			shell_properties: Vec::new(),
			shell_carriers: Vec::new(),
			serialized_root_ordinals: vec![2],
		};

		let compiled = compile(&payload, &envelope, &canonical, "Game", &manifest, &|| false).unwrap();
		assert!(
			!compiled
				.projected_tree
				.get_instance(mapped_id)
				.unwrap()
				.properties
				.contains_key(&Ustr::from("Source")),
			"the retained live projection must remain property-sparse"
		);
		let (_, staged, _) = compiled
			.stage_composite("Game".to_owned(), &manifest, &|| false)
			.unwrap();
		let loaded = artifact_store::load_tree(staged.artifact()).unwrap();
		assert_eq!(
			loaded
				.tree
				.get_instance(mapped_id)
				.unwrap()
				.properties
				.get(&Ustr::from("Source")),
			Some(&Variant::String(source))
		);

		drop(staged);
		std::fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn chunk_alignment_reconstructs_a_deep_frontier_cut_without_overlap() {
		use crate::capture_provider::{CaptureHierarchyNode, CaptureServiceRoot};
		use rbx_dom_weak::{InstanceBuilder, WeakDom};

		let parent_dom = WeakDom::new(
			InstanceBuilder::new("DataModel").with_child(InstanceBuilder::new("Folder").with_name("Parent")),
		);
		let mut parent_model = Vec::new();
		rbx_binary::to_writer(&mut parent_model, &parent_dom, parent_dom.root().children()).unwrap();
		let child_dom =
			WeakDom::new(InstanceBuilder::new("DataModel").with_child(InstanceBuilder::new("Part").with_name("Child")));
		let mut child_model = Vec::new();
		rbx_binary::to_writer(&mut child_model, &child_dom, child_dom.root().children()).unwrap();
		let bytes = framed_capture(&[(vec![2], parent_model), (vec![3], child_model)]);
		let path = std::env::temp_dir().join(format!("carbon-frontier-cut-{}.rbxm", uuid::Uuid::new_v4()));
		std::fs::write(&path, &bytes).unwrap();
		let envelope = CaptureEnvelope {
			capture_id: "frontier-cut".to_owned(),
			engine_generation: 1,
			hierarchy_sequence_before: 1,
			hierarchy_sequence_after: 1,
			change_sequence_before: 1,
			change_sequence_after: 1,
			model_bytes: bytes.len() as u64,
			model_digest: sha2::Sha256::digest(&bytes).into(),
			studio_session_id: "studio".to_owned(),
			instance_id: "instance".to_owned(),
			managed_contract_id: "contract".to_owned(),
			reflection_schema_hash: "schema".to_owned(),
			source_generation: "source".to_owned(),
			digest_algorithm: "sha256".to_owned(),
			manifest_identities_authoritative: false,
			manifest_identities: Vec::new(),
			nodes: vec![
				CaptureHierarchyNode {
					parent_ordinal: u32::MAX,
					class_name: Ustr::from("DataModel"),
					name: "DataModel".to_owned(),
					flags: 1,
				},
				CaptureHierarchyNode {
					parent_ordinal: 0,
					class_name: Ustr::from("Workspace"),
					name: "Workspace".to_owned(),
					flags: 1,
				},
				CaptureHierarchyNode {
					parent_ordinal: 1,
					class_name: Ustr::from("Folder"),
					name: "Parent".to_owned(),
					flags: 1,
				},
				CaptureHierarchyNode {
					parent_ordinal: 2,
					class_name: Ustr::from("Part"),
					name: "Child".to_owned(),
					flags: 1,
				},
			],
			roots: vec![CaptureServiceRoot {
				hierarchy_ordinal: 1,
				class_name: "Workspace".to_owned(),
				name: "Workspace".to_owned(),
				first_serialized_root: 0,
				serialized_root_count: 1,
			}],
			mapped_bindings: Vec::new(),
			external_references: Vec::new(),
			shell_properties: Vec::new(),
			shell_carriers: Vec::new(),
			serialized_root_ordinals: vec![2, 3],
		};
		let artifact = CaptureModelArtifact::open(&path).unwrap();
		artifact
			.validate_root_order(&envelope.serialized_root_ordinals)
			.unwrap();
		let (_, ordinal_refs) = CaptureArena::from_envelope(&envelope).unwrap();
		let mut topology = CaptureTopology::new(&envelope, &artifact, ordinal_refs).unwrap();
		let mut start = 0;
		for (index, chunk) in artifact.chunks.iter().enumerate() {
			let source = rbx_binary::Deserializer::new()
				.strict(true)
				.reflection_database(util::get_reflection_database())
				.deserialize_source(artifact.open_chunk(&path, index).unwrap())
				.unwrap();
			let aligned = topology
				.align_chunk(&envelope, chunk, start, &source, &HashMap::new(), true)
				.unwrap();
			assert_eq!(aligned.persistent.len(), 1);
			start += chunk.root_ordinals.len();
		}
		topology.validate_complete().unwrap();
		std::fs::remove_file(path).unwrap();
	}

	#[test]
	#[ignore = "external million-instance native capture compile/stage acceptance probe"]
	fn external_million_native_capture_compile_stage_is_bounded_and_noop() {
		use sha2::{Digest, Sha256};

		if std::env::var_os("CARBON_CAPTURE_PROBE_PHASES").is_some() {
			let _ = env_logger::builder()
				.is_test(true)
				.filter_level(log::LevelFilter::Info)
				.try_init();
		}
		let mut payload = PathBuf::from(
			std::env::var("CARBON_CAPTURE_PROBE_PAYLOAD")
				.expect("set CARBON_CAPTURE_PROBE_PAYLOAD to the captured RBXM payload"),
		);
		let envelope_path = PathBuf::from(
			std::env::var("CARBON_CAPTURE_PROBE_ENVELOPE")
				.expect("set CARBON_CAPTURE_PROBE_ENVELOPE to the matching capture envelope"),
		);
		let manifest = PathBuf::from(
			std::env::var("CARBON_CAPTURE_PROBE_MANIFEST")
				.expect("set CARBON_CAPTURE_PROBE_MANIFEST to the active composite manifest"),
		);
		let minimum_instances = std::env::var("CARBON_CAPTURE_PROBE_MIN_INSTANCES")
			.ok()
			.and_then(|value| value.parse().ok())
			.unwrap_or(1_000_000_usize);
		let maximum_rss_kib = std::env::var("CARBON_CAPTURE_PROBE_MAX_RSS_KIB")
			.ok()
			.and_then(|value| value.parse().ok())
			.unwrap_or(768 * 1024_u64);
		let maximum_elapsed_ms = std::env::var("CARBON_CAPTURE_PROBE_MAX_ELAPSED_MS")
			.ok()
			.and_then(|value| value.parse::<u128>().ok());
		let force_changed = std::env::var_os("CARBON_CAPTURE_PROBE_FORCE_CHANGED").is_some();
		let rename_first = std::env::var_os("CARBON_CAPTURE_PROBE_RENAME_FIRST").is_some();

		let mut envelope = CaptureEnvelope::decode(&std::fs::read(envelope_path).unwrap()).unwrap();
		let mut renamed_payload = None;
		if rename_first {
			let mut dom = rbx_binary::from_reader(BufReader::new(File::open(&payload).unwrap())).unwrap();
			let matches = dom
				.descendants()
				.filter(|instance| instance.name == "Node0000000")
				.map(|instance| instance.referent())
				.collect::<Vec<_>>();
			assert_eq!(matches.len(), 1, "40k probe does not contain one Node0000000 instance");
			dom.get_by_ref_mut(matches[0]).unwrap().name = "Node0000000_Renamed".to_owned();
			let envelope_matches = envelope
				.nodes
				.iter_mut()
				.filter(|node| node.name == "Node0000000")
				.collect::<Vec<_>>();
			assert_eq!(
				envelope_matches.len(),
				1,
				"40k probe envelope does not contain one Node0000000 instance"
			);
			envelope_matches.into_iter().next().unwrap().name = "Node0000000_Renamed".to_owned();
			let renamed = std::env::temp_dir().join(format!("carbon-capture-rename-{}.rbxm", uuid::Uuid::new_v4()));
			rbx_binary::to_writer(File::create(&renamed).unwrap(), &dom, dom.root().children()).unwrap();
			let bytes = std::fs::read(&renamed).unwrap();
			envelope.model_bytes = bytes.len() as u64;
			envelope.model_digest = Sha256::digest(&bytes).into();
			payload = renamed.clone();
			renamed_payload = Some(renamed);
		}
		let mapped_roots = envelope
			.mapped_bindings
			.iter()
			.map(|binding| binding.source_id.parse::<Ref>().unwrap())
			.collect::<HashSet<_>>();
		let canonical = artifact_store::capture_probe_projection(&manifest, &mapped_roots).unwrap();
		let before = artifact_store::capture_probe_file_fingerprints(&manifest).unwrap();
		let started = Instant::now();
		let active_project_name = artifact_store::inspect(&manifest).unwrap().name;
		let project_name = if force_changed {
			format!("{active_project_name}-ChangedProbe")
		} else {
			active_project_name
		};
		let receipt = artifact_store::validated_artifact_receipt(&manifest).unwrap();
		let project_generation = receipt
			.project_generation()
			.expect("capture probe manifest has no project realization generation")
			.to_owned();
		let classification = classify_validated_receipt(&envelope, &project_name, &project_generation, receipt);
		let exact_expected = !force_changed && !rename_first;
		let instances = match classification {
			ValidatedCaptureClass::ExactNoop(_) => {
				assert!(
					exact_expected,
					"changed million-node probe was classified as an exact no-op"
				);
				envelope.nodes.len()
			}
			ValidatedCaptureClass::Rebuild(baseline) => {
				assert!(
					!exact_expected,
					"unchanged million-node probe did not take the pre-decode fast path"
				);
				let compiled = compile_validated(
					&payload,
					&envelope,
					&canonical,
					&project_name,
					&manifest,
					&baseline,
					&project_generation,
					&|| false,
				)
				.unwrap();
				let instances = compiled.arena.nodes.len();
				assert!(!compiled.semantic_noop);
				assert_eq!(
					compiled.stage_plan.dirty_buckets().len(),
					1 << 12,
					"authoritative changed capture must rebuild every stable partition from fresh evidence"
				);
				let (projected_tree, staged, _) = compiled.stage_composite(project_name, &manifest, &|| false).unwrap();
				drop(projected_tree);
				assert!(!staged.is_noop().unwrap());
				instances
			}
		};
		let after = artifact_store::capture_probe_file_fingerprints(&manifest).unwrap();
		assert_eq!(after, before, "compile/stage changed active canonical bytes or mtimes");
		let elapsed = started.elapsed();
		if let Some(maximum) = maximum_elapsed_ms {
			assert!(
				elapsed.as_millis() <= maximum,
				"native capture compile/stage elapsed {:.3}s exceeds {:.3}s",
				elapsed.as_secs_f64(),
				maximum as f64 / 1_000.0
			);
		}
		assert!(
			instances >= minimum_instances,
			"fixture has {instances} instances, below acceptance minimum {minimum_instances}"
		);
		let peak_rss_kib = linux_peak_rss_kib();
		if let Some(peak) = peak_rss_kib {
			assert!(
				peak <= maximum_rss_kib,
				"native capture peak RSS {peak} KiB exceeds {maximum_rss_kib} KiB"
			);
		}
		eprintln!(
			"native capture probe: instances={instances} elapsed={:.3}s peak_rss_kib={}",
			elapsed.as_secs_f64(),
			peak_rss_kib
				.map(|value| value.to_string())
				.unwrap_or_else(|| "unavailable".to_owned())
		);
		if let Some(renamed) = renamed_payload {
			std::fs::remove_file(renamed).unwrap();
		}
	}

	#[test]
	fn indexed_identity_claims_unchanged_32k_group_before_rename_fallback() {
		const COUNT: usize = 32_769;
		let root_id = stable_ref("capture-identity-test-root");
		let mut canonical = Snapshot::new()
			.with_id(root_id)
			.with_name("DataModel")
			.with_class("DataModel");
		for index in 0..COUNT {
			let name = format!("Node{index:07}");
			let id = stable_ref(&format!("capture:{root_id}:Folder:{name}:0"));
			let mut child = folder(id, &name);
			// Artifact default keepers retain a reflected default on a bounded
			// subset while neighboring records omit it. Both forms must digest as
			// the same semantic property set.
			if index % 4096 == 0 {
				child.properties.insert(Ustr::from("Archivable"), Variant::Bool(true));
			}
			if index == 1 {
				child.raw_name = Some(serde_bytes::ByteBuf::from(name.as_bytes().to_vec()));
			}
			canonical.children.push(child);
		}

		let incoming_root = Ref::new();
		let children = (0..COUNT).map(|_| Ref::new()).collect::<Vec<_>>();
		let mut nodes = Vec::with_capacity(COUNT + 1);
		nodes.push(CaptureNode {
			referent: incoming_root,
			parent: Ref::none(),
			class: Ustr::from("DataModel"),
			name: "DataModel".to_owned(),
			properties: UstrMap::default(),
			children: children.clone(),
			property_digest: DigestMultiset::default(),
			observed_properties: Vec::new(),
			digest: [0; 32],
			semantic_name: semantic_name("DataModel", None),
		});
		for (index, referent) in children.iter().copied().enumerate() {
			let name = if index == 0 {
				"Renamed".to_owned()
			} else {
				format!("Node{index:07}")
			};
			nodes.push(CaptureNode {
				referent,
				parent: incoming_root,
				class: Ustr::from("Folder"),
				name: name.clone(),
				properties: UstrMap::default(),
				children: Vec::new(),
				property_digest: DigestMultiset::default(),
				observed_properties: Vec::new(),
				digest: [0; 32],
				semantic_name: semantic_name(&name, None),
			});
		}
		let by_ref = nodes
			.iter()
			.enumerate()
			.map(|(index, node)| (node.referent, index))
			.collect();
		let mut arena = CaptureArena {
			nodes,
			by_ref,
			root: incoming_root,
		};
		arena.finish_digests().unwrap();
		let mut exact = HashMap::from([(incoming_root, root_id)]);
		let (manifest, prior_ids) = prior_artifact(&canonical, "32k");
		reconcile_identities(&mut arena, &prior_ids, &manifest, &mut exact, &|| false).unwrap();
		assert_eq!(exact[&children[0]], canonical.children[0].id);
		assert_eq!(exact[&children[32_768]], canonical.children[32_768].id);
		std::fs::remove_dir_all(manifest.parent().unwrap()).unwrap();
	}

	#[test]
	fn sequential_identity_claims_unchanged_40k_group_before_rename_fallback() {
		const COUNT: usize = 40_000;
		let root_id = sequential_ref(1);
		let mut canonical = Snapshot::new()
			.with_id(root_id)
			.with_name("DataModel")
			.with_class("DataModel");
		for index in 0..COUNT {
			canonical
				.children
				.push(folder(sequential_ref(index as u128 + 2), &format!("Node{index:07}")));
		}

		let incoming_root = sequential_ref(1_000_001);
		let children = (0..COUNT)
			.map(|index| sequential_ref(index as u128 + 2_000_001))
			.collect::<Vec<_>>();
		let mut nodes = Vec::with_capacity(COUNT + 1);
		nodes.push(CaptureNode {
			referent: incoming_root,
			parent: Ref::none(),
			class: Ustr::from("DataModel"),
			name: "DataModel".to_owned(),
			properties: UstrMap::default(),
			children: children.clone(),
			property_digest: DigestMultiset::default(),
			observed_properties: Vec::new(),
			digest: [0; 32],
			semantic_name: semantic_name("DataModel", None),
		});
		for (index, referent) in children.iter().copied().enumerate() {
			let name = if index == 0 {
				"Node0000000_Renamed".to_owned()
			} else {
				format!("Node{index:07}")
			};
			nodes.push(CaptureNode {
				referent,
				parent: incoming_root,
				class: Ustr::from("Folder"),
				name: name.clone(),
				properties: UstrMap::default(),
				children: Vec::new(),
				property_digest: DigestMultiset::default(),
				observed_properties: Vec::new(),
				digest: [0; 32],
				semantic_name: semantic_name(&name, None),
			});
		}
		let by_ref = nodes
			.iter()
			.enumerate()
			.map(|(index, node)| (node.referent, index))
			.collect();
		let mut arena = CaptureArena {
			nodes,
			by_ref,
			root: incoming_root,
		};
		arena.finish_digests().unwrap();
		let mut exact = HashMap::from([(incoming_root, root_id)]);
		let (manifest, prior_ids) = prior_artifact(&canonical, "sequential-40k");
		reconcile_identities(&mut arena, &prior_ids, &manifest, &mut exact, &|| false).unwrap();
		assert_eq!(exact.len(), COUNT + 1);
		assert_eq!(exact[&children[0]], canonical.children[0].id);
		assert_eq!(exact[&children[COUNT - 1]], canonical.children[COUNT - 1].id);
		std::fs::remove_dir_all(manifest.parent().unwrap()).unwrap();
	}

	#[test]
	fn genuinely_duplicate_residual_identity_remains_ambiguous() {
		let root_id = sequential_ref(1);
		let mut canonical = Snapshot::new()
			.with_id(root_id)
			.with_name("DataModel")
			.with_class("DataModel");
		canonical.children.push(folder(sequential_ref(2), "Same"));
		canonical.children.push(folder(sequential_ref(3), "Same"));

		let incoming_root = sequential_ref(101);
		let incoming = sequential_ref(102);
		let mut arena = CaptureArena {
			nodes: vec![
				CaptureNode {
					referent: incoming_root,
					parent: Ref::none(),
					class: Ustr::from("DataModel"),
					name: "DataModel".to_owned(),
					properties: UstrMap::default(),
					children: vec![incoming],
					property_digest: DigestMultiset::default(),
					observed_properties: Vec::new(),
					digest: [0; 32],
					semantic_name: semantic_name("DataModel", None),
				},
				CaptureNode {
					referent: incoming,
					parent: incoming_root,
					class: Ustr::from("Folder"),
					name: "Renamed".to_owned(),
					properties: UstrMap::default(),
					children: Vec::new(),
					property_digest: DigestMultiset::default(),
					observed_properties: Vec::new(),
					digest: [0; 32],
					semantic_name: semantic_name("Renamed", None),
				},
			],
			by_ref: HashMap::new(),
			root: incoming_root,
		};
		arena.rebuild_index().unwrap();
		arena.finish_digests().unwrap();
		let mut exact = HashMap::from([(incoming_root, root_id)]);
		let (manifest, prior_ids) = prior_artifact(&canonical, "genuine-duplicate");
		let error = reconcile_identities(&mut arena, &prior_ids, &manifest, &mut exact, &|| false)
			.unwrap_err()
			.to_string();
		assert!(error.contains("identity-ambiguity blocker at Renamed"), "{error}");
		assert!(error.contains("2 prior instances"), "{error}");
		std::fs::remove_dir_all(manifest.parent().unwrap()).unwrap();
	}

	#[test]
	fn authoritative_identity_deletes_either_of_two_identical_siblings_without_ambiguity() {
		let root_id = sequential_ref(1);
		let left_id = sequential_ref(2);
		let right_id = sequential_ref(3);
		let mut canonical = Snapshot::new()
			.with_id(root_id)
			.with_name("DataModel")
			.with_class("DataModel");
		canonical.children.push(folder(left_id, "Part"));
		canonical.children.push(folder(right_id, "Part"));

		let incoming_root = sequential_ref(101);
		let survivor = sequential_ref(102);
		let mut arena = CaptureArena {
			nodes: vec![
				CaptureNode {
					referent: incoming_root,
					parent: Ref::none(),
					class: Ustr::from("DataModel"),
					name: "DataModel".to_owned(),
					properties: UstrMap::default(),
					children: vec![survivor],
					property_digest: DigestMultiset::default(),
					observed_properties: Vec::new(),
					digest: [0; 32],
					semantic_name: semantic_name("DataModel", None),
				},
				CaptureNode {
					referent: survivor,
					parent: incoming_root,
					class: Ustr::from("Folder"),
					name: "Part".to_owned(),
					properties: UstrMap::default(),
					children: Vec::new(),
					property_digest: DigestMultiset::default(),
					observed_properties: Vec::new(),
					digest: [0; 32],
					semantic_name: semantic_name("Part", None),
				},
			],
			by_ref: HashMap::new(),
			root: incoming_root,
		};
		arena.rebuild_index().unwrap();
		let mut exact = HashMap::from([(incoming_root, root_id)]);
		apply_authoritative_identities(&arena, &[incoming_root, survivor], &[root_id, right_id], &mut exact).unwrap();
		let (manifest, prior_ids) = prior_artifact(&canonical, "authoritative-duplicate-delete");
		reconcile_identities(&mut arena, &prior_ids, &manifest, &mut exact, &|| false).unwrap();
		assert_eq!(exact[&survivor], right_id);
		assert!(!exact.values().any(|identity| *identity == left_id));
		std::fs::remove_dir_all(manifest.parent().unwrap()).unwrap();
	}

	#[test]
	fn authoritative_identity_preserves_rename_reparent_and_distinguishes_clone_from_deleted_original() {
		let root = sequential_ref(1);
		let old_parent = sequential_ref(2);
		let new_parent = sequential_ref(3);
		let original = sequential_ref(4);
		let clone = sequential_ref(5);
		let incoming = [
			sequential_ref(101),
			sequential_ref(102),
			sequential_ref(103),
			sequential_ref(104),
		];
		let nodes = vec![
			CaptureNode {
				referent: incoming[0],
				parent: Ref::none(),
				class: Ustr::from("DataModel"),
				name: "DataModel".to_owned(),
				properties: UstrMap::default(),
				children: vec![incoming[1], incoming[2]],
				property_digest: DigestMultiset::default(),
				observed_properties: Vec::new(),
				digest: [0; 32],
				semantic_name: semantic_name("DataModel", None),
			},
			CaptureNode {
				referent: incoming[1],
				parent: incoming[0],
				class: Ustr::from("Folder"),
				name: "Old".to_owned(),
				properties: UstrMap::default(),
				children: Vec::new(),
				property_digest: DigestMultiset::default(),
				observed_properties: Vec::new(),
				digest: [0; 32],
				semantic_name: semantic_name("Old", None),
			},
			CaptureNode {
				referent: incoming[2],
				parent: incoming[0],
				class: Ustr::from("Folder"),
				name: "New".to_owned(),
				properties: UstrMap::default(),
				children: vec![incoming[3]],
				property_digest: DigestMultiset::default(),
				observed_properties: Vec::new(),
				digest: [0; 32],
				semantic_name: semantic_name("New", None),
			},
			CaptureNode {
				referent: incoming[3],
				parent: incoming[2],
				class: Ustr::from("Part"),
				name: "Renamed".to_owned(),
				properties: UstrMap::default(),
				children: Vec::new(),
				property_digest: DigestMultiset::default(),
				observed_properties: Vec::new(),
				digest: [0; 32],
				semantic_name: semantic_name("Renamed", None),
			},
		];
		let arena = CaptureArena {
			by_ref: nodes
				.iter()
				.enumerate()
				.map(|(index, node)| (node.referent, index))
				.collect(),
			nodes,
			root: incoming[0],
		};
		let mut exact = HashMap::new();
		apply_authoritative_identities(&arena, &incoming, &[root, old_parent, new_parent, clone], &mut exact).unwrap();
		assert_eq!(exact[&incoming[3]], clone);
		assert!(!exact.values().any(|identity| *identity == original));
	}

	#[test]
	fn authoritative_identity_rejects_duplicate_source_ids() {
		let root = sequential_ref(1);
		let child = sequential_ref(2);
		let nodes = vec![
			CaptureNode {
				referent: root,
				parent: Ref::none(),
				class: Ustr::from("DataModel"),
				name: "DataModel".to_owned(),
				properties: UstrMap::default(),
				children: vec![child],
				property_digest: DigestMultiset::default(),
				observed_properties: Vec::new(),
				digest: [0; 32],
				semantic_name: semantic_name("DataModel", None),
			},
			CaptureNode {
				referent: child,
				parent: root,
				class: Ustr::from("Folder"),
				name: "Child".to_owned(),
				properties: UstrMap::default(),
				children: Vec::new(),
				property_digest: DigestMultiset::default(),
				observed_properties: Vec::new(),
				digest: [0; 32],
				semantic_name: semantic_name("Child", None),
			},
		];
		let mut arena = CaptureArena {
			by_ref: nodes
				.iter()
				.enumerate()
				.map(|(index, node)| (node.referent, index))
				.collect(),
			nodes,
			root,
		};
		let mut exact = HashMap::new();
		apply_authoritative_identities(&arena, &[root, child], &[root, root], &mut exact).unwrap();
		let error = arena.assign_stable_referents(exact, &|| false).unwrap_err().to_string();
		assert!(error.contains("is duplicated"), "{error}");
	}

	#[test]
	fn authoritative_identity_count_excludes_grafted_mapped_nodes() {
		let observed = sequential_ref(101);
		let grafted = sequential_ref(202);
		let stable = sequential_ref(1);
		let mut arena = CaptureArena {
			nodes: vec![
				CaptureNode {
					referent: observed,
					parent: Ref::none(),
					class: Ustr::from("DataModel"),
					name: "DataModel".to_owned(),
					properties: UstrMap::default(),
					children: vec![grafted],
					property_digest: DigestMultiset::default(),
					observed_properties: Vec::new(),
					digest: [0; 32],
					semantic_name: semantic_name("DataModel", None),
				},
				CaptureNode {
					referent: grafted,
					parent: observed,
					class: Ustr::from("Folder"),
					name: "Mapped".to_owned(),
					properties: UstrMap::default(),
					children: Vec::new(),
					property_digest: DigestMultiset::default(),
					observed_properties: Vec::new(),
					digest: [0; 32],
					semantic_name: semantic_name("Mapped", None),
				},
			],
			by_ref: HashMap::new(),
			root: observed,
		};
		arena.rebuild_index().unwrap();
		let mut exact = HashMap::from([(grafted, grafted)]);

		apply_authoritative_identities(&arena, &[observed], &[stable], &mut exact).unwrap();

		assert_eq!(exact[&observed], stable);
		assert_eq!(exact[&grafted], grafted);
	}

	#[test]
	fn residual_ambiguity_is_not_reported_before_unique_name_evidence() {
		let root_id = stable_ref("capture-identity-name-root");
		let mut canonical = Snapshot::new()
			.with_id(root_id)
			.with_name("DataModel")
			.with_class("DataModel");
		canonical.children.push(folder(stable_ref("capture-identity-a"), "A"));
		canonical.children.push(folder(stable_ref("capture-identity-b"), "B"));

		let incoming_root = Ref::new();
		let incoming = Ref::new();
		let mut edited = UstrMap::default();
		edited.insert(Ustr::from("Archivable"), Variant::Bool(false));
		let mut digest = DigestMultiset::default();
		digest.insert(property_digest(&Ustr::from("Archivable"), &Variant::Bool(false)).unwrap());
		let mut arena = CaptureArena {
			nodes: vec![
				CaptureNode {
					referent: incoming_root,
					parent: Ref::none(),
					class: Ustr::from("DataModel"),
					name: "DataModel".to_owned(),
					properties: UstrMap::default(),
					children: vec![incoming],
					property_digest: DigestMultiset::default(),
					observed_properties: Vec::new(),
					digest: [0; 32],
					semantic_name: semantic_name("DataModel", None),
				},
				CaptureNode {
					referent: incoming,
					parent: incoming_root,
					class: Ustr::from("Folder"),
					name: "A".to_owned(),
					properties: edited,
					children: Vec::new(),
					property_digest: digest,
					observed_properties: Vec::new(),
					digest: [0; 32],
					semantic_name: semantic_name("A", None),
				},
			],
			by_ref: HashMap::new(),
			root: incoming_root,
		};
		arena.rebuild_index().unwrap();
		arena.finish_digests().unwrap();
		let mut exact = HashMap::from([(incoming_root, root_id)]);
		let (manifest, prior_ids) = prior_artifact(&canonical, "named");
		reconcile_identities(&mut arena, &prior_ids, &manifest, &mut exact, &|| false).unwrap();
		assert_eq!(exact[&incoming], canonical.children[0].id);
		std::fs::remove_dir_all(manifest.parent().unwrap()).unwrap();
	}
}
