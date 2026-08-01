//! Strict hybrid Carbon project evaluation.
//!
//! `*.carbon.json` is the user-authored mapping document. The generated
//! Studio-owned complement is stored in the mandatory sibling
//! `*.carbon.data/state.carbon`. This module is deliberately the only layer
//! that knows how those two ownership domains compose.

use anyhow::{bail, ensure, Context, Result};
use indexmap::IndexMap;
use path_clean::PathClean;
use rbx_dom_weak::{
	types::{Attributes, Content, ContentType, Ref, Variant, VariantType},
	HashMapExt, Ustr, UstrMap, WeakDom,
};
use rbx_reflection::{ClassTag, DataType, PropertyKind, PropertySerialization, PropertyTag};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
	collections::{BTreeMap, BTreeSet, HashMap, HashSet},
	fs::{self, File},
	io::{BufReader, BufWriter, Read, Write},
	path::{Component, Path, PathBuf},
	sync::atomic::{AtomicBool, Ordering},
};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::{
	artifact_store::{self, CompileReport, ExtractReport, WorktreeContract},
	core::{
		changes::Changes,
		snapshot::{AddedSnapshot, Snapshot, UpdatedSnapshot},
		tree::Tree,
	},
	ext::PathExt,
	manifest_identity::ManifestIdentityAllocator,
	resolution::UnresolvedValue,
	source_wire, util,
};

const PROJECT_SUFFIX: &str = ".carbon.json";
const DATA_ARTIFACT: &str = "state.carbon";
const TRANSACTION_VERSION: u32 = 1;
// This is the cross-release compiled-output compatibility boundary. Bump it
// whenever Carbon changes the meaning or binary encoding of a build output.
const BUILD_CACHE_SCHEMA: u32 = 4;

#[derive(Clone, Debug)]
struct Project {
	name: String,
	tree: Node,
}

#[derive(Clone, Debug, Default)]
struct Node {
	class_name: Option<String>,
	path: Option<PathBuf>,
	path_optional: bool,
	properties: BTreeMap<String, UnresolvedValue>,
	attributes: BTreeMap<String, UnresolvedValue>,
	id: Option<String>,
	children: IndexMap<String, Node>,
}

impl Node {
	fn owns_subtree(&self) -> bool {
		self.path.is_some() || self.class_name.is_some() || !self.properties.is_empty() || !self.attributes.is_empty()
	}
}

#[derive(Clone, Debug)]
pub struct Inspection {
	pub name: String,
	pub data_artifact: PathBuf,
}

impl Inspection {
	pub fn is_place(&self) -> bool {
		true
	}
}

#[derive(Debug)]
pub struct MaterializedProject {
	pub name: String,
	pub directory: PathBuf,
	pub manifest_path: PathBuf,
	pub mapped_refs: HashSet<Ref>,
	pub identity_exclusions: HashSet<Ref>,
	pub(crate) identity_rebindings: Vec<ManagedIdentityRebinding>,
	pub mapped_roots: Vec<Ref>,
	pub mapped_watch_roots: Vec<PathBuf>,
	pub routing_refs: HashSet<Ref>,
	pub snapshot: Snapshot,
}

struct BuildComposite {
	directory: PathBuf,
	manifest_path: PathBuf,
	generation: String,
}

struct BuildMaterializedProject {
	mapped_refs: HashSet<Ref>,
	identity_exclusions: HashSet<Ref>,
	mapped_roots: Vec<Ref>,
	mapped_watch_roots: Vec<PathBuf>,
	routing_refs: HashSet<Ref>,
	tree: Tree,
	composite: Option<BuildComposite>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedIdentityRebindingKind {
	HumanoidStatus,
	ConfigureServerService,
	FilteredSelection,
	AccessoryWeld,
	HeadWeld,
	Descendant,
}

impl ManagedIdentityRebindingKind {
	pub(crate) fn wire_name(self) -> &'static str {
		match self {
			Self::HumanoidStatus => "humanoidStatus",
			Self::ConfigureServerService => "configureServerService",
			Self::FilteredSelection => "filteredSelection",
			Self::AccessoryWeld => "accessoryWeld",
			Self::HeadWeld => "headWeld",
			Self::Descendant => "descendant",
		}
	}
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedIdentityRebinding {
	pub source_id: Ref,
	pub parent_source_id: Ref,
	pub class_name: String,
	pub name: String,
	pub kind: ManagedIdentityRebindingKind,
	pub related_source_id: Option<Ref>,
}

#[derive(Clone, Debug)]
pub struct LivePolicy {
	pub project_path: PathBuf,
	pub project_document: Vec<u8>,
	pub composite_manifest: PathBuf,
	pub mapped_refs: HashSet<Ref>,
	pub retired_mapped_refs: HashSet<Ref>,
	pub mapped_roots: Vec<Ref>,
	pub mapped_watch_roots: Vec<PathBuf>,
	pub routing_refs: HashSet<Ref>,
}

impl LivePolicy {
	pub fn reject_studio_changes(&self, canonical: &Snapshot, changes: &mut Changes) -> Result<()> {
		let mut canonical_index = HashMap::new();
		index_canonical_snapshot(canonical, None, &mut canonical_index);
		// A filesystem transaction may replace mapped identities while Studio's
		// causal watcher is still draining signals for the old realization. Those
		// identities can no longer describe user-authored state and must never be
		// allowed to fall through into the Studio-owned manifest transaction.
		changes.additions.retain(|addition| {
			!self.retired_mapped_refs.contains(&addition.id) && !self.retired_mapped_refs.contains(&addition.parent)
		});
		changes
			.updates
			.retain(|update| !self.retired_mapped_refs.contains(&update.id));
		changes.removals.retain(|id| !self.retired_mapped_refs.contains(id));
		changes.additions.retain(|addition| {
			!self.mapped_refs.contains(&addition.parent) || !canonical_addition_echo(addition, &canonical_index)
		});
		changes.updates.retain(|update| {
			if self.routing_refs.contains(&update.id) {
				!canonical_routing_update_echo(update, &canonical_index)
			} else {
				!self.mapped_refs.contains(&update.id) || !canonical_update_echo(update, &canonical_index)
			}
		});
		let mut reasons = Vec::new();
		for addition in &changes.additions {
			if self.mapped_refs.contains(&addition.parent) {
				reasons.push(format!(
					"creation of {} '{}' beneath a filesystem-owned mapping barrier",
					addition.class, addition.name
				));
			}
		}
		for update in &changes.updates {
			if self.mapped_refs.contains(&update.id) {
				let fields = describe_noncanonical_update(update, canonical_index.get(&update.id));
				reasons.push(format!(
					"mutation of filesystem-owned instance {} ({fields})",
					update.id
				));
			}
			if update.parent.is_some_and(|parent| self.mapped_refs.contains(&parent)) {
				reasons.push(format!(
					"move of instance {} beneath a filesystem-owned mapping barrier",
					update.id
				));
			}
			if self.routing_refs.contains(&update.id)
				&& (update.parent.is_some() || update.name.is_some() || update.class.is_some())
			{
				let fields = describe_noncanonical_update(update, canonical_index.get(&update.id));
				reasons.push(format!(
					"structural mutation of engine mapping route instance {} ({fields})",
					update.id,
				));
			}
		}
		for removal in &changes.removals {
			if self.mapped_refs.contains(removal) {
				reasons.push(format!("deletion of filesystem-owned instance {removal}"));
			}
			if self.routing_refs.contains(removal) {
				reasons.push(format!("deletion of engine mapping route instance {removal}"));
			}
		}
		reasons.sort();
		reasons.dedup();
		ensure!(
			reasons.is_empty(),
			"unsupported Studio change rejected; mapped project source is authoritative: {}",
			reasons.join("; ")
		);
		Ok(())
	}
}

struct CanonicalLocation<'a> {
	snapshot: &'a Snapshot,
	parent: Option<Ref>,
}

fn index_canonical_snapshot<'a>(
	snapshot: &'a Snapshot,
	parent: Option<Ref>,
	index: &mut HashMap<Ref, CanonicalLocation<'a>>,
) {
	index.insert(snapshot.id, CanonicalLocation { snapshot, parent });
	for child in &snapshot.children {
		index_canonical_snapshot(child, Some(snapshot.id), index);
	}
}

fn canonical_addition_echo(addition: &AddedSnapshot, index: &HashMap<Ref, CanonicalLocation<'_>>) -> bool {
	let Some(canonical) = index.get(&addition.id) else {
		return false;
	};
	canonical.parent == Some(addition.parent)
		&& addition.name == canonical.snapshot.name
		&& addition.raw_name == canonical.snapshot.raw_name
		&& addition.class == canonical.snapshot.class
		&& mapped_property_maps_equal(
			canonical.snapshot.class.as_str(),
			&canonical.snapshot.properties,
			&addition.properties,
		) && mapped_children_equal(&canonical.snapshot.children, &addition.children)
}

fn canonical_update_echo(update: &UpdatedSnapshot, index: &HashMap<Ref, CanonicalLocation<'_>>) -> bool {
	let Some(canonical) = index.get(&update.id) else {
		return false;
	};
	if update.parent.is_some_and(|parent| Some(parent) != canonical.parent)
		|| update
			.name
			.as_ref()
			.is_some_and(|name| name != &canonical.snapshot.name)
		|| update
			.raw_name
			.as_ref()
			.is_some_and(|raw_name| Some(raw_name) != canonical.snapshot.raw_name.as_ref())
		|| update
			.class
			.as_ref()
			.is_some_and(|class| class != &canonical.snapshot.class)
	{
		return false;
	}
	if let Some(properties) = &update.properties {
		for (name, value) in properties {
			if !mapped_property_equal(
				canonical.snapshot.class.as_str(),
				name.as_str(),
				canonical.snapshot.properties.get(name),
				Some(value),
			) {
				return false;
			}
		}
	}
	update.removed_properties.iter().all(|name| {
		mapped_property_equal(
			canonical.snapshot.class.as_str(),
			name.as_str(),
			canonical.snapshot.properties.get(name),
			None,
		)
	})
}

fn canonical_routing_update_echo(update: &UpdatedSnapshot, index: &HashMap<Ref, CanonicalLocation<'_>>) -> bool {
	canonical_update_echo(update, index)
}

fn mapped_children_equal(left: &[Snapshot], right: &[Snapshot]) -> bool {
	if left.len() != right.len() {
		return false;
	}
	let right = right.iter().map(|child| (child.id, child)).collect::<HashMap<_, _>>();
	left.iter().all(|left| {
		right.get(&left.id).is_some_and(|right| {
			left.name == right.name
				&& left.raw_name == right.raw_name
				&& left.class == right.class
				&& mapped_property_maps_equal(left.class.as_str(), &left.properties, &right.properties)
				&& mapped_children_equal(&left.children, &right.children)
		})
	})
}

fn mapped_property_maps_equal(class: &str, left: &UstrMap<Variant>, right: &UstrMap<Variant>) -> bool {
	let mut names = BTreeSet::new();
	names.extend(left.keys().copied());
	names.extend(right.keys().copied());
	names
		.into_iter()
		.all(|name| mapped_property_equal(class, name.as_str(), left.get(&name), right.get(&name)))
}

fn mapped_property_equal(class: &str, name: &str, left: Option<&Variant>, right: Option<&Variant>) -> bool {
	let default = reflection_default(class, name);
	let left = left.or(default);
	let right = right.or(default);
	match (left, right) {
		(Some(left), Some(right)) if name == "Attributes" => mapped_attributes_equal(left, right),
		(Some(Variant::String(left)), Some(Variant::BinaryString(right))) if name == "Source" => {
			<rbx_dom_weak::types::BinaryString as AsRef<[u8]>>::as_ref(right) == left.as_bytes()
		}
		(Some(Variant::BinaryString(left)), Some(Variant::String(right))) if name == "Source" => {
			<rbx_dom_weak::types::BinaryString as AsRef<[u8]>>::as_ref(left) == right.as_bytes()
		}
		_ => left == right,
	}
}

fn mapped_attributes_equal(left: &Variant, right: &Variant) -> bool {
	fn decode(value: &Variant) -> Option<Attributes> {
		match value {
			Variant::Attributes(attributes) => Some(attributes.clone()),
			Variant::BinaryString(raw) => Attributes::from_reader(AsRef::<[u8]>::as_ref(raw)).ok(),
			_ => None,
		}
	}
	let (Some(left), Some(right)) = (decode(left), decode(right)) else {
		return left == right;
	};
	if left.len() != right.len() {
		return false;
	}
	left.iter().all(|(name, left)| {
		right
			.get(name.as_str())
			.is_some_and(|right| mapped_attribute_value_equal(left, right))
	})
}

fn mapped_attribute_value_equal(left: &Variant, right: &Variant) -> bool {
	match (left, right) {
		(Variant::String(left), Variant::BinaryString(right)) => {
			<rbx_dom_weak::types::BinaryString as AsRef<[u8]>>::as_ref(right) == left.as_bytes()
		}
		(Variant::BinaryString(left), Variant::String(right)) => {
			<rbx_dom_weak::types::BinaryString as AsRef<[u8]>>::as_ref(left) == right.as_bytes()
		}
		_ => left == right,
	}
}

fn describe_update(update: &UpdatedSnapshot) -> String {
	let mut fields = Vec::new();
	if update.parent.is_some() {
		fields.push("parent".to_owned());
	}
	if update.name.is_some() {
		fields.push("name".to_owned());
	}
	if update.class.is_some() {
		fields.push("class".to_owned());
	}
	if let Some(properties) = &update.properties {
		fields.extend(properties.keys().map(|name| format!("property {name}")));
	}
	fields.extend(
		update
			.removed_properties
			.iter()
			.map(|name| format!("removed property {name}")),
	);
	if fields.is_empty() {
		"empty update".to_owned()
	} else {
		fields.join(", ")
	}
}

fn describe_noncanonical_update(update: &UpdatedSnapshot, canonical: Option<&CanonicalLocation<'_>>) -> String {
	let Some(canonical) = canonical else {
		return describe_update(update);
	};
	let mut fields = Vec::new();
	if update.parent.is_some_and(|parent| Some(parent) != canonical.parent) {
		fields.push("parent".to_owned());
	}
	if update
		.name
		.as_ref()
		.is_some_and(|name| name != &canonical.snapshot.name)
	{
		fields.push("name".to_owned());
	}
	if update
		.raw_name
		.as_ref()
		.is_some_and(|raw_name| Some(raw_name) != canonical.snapshot.raw_name.as_ref())
	{
		fields.push("rawName".to_owned());
	}
	if update
		.class
		.as_ref()
		.is_some_and(|class| class != &canonical.snapshot.class)
	{
		fields.push("class".to_owned());
	}
	if let Some(properties) = &update.properties {
		fields.extend(
			properties
				.iter()
				.filter(|(name, value)| {
					!mapped_property_equal(
						canonical.snapshot.class.as_str(),
						name.as_str(),
						canonical.snapshot.properties.get(name),
						Some(value),
					)
				})
				.map(|(name, _)| format!("property {name}")),
		);
	}
	fields.extend(
		update
			.removed_properties
			.iter()
			.filter(|name| {
				!mapped_property_equal(
					canonical.snapshot.class.as_str(),
					name.as_str(),
					canonical.snapshot.properties.get(name),
					None,
				)
			})
			.map(|name| format!("removed property {name}")),
	);
	if fields.is_empty() {
		"noncanonical wire representation".to_owned()
	} else {
		fields.join(", ")
	}
}

pub fn is_project_path(path: &Path) -> bool {
	path.file_name()
		.and_then(|name| name.to_str())
		.is_some_and(|name| name.ends_with(PROJECT_SUFFIX))
}

pub fn data_dir(project_path: &Path) -> Result<PathBuf> {
	let name = project_path
		.file_name()
		.and_then(|name| name.to_str())
		.context("Carbon project path has no UTF-8 file name")?;
	let stem = name
		.strip_suffix(".json")
		.context("Carbon project must end in .carbon.json")?;
	Ok(project_path.with_file_name(format!("{stem}.data")))
}

pub fn data_artifact(project_path: &Path) -> Result<PathBuf> {
	Ok(data_dir(project_path)?.join(DATA_ARTIFACT))
}

pub fn inspect(project_path: &Path) -> Result<Inspection> {
	recover(project_path)?;
	let project = load_project(project_path)?;
	let manifest = data_artifact(project_path)?;
	ensure!(
		manifest.is_file(),
		"mandatory Studio data manifest is missing: {}",
		manifest.display()
	);
	let studio = artifact_store::inspect(&manifest)?;
	ensure!(studio.is_place(), "Studio data manifest root must be DataModel");
	Ok(Inspection {
		name: project.name,
		data_artifact: manifest,
	})
}

pub fn initialize(project_path: &Path, name: String) -> Result<ExtractReport> {
	ensure!(is_project_path(project_path), "output must be a .carbon.json project");
	ensure!(
		!project_path.exists(),
		"Carbon project already exists: {}",
		project_path.display()
	);
	let data = data_dir(project_path)?;
	ensure!(
		!data.exists(),
		"Carbon data directory already exists: {}",
		data.display()
	);
	let root = project_path.parent().unwrap_or_else(|| Path::new("."));
	for relative in starter_files().keys() {
		ensure!(
			!root.join(relative).exists(),
			"starter source path already exists: {}",
			root.join(relative).display()
		);
	}

	let stage = transaction_stage(project_path)?;
	fs::create_dir_all(&stage)?;
	let staged_project = stage.join(project_path.get_name());
	write_json(&staged_project, &starter_project_json(&name))?;
	for (relative, contents) in starter_files() {
		write_text(&stage.join(relative), contents)?;
	}
	let studio = empty_studio_snapshot();
	let generated = generate_data_store(&stage, &studio, &name)?;
	let staged_data = stage.join(data.file_name().context("data directory has no name")?);
	fs::rename(generated, &staged_data)?;
	let report = artifact_store_report(&staged_data.join(DATA_ARTIFACT))?;
	commit_new_project(
		project_path,
		&stage,
		&staged_project,
		&staged_data,
		starter_files().keys().cloned(),
	)?;
	Ok(report)
}

pub fn extract_binary(input: &Path, project_path: &Path) -> Result<ExtractReport> {
	ensure!(is_project_path(project_path), "output must be a .carbon.json project");
	ensure!(
		!project_path.exists(),
		"Carbon project already exists: {}",
		project_path.display()
	);
	let data = data_dir(project_path)?;
	ensure!(
		!data.exists(),
		"Carbon data directory already exists: {}",
		data.display()
	);
	let project_root = project_path.parent().unwrap_or_else(|| Path::new("."));
	let file = File::open(input).with_context(|| format!("failed to open {}", input.display()))?;
	let dom = rbx_binary::Deserializer::new(util::get_reflection_database()).deserialize(BufReader::new(file))?;
	let mut snapshot = dom_snapshot(&dom, dom.root_ref())?;
	stabilize_snapshot_ids(&mut snapshot)?;
	let extracted = plan_script_extraction(&snapshot);
	for output in &extracted.outputs {
		ensure!(
			!project_root.join(output).exists(),
			"extraction output collision: {}",
			project_root.join(output).display()
		);
	}

	let stage = transaction_stage(project_path)?;
	fs::create_dir_all(&stage)?;
	let staged_project = stage.join(project_path.get_name());
	write_json(&staged_project, &project_json(input.get_stem(), &extracted.entries))?;
	for entry in &extracted.entries {
		write_mapped_snapshot(&stage.join(&entry.output), &entry.snapshot, &extracted.required_ids)?;
	}
	let studio = prune_routes(snapshot, &extracted.routes)?;
	let generated = generate_data_store(&stage, &studio, input.get_stem())?;
	let staged_data = stage.join(data.file_name().context("data directory has no name")?);
	fs::rename(generated, &staged_data)?;
	let report = artifact_store_report(&staged_data.join(DATA_ARTIFACT))?;
	commit_new_project(project_path, &stage, &staged_project, &staged_data, extracted.outputs)?;
	Ok(report)
}

pub fn materialize(project_path: &Path) -> Result<MaterializedProject> {
	materialize_mode(project_path, false)
}

pub fn materialize_for_capture(project_path: &Path) -> Result<MaterializedProject> {
	let materialized = materialize_mode(project_path, true)?;
	let policy = live_policy(project_path, &materialized);
	persist_studio_domain(&policy)?;
	Ok(materialized)
}

fn materialize_for_build(project_path: &Path, exact_generation: bool) -> Result<BuildMaterializedProject> {
	recover(project_path)?;
	let project = load_project(project_path)?;
	let manifest = data_artifact(project_path)?;
	let studio = artifact_store::load_tree(&manifest)?;
	let studio_snapshot = studio.tree.into_snapshot()?;
	let mut evaluation = Evaluation::new(project_path, &project, studio_snapshot, false)?;
	evaluation.apply()?;
	let Evaluation {
		snapshot,
		barrier_routes,
		mapped_traversal,
		..
	} = evaluation;
	let (mapped_refs, mapped_roots, routing_refs) = snapshot_refs_for_routes(&snapshot, &barrier_routes)?;
	validate_no_cross_domain_references(&snapshot, &mapped_refs)?;
	let identity_exclusions = managed_build_identity_exclusions(&snapshot, &mapped_refs);
	let tree = Tree::new(snapshot);
	let composite = if exact_generation {
		let directory = project_path
			.parent()
			.unwrap_or_else(|| Path::new("."))
			.join(format!(".carbon-composite-{}", Uuid::new_v4().simple()));
		fs::create_dir_all(&directory)?;
		let manifest_path = directory.join("state.carbon");
		let staged = (|| -> Result<BuildComposite> {
			artifact_store::extract_tree(&tree, project.name.clone(), &manifest_path)
				.context("failed to stage composed managed source")?;
			let generation = artifact_store::validated_artifact_receipt(&manifest_path)
				.context("failed to validate composed managed source")?
				.generation()
				.to_owned();
			Ok(BuildComposite {
				directory: directory.clone(),
				manifest_path,
				generation,
			})
		})();
		if staged.is_err() {
			let _ = fs::remove_dir_all(&directory);
		}
		Some(staged?)
	} else {
		None
	};
	Ok(BuildMaterializedProject {
		mapped_refs,
		identity_exclusions,
		mapped_roots,
		mapped_watch_roots: mapped_traversal.watch_roots.into_iter().collect(),
		routing_refs,
		tree,
		composite,
	})
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BuildCacheMetadata {
	schema: u32,
	key: String,
	bytes: u64,
	hash: String,
	report: CompileReport,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BuildLayerMetadata {
	schema: u32,
	key: String,
	bytes: u64,
	hash: String,
	report: CompileReport,
	scripts: Vec<CachedScriptPosition>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CachedScriptPosition {
	source_id: String,
	class_name: String,
	index: u32,
}

struct MappedScriptSource {
	id: Ref,
	source: Vec<u8>,
}

struct MappedSourceState {
	generation: String,
	structure: String,
	scripts: Vec<MappedScriptSource>,
}

#[cfg(test)]
fn build_cache_root(project_path: &Path) -> Option<PathBuf> {
	Some(
		project_path
			.parent()
			.unwrap_or_else(|| Path::new("."))
			.join(format!(".{}.build-cache", project_path.get_name())),
	)
}

#[cfg(not(test))]
fn build_cache_root(_project_path: &Path) -> Option<PathBuf> {
	directories::BaseDirs::new().map(|base| base.cache_dir().join("carbon/builds/v1"))
}

fn hash_build_cache_field(hasher: &mut blake3::Hasher, name: &str, value: &[u8]) {
	hasher.update(&(name.len() as u64).to_le_bytes());
	hasher.update(name.as_bytes());
	hasher.update(&(value.len() as u64).to_le_bytes());
	hasher.update(value);
}

fn mapped_source_state(project_path: &Path, project: &Project) -> Result<MappedSourceState> {
	// This baseline is deliberately deterministic. `empty_studio_snapshot`
	// allocates session identities and would make identical source miss cache.
	let baseline = Snapshot::new()
		.with_id(stable_ref("build-cache:DataModel"))
		.with_name("DataModel")
		.with_class("DataModel");
	let mut evaluation = Evaluation::new(project_path, project, baseline, false)?;
	evaluation.apply()?;
	let encoded = rmp_serde::to_vec_named(&(&project.name, &evaluation.snapshot, &evaluation.barrier_routes))?;
	let generation = blake3::hash(&encoded).to_hex().to_string();
	let mut structural_snapshot = evaluation.snapshot;
	let mut scripts = Vec::new();
	fn collect(snapshot: &mut Snapshot, scripts: &mut Vec<MappedScriptSource>) -> Result<()> {
		if is_script_class(snapshot.class.as_str()) {
			scripts.push(MappedScriptSource {
				id: snapshot.id,
				source: script_source(snapshot)?.into_bytes(),
			});
			snapshot.properties.remove(&Ustr::from("Source"));
		}
		for child in &mut snapshot.children {
			collect(child, scripts)?;
		}
		Ok(())
	}
	collect(&mut structural_snapshot, &mut scripts)?;
	scripts.sort_unstable_by_key(|script| script.id.to_string());
	let structural = rmp_serde::to_vec_named(&(&project.name, &structural_snapshot, &evaluation.barrier_routes))?;
	Ok(MappedSourceState {
		generation,
		structure: blake3::hash(&structural).to_hex().to_string(),
		scripts,
	})
}

fn mapped_source_generation(project_path: &Path, project: &Project) -> Result<String> {
	Ok(mapped_source_state(project_path, project)?.generation)
}

fn build_cache_key(
	project_path: &Path,
	project: &Project,
	hybrid: bool,
	worktree: Option<&WorktreeContract>,
) -> Result<String> {
	let receipt = artifact_store::validated_artifact_receipt(&data_artifact(project_path)?)?;
	let mut hasher = blake3::Hasher::new();
	hasher.update(b"carbon-build-cache-v1\0");
	hash_build_cache_field(&mut hasher, "schema", &BUILD_CACHE_SCHEMA.to_le_bytes());
	hash_build_cache_field(&mut hasher, "os", std::env::consts::OS.as_bytes());
	hash_build_cache_field(&mut hasher, "arch", std::env::consts::ARCH.as_bytes());
	hash_build_cache_field(&mut hasher, "studio", receipt.build_generation().as_bytes());
	hash_build_cache_field(&mut hasher, "projectName", project.name.as_bytes());
	if hybrid {
		hash_build_cache_field(
			&mut hasher,
			"mappedSource",
			mapped_source_generation(project_path, project)?.as_bytes(),
		);
	} else {
		hash_build_cache_field(&mut hasher, "mappedSource", b"direct");
	}
	match worktree {
		Some(contract) => {
			hash_build_cache_field(&mut hasher, "mode", b"managed");
			// Per-launch transport values live only in Workspace.Attributes. They
			// are rewritten directly on a cache hit and must not invalidate the
			// hundreds of thousands of unrelated instances in the place payload.
			let mut exclusions = contract
				.identity_exclusions
				.iter()
				.map(ToString::to_string)
				.collect::<Vec<_>>();
			exclusions.sort();
			for exclusion in exclusions {
				hash_build_cache_field(&mut hasher, "identityExclusion", exclusion.as_bytes());
			}
		}
		None => hash_build_cache_field(&mut hasher, "mode", b"unmanaged"),
	}
	Ok(hasher.finalize().to_hex().to_string())
}

fn build_layer_key(project_path: &Path, project: &Project, source: &MappedSourceState) -> Result<String> {
	let receipt = artifact_store::validated_artifact_receipt(&data_artifact(project_path)?)?;
	let mut hasher = blake3::Hasher::new();
	hasher.update(b"carbon-build-layer-v1\0");
	hash_build_cache_field(&mut hasher, "schema", &BUILD_CACHE_SCHEMA.to_le_bytes());
	hash_build_cache_field(&mut hasher, "os", std::env::consts::OS.as_bytes());
	hash_build_cache_field(&mut hasher, "arch", std::env::consts::ARCH.as_bytes());
	hash_build_cache_field(&mut hasher, "studio", receipt.build_generation().as_bytes());
	hash_build_cache_field(&mut hasher, "projectName", project.name.as_bytes());
	hash_build_cache_field(&mut hasher, "mappedStructure", source.structure.as_bytes());
	hash_build_cache_field(&mut hasher, "mode", b"unmanaged");
	Ok(hasher.finalize().to_hex().to_string())
}

fn build_output_hash(path: &Path) -> Result<(u64, String)> {
	let mut file = File::open(path)?;
	let mut hasher = blake3::Hasher::new();
	let mut bytes = 0_u64;
	let mut buffer = [0_u8; 64 * 1024];
	loop {
		let read = file.read(&mut buffer)?;
		if read == 0 {
			break;
		}
		bytes = bytes.checked_add(read as u64).context("build output is too large")?;
		hasher.update(&buffer[..read]);
	}
	Ok((bytes, hasher.finalize().to_hex().to_string()))
}

fn stage_cached_build(project_path: &Path, key: &str, output: &Path) -> Option<(PathBuf, CompileReport)> {
	fn stage(root: &Path, key: &str, output: &Path) -> Result<Option<(PathBuf, CompileReport)>> {
		let entry = root.join(key);
		let metadata_path = entry.join("metadata.json");
		let cached_output = entry.join("output.rbxl");
		if !metadata_path.is_file() || !cached_output.is_file() {
			return Ok(None);
		}
		let metadata: BuildCacheMetadata = serde_json::from_slice(&fs::read(&metadata_path)?)?;
		if metadata.schema != BUILD_CACHE_SCHEMA || metadata.key != key {
			return Ok(None);
		}
		let (bytes, hash) = build_output_hash(&cached_output)?;
		if bytes != metadata.bytes || hash != metadata.hash {
			return Ok(None);
		}
		let output_name = output
			.file_name()
			.and_then(|name| name.to_str())
			.context("build output has no UTF-8 file name")?;
		if let Some(parent) = output.parent() {
			fs::create_dir_all(parent)?;
		}
		let staged = output.with_file_name(format!(".{output_name}.carbon-cache-{}", Uuid::new_v4().simple()));
		let copied = (|| -> Result<()> {
			fs::copy(&cached_output, &staged)?;
			let (copied_bytes, copied_hash) = build_output_hash(&staged)?;
			ensure!(
				copied_bytes == metadata.bytes && copied_hash == metadata.hash,
				"cached build changed while it was being copied"
			);
			Ok(())
		})();
		if let Err(error) = copied {
			let _ = fs::remove_file(&staged);
			return Err(error);
		}
		Ok(Some((staged, metadata.report)))
	}

	build_cache_root(project_path).and_then(|root| stage(&root, key, output).ok().flatten())
}

fn store_cached_build(project_path: &Path, key: &str, output: &Path, report: &CompileReport) {
	fn store(root: &Path, key: &str, output: &Path, report: &CompileReport) -> Result<()> {
		let (bytes, hash) = build_output_hash(output)?;
		let entry = root.join(key);
		fs::create_dir_all(&entry)?;
		let cached_output = entry.join("output.rbxl");
		let staged_output = entry.join(format!(".output-{}.tmp", Uuid::new_v4().simple()));
		fs::copy(output, &staged_output)?;
		let (copied_bytes, copied_hash) = build_output_hash(&staged_output)?;
		ensure!(
			copied_bytes == bytes && copied_hash == hash,
			"build output changed while it was being cached"
		);
		fs::rename(&staged_output, &cached_output)?;
		let metadata = BuildCacheMetadata {
			schema: BUILD_CACHE_SCHEMA,
			key: key.to_owned(),
			bytes,
			hash,
			report: report.clone(),
		};
		let staged_metadata = entry.join(format!(".metadata-{}.tmp", Uuid::new_v4().simple()));
		write_json(&staged_metadata, &serde_json::to_value(metadata)?)?;
		fs::rename(staged_metadata, entry.join("metadata.json"))?;
		Ok(())
	}

	if let Some(root) = build_cache_root(project_path) {
		let _ = store(&root, key, output, report);
	}
}

fn stage_cached_source_layer(
	project_path: &Path,
	key: &str,
	source: &MappedSourceState,
	output: &Path,
) -> Option<(PathBuf, CompileReport)> {
	fn stage(
		root: &Path,
		key: &str,
		source: &MappedSourceState,
		output: &Path,
	) -> Result<Option<(PathBuf, CompileReport)>> {
		let entry = root.join("layers").join(key);
		let metadata_path = entry.join("metadata.json");
		let cached_output = entry.join("output.rbxl");
		if !metadata_path.is_file() || !cached_output.is_file() {
			return Ok(None);
		}
		let metadata: BuildLayerMetadata = serde_json::from_slice(&fs::read(&metadata_path)?)?;
		if metadata.schema != BUILD_CACHE_SCHEMA || metadata.key != key {
			return Ok(None);
		}
		let (bytes, hash) = build_output_hash(&cached_output)?;
		if bytes != metadata.bytes || hash != metadata.hash {
			return Ok(None);
		}

		let sources = source
			.scripts
			.iter()
			.map(|script| (script.id.to_string(), script.source.as_slice()))
			.collect::<HashMap<_, _>>();
		if sources.len() != metadata.scripts.len() {
			return Ok(None);
		}
		let mut patches = Vec::with_capacity(metadata.scripts.len());
		for position in &metadata.scripts {
			let Some(source) = sources.get(&position.source_id) else {
				return Ok(None);
			};
			patches.push(rbx_binary::ScriptSourcePatch {
				class_name: position.class_name.clone(),
				index: position.index,
				source: source.to_vec(),
			});
		}

		let output_name = output
			.file_name()
			.and_then(|name| name.to_str())
			.context("build output has no UTF-8 file name")?;
		if let Some(parent) = output.parent() {
			fs::create_dir_all(parent)?;
		}
		let staged = output.with_file_name(format!(".{output_name}.carbon-layer-{}", Uuid::new_v4().simple()));
		let patched = (|| -> Result<()> {
			fs::copy(&cached_output, &staged)?;
			let (copied_bytes, copied_hash) = build_output_hash(&staged)?;
			ensure!(
				copied_bytes == metadata.bytes && copied_hash == metadata.hash,
				"cached build layer changed while it was being copied"
			);
			artifact_store::rewrite_script_sources(&staged, &patches)
		})();
		if let Err(error) = patched {
			let _ = fs::remove_file(&staged);
			return Err(error);
		}
		Ok(Some((staged, metadata.report)))
	}

	build_cache_root(project_path).and_then(|root| stage(&root, key, source, output).ok().flatten())
}

fn store_cached_source_layer(
	project_path: &Path,
	key: &str,
	source: &MappedSourceState,
	positions: &HashMap<Ref, rbx_binary::SerializedInstancePosition>,
	output: &Path,
	report: &CompileReport,
) {
	fn store(
		root: &Path,
		key: &str,
		source: &MappedSourceState,
		positions: &HashMap<Ref, rbx_binary::SerializedInstancePosition>,
		output: &Path,
		report: &CompileReport,
	) -> Result<()> {
		let mut scripts = Vec::with_capacity(source.scripts.len());
		for script in &source.scripts {
			let position = positions
				.get(&script.id)
				.with_context(|| format!("compiled output omitted mapped script {}", script.id))?;
			ensure!(
				is_script_class(&position.class_name),
				"mapped script {} compiled into non-script class {}",
				script.id,
				position.class_name
			);
			scripts.push(CachedScriptPosition {
				source_id: script.id.to_string(),
				class_name: position.class_name.clone(),
				index: position.index,
			});
		}
		scripts.sort_by(|left, right| left.source_id.cmp(&right.source_id));

		let (bytes, hash) = build_output_hash(output)?;
		let entry = root.join("layers").join(key);
		fs::create_dir_all(&entry)?;
		let cached_output = entry.join("output.rbxl");
		let staged_output = entry.join(format!(".output-{}.tmp", Uuid::new_v4().simple()));
		fs::copy(output, &staged_output)?;
		let (copied_bytes, copied_hash) = build_output_hash(&staged_output)?;
		ensure!(
			copied_bytes == bytes && copied_hash == hash,
			"build output changed while its source layer was being cached"
		);
		fs::rename(&staged_output, &cached_output)?;
		let metadata = BuildLayerMetadata {
			schema: BUILD_CACHE_SCHEMA,
			key: key.to_owned(),
			bytes,
			hash,
			report: report.clone(),
			scripts,
		};
		let staged_metadata = entry.join(format!(".metadata-{}.tmp", Uuid::new_v4().simple()));
		write_json(&staged_metadata, &serde_json::to_value(metadata)?)?;
		fs::rename(staged_metadata, entry.join("metadata.json"))?;
		Ok(())
	}

	if let Some(root) = build_cache_root(project_path) {
		let _ = store(&root, key, source, positions, output, report);
	}
}

fn managed_build_identity_anchors(snapshot: &Snapshot) -> (HashSet<Ref>, HashSet<Ref>) {
	fn scan(
		node: &Snapshot,
		parent: Option<&Snapshot>,
		edit_cameras: &mut HashSet<Ref>,
		accessory_handles: &mut HashSet<Ref>,
	) {
		if node.class.as_str() == "Workspace" {
			if let Some(Variant::Ref(camera)) = node.properties.get(&Ustr::from("CurrentCamera")) {
				if camera.is_some() {
					edit_cameras.insert(*camera);
				}
			}
		}
		if node.class.as_str() == "Part"
			&& node.name == "Handle"
			&& parent.is_some_and(|parent| parent.class.as_str() == "Accessory")
			&& node
				.children
				.iter()
				.any(|child| child.class.as_str() == "Weld" && child.name == "AccessoryWeld")
		{
			accessory_handles.insert(node.id);
		}
		for child in &node.children {
			scan(child, Some(node), edit_cameras, accessory_handles);
		}
	}

	let mut edit_cameras = HashSet::new();
	let mut accessory_handles = HashSet::new();
	scan(snapshot, None, &mut edit_cameras, &mut accessory_handles);
	(edit_cameras, accessory_handles)
}

/// Canonical identities whose disposable build instances are replaced by
/// Studio. The descriptors are in parent-before-child order so RML can bind
/// each replacement beneath an already authoritative parent.
pub(crate) fn managed_build_identity_rebindings(
	snapshot: &Snapshot,
	mapped_refs: &HashSet<Ref>,
) -> Vec<ManagedIdentityRebinding> {
	fn classify(
		node: &Snapshot,
		parent: Option<&Snapshot>,
		grandparent: Option<&Snapshot>,
		context: &IdentityRebindingContext<'_>,
	) -> Option<(ManagedIdentityRebindingKind, Option<Ref>)> {
		if node.class.as_str() == "Status" && parent.is_some_and(|parent| parent.class.as_str() == "Humanoid") {
			return Some((ManagedIdentityRebindingKind::HumanoidStatus, None));
		}
		if node.class.as_str() == "ConfigureServerService"
			&& parent.is_some_and(|parent| parent.class.as_str() == "DataModel")
		{
			return Some((ManagedIdentityRebindingKind::ConfigureServerService, None));
		}
		if node.class.as_str() == "Instance"
			&& node.name == "FilteredSelection"
			&& node.children.is_empty()
			&& parent.is_some_and(|parent| parent.class.as_str() == "DataModel")
		{
			return Some((ManagedIdentityRebindingKind::FilteredSelection, None));
		}
		if node.class.as_str() == "Weld"
			&& node.name == "AccessoryWeld"
			&& parent.is_some_and(|parent| parent.class.as_str() == "Part" && parent.name == "Handle")
			&& grandparent.is_some_and(|grandparent| grandparent.class.as_str() == "Accessory")
		{
			return Some((ManagedIdentityRebindingKind::AccessoryWeld, None));
		}
		if node.class.as_str() == "Weld"
			&& node.name == "HeadWeld"
			&& parent.is_some_and(|parent| parent.class.as_str() == "Part" && parent.name == "Head")
		{
			if let Some(Variant::Ref(target)) = node.properties.get(&Ustr::from("Part1")) {
				if context.accessory_handles.contains(target) {
					return Some((ManagedIdentityRebindingKind::HeadWeld, Some(*target)));
				}
			}
		}
		None
	}

	fn collect(
		node: &Snapshot,
		parent: Option<&Snapshot>,
		grandparent: Option<&Snapshot>,
		ancestor_rehydrated: bool,
		ancestor_unbound: bool,
		context: &IdentityRebindingContext<'_>,
		rebindings: &mut Vec<ManagedIdentityRebinding>,
	) {
		let unbound_here =
			ancestor_unbound || context.mapped_refs.contains(&node.id) || context.edit_cameras.contains(&node.id);
		let classified = (!ancestor_rehydrated && !unbound_here)
			.then(|| classify(node, parent, grandparent, context))
			.flatten();
		let rehydrated_here = !unbound_here && (ancestor_rehydrated || classified.is_some());
		if rehydrated_here {
			let (kind, related_source_id) = classified.unwrap_or((ManagedIdentityRebindingKind::Descendant, None));
			if let Some(parent) = parent {
				rebindings.push(ManagedIdentityRebinding {
					source_id: node.id,
					parent_source_id: parent.id,
					class_name: node.class.to_string(),
					name: node.name.clone(),
					kind,
					related_source_id,
				});
			}
		}
		for child in &node.children {
			collect(
				child,
				Some(node),
				parent,
				rehydrated_here,
				unbound_here,
				context,
				rebindings,
			);
		}
	}
	struct IdentityRebindingContext<'a> {
		mapped_refs: &'a HashSet<Ref>,
		edit_cameras: &'a HashSet<Ref>,
		accessory_handles: &'a HashSet<Ref>,
	}

	let (edit_cameras, accessory_handles) = managed_build_identity_anchors(snapshot);
	let context = IdentityRebindingContext {
		mapped_refs,
		edit_cameras: &edit_cameras,
		accessory_handles: &accessory_handles,
	};
	let mut rebindings = Vec::new();
	collect(snapshot, None, None, false, false, &context, &mut rebindings);
	rebindings
}

/// Source identities that cannot carry marker attributes into the hydrated
/// DataModel. Filesystem-owned and omitted edit-camera subtrees stay outside
/// the manifest ledger; other Studio-rehydrated identities are restored by
/// the bootstrap contract.
fn managed_build_identity_exclusions(snapshot: &Snapshot, mapped_refs: &HashSet<Ref>) -> HashSet<Ref> {
	fn collect_unbound(
		node: &Snapshot,
		mapped_refs: &HashSet<Ref>,
		edit_cameras: &HashSet<Ref>,
		ancestor_unbound: bool,
		excluded: &mut HashSet<Ref>,
	) {
		let unbound_here = ancestor_unbound || mapped_refs.contains(&node.id) || edit_cameras.contains(&node.id);
		if unbound_here {
			excluded.insert(node.id);
		}
		for child in &node.children {
			collect_unbound(child, mapped_refs, edit_cameras, unbound_here, excluded);
		}
	}

	let (edit_cameras, _) = managed_build_identity_anchors(snapshot);
	let rebindings = managed_build_identity_rebindings(snapshot, mapped_refs);
	let mut excluded = rebindings.iter().map(|rebinding| rebinding.source_id).collect();
	collect_unbound(snapshot, mapped_refs, &edit_cameras, false, &mut excluded);
	excluded
}

fn materialize_mode(project_path: &Path, allow_transitions: bool) -> Result<MaterializedProject> {
	recover(project_path)?;
	let project = load_project(project_path)?;
	let manifest = data_artifact(project_path)?;
	let studio = artifact_store::load_tree(&manifest)?;
	let studio_snapshot = studio.tree.into_snapshot()?;
	let mut evaluation = Evaluation::new(project_path, &project, studio_snapshot, allow_transitions)?;
	evaluation.apply()?;
	let Evaluation {
		snapshot,
		barrier_routes,
		mapped_traversal,
		..
	} = evaluation;
	let directory = project_path
		.parent()
		.unwrap_or_else(|| Path::new("."))
		.join(format!(".carbon-composite-{}", Uuid::new_v4().simple()));
	fs::create_dir_all(&directory)?;
	let manifest_path = directory.join("state.carbon");
	if let Err(error) = artifact_store::extract_snapshot(snapshot.clone(), project.name.clone(), &manifest_path) {
		let _ = fs::remove_dir_all(&directory);
		return Err(error).context("failed to stage composed hybrid source");
	}
	let route_tree = artifact_store::load_tree(&manifest_path)?.tree;
	let (mapped_refs, mapped_roots) = refs_for_routes(&route_tree, &barrier_routes)?;
	let routing_refs = routing_refs(&route_tree, &mapped_roots)?;
	validate_no_cross_domain_references(&snapshot, &mapped_refs)?;
	let identity_rebindings = managed_build_identity_rebindings(&snapshot, &mapped_refs);
	let identity_exclusions = managed_build_identity_exclusions(&snapshot, &mapped_refs);
	let mapped_watch_roots = mapped_traversal.watch_roots.into_iter().collect();
	Ok(MaterializedProject {
		name: project.name,
		directory,
		manifest_path,
		mapped_refs,
		identity_exclusions,
		identity_rebindings,
		mapped_roots,
		mapped_watch_roots,
		routing_refs,
		snapshot,
	})
}

pub fn compile(project_path: &Path, output: &Path, worktree: Option<&WorktreeContract>) -> Result<CompileReport> {
	recover(project_path)?;
	let project = load_project(project_path)?;
	fn contributes(node: &Node, root: bool) -> bool {
		node.path.is_some()
			|| !node.properties.is_empty()
			|| !node.attributes.is_empty()
			|| node.id.is_some()
			|| (!root && node.class_name.is_some())
			|| node.children.values().any(|child| contributes(child, false))
	}
	let hybrid = contributes(&project.tree, true);
	let source_layer = if hybrid && worktree.is_none() {
		Some(mapped_source_state(project_path, &project)?)
	} else {
		None
	};
	let initial_mapped_generation = if hybrid {
		Some(match source_layer.as_ref() {
			Some(source) => source.generation.clone(),
			None => mapped_source_generation(project_path, &project)?,
		})
	} else {
		None
	};
	let cache_key = build_cache_key(project_path, &project, hybrid, worktree)?;
	if let Some((staged, report)) = stage_cached_build(project_path, &cache_key, output) {
		let transport_ready = worktree
			.map(|contract| artifact_store::rewrite_worktree_contract(&staged, contract).is_ok())
			.unwrap_or(true);
		let unchanged = transport_ready
			&& build_cache_key(project_path, &project, hybrid, worktree).is_ok_and(|candidate| candidate == cache_key);
		if unchanged && fs::rename(&staged, output).is_ok() {
			return Ok(report);
		}
		let _ = fs::remove_file(staged);
	}
	if let Some(source_layer) = source_layer.as_ref() {
		let layer_key = build_layer_key(project_path, &project, source_layer)?;
		if let Some((staged, report)) = stage_cached_source_layer(project_path, &layer_key, source_layer, output) {
			let unchanged =
				build_cache_key(project_path, &project, hybrid, worktree).is_ok_and(|candidate| candidate == cache_key);
			if unchanged && fs::rename(&staged, output).is_ok() {
				store_cached_build(project_path, &cache_key, output, &report);
				return Ok(report);
			}
			let _ = fs::remove_file(staged);
		}
	}
	if !hybrid {
		let artifact = data_artifact(project_path)?;
		let output_name = output
			.file_name()
			.and_then(|name| name.to_str())
			.context("build output has no UTF-8 file name")?;
		let staged_output = output.with_file_name(format!(".{output_name}.carbon-build-{}", Uuid::new_v4().simple()));
		let result = match worktree {
			Some(contract) => artifact_store::compile_worktree(&artifact, &staged_output, contract),
			None => artifact_store::compile(&artifact, &staged_output),
		};
		let result = result.and_then(|report| {
			fs::rename(&staged_output, output).with_context(|| {
				format!(
					"failed to promote staged build {} to {}",
					staged_output.display(),
					output.display()
				)
			})?;
			Ok(report)
		});
		let _ = fs::remove_file(&staged_output);
		if let Ok(report) = &result {
			if build_cache_key(project_path, &project, hybrid, worktree).is_ok_and(|key| key == cache_key) {
				store_cached_build(project_path, &cache_key, output, report);
			}
		}
		return result;
	}
	let mut materialized = materialize_for_build(project_path, worktree.is_some())?;
	let output_name = output
		.file_name()
		.and_then(|name| name.to_str())
		.context("build output has no UTF-8 file name")?;
	let staged_output = output.with_file_name(format!(".{output_name}.carbon-build-{}", Uuid::new_v4().simple()));
	let contract = worktree.map(|contract| {
		let mut contract = contract.clone();
		contract.identity_exclusions = materialized.identity_exclusions.clone();
		contract
	});
	let indexed_refs = source_layer
		.as_ref()
		.map(|source| source.scripts.iter().map(|script| script.id).collect())
		.unwrap_or_default();
	let composite_manifest = materialized
		.composite
		.as_ref()
		.map(|composite| composite.manifest_path.clone())
		.unwrap_or(data_artifact(project_path)?);
	let policy = LivePolicy {
		project_path: project_path.to_owned(),
		project_document: fs::read(project_path).context("validated project document disappeared")?,
		composite_manifest,
		mapped_refs: materialized.mapped_refs.clone(),
		retired_mapped_refs: HashSet::new(),
		mapped_roots: materialized.mapped_roots.clone(),
		mapped_watch_roots: materialized.mapped_watch_roots.clone(),
		routing_refs: materialized.routing_refs.clone(),
	};
	let studio_stage = stage_studio_domain_from_tree(&policy, &materialized.tree, project.name.clone())
		.context("failed to stage mapping-barrier pruning")?;
	let generation = materialized
		.composite
		.as_ref()
		.map(|composite| composite.generation.clone());
	let result = artifact_store::compile_tree(
		&mut materialized.tree,
		&staged_output,
		contract.as_ref(),
		generation.as_deref(),
		&indexed_refs,
	);
	let result = result.and_then(|(report, positions)| {
		promote_studio_domain(&studio_stage)
			.context("failed to atomically prune mapping barriers from the manifest")?;
		fs::rename(&staged_output, output).with_context(|| {
			format!(
				"failed to promote staged build {} to {}",
				staged_output.display(),
				output.display()
			)
		})?;
		Ok((report, positions))
	});
	let _ = fs::remove_file(&staged_output);
	let cleanup = materialized
		.composite
		.as_ref()
		.map(|composite| fs::remove_dir_all(&composite.directory))
		.unwrap_or(Ok(()));
	let result = match (result, cleanup) {
		(Ok(compiled), Ok(())) => Ok(compiled),
		(Ok(_), Err(error)) => Err(error).context("failed to clean composed source staging"),
		(Err(error), _) => Err(error),
	};
	if let Ok((report, positions)) = &result {
		let mapped_source_unchanged = initial_mapped_generation.as_ref().is_none_or(|initial| {
			mapped_source_generation(project_path, &project).is_ok_and(|current| current == *initial)
		});
		if mapped_source_unchanged {
			if let Ok(post_cache_key) = build_cache_key(project_path, &project, hybrid, worktree) {
				store_cached_build(project_path, &post_cache_key, output, report);
				if let Some(source_layer) = source_layer.as_ref() {
					if let Ok(layer_key) = build_layer_key(project_path, &project, source_layer) {
						store_cached_source_layer(project_path, &layer_key, source_layer, positions, output, report);
					}
				}
			}
		}
	}
	result.map(|(report, _)| report)
}

pub fn write_sourcemap(project_path: &Path, output: &Path) -> Result<u64> {
	let project = load_project(project_path)?;
	let root = project_path.parent().unwrap_or_else(|| Path::new("."));
	let mut paths = HashMap::<Vec<String>, PathBuf>::new();
	let mut traversal = MappedTraversal::default();
	collect_project_script_paths(&project.tree, root, &mut Vec::new(), &mut paths, &mut traversal)?;
	let materialized = materialize(project_path)?;
	let temporary = materialized.directory.join("sourcemap.json");
	let count = artifact_store::write_sourcemap(&materialized.manifest_path, &temporary)?;
	let mut value: Value = serde_json::from_slice(&fs::read(&temporary)?)?;
	fn rewrite(node: &mut Value, route: &mut Vec<String>, paths: &HashMap<Vec<String>, PathBuf>, root_file: &Path) {
		if route.is_empty() {
			node["filePaths"] = Value::Array(vec![Value::String(root_file.to_string_lossy().into_owned())]);
		} else if let Some(path) = paths.get(route) {
			node["filePaths"] = Value::Array(vec![Value::String(path.to_string_lossy().into_owned())]);
		} else if let Some(object) = node.as_object_mut() {
			object.remove("filePaths");
		}
		if let Some(children) = node.get_mut("children").and_then(Value::as_array_mut) {
			for child in children {
				let Some(name) = child.get("name").and_then(Value::as_str).map(str::to_owned) else {
					continue;
				};
				route.push(name);
				rewrite(child, route, paths, root_file);
				route.pop();
			}
		}
	}
	let root_file = project_path
		.file_name()
		.map(PathBuf::from)
		.unwrap_or_else(|| project_path.to_owned());
	rewrite(&mut value, &mut Vec::new(), &paths, &root_file);
	write_json(output, &value)?;
	fs::remove_dir_all(materialized.directory)?;
	Ok(count)
}

fn collect_project_script_paths(
	node: &Node,
	root: &Path,
	route: &mut Vec<String>,
	paths: &mut HashMap<Vec<String>, PathBuf>,
	traversal: &mut MappedTraversal,
) -> Result<()> {
	if let Some(relative) = &node.path {
		let absolute = resolve_mapped_path(root, relative)?;
		let file_type = traversal.path_type(&absolute)?;
		if file_type.is_file() {
			if is_script_class(script_class(absolute.get_name())?) {
				paths.insert(route.clone(), relative.clone());
			}
		} else if file_type.is_dir() {
			collect_directory_script_paths(&absolute, relative, route, paths, traversal, &mut Vec::new())?;
		}
	}
	for (name, child) in &node.children {
		route.push(name.clone());
		collect_project_script_paths(child, root, route, paths, traversal)?;
		route.pop();
	}
	Ok(())
}

fn collect_directory_script_paths(
	absolute: &Path,
	relative: &Path,
	route: &mut Vec<String>,
	paths: &mut HashMap<Vec<String>, PathBuf>,
	traversal: &mut MappedTraversal,
	canonical_stack: &mut Vec<PathBuf>,
) -> Result<()> {
	with_mapped_directory(absolute, canonical_stack, |canonical_stack| {
		if let Some(project_path) = default_project_path(absolute, traversal)? {
			let tree = load_nested_project_tree(&project_path)?;
			return collect_nested_project_script_paths(
				&tree,
				absolute,
				relative,
				route,
				paths,
				traversal,
				canonical_stack,
			);
		}
		let mut entries = fs::read_dir(absolute)?.collect::<std::io::Result<Vec<_>>>()?;
		entries.sort_by_key(|entry| entry.file_name());
		for entry in entries {
			let path = entry.path();
			let file_type = traversal.file_type(&path, entry.file_type()?)?;
			let name = entry.file_name();
			let name = name.to_str().context("mapped source path is not UTF-8")?;
			let child_relative = relative.join(name);
			if file_type.is_dir() {
				route.push(name.to_owned());
				collect_directory_script_paths(&path, &child_relative, route, paths, traversal, canonical_stack)?;
				route.pop();
			} else if file_type.is_file() && is_init_script_file(name) {
				paths.insert(route.clone(), child_relative);
			} else if file_type.is_file() && is_script_source_file(name) {
				route.push(script_name(name)?);
				paths.insert(route.clone(), child_relative);
				route.pop();
			}
		}
		Ok(())
	})
}

#[allow(clippy::too_many_arguments)]
fn collect_nested_project_script_paths(
	node: &Node,
	project_root: &Path,
	logical_root: &Path,
	route: &mut Vec<String>,
	paths: &mut HashMap<Vec<String>, PathBuf>,
	traversal: &mut MappedTraversal,
	canonical_stack: &mut Vec<PathBuf>,
) -> Result<()> {
	if let Some(relative) = &node.path {
		let absolute = resolve_mapped_path(project_root, relative)?;
		let logical = logical_root.join(relative);
		let Some(file_type) = traversal.optional_path_type(&absolute, node.path_optional)? else {
			return Ok(());
		};
		if file_type.is_file() {
			if is_script_class(script_class(absolute.get_name())?) {
				paths.insert(route.clone(), logical);
			}
		} else if file_type.is_dir() {
			collect_directory_script_paths(&absolute, &logical, route, paths, traversal, canonical_stack)?;
		}
	}
	for (name, child) in &node.children {
		route.push(name.clone());
		collect_nested_project_script_paths(
			child,
			project_root,
			logical_root,
			route,
			paths,
			traversal,
			canonical_stack,
		)?;
		route.pop();
	}
	Ok(())
}

pub fn live_policy(project_path: &Path, materialized: &MaterializedProject) -> LivePolicy {
	LivePolicy {
		project_path: project_path.to_owned(),
		project_document: fs::read(project_path).expect("validated project document disappeared"),
		composite_manifest: materialized.manifest_path.clone(),
		mapped_refs: materialized.mapped_refs.clone(),
		retired_mapped_refs: HashSet::new(),
		mapped_roots: materialized.mapped_roots.clone(),
		mapped_watch_roots: materialized.mapped_watch_roots.clone(),
		routing_refs: materialized.routing_refs.clone(),
	}
}

#[derive(Clone, Debug)]
enum IdentityLocation {
	Persisted,
	StableLinked,
	Directory(PathBuf),
	ScriptFile {
		path: PathBuf,
		project_route: Option<Vec<String>>,
		project_relative: Option<PathBuf>,
	},
	Inline(Vec<String>),
}

fn find_identity_locations(
	node: &Node,
	project_root: &Path,
	route: &mut Vec<String>,
	target: Ref,
	matches: &mut Vec<IdentityLocation>,
) -> Result<()> {
	if node.id.as_deref().is_some_and(|id| identity_ref(id) == target) {
		matches.push(IdentityLocation::Persisted);
	} else if node.path.is_none() && node.owns_subtree() && stable_ref(&format!("inline:{}", route.join("."))) == target
	{
		matches.push(IdentityLocation::Inline(route.clone()));
	}
	if let Some(relative) = &node.path {
		let path = resolve_mapped_path(project_root, relative)?;
		find_path_identity_locations(&path, target, Some(route.clone()), Some(relative.clone()), matches)?;
	}
	for (name, child) in &node.children {
		route.push(name.clone());
		find_identity_locations(child, project_root, route, target, matches)?;
		route.pop();
	}
	Ok(())
}

fn find_path_identity_locations(
	path: &Path,
	target: Ref,
	project_route: Option<Vec<String>>,
	project_relative: Option<PathBuf>,
	matches: &mut Vec<IdentityLocation>,
) -> Result<()> {
	find_path_identity_locations_inner(
		path,
		target,
		project_route,
		project_relative,
		matches,
		false,
		&mut MappedTraversal::default(),
		&mut Vec::new(),
	)
}

#[allow(clippy::too_many_arguments)]
fn find_path_identity_locations_inner(
	path: &Path,
	target: Ref,
	project_route: Option<Vec<String>>,
	project_relative: Option<PathBuf>,
	matches: &mut Vec<IdentityLocation>,
	linked_ancestor: bool,
	traversal: &mut MappedTraversal,
	canonical_stack: &mut Vec<PathBuf>,
) -> Result<()> {
	let lexical = fs::symlink_metadata(path)
		.with_context(|| format!("failed to inspect mapped identity path {}", path.display()))?;
	let linked = linked_ancestor || lexical.file_type().is_symlink();
	let file_type = traversal.file_type(path, lexical.file_type())?;
	if file_type.is_file() {
		if stable_ref(&format!("source:{}", path.display())) == target {
			if linked {
				matches.push(IdentityLocation::StableLinked);
			} else {
				matches.push(IdentityLocation::ScriptFile {
					path: path.to_owned(),
					project_route,
					project_relative,
				});
			}
		}
		return Ok(());
	}
	if !file_type.is_dir() {
		return Ok(());
	}
	with_mapped_directory(path, canonical_stack, |canonical_stack| {
		if let Some(project_path) = default_project_path(path, traversal)? {
			let tree = load_nested_project_tree(&project_path)?;
			let identity_route = vec![format!("nested-project:{}", project_path.display())];
			return find_nested_project_identity_locations(
				&tree,
				path,
				&identity_route,
				target,
				matches,
				linked,
				traversal,
				canonical_stack,
			);
		}
		let persisted = path
			.join("meta.json")
			.is_file()
			.then(|| read_meta(&path.join("meta.json")))
			.transpose()?
			.and_then(|meta| meta.id.map(|id| identity_ref(&id)));
		if persisted == Some(target) {
			matches.push(IdentityLocation::Persisted);
		} else if persisted.is_none() && stable_ref(&format!("source:{}", path.display())) == target {
			if linked {
				matches.push(IdentityLocation::StableLinked);
			} else {
				matches.push(IdentityLocation::Directory(path.to_owned()));
			}
		}
		let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
		entries.sort_by_key(|entry| entry.file_name());
		for entry in entries {
			let child = entry.path();
			let name = entry.file_name();
			let name = name.to_str().context("mapped identity path is not UTF-8")?;
			if name == "meta.json" || is_init_script_file(name) {
				continue;
			}
			let child_type = traversal.file_type(&child, entry.file_type()?)?;
			if child_type.is_dir() || child_type.is_file() && is_script_source_file(name) {
				find_path_identity_locations_inner(
					&child,
					target,
					None,
					None,
					matches,
					linked,
					traversal,
					canonical_stack,
				)?;
			}
		}
		Ok(())
	})
}

#[allow(clippy::too_many_arguments)]
fn find_nested_project_identity_locations(
	node: &Node,
	project_root: &Path,
	identity_route: &[String],
	target: Ref,
	matches: &mut Vec<IdentityLocation>,
	linked_ancestor: bool,
	traversal: &mut MappedTraversal,
	canonical_stack: &mut Vec<PathBuf>,
) -> Result<()> {
	if node.id.as_deref().is_some_and(|id| identity_ref(id) == target) {
		matches.push(IdentityLocation::Persisted);
	}
	if let Some(relative) = &node.path {
		let path = resolve_mapped_path(project_root, relative)?;
		let Some(_) = traversal.optional_path_type(&path, node.path_optional)? else {
			return Ok(());
		};
		find_path_identity_locations_inner(
			&path,
			target,
			None,
			None,
			matches,
			linked_ancestor,
			traversal,
			canonical_stack,
		)?;
	} else if node.id.is_none() && stable_ref(&format!("inline:{}", identity_route.join("."))) == target {
		matches.push(IdentityLocation::StableLinked);
	}
	for (name, child) in &node.children {
		let mut child_route = identity_route.to_vec();
		child_route.push(name.clone());
		find_nested_project_identity_locations(
			child,
			project_root,
			&child_route,
			target,
			matches,
			linked_ancestor,
			traversal,
			canonical_stack,
		)?;
	}
	Ok(())
}

fn project_json_node_mut<'a>(value: &'a mut Value, route: &[String]) -> Result<&'a mut Map<String, Value>> {
	let mut node = value
		.get_mut("tree")
		.and_then(Value::as_object_mut)
		.context("project is missing tree")?;
	for component in route {
		node = node
			.get_mut(component)
			.and_then(Value::as_object_mut)
			.with_context(|| format!("project route {} is missing", route.join(".")))?;
	}
	Ok(node)
}

#[derive(Debug)]
pub(crate) struct StagedMappedIdentities {
	root: Option<PathBuf>,
	domains: Vec<(PathBuf, PathBuf, PathBuf)>,
	retirements: Vec<(PathBuf, PathBuf)>,
	cleanup_on_drop: AtomicBool,
}

impl StagedMappedIdentities {
	fn empty() -> Self {
		Self {
			root: None,
			domains: Vec::new(),
			retirements: Vec::new(),
			cleanup_on_drop: AtomicBool::new(true),
		}
	}

	fn preserve_for_recovery(&self) {
		self.cleanup_on_drop.store(false, Ordering::Release);
	}
}

impl Drop for StagedMappedIdentities {
	fn drop(&mut self) {
		if self.cleanup_on_drop.load(Ordering::Acquire) {
			if let Some(root) = &self.root {
				let _ = remove_any(root);
			}
		}
	}
}

fn capture_backup(target: &Path) -> Result<PathBuf> {
	let parent = target.parent().context("capture promotion target has no parent")?;
	Ok(parent.join(format!(
		".{}.capture-backup-{}",
		target.get_name(),
		Uuid::new_v4().simple()
	)))
}

/// Stage every identity required by a manifest-owned capture reference. No
/// active project source is changed until this stage joins the capture commit.
pub(crate) fn stage_mapped_identities(
	policy: &LivePolicy,
	targets: &HashSet<Ref>,
	cancelled: &dyn Fn() -> bool,
) -> Result<StagedMappedIdentities> {
	if targets.is_empty() {
		return Ok(StagedMappedIdentities::empty());
	}
	ensure!(
		targets.is_subset(&policy.mapped_refs),
		"capture requested identity persistence for a non-mapped target"
	);
	recover(&policy.project_path)?;
	let project = load_project(&policy.project_path)?;
	let project_root = policy.project_path.parent().unwrap_or_else(|| Path::new("."));
	let mut ordered = targets.iter().copied().collect::<Vec<_>>();
	ordered.sort_by_key(ToString::to_string);
	let mut planned = Vec::with_capacity(ordered.len());
	for target in ordered {
		ensure!(
			!cancelled(),
			"Capture Manifest was cancelled during mapped-identity staging"
		);
		let mut locations = Vec::new();
		find_identity_locations(&project.tree, project_root, &mut Vec::new(), target, &mut locations)?;
		ensure!(
			locations.len() == 1,
			"mapped reference target {target} has {} source identity locations; expected exactly one",
			locations.len()
		);
		planned.push((target, locations.pop().unwrap()));
	}
	if planned
		.iter()
		.all(|(_, location)| matches!(location, IdentityLocation::Persisted | IdentityLocation::StableLinked))
	{
		return Ok(StagedMappedIdentities::empty());
	}

	let stage = transaction_stage(&policy.project_path)?;
	fs::create_dir_all(&stage)?;
	let result = (|| -> Result<StagedMappedIdentities> {
		let mut domains = Vec::new();
		let mut retirements = Vec::new();
		let mut project_json: Value = serde_json::from_slice(&policy.project_document)?;
		let mut project_changed = false;
		for (index, (target, location)) in planned.into_iter().enumerate() {
			ensure!(
				!cancelled(),
				"Capture Manifest was cancelled during mapped-identity staging"
			);
			match location {
				IdentityLocation::Persisted | IdentityLocation::StableLinked => {}
				IdentityLocation::Directory(directory) => {
					let meta = directory.join("meta.json");
					let mut value = if meta.is_file() {
						serde_json::from_slice::<Value>(&fs::read(&meta)?)?
					} else {
						Value::Object(Map::new())
					};
					let object = value.as_object_mut().context("meta.json must be an object")?;
					ensure!(
						!object.contains_key("id"),
						"mapped directory identity changed while staging {}",
						directory.display()
					);
					object.insert("id".to_owned(), Value::String(target.to_string()));
					let staged = stage.join(format!("identity-{index}.json"));
					write_json(&staged, &value)?;
					domains.push((staged, meta.clone(), capture_backup(&meta)?));
				}
				IdentityLocation::Inline(route) => {
					let node = project_json_node_mut(&mut project_json, &route)?;
					ensure!(
						!node.contains_key("$id"),
						"inline identity changed while staging {}",
						route.join(".")
					);
					node.insert("$id".to_owned(), Value::String(target.to_string()));
					project_changed = true;
				}
				IdentityLocation::ScriptFile {
					path,
					project_route,
					project_relative,
				} => {
					let file_name = path
						.file_name()
						.and_then(|name| name.to_str())
						.context("mapped script path is not UTF-8")?;
					let class = script_class(file_name)?;
					let instance_name = script_name(file_name)?;
					let target_directory = path
						.parent()
						.context("mapped script path has no parent")?
						.join(&instance_name);
					ensure!(
						!target_directory.exists(),
						"cannot persist mapped script identity because {} already exists",
						target_directory.display()
					);
					let staged = stage.join(format!("script-{index}"));
					fs::create_dir_all(&staged)?;
					write_text(
						&staged.join(format!("init{}", script_suffix(class))),
						&read_script_source(&path)?,
					)?;
					write_json(
						&staged.join("meta.json"),
						&serde_json::json!({"id": target.to_string()}),
					)?;
					domains.push((staged, target_directory.clone(), capture_backup(&target_directory)?));
					retirements.push((path.clone(), capture_backup(&path)?));
					match (project_route, project_relative) {
						(Some(route), Some(relative)) => {
							let node = project_json_node_mut(&mut project_json, &route)?;
							let relative = relative.parent().unwrap_or_else(|| Path::new("")).join(&instance_name);
							node.insert(
								"$path".to_owned(),
								Value::String(relative.to_string_lossy().replace('\\', "/")),
							);
							project_changed = true;
						}
						(None, None) => {}
						_ => bail!("mapped script identity provenance is incomplete"),
					}
				}
			}
		}
		if project_changed {
			let staged = stage.join("project.json");
			write_json(&staged, &project_json)?;
			domains.push((
				staged,
				policy.project_path.clone(),
				capture_backup(&policy.project_path)?,
			));
		}
		Ok(StagedMappedIdentities {
			root: Some(stage.clone()),
			domains,
			retirements,
			cleanup_on_drop: AtomicBool::new(true),
		})
	})();
	if result.is_err() {
		let _ = remove_any(&stage);
	}
	result
}

fn remove_any(path: &Path) -> Result<()> {
	if path.is_dir() {
		fs::remove_dir_all(path)?;
	} else if path.exists() {
		fs::remove_file(path)?;
	}
	Ok(())
}

#[derive(Debug)]
pub(crate) struct StagedStudioDomain {
	root: PathBuf,
	project_path: PathBuf,
	target: PathBuf,
	generated: Option<PathBuf>,
	cleanup_on_drop: AtomicBool,
}

impl StagedStudioDomain {
	fn staged(&self) -> Option<&Path> {
		self.generated.as_deref()
	}

	fn target(&self) -> &Path {
		&self.target
	}

	fn preserve_for_recovery(&self) {
		self.cleanup_on_drop.store(false, Ordering::Release);
	}
}

impl Drop for StagedStudioDomain {
	fn drop(&mut self) {
		if self.cleanup_on_drop.load(Ordering::Acquire) && self.root.exists() {
			let _ = fs::remove_dir_all(&self.root);
		}
	}
}

pub(crate) fn stage_studio_domain(
	policy: &LivePolicy,
	composite_manifest: &Path,
	cancelled: &dyn Fn() -> bool,
) -> Result<StagedStudioDomain> {
	stage_studio_domain_impl(policy, composite_manifest, None, cancelled)
}

pub(crate) fn stage_captured_studio_domain(
	policy: &LivePolicy,
	composite: &artifact_store::StagedCompiledCapture,
	cancelled: &dyn Fn() -> bool,
) -> Result<StagedStudioDomain> {
	let validated = composite.studio_domain_artifact(&policy.mapped_refs)?;
	stage_studio_domain_impl(policy, composite.artifact(), validated, cancelled)
}

fn stage_studio_domain_impl<'a>(
	policy: &LivePolicy,
	composite_manifest: &'a Path,
	validated: Option<(&'a Path, &'a artifact_store::ValidatedArtifactReceipt)>,
	cancelled: &dyn Fn() -> bool,
) -> Result<StagedStudioDomain> {
	recover(&policy.project_path)?;
	let stage = transaction_stage(&policy.project_path)?;
	fs::create_dir_all(&stage)?;
	let staged = (|| -> Result<(PathBuf, Option<PathBuf>)> {
		let target = data_dir(&policy.project_path)?;
		let generated = stage.join(target.file_name().context("data directory has no name")?);
		fs::create_dir_all(&generated)?;
		let staged_manifest = generated.join(DATA_ARTIFACT);
		let active_manifest = target.join(DATA_ARTIFACT);
		ensure!(
			!cancelled(),
			"Capture Manifest was cancelled during Studio-domain staging"
		);
		let changed = match validated {
			Some((artifact, receipt)) => artifact_store::stage_validated_unfiltered_artifact(
				artifact,
				receipt,
				&active_manifest,
				&staged_manifest,
				cancelled,
			)?,
			None if policy.mapped_refs.is_empty() => artifact_store::stage_unfiltered_artifact(
				composite_manifest,
				&active_manifest,
				&staged_manifest,
				cancelled,
			)?,
			None => {
				artifact_store::write_filtered_artifact(
					composite_manifest,
					&policy.mapped_refs,
					&staged_manifest,
					cancelled,
				)?;
				true
			}
		};
		ensure!(
			!cancelled(),
			"Capture Manifest was cancelled during Studio-domain staging"
		);
		let generated = if !changed
			|| (validated.is_none()
				&& !policy.mapped_refs.is_empty()
				&& active_manifest.exists()
				&& fs::read(&active_manifest)? == fs::read(&staged_manifest)?)
		{
			None
		} else {
			Some(generated)
		};
		Ok((target, generated))
	})();
	match staged {
		Ok((target, generated)) => {
			if generated.is_none() {
				fs::remove_dir_all(&stage)?;
			}
			Ok(StagedStudioDomain {
				root: stage,
				project_path: policy.project_path.clone(),
				target,
				generated,
				cleanup_on_drop: AtomicBool::new(true),
			})
		}
		Err(error) => {
			let _ = fs::remove_dir_all(&stage);
			Err(error)
		}
	}
}

fn promote_studio_domain(stage: &StagedStudioDomain) -> Result<()> {
	let Some(generated) = stage.staged() else {
		return Ok(());
	};
	let project_root = stage.project_path.parent().unwrap_or_else(|| Path::new("."));
	let target = stage.target();
	let backup = project_root.join(format!(".{}.backup-{}", target.get_name(), Uuid::new_v4().simple()));
	let journal = transaction_journal(&stage.project_path)?;
	write_json(
		&journal,
		&serde_json::json!({
			"version": TRANSACTION_VERSION,
			"mode": "replaceData",
			"target": target,
			"backup": backup,
			"staged": generated,
		}),
	)?;
	if target.exists() {
		fs::rename(target, &backup)?;
	}
	fs::rename(generated, target)?;
	if backup.exists() {
		fs::remove_dir_all(&backup)?;
	}
	fs::remove_file(journal)?;
	Ok(())
}

pub fn persist_studio_domain(policy: &LivePolicy) -> Result<()> {
	let stage = stage_studio_domain(policy, &policy.composite_manifest, &|| false)?;
	promote_studio_domain(&stage)
}

fn stage_studio_domain_from_tree(policy: &LivePolicy, tree: &Tree, name: String) -> Result<StagedStudioDomain> {
	recover(&policy.project_path)?;
	let stage = transaction_stage(&policy.project_path)?;
	fs::create_dir_all(&stage)?;
	let staged = (|| -> Result<StagedStudioDomain> {
		let target = data_dir(&policy.project_path)?;
		let generated = stage.join(target.file_name().context("data directory has no name")?);
		fs::create_dir_all(&generated)?;
		let staged_manifest = generated.join(DATA_ARTIFACT);
		let active_manifest = target.join(DATA_ARTIFACT);
		let metadata = artifact_store::validated_artifact_receipt(&active_manifest)?
			.metadata()
			.clone();
		artifact_store::write_filtered_tree(tree, &policy.mapped_refs, name, metadata, &staged_manifest, &|| false)?;
		let staged_generation = artifact_store::validated_artifact_receipt(&staged_manifest)?
			.generation()
			.to_owned();
		let unchanged = active_manifest.exists()
			&& artifact_store::validated_artifact_receipt(&active_manifest)
				.is_ok_and(|active| active.generation() == staged_generation);
		let generated = if unchanged {
			fs::remove_dir_all(&stage)?;
			None
		} else {
			Some(generated)
		};
		Ok(StagedStudioDomain {
			root: stage.clone(),
			project_path: policy.project_path.clone(),
			target,
			generated,
			cleanup_on_drop: AtomicBool::new(true),
		})
	})();
	match staged {
		Ok(stage) => Ok(stage),
		Err(error) => {
			let _ = fs::remove_dir_all(&stage);
			Err(error)
		}
	}
}

#[derive(Debug)]
pub(crate) struct CapturePromotionCleanup {
	backups: Vec<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct PreparedCapturePromotion {
	composite: artifact_store::StagedCompiledCapture,
	studio: StagedStudioDomain,
	identities: StagedMappedIdentities,
	domains: Vec<(PathBuf, PathBuf, PathBuf)>,
	retirements: Vec<(PathBuf, PathBuf)>,
}

impl PreparedCapturePromotion {
	pub(crate) fn artifact_receipt(&self) -> &artifact_store::ValidatedArtifactReceipt {
		self.composite.receipt()
	}
}

impl Drop for CapturePromotionCleanup {
	fn drop(&mut self) {
		for backup in &self.backups {
			let _ = remove_any(backup);
		}
	}
}

/// Resolve no-op state and every staged/target/backup path before the commit
/// claim. The resulting promotion contains no artifact reads.
#[cfg(test)]
fn prepare_capture_promotion(
	composite: artifact_store::StagedCompiledCapture,
	studio: StagedStudioDomain,
) -> Result<PreparedCapturePromotion> {
	prepare_capture_promotion_with_identities(composite, studio, StagedMappedIdentities::empty())
}

pub(crate) fn prepare_capture_promotion_with_identities(
	composite: artifact_store::StagedCompiledCapture,
	studio: StagedStudioDomain,
	mut identities: StagedMappedIdentities,
) -> Result<PreparedCapturePromotion> {
	let mut domains = Vec::<(PathBuf, PathBuf, PathBuf)>::new();
	if !composite.is_noop()? {
		let active_data = composite.active_data_dir()?;
		let active_data_parent = active_data.parent().unwrap_or_else(|| Path::new("."));
		domains.push((
			composite.staged_data_dir()?,
			active_data.clone(),
			active_data_parent.join(format!(
				".{}.capture-backup-{}",
				active_data.get_name(),
				Uuid::new_v4().simple()
			)),
		));
	}
	if let Some(staged) = studio.staged() {
		let studio_parent = studio.target().parent().unwrap_or_else(|| Path::new("."));
		domains.push((
			staged.to_owned(),
			studio.target().to_owned(),
			studio_parent.join(format!(
				".{}.capture-backup-{}",
				studio.target().get_name(),
				Uuid::new_v4().simple()
			)),
		));
	}
	domains.append(&mut identities.domains);
	let retirements = std::mem::take(&mut identities.retirements);
	Ok(PreparedCapturePromotion {
		composite,
		studio,
		identities,
		domains,
		retirements,
	})
}

/// Promote the precomputed composite and Studio-owned complement with only a
/// bounded journal write and filesystem metadata operations after the claim.
pub(crate) fn promote_capture_domains(promotion: &PreparedCapturePromotion) -> Result<CapturePromotionCleanup> {
	promote_capture_domains_impl(promotion, None)
}

fn promote_capture_domains_impl(
	promotion: &PreparedCapturePromotion,
	fail_after_domain: Option<usize>,
) -> Result<CapturePromotionCleanup> {
	fn promote_path(staged: &Path, target: &Path, backup: &Path) -> Result<()> {
		if staged.exists() {
			if target.exists() && !backup.exists() {
				fs::rename(target, backup)?;
			}
			ensure!(
				!target.exists(),
				"capture promotion target was not retired: {}",
				target.display()
			);
			fs::rename(staged, target)?;
		}
		ensure!(
			target.exists(),
			"capture promotion target is missing: {}",
			target.display()
		);
		Ok(())
	}
	if promotion.domains.is_empty() && promotion.retirements.is_empty() {
		return Ok(CapturePromotionCleanup { backups: Vec::new() });
	}
	let journal = transaction_journal(&promotion.studio.project_path)?;
	write_json(
		&journal,
		&serde_json::json!({
			"version": TRANSACTION_VERSION,
			"mode": "captureData",
			"domains": promotion.domains.iter().map(|(staged, target, backup)| serde_json::json!({
				"staged": staged,
				"target": target,
				"backup": backup,
			})).collect::<Vec<_>>(),
			"retirements": promotion.retirements.iter().map(|(target, backup)| serde_json::json!({
				"target": target,
				"backup": backup,
			})).collect::<Vec<_>>(),
		}),
	)?;
	let promotion_result = (|| -> Result<()> {
		for (index, (staged, target, backup)) in promotion.domains.iter().enumerate() {
			promote_path(staged, target, backup)?;
			if fail_after_domain == Some(index) {
				bail!("injected capture promotion failure after domain {index}");
			}
		}
		for (target, backup) in &promotion.retirements {
			if target.exists() && !backup.exists() {
				fs::rename(target, backup)?;
			}
			ensure!(
				!target.exists() && backup.exists(),
				"capture retirement did not preserve {}",
				target.display()
			);
		}
		fs::remove_file(&journal)?;
		Ok(())
	})();
	if let Err(error) = promotion_result {
		if let Err(recovery) = recover(&promotion.studio.project_path) {
			promotion.composite.preserve_for_recovery();
			promotion.studio.preserve_for_recovery();
			promotion.identities.preserve_for_recovery();
			return Err(error).context(format!(
				"capture promotion failed and synchronous roll-forward also failed: {recovery:#}"
			));
		}
		return Err(error)
			.context("capture promotion failed after the journal was written; synchronous roll-forward completed");
	}
	Ok(CapturePromotionCleanup {
		backups: promotion
			.domains
			.iter()
			.map(|(_, _, backup)| backup.clone())
			.chain(promotion.retirements.iter().map(|(_, backup)| backup.clone()))
			.collect(),
	})
}

fn reference_target(value: &Variant) -> Option<Ref> {
	match value {
		Variant::Ref(target) if target.is_some() => Some(*target),
		Variant::Content(content) => match content.value() {
			ContentType::Object(target) if target.is_some() => Some(*target),
			_ => None,
		},
		_ => None,
	}
}

fn validate_no_cross_domain_references(snapshot: &Snapshot, mapped_refs: &HashSet<Ref>) -> Result<()> {
	fn index(snapshot: &Snapshot, path: String, mapped_refs: &HashSet<Ref>, out: &mut HashMap<Ref, (String, bool)>) {
		let mapped = mapped_refs.contains(&snapshot.id);
		out.insert(snapshot.id, (path.clone(), mapped));
		for child in &snapshot.children {
			index(child, format!("{path}.{}", child.name), mapped_refs, out);
		}
	}
	fn inspect(snapshot: &Snapshot, index: &HashMap<Ref, (String, bool)>, blockers: &mut Vec<String>) {
		let Some((owner_path, owner_mapped)) = index.get(&snapshot.id) else {
			return;
		};
		for (property, value) in &snapshot.properties {
			let Some(target) = reference_target(value) else {
				continue;
			};
			let Some((target_path, target_mapped)) = index.get(&target) else {
				continue;
			};
			if *owner_mapped && !*target_mapped {
				blockers.push(format!(
					"filesystem-owned reference at {owner_path}.{} targets manifest-owned {target_path}",
					property.as_str()
				));
			}
		}
		for child in &snapshot.children {
			inspect(child, index, blockers);
		}
	}

	let mut index_by_ref = HashMap::new();
	index(snapshot, "game".to_owned(), mapped_refs, &mut index_by_ref);
	let mut blockers = Vec::new();
	inspect(snapshot, &index_by_ref, &mut blockers);
	blockers.sort();
	blockers.dedup();
	ensure!(
		blockers.is_empty(),
		"filesystem-owned instances may not reference manifest-owned instances:\n{}",
		blockers.join("\n")
	);
	Ok(())
}

#[cfg(test)]
pub(crate) fn validate_capture_cross_domain_references(
	canonical: &Snapshot,
	changes: &Changes,
	mapped_refs: &HashSet<Ref>,
) -> Result<()> {
	fn index_snapshot(
		snapshot: &Snapshot,
		path: String,
		mapped_refs: &HashSet<Ref>,
		out: &mut HashMap<Ref, (String, bool)>,
	) {
		out.insert(snapshot.id, (path.clone(), mapped_refs.contains(&snapshot.id)));
		for child in &snapshot.children {
			index_snapshot(child, format!("{path}.{}", child.name), mapped_refs, out);
		}
	}
	fn index_addition(
		addition: &AddedSnapshot,
		parent_path: &str,
		mapped_refs: &HashSet<Ref>,
		out: &mut HashMap<Ref, (String, bool)>,
	) {
		let path = format!("{parent_path}.{}", addition.name);
		out.insert(addition.id, (path.clone(), mapped_refs.contains(&addition.id)));
		fn index_child(
			child: &Snapshot,
			parent_path: &str,
			mapped_refs: &HashSet<Ref>,
			out: &mut HashMap<Ref, (String, bool)>,
		) {
			let path = format!("{parent_path}.{}", child.name);
			out.insert(child.id, (path.clone(), mapped_refs.contains(&child.id)));
			for descendant in &child.children {
				index_child(descendant, &path, mapped_refs, out);
			}
		}
		for child in &addition.children {
			index_child(child, &path, mapped_refs, out);
		}
	}
	fn inspect(
		owner: Ref,
		properties: &UstrMap<Variant>,
		index: &HashMap<Ref, (String, bool)>,
		blockers: &mut Vec<String>,
	) {
		let Some((owner_path, owner_mapped)) = index.get(&owner) else {
			return;
		};
		for (property, value) in properties {
			let Some(target) = reference_target(value) else {
				continue;
			};
			let Some((target_path, target_mapped)) = index.get(&target) else {
				continue;
			};
			if *owner_mapped && !*target_mapped {
				blockers.push(format!(
					"filesystem-owned reference at {owner_path}.{} targets manifest-owned {target_path}",
					property.as_str()
				));
			}
		}
	}
	fn inspect_addition(addition: &AddedSnapshot, index: &HashMap<Ref, (String, bool)>, blockers: &mut Vec<String>) {
		inspect(addition.id, &addition.properties, index, blockers);
		fn inspect_child(child: &Snapshot, index: &HashMap<Ref, (String, bool)>, blockers: &mut Vec<String>) {
			inspect(child.id, &child.properties, index, blockers);
			for descendant in &child.children {
				inspect_child(descendant, index, blockers);
			}
		}
		for child in &addition.children {
			inspect_child(child, index, blockers);
		}
	}

	let mut index = HashMap::new();
	index_snapshot(canonical, "game".to_owned(), mapped_refs, &mut index);
	for addition in &changes.additions {
		let parent_path = index
			.get(&addition.parent)
			.map(|(path, _)| path.clone())
			.unwrap_or_else(|| format!("<unknown:{}>", addition.parent));
		index_addition(addition, &parent_path, mapped_refs, &mut index);
	}
	let mut blockers = Vec::new();
	for addition in &changes.additions {
		inspect_addition(addition, &index, &mut blockers);
	}
	for update in &changes.updates {
		if let Some(properties) = &update.properties {
			inspect(update.id, properties, &index, &mut blockers);
		}
	}
	blockers.sort();
	blockers.dedup();
	ensure!(
		blockers.is_empty(),
		"filesystem-owned instances may not reference manifest-owned instances:\n{}",
		blockers.join("\n")
	);
	Ok(())
}

pub(crate) fn retired_source_refs(
	previous_mapped: &HashSet<Ref>,
	next_mapped: &HashSet<Ref>,
	removals: &[Ref],
) -> HashSet<Ref> {
	previous_mapped
		.difference(next_mapped)
		.copied()
		.chain(removals.iter().copied().filter(|id| !next_mapped.contains(id)))
		.collect()
}

pub fn reevaluate(project_path: &Path) -> Result<(Snapshot, Vec<Vec<String>>)> {
	let project = load_project(project_path)?;
	reevaluate_project(project_path, &project)
}

pub(crate) fn frozen_project_document(project_path: &Path) -> Result<Vec<u8>> {
	fs::read(project_path).with_context(|| format!("failed to read Carbon project {}", project_path.display()))
}

pub(crate) fn capture_service_anchor(canonical: &Snapshot, class: &str, name: &str) -> Result<Ref> {
	let mut matches = canonical
		.children
		.iter()
		.filter(|child| child.class.as_str() == class && child.name == name);
	let first = matches.next();
	ensure!(
		matches.next().is_none(),
		"capture service shell {class} '{name}' has no unique canonical anchor"
	);
	if let Some(anchor) = first {
		return Ok(anchor.id);
	}

	// Studio can materialize persistent services after the previous manifest was
	// written. Give a genuinely new singleton service a stable, domain-separated
	// identity rooted in this canonical DataModel. Its next capture resolves by
	// the exact class/name anchor above.
	let anchor = stable_ref(&format!(
		"capture-service-v1:{}:{}:{class}:{}:{name}",
		canonical.id,
		class.len(),
		name.len()
	));
	fn contains_id(snapshot: &Snapshot, id: Ref) -> bool {
		snapshot.id == id || snapshot.children.iter().any(|child| contains_id(child, id))
	}
	ensure!(
		!contains_id(canonical, anchor),
		"new capture service identity for {class} '{name}' collides with canonical state"
	);
	Ok(anchor)
}

fn is_capture_identity_only_property(property: &str) -> bool {
	matches!(property, "Name" | "Parent" | "HistoryId" | "UniqueId")
}

fn projected_realization_changes_pending(changes: &Changes, mapped_refs: &HashSet<Ref>) -> bool {
	if !changes.additions.is_empty() || !changes.removals.is_empty() {
		return true;
	}
	changes.updates.iter().any(|update| {
		!mapped_refs.contains(&update.id)
			|| update.parent.is_some()
			|| update.name.is_some()
			|| update.raw_name.is_some()
			|| update.class.is_some()
			|| update
				.properties
				.as_ref()
				.is_some_and(|properties| !properties.is_empty())
			|| update.removed_properties.is_empty()
			|| update.removed_properties.iter().any(|property| {
				!matches!(
					property.as_str(),
					"Capabilities" | "LinkedSource" | "Sandboxed" | "SourceAssetId"
				)
			})
	})
}

/// Re-evaluate frozen filesystem mappings against the small hierarchy retained
/// by a hybrid serve session. The projected baseline contains the engine route
/// anchors and the previous mapped realization, but none of the potentially
/// enormous Studio-owned complement. Removing the old mapping barriers leaves
/// exactly the skeleton that `Evaluation` needs to rebuild filesystem source.
pub(crate) fn reevaluate_projected_frozen(
	project_path: &Path,
	frozen_project_document: &[u8],
	previous_projected: &Snapshot,
	mapped_refs: &HashSet<Ref>,
) -> Result<(Snapshot, Vec<Vec<String>>)> {
	let (snapshot, routes, _) =
		reevaluate_projected_frozen_tracked(project_path, frozen_project_document, previous_projected, mapped_refs)?;
	Ok((snapshot, routes))
}

pub(crate) fn reevaluate_projected_frozen_tracked(
	project_path: &Path,
	frozen_project_document: &[u8],
	previous_projected: &Snapshot,
	mapped_refs: &HashSet<Ref>,
) -> Result<(Snapshot, Vec<Vec<String>>, Vec<PathBuf>)> {
	fn prune_mapped(mut snapshot: Snapshot, mapped_refs: &HashSet<Ref>) -> Result<Snapshot> {
		let children = std::mem::take(&mut snapshot.children);
		let mut retained = Vec::with_capacity(children.len());
		for child in children {
			if mapped_refs.contains(&child.id) {
				continue;
			}
			retained.push(prune_mapped(child, mapped_refs)?);
		}
		snapshot.children = retained;
		Ok(snapshot)
	}

	let project = parse_project(project_path, frozen_project_document)?;
	let studio_snapshot = prune_mapped(previous_projected.clone(), mapped_refs)?;
	let mut evaluation = Evaluation::new(project_path, &project, studio_snapshot, true)?;
	evaluation.apply()?;
	Ok((
		evaluation.snapshot,
		evaluation.barrier_routes,
		evaluation.mapped_traversal.watch_roots.into_iter().collect(),
	))
}

#[derive(Debug)]
pub(crate) struct ProjectSynchronizationPending;

impl std::fmt::Display for ProjectSynchronizationPending {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str(
			"filesystem mapping realization has pending changes; wait for project synchronization before Capture Manifest",
		)
	}
}

impl std::error::Error for ProjectSynchronizationPending {}

pub(crate) fn exact_projected_realization_generation(
	project_path: &Path,
	frozen_project_document: &[u8],
	previous_projected: &Snapshot,
	mapped_refs: &HashSet<Ref>,
) -> Result<String> {
	let (candidate, routes) =
		reevaluate_projected_frozen(project_path, frozen_project_document, previous_projected, mapped_refs)?;
	let changes = diff_snapshots(previous_projected, &candidate)?;
	if projected_realization_changes_pending(&changes, mapped_refs) {
		return Err(ProjectSynchronizationPending.into());
	}
	projected_realization_generation(candidate, routes)
}

fn projected_realization_generation(mut candidate: Snapshot, routes: Vec<Vec<String>>) -> Result<String> {
	fn canonicalize_nonauthored_order(snapshot: &mut Snapshot) {
		for child in &mut snapshot.children {
			canonicalize_nonauthored_order(child);
		}
		snapshot.children.sort_unstable_by_key(|child| child.id.to_string());
	}
	canonicalize_nonauthored_order(&mut candidate);
	let encoded = rmp_serde::to_vec_named(&(candidate, routes))?;
	Ok(blake3::hash(&encoded).to_hex().to_string())
}

fn reevaluate_project(project_path: &Path, project: &Project) -> Result<(Snapshot, Vec<Vec<String>>)> {
	let studio = artifact_store::load_tree(&data_artifact(project_path)?)?;
	let studio_snapshot = tree_snapshot(&studio.tree, studio.tree.root_ref())?;
	let mut evaluation = Evaluation::new(project_path, project, studio_snapshot, true)?;
	evaluation.apply()?;
	Ok((evaluation.snapshot, evaluation.barrier_routes))
}

pub fn diff_snapshots(old: &Snapshot, new: &Snapshot) -> Result<Changes> {
	let old_nodes = flatten_snapshot(old);
	let new_nodes = flatten_snapshot(new);
	let mut changes = Changes::new();
	let mut removed = Vec::new();
	for id in old_nodes.keys() {
		if !new_nodes.contains_key(id) && !has_removed_ancestor(*id, &old_nodes, &new_nodes) {
			removed.push(*id);
		}
	}
	removed.sort_by_key(ToString::to_string);
	changes.removals = removed;

	for (id, new_node) in &new_nodes {
		if old_nodes.contains_key(id) {
			continue;
		}
		if new_node.parent.is_some_and(|parent| !old_nodes.contains_key(&parent)) {
			continue;
		}
		changes.additions.push(AddedSnapshot {
			id: *id,
			parent: new_node.parent.unwrap_or_else(Ref::none),
			name: new_node.snapshot.name.clone(),
			raw_name: new_node.snapshot.raw_name.clone(),
			class: new_node.snapshot.class,
			properties: new_node.snapshot.properties.clone(),
			children: new_node.snapshot.children.clone(),
		});
	}
	for (id, new_node) in &new_nodes {
		let Some(old_node) = old_nodes.get(id) else { continue };
		let mut update = UpdatedSnapshot::new(*id);
		if old_node.parent != new_node.parent {
			update.parent = new_node.parent;
		}
		if old_node.snapshot.name != new_node.snapshot.name {
			update.name = Some(new_node.snapshot.name.clone());
			update.raw_name = new_node.snapshot.raw_name.clone();
		}
		if old_node.snapshot.class != new_node.snapshot.class {
			update.class = Some(new_node.snapshot.class);
		}
		let mut property_names = BTreeSet::new();
		property_names.extend(old_node.snapshot.properties.keys().copied());
		property_names.extend(new_node.snapshot.properties.keys().copied());
		let mut properties = UstrMap::default();
		for property in property_names {
			if is_capture_identity_only_property(property.as_str()) {
				continue;
			}
			let old_value = old_node.snapshot.properties.get(&property);
			let new_value = new_node.snapshot.properties.get(&property);
			if property_values_semantically_equal(
				new_node.snapshot.class.as_str(),
				property.as_str(),
				old_value,
				new_value,
			) {
				continue;
			}
			if let Some(value) = new_value {
				properties.insert(property, value.clone());
			} else {
				update.removed_properties.push(property);
			}
		}
		if !properties.is_empty() {
			update.properties = Some(properties);
		}
		if !update.is_empty() {
			changes.updates.push(update);
		}
	}
	Ok(changes)
}

/// Reconcile capture-local random identities against the previous manifest and
/// assign fresh Carbon identities to genuinely new nodes. A rename or
/// reparent retains identity only when structural evidence selects one prior
/// object; ambiguity is a capture blocker.
pub fn reconcile_capture_identities(canonical: &Snapshot, changes: &mut Changes) -> Result<HashMap<Ref, Ref>> {
	log::debug!(
		"reconciling Capture Manifest identities: {} additions, {} updates, {} removals",
		changes.additions.len(),
		changes.updates.len(),
		changes.removals.len()
	);
	fn normalized_digest(snapshot: &Snapshot, cache: &mut HashMap<Ref, blake3::Hash>) -> Result<blake3::Hash> {
		if let Some(digest) = cache.get(&snapshot.id) {
			return Ok(*digest);
		}
		let mut hasher = blake3::Hasher::new();
		hasher.update(snapshot.class.as_bytes());
		let mut properties = snapshot
			.properties
			.iter()
			.filter(|(name, _)| !is_capture_identity_only_property(name.as_str()))
			.collect::<Vec<_>>();
		properties.sort_by_key(|(name, _)| name.as_str());
		for (name, value) in properties {
			hasher.update(name.as_bytes());
			if matches!(value, Variant::Ref(_)) {
				hasher.update(b"instance-reference");
			} else {
				hasher.update(&rmp_serde::to_vec_named(value)?);
			}
		}
		let mut children = snapshot
			.children
			.iter()
			.map(|child| normalized_digest(child, cache))
			.collect::<Result<Vec<_>>>()?;
		children.sort_by_key(|digest| digest.as_bytes().to_vec());
		for child in children {
			hasher.update(child.as_bytes());
		}
		let digest = hasher.finalize();
		cache.insert(snapshot.id, digest);
		Ok(digest)
	}
	fn addition_snapshot(addition: &AddedSnapshot) -> Snapshot {
		Snapshot {
			id: addition.id,
			name: addition.name.clone(),
			raw_name: addition.raw_name.clone(),
			class: addition.class,
			properties: addition.properties.clone(),
			children: addition.children.clone(),
		}
	}
	fn choose<'a>(new: &Snapshot, candidates: Vec<&'a Snapshot>, location: &str) -> Result<Option<&'a Snapshot>> {
		if candidates.len() <= 1 {
			return Ok(candidates.into_iter().next());
		}
		let exact_name = candidates
			.iter()
			.copied()
			.filter(|candidate| candidate.name == new.name && candidate.raw_name == new.raw_name)
			.collect::<Vec<_>>();
		if exact_name.len() == 1 {
			return Ok(exact_name.into_iter().next());
		}
		anyhow::bail!(
			"identity-ambiguity blocker at {location}: {} prior instances could be the same object",
			candidates.len()
		)
	}
	fn pair(
		old: &Snapshot,
		new: &Snapshot,
		location: &str,
		remap: &mut HashMap<Ref, Ref>,
		old_digests: &HashMap<Ref, blake3::Hash>,
		new_digests: &HashMap<Ref, blake3::Hash>,
	) -> Result<()> {
		remap.insert(new.id, old.id);
		let mut used = HashSet::new();
		for child in &new.children {
			let digest = new_digests
				.get(&child.id)
				.context("new capture identity digest is missing")?;
			let exact = old
				.children
				.iter()
				.filter(|candidate| !used.contains(&candidate.id))
				.filter(|candidate| candidate.class == child.class)
				.filter(|candidate| old_digests.get(&candidate.id) == Some(digest))
				.collect::<Vec<_>>();
			let candidates = if exact.is_empty() {
				old.children
					.iter()
					.filter(|candidate| !used.contains(&candidate.id))
					.filter(|candidate| candidate.class == child.class)
					.collect()
			} else {
				exact
			};
			if let Some(candidate) = choose(child, candidates, &format!("{location}.{}", child.name))? {
				used.insert(candidate.id);
				pair(
					candidate,
					child,
					&format!("{location}.{}", child.name),
					remap,
					old_digests,
					new_digests,
				)?;
			}
		}
		Ok(())
	}
	fn assign(
		snapshot: &mut Snapshot,
		_parent: Ref,
		_occurrence: usize,
		remap: &mut HashMap<Ref, Ref>,
		used: &mut HashSet<Ref>,
		allocator: &mut ManifestIdentityAllocator,
	) {
		let incoming = snapshot.id;
		let mut assigned = remap
			.get(&incoming)
			.copied()
			.unwrap_or_else(|| Ref::some(allocator.next()));
		while used.contains(&assigned) && remap.get(&incoming) != Some(&assigned) {
			assigned = Ref::some(allocator.next());
		}
		remap.insert(incoming, assigned);
		used.insert(assigned);
		snapshot.id = assigned;
		let mut occurrences = HashMap::<(Ustr, String), usize>::new();
		for child in &mut snapshot.children {
			let key = (child.class, child.name.clone());
			let occurrence = occurrences.entry(key).or_default();
			assign(child, assigned, *occurrence, remap, used, allocator);
			*occurrence += 1;
		}
	}
	fn remap_properties(properties: &mut UstrMap<Variant>, remap: &HashMap<Ref, Ref>) {
		for value in properties.values_mut() {
			match value {
				Variant::Ref(target) => {
					if let Some(replacement) = remap.get(target) {
						*target = *replacement;
					}
				}
				Variant::Content(content) => {
					if let ContentType::Object(target) = content.value() {
						if let Some(replacement) = remap.get(target) {
							*content = Content::from_referent(*replacement);
						}
					}
				}
				_ => {}
			}
		}
	}
	fn remap_snapshot(snapshot: &mut Snapshot, remap: &HashMap<Ref, Ref>) {
		remap_properties(&mut snapshot.properties, remap);
		for child in &mut snapshot.children {
			remap_snapshot(child, remap);
		}
	}

	let canonical_nodes = flatten_snapshot(canonical);
	let removed = changes
		.removals
		.iter()
		.filter_map(|id| canonical_nodes.get(id).map(|node| node.snapshot))
		.collect::<Vec<_>>();
	log::debug!(
		"Capture Manifest identity candidates: {} removed roots, {} addition roots",
		removed.len(),
		changes.additions.len()
	);
	let mut old_digests = HashMap::with_capacity(canonical_nodes.len());
	normalized_digest(canonical, &mut old_digests)?;
	let mut new_digests = HashMap::new();
	for addition in &changes.additions {
		normalized_digest(&addition_snapshot(addition), &mut new_digests)?;
	}
	let mut removed_by_class = HashMap::<Ustr, Vec<&Snapshot>>::new();
	let mut removed_by_digest = HashMap::<(Ustr, blake3::Hash), Vec<&Snapshot>>::new();
	for candidate in &removed {
		removed_by_class.entry(candidate.class).or_default().push(candidate);
		let digest = *old_digests
			.get(&candidate.id)
			.context("prior capture identity digest is missing")?;
		removed_by_digest
			.entry((candidate.class, digest))
			.or_default()
			.push(candidate);
	}
	let mut remap = HashMap::new();
	let mut claimed = HashSet::new();
	for addition in &changes.additions {
		let new = addition_snapshot(addition);
		let digest = *new_digests
			.get(&new.id)
			.context("new capture identity digest is missing")?;
		let exact = removed_by_digest
			.get(&(new.class, digest))
			.into_iter()
			.flatten()
			.copied()
			.filter(|candidate| !claimed.contains(&candidate.id))
			.collect::<Vec<_>>();
		let candidates = if exact.is_empty() {
			removed_by_class
				.get(&new.class)
				.into_iter()
				.flatten()
				.copied()
				.filter(|candidate| !claimed.contains(&candidate.id))
				.collect()
		} else {
			exact
		};
		if let Some(candidate) = choose(&new, candidates, &new.name)? {
			claimed.insert(candidate.id);
			pair(candidate, &new, &new.name, &mut remap, &old_digests, &new_digests)?;
		}
	}

	let mut used = flatten_snapshot(canonical).keys().copied().collect::<HashSet<_>>();
	let mut allocator = ManifestIdentityAllocator::new();
	let mut occurrences = HashMap::<(Ref, Ustr, String), usize>::new();
	for addition in &mut changes.additions {
		let key = (addition.parent, addition.class, addition.name.clone());
		let occurrence = occurrences.entry(key).or_default();
		let incoming = addition.id;
		let mut root = addition_snapshot(addition);
		assign(
			&mut root,
			addition.parent,
			*occurrence,
			&mut remap,
			&mut used,
			&mut allocator,
		);
		*occurrence += 1;
		addition.id = root.id;
		addition.children = root.children;
		remap.insert(incoming, addition.id);
	}
	for addition in &mut changes.additions {
		if let Some(parent) = remap.get(&addition.parent) {
			addition.parent = *parent;
		}
		remap_properties(&mut addition.properties, &remap);
		for child in &mut addition.children {
			remap_snapshot(child, &remap);
		}
	}
	for update in &mut changes.updates {
		if let Some(id) = remap.get(&update.id) {
			update.id = *id;
		}
		if let Some(parent) = update.parent.and_then(|parent| remap.get(&parent).copied()) {
			update.parent = Some(parent);
		}
		if let Some(properties) = &mut update.properties {
			remap_properties(properties, &remap);
		}
	}
	Ok(remap)
}

struct FlatNode<'a> {
	parent: Option<Ref>,
	snapshot: &'a Snapshot,
}

fn flatten_snapshot(root: &Snapshot) -> HashMap<Ref, FlatNode<'_>> {
	fn visit<'a>(snapshot: &'a Snapshot, parent: Option<Ref>, out: &mut HashMap<Ref, FlatNode<'a>>) {
		out.insert(snapshot.id, FlatNode { parent, snapshot });
		for child in &snapshot.children {
			visit(child, Some(snapshot.id), out);
		}
	}
	let mut result = HashMap::new();
	visit(root, None, &mut result);
	result
}

fn has_removed_ancestor(id: Ref, old: &HashMap<Ref, FlatNode<'_>>, new: &HashMap<Ref, FlatNode<'_>>) -> bool {
	let mut parent = old.get(&id).and_then(|node| node.parent);
	while let Some(id) = parent {
		if !new.contains_key(&id) {
			return true;
		}
		parent = old.get(&id).and_then(|node| node.parent);
	}
	false
}

struct Evaluation<'a> {
	project_path: &'a Path,
	project: &'a Project,
	snapshot: Snapshot,
	barrier_routes: Vec<Vec<String>>,
	explicit_ids: HashMap<String, String>,
	mapped_traversal: MappedTraversal,
	allow_transitions: bool,
}

impl<'a> Evaluation<'a> {
	fn new(project_path: &'a Path, project: &'a Project, snapshot: Snapshot, allow_transitions: bool) -> Result<Self> {
		ensure!(
			snapshot.class.as_str() == "DataModel",
			"Studio data root must be DataModel"
		);
		Ok(Self {
			project_path,
			project,
			snapshot,
			barrier_routes: Vec::new(),
			explicit_ids: HashMap::new(),
			mapped_traversal: MappedTraversal::default(),
			allow_transitions,
		})
	}

	fn apply(&mut self) -> Result<()> {
		ensure!(
			self.project.tree.class_name.as_deref() == Some("DataModel"),
			"project tree must declare $className: DataModel"
		);
		ensure!(
			self.project.tree.path.is_none(),
			"$path is forbidden on the DataModel project root"
		);
		apply_node_metadata(
			&mut self.snapshot,
			&self.project.tree,
			"DataModel",
			&mut self.explicit_ids,
		)?;
		let root = self.project_path.parent().unwrap_or_else(|| Path::new("."));
		for (name, node) in &self.project.tree.children {
			ensure!(
				is_service(name),
				"project route DataModel.{name} is not an engine Service anchor"
			);
			let anchor = find_or_create_child(&mut self.snapshot, name, name)?;
			let mut route = vec![name.clone()];
			Self::apply_anchor(
				self.project_path,
				self.allow_transitions,
				&mut self.barrier_routes,
				&mut self.explicit_ids,
				&mut self.mapped_traversal,
				anchor,
				node,
				root,
				&mut route,
				AnchorKind::Service,
			)?;
		}
		validate_unique_snapshot_ids(&self.snapshot)?;
		validate_snapshot_references(&self.snapshot)?;
		Ok(())
	}

	#[allow(clippy::too_many_arguments)]
	fn apply_anchor(
		project_path: &Path,
		_allow_transitions: bool,
		barrier_routes: &mut Vec<Vec<String>>,
		explicit_ids: &mut HashMap<String, String>,
		mapped_traversal: &mut MappedTraversal,
		anchor: &mut Snapshot,
		node: &Node,
		root: &Path,
		route: &mut Vec<String>,
		kind: AnchorKind,
	) -> Result<()> {
		ensure!(
			node.class_name.is_none(),
			"$className is forbidden on engine anchor {}",
			route.join(".")
		);
		apply_node_metadata(anchor, node, anchor.class.as_str(), explicit_ids)?;
		if let Some(relative) = &node.path {
			let path = resolve_mapped_path(root, relative)?;
			ensure!(
				mapped_traversal.mapped_path_type(root, &path)?.is_dir(),
				"engine anchor $path must be a directory: {}",
				path.display()
			);
			let contribution = read_directory_contents_tracked(&path, &data_dir(project_path)?, mapped_traversal)?;
			// A mapping is a hard ownership barrier. Filesystem source replaces and
			// prunes any stale manifest records below it; Studio state is never used
			// to seed or validate a mapping.
			anchor.children = contribution;
			barrier_routes.push(route.clone());
		}
		for (child_name, child_node) in &node.children {
			if kind == AnchorKind::Service
				&& anchor.class.as_str() == "StarterPlayer"
				&& matches!(child_name.as_str(), "StarterPlayerScripts" | "StarterCharacterScripts")
			{
				let child = find_or_create_child(anchor, child_name, child_name)?;
				route.push(child_name.clone());
				Self::apply_anchor(
					project_path,
					_allow_transitions,
					barrier_routes,
					explicit_ids,
					mapped_traversal,
					child,
					child_node,
					root,
					route,
					AnchorKind::FixedContainer,
				)?;
				route.pop();
				continue;
			}
			ensure!(
				child_node.owns_subtree() || node.path.is_some(),
				"mapping {} may not route through an arbitrary manifest-owned instance",
				format_route(route, child_name)
			);
			route.push(child_name.clone());
			let mapped = evaluate_owned_node(
				child_name,
				child_node,
				root,
				&data_dir(project_path)?,
				route,
				explicit_ids,
				mapped_traversal,
			)?;
			if let Some(index) = anchor.children.iter().position(|child| child.name == *child_name) {
				anchor.children[index] = mapped;
				barrier_routes.push(route.clone());
				route.pop();
				continue;
			}
			anchor.children.push(mapped);
			barrier_routes.push(route.clone());
			route.pop();
		}
		anchor.children.sort_unstable_by_key(|child| child.id.to_string());
		Ok(())
	}
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum AnchorKind {
	Service,
	FixedContainer,
}

fn evaluate_owned_node(
	name: &str,
	node: &Node,
	project_root: &Path,
	generated_data: &Path,
	identity_route: &[String],
	explicit_ids: &mut HashMap<String, String>,
	mapped_traversal: &mut MappedTraversal,
) -> Result<Snapshot> {
	let mut snapshot = if let Some(relative) = &node.path {
		let path = resolve_mapped_path(project_root, relative)?;
		let file_type = mapped_traversal
			.mapped_path_type(project_root, &path)
			.with_context(|| format!("required mapped path is missing: {}", path.display()))?;
		if file_type.is_dir() {
			read_directory_instance_tracked(&path, name, generated_data, mapped_traversal)?
		} else if file_type.is_file() {
			read_script_file(&path, name)?
		} else {
			bail!("mapped path is neither a file nor directory: {}", path.display())
		}
	} else {
		Snapshot::new()
			.with_id(stable_ref(&format!("inline:{}", identity_route.join("."))))
			.with_name(name)
			.with_class(node.class_name.as_deref().unwrap_or("Folder"))
	};
	if let Some(class) = &node.class_name {
		ensure!(is_filesystem_class(class), "unsupported filesystem-owned class {class}");
		ensure!(
			node.path.is_none() || snapshot.class.as_str() == class,
			"$className {class} conflicts with inferred mapped class {} at {name}",
			snapshot.class
		);
		snapshot.class = Ustr::from(class);
	}
	let inferred_class = snapshot.class.to_string();
	apply_node_metadata(&mut snapshot, node, &inferred_class, explicit_ids)?;
	for (child_name, child_node) in &node.children {
		let mut child_route = identity_route.to_vec();
		child_route.push(child_name.clone());
		let contribution = evaluate_owned_node(
			child_name,
			child_node,
			project_root,
			generated_data,
			&child_route,
			explicit_ids,
			mapped_traversal,
		)?;
		if let Some(index) = snapshot.children.iter().position(|child| child.name == *child_name) {
			ensure!(
				snapshot.children[index].class == contribution.class,
				"mapped contributions for {child_name} infer conflicting classes"
			);
			let explicit_id = child_node.id.is_some().then_some(contribution.id);
			snapshot.children[index] = merge_snapshots(snapshot.children[index].clone(), contribution)?;
			if let Some(id) = explicit_id {
				snapshot.children[index].id = id;
			}
		} else {
			snapshot.children.push(contribution);
		}
	}
	validate_filesystem_subtree(&snapshot, name)?;
	Ok(snapshot)
}

#[allow(clippy::too_many_arguments)]
fn evaluate_nested_project_node(
	name: &str,
	node: &Node,
	project_root: &Path,
	generated_data: &Path,
	identity_route: &[String],
	explicit_ids: &mut HashMap<String, String>,
	mapped_traversal: &mut MappedTraversal,
	canonical_stack: &mut Vec<PathBuf>,
) -> Result<Option<Snapshot>> {
	let mut snapshot = if let Some(relative) = &node.path {
		let path = resolve_mapped_path(project_root, relative)?;
		let file_type = mapped_traversal
			.optional_mapped_path_type(project_root, &path, node.path_optional)
			.with_context(|| format!("required nested Rojo path is missing: {}", path.display()))?;
		let Some(file_type) = file_type else {
			return Ok(None);
		};
		if file_type.is_dir() {
			read_directory_instance_inner(&path, name, generated_data, mapped_traversal, canonical_stack)?.snapshot
		} else if file_type.is_file() {
			read_script_file(&path, name)?
		} else {
			bail!("nested Rojo path is neither a file nor directory: {}", path.display())
		}
	} else {
		Snapshot::new()
			.with_id(stable_ref(&format!("inline:{}", identity_route.join("."))))
			.with_name(name)
			.with_class(node.class_name.as_deref().unwrap_or("Folder"))
	};
	if let Some(class) = &node.class_name {
		ensure!(is_filesystem_class(class), "unsupported filesystem-owned class {class}");
		ensure!(
			node.path.is_none() || snapshot.class.as_str() == class,
			"$className {class} conflicts with inferred nested Rojo class {} at {name}",
			snapshot.class
		);
		snapshot.class = Ustr::from(class);
	}
	let inferred_class = snapshot.class.to_string();
	apply_node_metadata(&mut snapshot, node, &inferred_class, explicit_ids)?;
	for (child_name, child_node) in &node.children {
		let mut child_route = identity_route.to_vec();
		child_route.push(child_name.clone());
		let Some(contribution) = evaluate_nested_project_node(
			child_name,
			child_node,
			project_root,
			generated_data,
			&child_route,
			explicit_ids,
			mapped_traversal,
			canonical_stack,
		)?
		else {
			continue;
		};
		if let Some(index) = snapshot.children.iter().position(|child| child.name == *child_name) {
			ensure!(
				snapshot.children[index].class == contribution.class,
				"nested Rojo contributions for {child_name} infer conflicting classes"
			);
			let explicit_id = child_node.id.is_some().then_some(contribution.id);
			snapshot.children[index] = merge_snapshots(snapshot.children[index].clone(), contribution)?;
			if let Some(id) = explicit_id {
				snapshot.children[index].id = id;
			}
		} else {
			snapshot.children.push(contribution);
		}
	}
	validate_filesystem_subtree(&snapshot, name)?;
	Ok(Some(snapshot))
}

fn merge_snapshots(mut base: Snapshot, overlay: Snapshot) -> Result<Snapshot> {
	ensure!(
		base.name == overlay.name && base.class == overlay.class,
		"cannot merge different mapped instances"
	);
	base.properties.extend(overlay.properties);
	for child in overlay.children {
		if let Some(index) = base.children.iter().position(|candidate| candidate.name == child.name) {
			base.children[index] = merge_snapshots(base.children[index].clone(), child)?;
		} else {
			base.children.push(child);
		}
	}
	Ok(base)
}

fn apply_node_metadata(
	snapshot: &mut Snapshot,
	node: &Node,
	class: &str,
	explicit_ids: &mut HashMap<String, String>,
) -> Result<()> {
	if let Some(id) = &node.id {
		validate_explicit_id(id)?;
		let location = format!("project node {}", snapshot.name);
		if let Some(previous) = explicit_ids.insert(id.clone(), location.clone()) {
			bail!("duplicate filesystem identity '{id}' at {previous} and {location}");
		}
		snapshot.id = identity_ref(id);
	}
	for (property, value) in &node.properties {
		validate_property(class, property)?;
		let resolved = resolve_mapped_property(value.clone(), class, property)?;
		snapshot.properties.insert(Ustr::from(property), resolved);
	}
	if !node.attributes.is_empty() {
		let mut attributes = Attributes::new();
		for (name, value) in &node.attributes {
			validate_attribute_name(name)?;
			let value = value
				.clone()
				.resolve_unambiguous()
				.with_context(|| format!("failed to resolve attribute {name}"))?;
			validate_attribute_value(name, &value)?;
			attributes.insert(name.clone(), value);
		}
		snapshot
			.properties
			.insert(Ustr::from("Attributes"), Variant::Attributes(attributes));
	}
	Ok(())
}

fn load_project(path: &Path) -> Result<Project> {
	ensure!(is_project_path(path), "project must be a strict *.carbon.json file");
	let bytes = fs::read(path).with_context(|| format!("failed to read Carbon project {}", path.display()))?;
	parse_project(path, &bytes)
}

fn parse_project(path: &Path, bytes: &[u8]) -> Result<Project> {
	let value: Value = serde_json::from_slice(bytes)
		.with_context(|| format!("failed to parse strict JSON project {}", path.display()))?;
	let mut object = value
		.as_object()
		.cloned()
		.context("Carbon project root must be an object")?;
	for key in object.keys() {
		ensure!(
			matches!(key.as_str(), "name" | "tree"),
			"unsupported project field {key}"
		);
	}
	let name = take_string(&mut object, "name")?;
	ensure!(!name.is_empty(), "project name may not be empty");
	let tree = parse_node(object.remove("tree").context("project is missing tree")?, "tree")?;
	ensure!(object.is_empty(), "project contains unsupported fields");
	Ok(Project { name, tree })
}

fn parse_node(value: Value, location: &str) -> Result<Node> {
	let mut object = value
		.as_object()
		.cloned()
		.with_context(|| format!("project node {location} must be an object"))?;
	let mut node = Node::default();
	if let Some(value) = object.remove("$className") {
		node.class_name = Some(value.as_str().context("$className must be a string")?.to_owned());
	}
	if let Some(value) = object.remove("$path") {
		node.path = Some(PathBuf::from(value.as_str().context("$path must be a string")?));
	}
	if let Some(value) = object.remove("$id") {
		let id = value.as_str().context("$id must be a string")?.to_owned();
		validate_explicit_id(&id)?;
		node.id = Some(id);
	}
	if let Some(value) = object.remove("$properties") {
		node.properties = serde_json::from_value(value).context("$properties must be an object")?;
	}
	if let Some(value) = object.remove("$attributes") {
		node.attributes = serde_json::from_value(value).context("$attributes must be an object")?;
	}
	for (key, value) in object {
		ensure!(
			!key.starts_with('$'),
			"unsupported project node field {key} at {location}"
		);
		validate_instance_name(&key)?;
		node.children
			.insert(key.clone(), parse_node(value, &format!("{location}.{key}"))?);
	}
	Ok(node)
}

fn load_nested_project_tree(path: &Path) -> Result<Node> {
	let bytes = fs::read(path).with_context(|| format!("failed to read nested Rojo project {}", path.display()))?;
	let value: Value = serde_json::from_slice(&bytes)
		.with_context(|| format!("failed to parse nested Rojo project {}", path.display()))?;
	let object = value
		.as_object()
		.with_context(|| format!("nested Rojo project {} must be an object", path.display()))?;
	let tree = object
		.get("tree")
		.cloned()
		.with_context(|| format!("nested Rojo project {} is missing tree", path.display()))?;
	parse_nested_project_node(tree, "tree")
}

fn parse_nested_project_node(value: Value, location: &str) -> Result<Node> {
	let mut object = value
		.as_object()
		.cloned()
		.with_context(|| format!("nested Rojo project node {location} must be an object"))?;
	let mut node = Node::default();
	if let Some(value) = object.remove("$className") {
		node.class_name = Some(value.as_str().context("$className must be a string")?.to_owned());
	}
	if let Some(value) = object.remove("$path") {
		if let Some(path) = value.as_str() {
			node.path = Some(PathBuf::from(path));
		} else {
			let mut optional = value
				.as_object()
				.cloned()
				.context("nested Rojo $path must be a string or an object containing optional")?;
			let path = optional
				.remove("optional")
				.context("nested Rojo optional $path is missing optional")?;
			node.path = Some(PathBuf::from(
				path.as_str().context("nested Rojo optional $path must be a string")?,
			));
			node.path_optional = true;
			ensure!(
				optional.is_empty(),
				"nested Rojo optional $path contains unsupported fields"
			);
		}
	}
	if let Some(value) = object.remove("$id") {
		let id = value.as_str().context("$id must be a string")?.to_owned();
		validate_explicit_id(&id)?;
		node.id = Some(id);
	}
	if let Some(value) = object.remove("$properties") {
		node.properties = serde_json::from_value(value).context("$properties must be an object")?;
	}
	if let Some(value) = object.remove("$attributes") {
		node.attributes = serde_json::from_value(value).context("$attributes must be an object")?;
	}
	if let Some(value) = object.remove("$ignoreUnknownInstances") {
		ensure!(value.is_boolean(), "$ignoreUnknownInstances must be a boolean");
	}
	for (key, value) in object {
		ensure!(
			!key.starts_with('$'),
			"unsupported nested Rojo project node field {key} at {location}"
		);
		validate_instance_name(&key)?;
		node.children.insert(
			key.clone(),
			parse_nested_project_node(value, &format!("{location}.{key}"))?,
		);
	}
	Ok(node)
}

fn take_string(object: &mut Map<String, Value>, key: &str) -> Result<String> {
	Ok(object
		.remove(key)
		.with_context(|| format!("project is missing {key}"))?
		.as_str()
		.with_context(|| format!("project {key} must be a string"))?
		.to_owned())
}

fn resolve_mapped_path(root: &Path, relative: &Path) -> Result<PathBuf> {
	ensure!(!relative.as_os_str().is_empty(), "$path may not be empty");
	ensure!(!relative.is_absolute(), "$path must be relative to the project file");
	ensure!(
		relative.components().all(|component| matches!(
			component,
			Component::CurDir | Component::ParentDir | Component::Normal(_)
		)),
		"$path contains unsafe components: {}",
		relative.display()
	);
	Ok(root.join(relative).clean())
}

#[derive(Default)]
struct MappedTraversal {
	watch_roots: BTreeSet<PathBuf>,
}

impl MappedTraversal {
	fn record_external_root(&mut self, root: &Path, path: &Path, file_type: fs::FileType) -> Result<()> {
		let root = fs::canonicalize(root.resolve()?)
			.with_context(|| format!("failed to resolve mapped project root {}", root.display()))?;
		let path =
			fs::canonicalize(path).with_context(|| format!("failed to resolve mapped path {}", path.display()))?;
		if path.starts_with(root) {
			return Ok(());
		}
		let watch_root = if file_type.is_dir() {
			path
		} else {
			path.parent().context("mapped path has no parent directory")?.to_owned()
		};
		self.watch_roots.insert(watch_root);
		Ok(())
	}

	fn file_type(&mut self, path: &Path, lexical_type: fs::FileType) -> Result<fs::FileType> {
		if !lexical_type.is_symlink() {
			return Ok(lexical_type);
		}
		let target =
			fs::metadata(path).with_context(|| format!("mapped symlink target is unavailable: {}", path.display()))?;
		let canonical =
			fs::canonicalize(path).with_context(|| format!("failed to resolve mapped symlink {}", path.display()))?;
		let watch_root = if target.is_dir() {
			canonical
		} else {
			canonical
				.parent()
				.context("mapped symlink target has no parent directory")?
				.to_owned()
		};
		self.watch_roots.insert(watch_root);
		Ok(target.file_type())
	}

	fn path_type(&mut self, path: &Path) -> Result<fs::FileType> {
		let lexical =
			fs::symlink_metadata(path).with_context(|| format!("failed to inspect mapped path {}", path.display()))?;
		self.file_type(path, lexical.file_type())
	}

	fn mapped_path_type(&mut self, root: &Path, path: &Path) -> Result<fs::FileType> {
		let file_type = self.path_type(path)?;
		self.record_external_root(root, path, file_type)?;
		Ok(file_type)
	}

	fn optional_path_type(&mut self, path: &Path, optional: bool) -> Result<Option<fs::FileType>> {
		let lexical = match fs::symlink_metadata(path) {
			Ok(lexical) => lexical,
			Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
			Err(error) => {
				return Err(error).with_context(|| format!("failed to inspect mapped path {}", path.display()))
			}
		};
		Ok(Some(self.file_type(path, lexical.file_type())?))
	}

	fn optional_mapped_path_type(&mut self, root: &Path, path: &Path, optional: bool) -> Result<Option<fs::FileType>> {
		let Some(file_type) = self.optional_path_type(path, optional)? else {
			return Ok(None);
		};
		self.record_external_root(root, path, file_type)?;
		Ok(Some(file_type))
	}
}

fn default_project_path(path: &Path, traversal: &mut MappedTraversal) -> Result<Option<PathBuf>> {
	let project = path.join("default.project.json");
	Ok(traversal
		.optional_path_type(&project, true)?
		.filter(fs::FileType::is_file)
		.map(|_| project))
}

fn with_mapped_directory<T>(
	path: &Path,
	canonical_stack: &mut Vec<PathBuf>,
	read: impl FnOnce(&mut Vec<PathBuf>) -> Result<T>,
) -> Result<T> {
	let canonical =
		fs::canonicalize(path).with_context(|| format!("failed to resolve mapped directory {}", path.display()))?;
	if let Some(ancestor) = canonical_stack.iter().find(|ancestor| **ancestor == canonical) {
		bail!(
			"mapped source symlink cycle: {} resolves to active ancestor {}",
			path.display(),
			ancestor.display()
		);
	}
	canonical_stack.push(canonical);
	let result = read(canonical_stack);
	canonical_stack.pop();
	result
}

fn read_directory_contents_tracked(
	path: &Path,
	generated_data: &Path,
	traversal: &mut MappedTraversal,
) -> Result<Vec<Snapshot>> {
	let root = read_directory_instance_tracked(path, "__contents", generated_data, traversal)?;
	ensure!(
		root.class.as_str() == "Folder",
		"engine anchor contents mount may not use an init script"
	);
	Ok(root.children)
}

#[cfg(test)]
fn read_directory_instance(path: &Path, name: &str, generated_data: &Path) -> Result<Snapshot> {
	let mut traversal = MappedTraversal::default();
	read_directory_instance_tracked(path, name, generated_data, &mut traversal)
}

fn read_directory_instance_tracked(
	path: &Path,
	name: &str,
	generated_data: &Path,
	traversal: &mut MappedTraversal,
) -> Result<Snapshot> {
	ensure!(
		traversal.path_type(path)?.is_dir(),
		"mapped directory path is not a directory: {}",
		path.display()
	);
	let mut canonical_stack = Vec::new();
	Ok(read_directory_instance_inner(path, name, generated_data, traversal, &mut canonical_stack)?.snapshot)
}

struct DirectoryRead {
	snapshot: Snapshot,
	// Bare directories are not source: Git cannot carry them to another
	// checkout. A recognized file or descendant makes this directory portable.
	has_portable_content: bool,
}

fn read_directory_instance_inner(
	path: &Path,
	name: &str,
	generated_data: &Path,
	traversal: &mut MappedTraversal,
	canonical_stack: &mut Vec<PathBuf>,
) -> Result<DirectoryRead> {
	validate_instance_name(name)?;
	with_mapped_directory(path, canonical_stack, |canonical_stack| {
		if let Some(project_path) = default_project_path(path, traversal)? {
			let tree = load_nested_project_tree(&project_path)?;
			let identity_route = vec![format!("nested-project:{}", project_path.display())];
			let snapshot = evaluate_nested_project_node(
				name,
				&tree,
				path,
				generated_data,
				&identity_route,
				&mut HashMap::new(),
				traversal,
				canonical_stack,
			)?
			.with_context(|| format!("nested Rojo project root did not resolve: {}", project_path.display()));
			return snapshot.map(|snapshot| DirectoryRead {
				snapshot,
				has_portable_content: true,
			});
		}
		let mut entries = fs::read_dir(path)
			.with_context(|| format!("failed to read mapped directory {}", path.display()))?
			.collect::<std::io::Result<Vec<_>>>()?;
		entries.sort_by_key(|entry| entry.file_name());
		let mut init = None;
		let mut metadata = None;
		let mut children = Vec::new();
		let mut has_portable_content = false;
		for entry in entries {
			let child_path = entry.path();
			if child_path == generated_data || child_path.starts_with(generated_data) {
				continue;
			}
			let file_type = traversal.file_type(&child_path, entry.file_type()?)?;
			let file_name = entry.file_name();
			let file_name = file_name.to_str().context("mapped source path is not UTF-8")?;
			match file_name {
				"meta.json" if file_type.is_file() => {
					has_portable_content = true;
					ensure!(metadata.is_none(), "mapped directory contains multiple meta.json files");
					metadata = Some(read_meta(&child_path)?);
				}
				_ if file_type.is_file() && is_init_script_file(file_name) => {
					has_portable_content = true;
					ensure!(
						init.is_none(),
						"mapped directory contains multiple init script variants: {}",
						path.display()
					);
					init = Some((file_name.to_owned(), fs::read_to_string(&child_path)?));
				}
				_ if file_type.is_dir() => {
					validate_instance_name(file_name)?;
					let child = read_directory_instance_inner(
						&child_path,
						file_name,
						generated_data,
						traversal,
						canonical_stack,
					)?;
					if child.has_portable_content {
						has_portable_content = true;
						children.push(child.snapshot);
					}
				}
				_ if file_type.is_file() && file_name.ends_with(".meta.json") => {
					bail!(
						"adjacent per-script metadata is unsupported; use directory meta.json: {}",
						child_path.display()
					)
				}
				_ if file_type.is_file() && is_script_source_file(file_name) => {
					has_portable_content = true;
					let child_name = script_name(file_name)?;
					validate_instance_name(&child_name)?;
					children.push(read_script_file(&child_path, &child_name)?);
				}
				_ if file_type.is_file() => {}
				_ => bail!("unsupported mapped source entry {}", child_path.display()),
			}
		}
		validate_sibling_names(&children, path)?;
		let (class, source) = match init {
			Some((file, source)) if file == "init.server.luau" || file == "init.server.lua" => ("Script", Some(source)),
			Some((file, source)) if file == "init.client.luau" || file == "init.client.lua" => {
				("LocalScript", Some(source))
			}
			Some((_, source)) => ("ModuleScript", Some(source)),
			None => ("Folder", None),
		};
		let mut snapshot = Snapshot::new()
			.with_id(stable_ref(&format!("source:{}", path.display())))
			.with_name(name)
			.with_class(class)
			.with_children(children);
		if let Some(source) = source {
			snapshot
				.properties
				.insert(Ustr::from("Source"), Variant::String(source));
		}
		if let Some(metadata) = metadata {
			apply_meta(&mut snapshot, metadata, path)?;
		}
		Ok(DirectoryRead {
			snapshot,
			has_portable_content,
		})
	})
}

fn read_script_file(path: &Path, name: &str) -> Result<Snapshot> {
	let file_name = path
		.file_name()
		.and_then(|name| name.to_str())
		.context("script path is not UTF-8")?;
	let class = script_class(file_name)?;
	let source = read_script_source(path)?;
	let mut properties = UstrMap::new();
	properties.insert(Ustr::from("Source"), Variant::String(source));
	Ok(Snapshot::new()
		.with_id(stable_ref(&format!("source:{}", path.display())))
		.with_name(name)
		.with_class(class)
		.with_properties(properties))
}

fn read_script_source(path: &Path) -> Result<String> {
	let file_name = path
		.file_name()
		.and_then(|name| name.to_str())
		.context("script path is not UTF-8")?;
	if file_name.ends_with(".toml") {
		let source =
			fs::read_to_string(path).with_context(|| format!("failed to read TOML module {}", path.display()))?;
		let value = toml::from_str::<toml::Value>(&source)
			.with_context(|| format!("failed to parse TOML module {}", path.display()))?;
		return Ok(toml_module_source(&value));
	}
	fs::read_to_string(path).with_context(|| format!("failed to read script source {}", path.display()))
}

#[derive(Default)]
struct Meta {
	id: Option<String>,
	properties: BTreeMap<String, UnresolvedValue>,
	attributes: BTreeMap<String, UnresolvedValue>,
}

fn read_meta(path: &Path) -> Result<Meta> {
	let value: Value =
		serde_json::from_slice(&fs::read(path)?).with_context(|| format!("failed to parse {}", path.display()))?;
	let mut object = value.as_object().cloned().context("meta.json must be an object")?;
	for key in object.keys() {
		ensure!(
			matches!(key.as_str(), "id" | "properties" | "attributes"),
			"unsupported meta.json field {key}"
		);
	}
	let id = object
		.remove("id")
		.map(|value| {
			value
				.as_str()
				.context("meta.json id must be a string")
				.map(str::to_owned)
		})
		.transpose()?;
	if let Some(id) = &id {
		validate_explicit_id(id)?;
	}
	let properties = object
		.remove("properties")
		.map(serde_json::from_value)
		.transpose()?
		.unwrap_or_default();
	let attributes = object
		.remove("attributes")
		.map(serde_json::from_value)
		.transpose()?
		.unwrap_or_default();
	Ok(Meta {
		id,
		properties,
		attributes,
	})
}

fn apply_meta(snapshot: &mut Snapshot, meta: Meta, path: &Path) -> Result<()> {
	if let Some(id) = meta.id {
		snapshot.id = identity_ref(&id);
	}
	for (property, value) in meta.properties {
		validate_property(snapshot.class.as_str(), &property)?;
		let value = resolve_mapped_property(value, snapshot.class.as_str(), &property)
			.with_context(|| format!("failed to resolve {} property {property}", path.display()))?;
		snapshot.properties.insert(Ustr::from(&property), value);
	}
	if !meta.attributes.is_empty() {
		let mut attributes = Attributes::new();
		for (name, value) in meta.attributes {
			validate_attribute_name(&name)?;
			let value = value.resolve_unambiguous()?;
			validate_attribute_value(&name, &value)?;
			attributes.insert(name, value);
		}
		snapshot
			.properties
			.insert(Ustr::from("Attributes"), Variant::Attributes(attributes));
	}
	Ok(())
}

fn validate_property(class: &str, property: &str) -> Result<()> {
	ensure!(
		!matches!(property, "Name" | "Parent" | "ClassName" | "Source" | "UniqueId"),
		"property {class}.{property} is structurally or internally managed and cannot be hydrated"
	);
	let database = util::get_reflection_database();
	let mut descriptor = database
		.classes
		.get(class)
		.with_context(|| format!("unknown Roblox class {class}"))?;
	let property_descriptor = loop {
		if let Some(property) = descriptor.properties.get(property) {
			break property;
		}
		let superclass = descriptor
			.superclass
			.with_context(|| format!("unknown property {class}.{property}"))?;
		descriptor = database
			.classes
			.get(superclass)
			.context("reflection superclass is missing")?;
	};
	ensure!(
		property == "Tags"
			|| (!property_descriptor.tags.contains(&PropertyTag::Hidden)
				&& !property_descriptor.tags.contains(&PropertyTag::ReadOnly)
				&& !property_descriptor.tags.contains(&PropertyTag::WriteOnly)),
		"property {class}.{property} is hidden or security-restricted"
	);
	if let PropertyKind::Canonical { serialization } = &property_descriptor.kind {
		ensure!(
			!matches!(serialization, PropertySerialization::DoesNotSerialize),
			"property {class}.{property} is not serializable"
		);
	}
	Ok(())
}

fn property_variant_type(class: &str, property: &str) -> Option<VariantType> {
	let database = util::get_reflection_database();
	let mut descriptor = database.classes.get(class)?;
	loop {
		if let Some(property) = descriptor.properties.get(property) {
			return match property.data_type {
				DataType::Value(value) => Some(value),
				DataType::Enum(_) => Some(VariantType::Enum),
				_ => None,
			};
		}
		descriptor = database.classes.get(descriptor.superclass?)?;
	}
}

fn resolve_mapped_property(value: UnresolvedValue, class: &str, property: &str) -> Result<Variant> {
	if property_variant_type(class, property) == Some(VariantType::Ref) {
		if let UnresolvedValue::FullyQualified(Variant::Ref(target)) = &value {
			return Ok(Variant::Ref(*target));
		}
		let target = value.as_str().with_context(|| {
			format!("reference property {class}.{property} must be a filesystem identity or manifest identity string")
		})?;
		if target == "null" || target.is_empty() {
			return Ok(Variant::Ref(Ref::none()));
		}
		let target = target
			.parse::<Ref>()
			.unwrap_or_else(|_| stable_ref(&format!("id:{target}")));
		return Ok(Variant::Ref(target));
	}
	value
		.resolve(class, property)
		.with_context(|| format!("failed to resolve mapped property {class}.{property}"))
}

fn validate_attribute_name(name: &str) -> Result<()> {
	ensure!(
		!name.is_empty() && name.len() <= 100,
		"attribute name must contain 1-100 bytes"
	);
	ensure!(
		!name.to_ascii_uppercase().starts_with("RBX"),
		"attribute name may not begin with RBX"
	);
	ensure!(
		name.chars().enumerate().all(|(index, character)| {
			character == '_' || character.is_ascii_alphanumeric() && (index != 0 || !character.is_ascii_digit())
		}),
		"invalid Roblox attribute name '{name}'"
	);
	Ok(())
}

fn validate_attribute_value(name: &str, value: &Variant) -> Result<()> {
	ensure!(
		!matches!(
			value.ty(),
			VariantType::Ref
				| VariantType::Attributes
				| VariantType::BinaryString
				| VariantType::SharedString
				| VariantType::Content
		),
		"unsupported attribute value type {:?} for {name}",
		value.ty()
	);
	Ok(())
}

fn validate_instance_name(name: &str) -> Result<()> {
	ensure!(!name.is_empty(), "filesystem-owned instance name may not be empty");
	ensure!(
		!name
			.chars()
			.any(|character| character < ' '
				|| matches!(character, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')),
		"filesystem-owned instance name is not portable: {name}"
	);
	ensure!(
		!name.ends_with([' ', '.']),
		"filesystem-owned instance name has a non-portable suffix: {name}"
	);
	let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
	ensure!(
		!matches!(
			stem.as_str(),
			"CON"
				| "PRN" | "AUX"
				| "NUL" | "COM1"
				| "COM2" | "COM3"
				| "COM4" | "COM5"
				| "COM6" | "COM7"
				| "COM8" | "COM9"
				| "LPT1" | "LPT2"
				| "LPT3" | "LPT4"
				| "LPT5" | "LPT6"
				| "LPT7" | "LPT8"
				| "LPT9"
		),
		"filesystem-owned instance name is reserved on Windows: {name}"
	);
	Ok(())
}

fn validate_sibling_names(children: &[Snapshot], path: &Path) -> Result<()> {
	let mut seen: HashMap<String, &Snapshot> = HashMap::new();
	for child in children {
		let normalized = child.name.nfc().collect::<String>().to_lowercase();
		if let Some(previous) = seen.insert(normalized, child) {
			bail!(
				"portable filesystem sibling collision in {}: {} '{}' and {} '{}'",
				path.display(),
				previous.class,
				previous.name,
				child.class,
				child.name
			);
		}
	}
	Ok(())
}

fn validate_filesystem_subtree(snapshot: &Snapshot, path: &str) -> Result<()> {
	ensure!(
		is_filesystem_class(snapshot.class.as_str()),
		"unsupported instance {} '{}' below mapping barrier {path}",
		snapshot.class,
		snapshot.name
	);
	validate_sibling_names(&snapshot.children, Path::new(path))?;
	for child in &snapshot.children {
		validate_filesystem_subtree(child, path)?;
	}
	Ok(())
}

fn script_name(file_name: &str) -> Result<String> {
	for suffix in [
		".server.luau",
		".client.luau",
		".server.lua",
		".client.lua",
		".luau",
		".lua",
		".toml",
	] {
		if let Some(name) = file_name.strip_suffix(suffix) {
			ensure!(!name.is_empty(), "script file has no Roblox name: {file_name}");
			return Ok(name.to_owned());
		}
	}
	bail!("unsupported mapped file form {file_name}; only Lua/Luau scripts, TOML modules, and meta.json are accepted")
}

fn script_class(file_name: &str) -> Result<&'static str> {
	if file_name.ends_with(".server.luau") || file_name.ends_with(".server.lua") {
		Ok("Script")
	} else if file_name.ends_with(".client.luau") || file_name.ends_with(".client.lua") {
		Ok("LocalScript")
	} else if file_name.ends_with(".luau") || file_name.ends_with(".lua") || file_name.ends_with(".toml") {
		Ok("ModuleScript")
	} else {
		bail!("unsupported mapped source file {file_name}")
	}
}

fn is_init_script_file(file_name: &str) -> bool {
	matches!(
		file_name,
		"init.luau" | "init.server.luau" | "init.client.luau" | "init.lua" | "init.server.lua" | "init.client.lua"
	)
}

fn is_script_source_file(file_name: &str) -> bool {
	file_name.ends_with(".luau") || file_name.ends_with(".lua") || file_name.ends_with(".toml")
}

fn toml_module_source(value: &toml::Value) -> String {
	let mut source = String::from("return ");
	write_toml_lua_value(value, 0, &mut source);
	source
}

fn write_toml_lua_value(value: &toml::Value, indent: usize, output: &mut String) {
	match value {
		toml::Value::String(value) => write_lua_string(value, output),
		toml::Value::Integer(value) => write_lua_number(*value as f64, output),
		toml::Value::Float(value) => write_lua_number(*value, output),
		toml::Value::Boolean(value) => output.push_str(if *value { "true" } else { "false" }),
		toml::Value::Datetime(value) => write_lua_string(&value.to_string(), output),
		toml::Value::Array(values) => {
			output.push('{');
			for (index, value) in values.iter().enumerate() {
				if index > 0 {
					output.push_str(", ");
				}
				write_toml_lua_value(value, indent, output);
			}
			output.push('}');
		}
		toml::Value::Table(values) => {
			output.push_str("{\n");
			for (key, value) in values {
				output.push_str(&"\t".repeat(indent + 1));
				write_lua_table_key(key, output);
				output.push_str(" = ");
				write_toml_lua_value(value, indent + 1, output);
				output.push_str(",\n");
			}
			output.push_str(&"\t".repeat(indent));
			output.push('}');
		}
	}
}

fn write_lua_number(value: f64, output: &mut String) {
	if value.is_nan() {
		output.push_str("0/0");
	} else if value == f64::INFINITY {
		output.push_str("math.huge");
	} else if value == f64::NEG_INFINITY {
		output.push_str("-math.huge");
	} else {
		output.push_str(&value.to_string());
	}
}

fn write_lua_string(value: &str, output: &mut String) {
	output.push('"');
	for character in value.chars() {
		match character {
			'"' => output.push_str("\\\""),
			'\r' => output.push_str("\\r"),
			'\n' => output.push_str("\\n"),
			'\t' => output.push_str("\\t"),
			'\\' => output.push_str("\\\\"),
			_ => output.push(character),
		}
	}
	output.push('"');
}

fn write_lua_table_key(key: &str, output: &mut String) {
	if is_lua_identifier(key) {
		output.push_str(key);
	} else {
		output.push('[');
		write_lua_string(key, output);
		output.push(']');
	}
}

fn is_lua_identifier(value: &str) -> bool {
	if matches!(
		value,
		"and"
			| "break" | "do"
			| "else" | "elseif"
			| "end" | "false"
			| "for" | "function"
			| "if" | "in"
			| "local" | "nil"
			| "not" | "or"
			| "repeat"
			| "return"
			| "then" | "true"
			| "until" | "while"
	) {
		return false;
	}
	let mut characters = value.chars();
	let Some(first) = characters.next() else {
		return false;
	};
	(first.is_ascii_alphabetic() || first == '_')
		&& characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn is_filesystem_class(class: &str) -> bool {
	matches!(class, "Folder" | "Script" | "LocalScript" | "ModuleScript")
}

fn is_script_class(class: &str) -> bool {
	matches!(class, "Script" | "LocalScript" | "ModuleScript")
}

pub(crate) fn is_service(class: &str) -> bool {
	util::get_reflection_database()
		.classes
		.get(class)
		.is_some_and(|descriptor| descriptor.tags.contains(&ClassTag::Service))
}

pub(crate) fn stable_ref(seed: &str) -> Ref {
	let mut hasher = blake3::Hasher::new();
	hasher.update(b"carbon-project-ref-v1\0");
	hasher.update(seed.as_bytes());
	let hex = hasher.finalize().to_hex();
	hex[..32].parse().expect("BLAKE3 prefix is a valid Roblox referent")
}

fn identity_ref(id: &str) -> Ref {
	id.parse::<Ref>().unwrap_or_else(|_| stable_ref(&format!("id:{id}")))
}

fn validate_explicit_id(id: &str) -> Result<()> {
	ensure!(!id.trim().is_empty(), "filesystem identity may not be empty");
	ensure!(
		id == id.trim(),
		"filesystem identity may not have surrounding whitespace"
	);
	Ok(())
}

fn find_or_create_child<'a>(parent: &'a mut Snapshot, name: &str, class: &str) -> Result<&'a mut Snapshot> {
	if let Some(index) = parent.children.iter().position(|child| child.name == name) {
		ensure!(
			parent.children[index].class.as_str() == class,
			"engine anchor {name} has class {}, expected {class}",
			parent.children[index].class
		);
		return Ok(&mut parent.children[index]);
	}
	parent.children.push(
		Snapshot::new()
			.with_id(stable_ref(&format!("anchor:{class}:{name}")))
			.with_name(name)
			.with_class(class),
	);
	Ok(parent.children.last_mut().unwrap())
}

fn format_route(route: &[String], child: &str) -> String {
	let mut route = route.to_vec();
	route.push(child.to_owned());
	route.join(".")
}

fn tree_snapshot(tree: &Tree, id: Ref) -> Result<Snapshot> {
	let node = tree.get_instance(id).context("source tree instance is missing")?;
	let mut snapshot = Snapshot::new()
		.with_id(id)
		.with_name(&node.name)
		.with_class(node.class.as_str())
		.with_properties(node.properties.clone());
	for child in node.children() {
		snapshot.children.push(tree_snapshot(tree, *child)?);
	}
	Ok(snapshot)
}

pub(crate) fn snapshot_from_tree(tree: &Tree) -> Result<Snapshot> {
	tree_snapshot(tree, tree.root_ref())
}

#[cfg(test)]
pub(crate) fn canonical_after_studio_commit(
	previous: &Snapshot,
	committed: &Snapshot,
	mapped_refs: &HashSet<Ref>,
) -> Result<Snapshot> {
	let previous_nodes = flatten_snapshot(previous);
	fn merge(
		node: &Snapshot,
		previous_nodes: &HashMap<Ref, FlatNode<'_>>,
		mapped_refs: &HashSet<Ref>,
	) -> Result<Snapshot> {
		if mapped_refs.contains(&node.id) {
			return previous_nodes
				.get(&node.id)
				.map(|node| node.snapshot.clone())
				.context("filesystem-owned instance disappeared from the hybrid baseline");
		}
		let mut merged = node.clone();
		merged.children = node
			.children
			.iter()
			.map(|child| merge(child, previous_nodes, mapped_refs))
			.collect::<Result<Vec<_>>>()?;
		Ok(merged)
	}
	merge(committed, &previous_nodes, mapped_refs)
}

#[cfg(test)]
pub(crate) fn corrective_changes(tree: &Tree, attempted: &Changes) -> Result<Changes> {
	let canonical = tree_snapshot(tree, tree.root_ref())?;
	corrective_changes_from_snapshot(&canonical, attempted)
}

#[cfg(test)]
pub(crate) fn corrective_changes_from_snapshot(canonical: &Snapshot, attempted: &Changes) -> Result<Changes> {
	let mut observed = Tree::new(canonical.clone());
	let mut unknown_updates = Vec::new();
	for addition in &attempted.additions {
		if observed.exists(addition.id) {
			let mut update = UpdatedSnapshot::new(addition.id);
			update.parent = Some(addition.parent);
			update.name = Some(addition.name.clone());
			update.raw_name = addition.raw_name.clone();
			update.class = Some(addition.class);
			update.properties = Some(addition.properties.clone());
			observed.apply_update(update)?;
		} else {
			observed.insert_instance_recursive(addition.clone().into(), addition.parent);
		}
	}
	for update in &attempted.updates {
		if observed.exists(update.id) {
			observed.apply_update(update.clone())?;
		} else {
			unknown_updates.push(update.id);
		}
	}
	for removal in &attempted.removals {
		observed.remove_instance(*removal);
	}
	let observed = tree_snapshot(&observed, observed.root_ref())?;
	let mut correction = diff_snapshots(&observed, canonical)?;
	correction.removals.extend(unknown_updates);
	correction.removals.sort_by_key(ToString::to_string);
	correction.removals.dedup();
	Ok(correction)
}

fn dom_snapshot(dom: &WeakDom, id: Ref) -> Result<Snapshot> {
	let instance = dom.get_by_ref(id).context("decoded place instance is missing")?;
	let mut snapshot = Snapshot::new()
		.with_id(id)
		.with_name(&instance.name)
		.with_class(instance.class.as_str())
		.with_properties(instance.properties.clone());
	for child in instance.children() {
		snapshot.children.push(dom_snapshot(dom, *child)?);
	}
	Ok(snapshot)
}

fn validate_unique_snapshot_ids(snapshot: &Snapshot) -> Result<()> {
	fn visit(snapshot: &Snapshot, seen: &mut HashMap<Ref, String>) -> Result<()> {
		let location = format!("{} '{}'", snapshot.class, snapshot.name);
		if let Some(previous) = seen.insert(snapshot.id, location.clone()) {
			bail!(
				"duplicate composed identity {} at {previous} and {location}",
				snapshot.id
			);
		}
		for child in &snapshot.children {
			visit(child, seen)?;
		}
		Ok(())
	}
	visit(snapshot, &mut HashMap::new())
}

fn validate_snapshot_references(snapshot: &Snapshot) -> Result<()> {
	fn collect(snapshot: &Snapshot, ids: &mut HashSet<Ref>) {
		ids.insert(snapshot.id);
		for child in &snapshot.children {
			collect(child, ids);
		}
	}
	fn validate(snapshot: &Snapshot, ids: &HashSet<Ref>) -> Result<()> {
		for (property, value) in &snapshot.properties {
			if let Variant::Ref(target) = value {
				ensure!(
					target.is_none() || ids.contains(target),
					"dangling composed reference {}.{} -> {}; restore the mapped source identity or modify the relevant manifest reference",
					snapshot.name,
					property,
					target
				);
			}
		}
		for child in &snapshot.children {
			validate(child, ids)?;
		}
		Ok(())
	}
	let mut ids = HashSet::new();
	collect(snapshot, &mut ids);
	validate(snapshot, &ids)
}

fn stabilize_snapshot_ids(snapshot: &mut Snapshot) -> Result<()> {
	fn visit(snapshot: &mut Snapshot, allocator: &mut ManifestIdentityAllocator, remap: &mut HashMap<Ref, Ref>) {
		let old = snapshot.id;
		let id = Ref::some(allocator.next());
		snapshot.id = id;
		remap.insert(old, id);
		for child in &mut snapshot.children {
			visit(child, allocator, remap);
		}
	}
	fn refs(snapshot: &mut Snapshot, remap: &HashMap<Ref, Ref>) -> Result<()> {
		for value in snapshot.properties.values_mut() {
			match value {
				Variant::Ref(target) if target.is_some() => {
					*target = *remap
						.get(target)
						.with_context(|| format!("reference target {target} is outside place"))?;
				}
				Variant::Content(content) => {
					if let ContentType::Object(target) = content.value() {
						if target.is_some() {
							*content = Content::from_referent(
								*remap
									.get(target)
									.with_context(|| format!("content target {target} is outside place"))?,
							);
						}
					}
				}
				_ => {}
			}
		}
		for child in &mut snapshot.children {
			refs(child, remap)?;
		}
		Ok(())
	}
	let mut remap = HashMap::new();
	visit(snapshot, &mut ManifestIdentityAllocator::new(), &mut remap);
	refs(snapshot, &remap)
}

struct ExtractionPlan {
	routes: Vec<Vec<String>>,
	entries: Vec<ExtractionEntry>,
	outputs: BTreeSet<PathBuf>,
	required_ids: HashSet<Ref>,
}

struct ExtractionEntry {
	route: Vec<String>,
	output: PathBuf,
	snapshot: Snapshot,
}

fn plan_script_extraction(snapshot: &Snapshot) -> ExtractionPlan {
	let mut anchors = Vec::new();
	fn find_anchors<'a>(
		snapshot: &'a Snapshot,
		route: &mut Vec<String>,
		anchors: &mut Vec<(Vec<String>, &'a Snapshot)>,
	) {
		if route.len() == 1 && is_service(snapshot.class.as_str()) {
			anchors.push((route.clone(), snapshot));
		}
		if route.as_slice() == ["StarterPlayer", "StarterPlayerScripts"]
			|| route.as_slice() == ["StarterPlayer", "StarterCharacterScripts"]
		{
			anchors.push((route.clone(), snapshot));
		}
		for child in &snapshot.children {
			route.push(child.name.clone());
			find_anchors(child, route, anchors);
			route.pop();
		}
	}
	for child in &snapshot.children {
		let mut route = vec![child.name.clone()];
		find_anchors(child, &mut route, &mut anchors);
	}
	let mut selected: BTreeMap<Vec<String>, Snapshot> = BTreeMap::new();
	for (anchor_route, anchor) in anchors {
		for child in &anchor.children {
			if anchor_route.as_slice() == ["StarterPlayer"]
				&& matches!(child.name.as_str(), "StarterPlayerScripts" | "StarterCharacterScripts")
			{
				continue;
			}
			if !contains_script(child) {
				continue;
			}
			let route = anchor_route
				.iter()
				.cloned()
				.chain([child.name.clone()])
				.collect::<Vec<_>>();
			// Keep subtrees that cannot round-trip through filesystem naming and
			// class rules in the Studio-owned state artifact.
			if is_filesystem_representable(child) {
				selected.insert(route, child.clone());
			}
		}
	}
	let mut routes = Vec::new();
	let mut entries = Vec::new();
	let mut outputs = BTreeSet::new();
	let mut mapped_ids = HashSet::new();
	fn collect_ids(snapshot: &Snapshot, ids: &mut HashSet<Ref>) {
		ids.insert(snapshot.id);
		for child in &snapshot.children {
			collect_ids(child, ids);
		}
	}
	for selected_snapshot in selected.values() {
		collect_ids(selected_snapshot, &mut mapped_ids);
	}
	let mut required_ids = HashSet::new();
	fn collect_reference_targets(snapshot: &Snapshot, mapped: &HashSet<Ref>, required: &mut HashSet<Ref>) {
		for value in snapshot.properties.values() {
			if let Variant::Ref(target) = value {
				if mapped.contains(target) {
					required.insert(*target);
				}
			}
		}
		for child in &snapshot.children {
			collect_reference_targets(child, mapped, required);
		}
	}
	collect_reference_targets(snapshot, &mapped_ids, &mut required_ids);
	for (route, snapshot) in selected {
		let mut output = PathBuf::from("src");
		for component in &route[..route.len() - 1] {
			output.push(component);
		}
		output.push(representation_name(&snapshot, &required_ids));
		outputs.insert(output.clone());
		routes.push(route.clone());
		entries.push(ExtractionEntry {
			route,
			output,
			snapshot,
		});
	}
	ExtractionPlan {
		routes,
		entries,
		outputs,
		required_ids,
	}
}

fn contains_script(snapshot: &Snapshot) -> bool {
	is_script_class(snapshot.class.as_str()) || snapshot.children.iter().any(contains_script)
}

fn is_filesystem_representable(snapshot: &Snapshot) -> bool {
	if !is_filesystem_class(snapshot.class.as_str()) {
		return false;
	}
	let mut seen = HashSet::new();
	for child in &snapshot.children {
		let key = child.name.nfc().collect::<String>().to_lowercase();
		if !seen.insert(key) || !is_filesystem_representable(child) {
			return false;
		}
	}
	true
}

fn property_values_semantically_equal(
	class: &str,
	property: &str,
	left: Option<&Variant>,
	right: Option<&Variant>,
) -> bool {
	if let (Some(Variant::CFrame(left)), Some(Variant::CFrame(right))) = (left, right) {
		return source_wire::cframe_semantically_equal(left, right);
	}
	mapped_property_equal(class, property, left, right)
}

fn reflection_default(class: &str, property: &str) -> Option<&'static Variant> {
	let database = util::get_reflection_database();
	let mut descriptor = database.classes.get(class)?;
	loop {
		if let Some(value) = descriptor.default_properties.get(property) {
			return Some(value);
		}
		descriptor = database.classes.get(descriptor.superclass?)?;
	}
}

fn representation_name(snapshot: &Snapshot, required_ids: &HashSet<Ref>) -> String {
	if is_script_class(snapshot.class.as_str())
		&& snapshot.children.is_empty()
		&& metadata_properties(snapshot).is_empty()
		&& !required_ids.contains(&snapshot.id)
	{
		format!("{}{}", snapshot.name, script_suffix(snapshot.class.as_str()))
	} else {
		snapshot.name.clone()
	}
}

fn script_suffix(class: &str) -> &'static str {
	match class {
		"Script" => ".server.luau",
		"LocalScript" => ".client.luau",
		_ => ".luau",
	}
}

fn write_mapped_snapshot(path: &Path, snapshot: &Snapshot, required_ids: &HashSet<Ref>) -> Result<()> {
	if is_script_class(snapshot.class.as_str())
		&& snapshot.children.is_empty()
		&& metadata_properties(snapshot).is_empty()
		&& !required_ids.contains(&snapshot.id)
	{
		let source = script_source(snapshot)?;
		return write_text(path, &source);
	}
	fs::create_dir_all(path)?;
	if is_script_class(snapshot.class.as_str()) {
		write_text(
			&path.join(format!("init{}", script_suffix(snapshot.class.as_str()))),
			&script_source(snapshot)?,
		)?;
	}
	let meta = snapshot_meta_json(snapshot, required_ids.contains(&snapshot.id))?;
	let has_metadata = meta.as_object().is_some_and(|object| !object.is_empty());
	let empty_folder_needs_marker = snapshot.class.as_str() == "Folder" && snapshot.children.is_empty();
	if empty_folder_needs_marker || has_metadata {
		write_json(&path.join("meta.json"), &meta)?;
	}
	for child in &snapshot.children {
		write_mapped_snapshot(
			&path.join(representation_name(child, required_ids)),
			child,
			required_ids,
		)?;
	}
	Ok(())
}

fn script_source(snapshot: &Snapshot) -> Result<String> {
	match snapshot.properties.get(&Ustr::from("Source")) {
		Some(Variant::String(source)) => Ok(source.clone()),
		Some(Variant::BinaryString(_)) => bail!("script {} has non-UTF-8 Source", snapshot.name),
		Some(_) => bail!("script {} has an invalid Source value", snapshot.name),
		None => Ok(String::new()),
	}
}

fn metadata_properties(snapshot: &Snapshot) -> BTreeMap<String, Variant> {
	snapshot
		.properties
		.iter()
		.filter(|(name, value)| {
			name.as_str() != "Source"
				&& name.as_str() != "Name"
				&& name.as_str() != "Parent"
				&& (name.as_str() == "Attributes" || validate_property(snapshot.class.as_str(), name.as_str()).is_ok())
				&& !is_reflection_default(snapshot.class.as_str(), name.as_str(), value)
		})
		.map(|(name, value)| (name.to_string(), value.clone()))
		.collect()
}

fn is_reflection_default(class: &str, property: &str, value: &Variant) -> bool {
	let database = util::get_reflection_database();
	let Some(mut descriptor) = database.classes.get(class) else {
		return false;
	};
	loop {
		if descriptor.default_properties.get(property) == Some(value) {
			return true;
		}
		let Some(superclass) = descriptor.superclass else {
			return false;
		};
		let Some(parent) = database.classes.get(superclass) else {
			return false;
		};
		descriptor = parent;
	}
}

fn snapshot_meta_json(snapshot: &Snapshot, persist_id: bool) -> Result<Value> {
	let mut root = Map::new();
	if persist_id {
		root.insert("id".to_owned(), Value::String(snapshot.id.to_string()));
	}
	let mut properties = Map::new();
	let mut attributes = None;
	for (name, value) in metadata_properties(snapshot) {
		if name == "Attributes" {
			let Variant::Attributes(values) = value else {
				bail!("Attributes metadata is invalid")
			};
			let mut object = Map::new();
			let mut keys = values.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>();
			keys.sort();
			for key in keys {
				let value = values
					.iter()
					.find(|(candidate, _)| candidate.as_str() == key)
					.unwrap()
					.1;
				let unresolved = match &value {
					Variant::Bool(_) | Variant::Float64(_) | Variant::String(_) => {
						UnresolvedValue::from_variant(value.clone(), "", "")
					}
					Variant::BinaryString(bytes) => {
						let text = std::str::from_utf8(bytes.as_ref())
							.context("mapped string attribute is not valid UTF-8")?;
						UnresolvedValue::from_variant(Variant::String(text.to_owned()), "", "")
					}
					_ => UnresolvedValue::FullyQualified(value.clone()),
				};
				object.insert(key.clone(), serde_json::to_value(unresolved)?);
			}
			attributes = Some(Value::Object(object));
			continue;
		}
		validate_property(snapshot.class.as_str(), &name)?;
		if let Variant::Ref(target) = value {
			properties.insert(
				name,
				Value::String(if target.is_none() {
					"null".to_owned()
				} else {
					target.to_string()
				}),
			);
			continue;
		}
		properties.insert(
			name.clone(),
			serde_json::to_value(UnresolvedValue::from_variant(value, snapshot.class.as_str(), &name))?,
		);
	}
	if !properties.is_empty() {
		root.insert("properties".to_owned(), Value::Object(properties));
	}
	if let Some(attributes) = attributes {
		root.insert("attributes".to_owned(), attributes);
	}
	Ok(Value::Object(root))
}

fn prune_routes(mut snapshot: Snapshot, routes: &[Vec<String>]) -> Result<Snapshot> {
	for route in routes {
		ensure!(!route.is_empty(), "mapping route is empty");
		remove_route(&mut snapshot, route)?;
	}
	Ok(snapshot)
}

fn remove_route(snapshot: &mut Snapshot, route: &[String]) -> Result<()> {
	if route.len() == 1 {
		snapshot.children.retain(|child| child.name != route[0]);
		return Ok(());
	}
	let child = snapshot
		.children
		.iter_mut()
		.find(|child| child.name == route[0])
		.with_context(|| format!("mapping route anchor {} is absent", route[0]))?;
	remove_route(child, &route[1..])
}

fn snapshot_refs_for_routes(
	snapshot: &Snapshot,
	routes: &[Vec<String>],
) -> Result<(HashSet<Ref>, Vec<Ref>, HashSet<Ref>)> {
	fn collect(snapshot: &Snapshot, refs: &mut HashSet<Ref>) {
		refs.insert(snapshot.id);
		for child in &snapshot.children {
			collect(child, refs);
		}
	}

	let mut refs = HashSet::new();
	let mut roots = Vec::new();
	let mut routing = HashSet::new();
	for route in routes {
		let mut node = snapshot;
		for (index, component) in route.iter().enumerate() {
			node = node
				.children
				.iter()
				.find(|child| child.name == *component)
				.with_context(|| format!("composed mapping route {} is missing", route.join(".")))?;
			if index + 1 < route.len() {
				routing.insert(node.id);
			}
		}
		roots.push(node.id);
		collect(node, &mut refs);
	}
	Ok((refs, roots, routing))
}

pub(crate) fn refs_for_routes(tree: &Tree, routes: &[Vec<String>]) -> Result<(HashSet<Ref>, Vec<Ref>)> {
	let mut refs = HashSet::new();
	let mut roots = Vec::new();
	for route in routes {
		let mut id = tree.root_ref();
		for component in route {
			let node = tree.get_instance(id).context("composed route parent is missing")?;
			id = *node
				.children()
				.iter()
				.find(|child| tree.get_instance(**child).is_some_and(|node| node.name == *component))
				.with_context(|| format!("composed mapping route {} is missing", route.join(".")))?;
		}
		roots.push(id);
		refs.extend(tree.subtree_refs(id)?);
	}
	Ok((refs, roots))
}

pub(crate) fn routing_refs(tree: &Tree, roots: &[Ref]) -> Result<HashSet<Ref>> {
	let mut result = HashSet::new();
	for root in roots {
		let mut current = tree.get_instance(*root).context("mapped root is missing")?.parent();
		while current.is_some() && current != tree.root_ref() {
			result.insert(current);
			current = tree
				.get_instance(current)
				.context("mapping route ancestor is missing")?
				.parent();
		}
	}
	Ok(result)
}

fn empty_studio_snapshot() -> Snapshot {
	let mut identities = ManifestIdentityAllocator::new();
	Snapshot::new()
		.with_id(Ref::some(identities.next()))
		.with_name("DataModel")
		.with_class("DataModel")
		.with_children(vec![
			Snapshot::new()
				.with_id(Ref::some(identities.next()))
				.with_name("Workspace")
				.with_class("Workspace"),
			Snapshot::new()
				.with_id(Ref::some(identities.next()))
				.with_name("ReplicatedStorage")
				.with_class("ReplicatedStorage"),
			Snapshot::new()
				.with_id(Ref::some(identities.next()))
				.with_name("ServerScriptService")
				.with_class("ServerScriptService"),
			Snapshot::new()
				.with_id(Ref::some(identities.next()))
				.with_name("ServerStorage")
				.with_class("ServerStorage"),
			Snapshot::new()
				.with_id(Ref::some(identities.next()))
				.with_name("StarterPlayer")
				.with_class("StarterPlayer")
				.with_children(vec![
					Snapshot::new()
						.with_id(Ref::some(identities.next()))
						.with_name("StarterPlayerScripts")
						.with_class("StarterPlayerScripts"),
					Snapshot::new()
						.with_id(Ref::some(identities.next()))
						.with_name("StarterCharacterScripts")
						.with_class("StarterCharacterScripts"),
				]),
		])
}

fn starter_project_json(name: &str) -> Value {
	serde_json::json!({
		"name": name,
		"tree": {
			"$className": "DataModel",
			"ReplicatedStorage": { "Shared": { "$path": "src/ReplicatedStorage/Shared" } },
			"ServerScriptService": { "Server": { "$path": "src/ServerScriptService/Server.server.luau" } },
			"StarterPlayer": { "StarterPlayerScripts": { "Client": { "$path": "src/StarterPlayer/StarterPlayerScripts/Client.client.luau" } } }
		}
	})
}

fn starter_files() -> BTreeMap<PathBuf, &'static str> {
	BTreeMap::from([
		(
			PathBuf::from("src/ReplicatedStorage/Shared/Example.luau"),
			"local Example = {}\n\nreturn Example\n",
		),
		(
			PathBuf::from("src/ServerScriptService/Server.server.luau"),
			"-- Server entry point.\n",
		),
		(
			PathBuf::from("src/StarterPlayer/StarterPlayerScripts/Client.client.luau"),
			"-- Client entry point.\n",
		),
	])
}

fn project_json(name: &str, entries: &[ExtractionEntry]) -> Value {
	let mut tree = Map::from_iter([("$className".to_owned(), Value::String("DataModel".to_owned()))]);
	for entry in entries {
		let route = &entry.route;
		let mut cursor = &mut tree;
		for (index, component) in route.iter().enumerate() {
			let value = cursor
				.entry(component.clone())
				.or_insert_with(|| Value::Object(Map::new()));
			let object = value.as_object_mut().unwrap();
			if index + 1 == route.len() {
				object.insert(
					"$path".to_owned(),
					Value::String(entry.output.to_string_lossy().into_owned()),
				);
			}
			cursor = object;
		}
	}
	Value::Object(Map::from_iter([
		("name".to_owned(), Value::String(name.to_owned())),
		("tree".to_owned(), Value::Object(tree)),
	]))
}

fn generate_data_store(stage_root: &Path, snapshot: &Snapshot, name: &str) -> Result<PathBuf> {
	let staging_data = stage_root.join(format!(".studio-{}.carbon.data", Uuid::new_v4().simple()));
	fs::create_dir_all(&staging_data)?;
	artifact_store::extract_snapshot(snapshot.clone(), name.to_owned(), &staging_data.join(DATA_ARTIFACT))?;
	Ok(staging_data)
}

#[cfg(test)]
fn install_data_store(project_path: &Path, snapshot: &Snapshot, name: &str) -> Result<()> {
	install_data_store_inner(project_path, snapshot, name)
}

#[cfg(test)]
fn install_data_store_inner(project_path: &Path, snapshot: &Snapshot, name: &str) -> Result<()> {
	recover(project_path)?;
	let project_root = project_path.parent().unwrap_or_else(|| Path::new("."));
	let stage = transaction_stage(project_path)?;
	fs::create_dir_all(&stage)?;
	let generated = generate_data_store(&stage, snapshot, name)?;
	let target = data_dir(project_path)?;
	let staged_manifest = generated.join(DATA_ARTIFACT);
	let active_manifest = target.join(DATA_ARTIFACT);
	if active_manifest.exists() && fs::read(&active_manifest)? == fs::read(&staged_manifest)? {
		// The canonical data artifact commits every
		// content-addressed blob path. An exact match is a complete no-op; keep
		// the active directory so unchanged captures preserve every mtime.
		fs::remove_dir_all(&generated)?;
		let _ = fs::remove_dir_all(stage);
		return Ok(());
	}
	let backup = project_root.join(format!(".{}.backup-{}", target.get_name(), Uuid::new_v4().simple()));
	let journal = transaction_journal(project_path)?;
	write_json(
		&journal,
		&serde_json::json!({
			"version": TRANSACTION_VERSION,
			"mode": "replaceData",
			"target": target,
			"backup": backup,
			"staged": generated,
		}),
	)?;
	if target.exists() {
		fs::rename(&target, &backup)?;
	}
	fs::rename(&generated, &target)?;
	if backup.exists() {
		fs::remove_dir_all(&backup)?;
	}
	fs::remove_file(journal)?;
	let _ = fs::remove_dir_all(stage);
	Ok(())
}

fn commit_new_project<I>(
	project_path: &Path,
	stage: &Path,
	staged_project: &Path,
	staged_data: &Path,
	outputs: I,
) -> Result<()>
where
	I: IntoIterator<Item = PathBuf>,
{
	let root = project_path.parent().unwrap_or_else(|| Path::new("."));
	let outputs = outputs
		.into_iter()
		.filter_map(|output| {
			let source = stage.join(&output);
			source.exists().then(|| (source, root.join(output)))
		})
		.collect::<Vec<_>>();
	for (_, target) in &outputs {
		ensure!(!target.exists(), "transaction output collision: {}", target.display());
	}
	let target_data = data_dir(project_path)?;
	let journal = transaction_journal(project_path)?;
	write_json(
		&journal,
		&serde_json::json!({
			"version": TRANSACTION_VERSION,
			"mode": "createProject",
			"targetProject": project_path,
			"stagedProject": staged_project,
			"targetData": target_data,
			"stagedData": staged_data,
			"outputs": outputs.iter().map(|(source, target)| serde_json::json!({"source": source, "target": target})).collect::<Vec<_>>(),
		}),
	)?;
	for (source, target) in outputs {
		if let Some(parent) = target.parent() {
			fs::create_dir_all(parent)?;
		}
		fs::rename(source, target)?;
	}
	fs::rename(staged_data, target_data)?;
	fs::rename(staged_project, project_path)?;
	fs::remove_file(journal)?;
	fs::remove_dir_all(stage)?;
	Ok(())
}

fn recover(project_path: &Path) -> Result<()> {
	let journal = transaction_journal(project_path)?;
	if !journal.exists() {
		return Ok(());
	}
	let value: Value = serde_json::from_slice(&fs::read(&journal)?)?;
	ensure!(
		value["version"].as_u64() == Some(u64::from(TRANSACTION_VERSION)),
		"unsupported Carbon recovery journal"
	);
	if value["mode"].as_str() == Some("createProject") {
		let outputs = value["outputs"]
			.as_array()
			.context("create-project recovery journal has no outputs")?;
		for output in outputs {
			let source = json_path(output, "source")?;
			let target = json_path(output, "target")?;
			if !target.exists() && source.exists() {
				if let Some(parent) = target.parent() {
					fs::create_dir_all(parent)?;
				}
				fs::rename(source, &target)?;
			}
			ensure!(
				target.exists(),
				"cannot recover transaction output {}",
				target.display()
			);
		}
		for (staged_key, target_key) in [("stagedData", "targetData"), ("stagedProject", "targetProject")] {
			let staged = json_path(&value, staged_key)?;
			let target = json_path(&value, target_key)?;
			if !target.exists() && staged.exists() {
				fs::rename(staged, &target)?;
			}
			ensure!(
				target.exists(),
				"cannot recover transaction target {}",
				target.display()
			);
		}
		if let Some(stage) = json_path(&value, "stagedProject")?.parent() {
			if stage.exists() {
				fs::remove_dir_all(stage)?;
			}
		}
		fs::remove_file(journal)?;
		return Ok(());
	}
	if value["mode"].as_str() == Some("captureData") {
		let domains = value["domains"]
			.as_array()
			.context("capture-data recovery journal has no domains")?;
		let mut backups = Vec::new();
		for domain in domains {
			let staged = json_path(domain, "staged")?;
			let target = json_path(domain, "target")?;
			let backup = json_path(domain, "backup")?;
			if staged.exists() {
				if target.exists() && !backup.exists() {
					fs::rename(&target, &backup)?;
				}
				ensure!(
					!target.exists(),
					"cannot recover occupied capture target {}",
					target.display()
				);
				fs::rename(&staged, &target)?;
			}
			ensure!(target.exists(), "cannot recover capture target {}", target.display());
			backups.push(backup);
		}
		if let Some(retirements) = value["retirements"].as_array() {
			for retirement in retirements {
				let target = json_path(retirement, "target")?;
				let backup = json_path(retirement, "backup")?;
				if target.exists() && !backup.exists() {
					fs::rename(&target, &backup)?;
				}
				ensure!(
					!target.exists() && backup.exists(),
					"cannot recover capture retirement {}",
					target.display()
				);
				backups.push(backup);
			}
		}
		fs::remove_file(journal)?;
		for backup in backups {
			remove_any(&backup)?;
		}
		return Ok(());
	}
	if value["mode"].as_str() == Some("promoteScript") {
		let source = json_path(&value, "source")?;
		let source_backup = json_path(&value, "sourceBackup")?;
		let target = json_path(&value, "target")?;
		let staged = json_path(&value, "staged")?;
		let project = json_optional_path(&value, "project")?;
		let project_backup = json_optional_path(&value, "projectBackup")?;
		let staged_project = json_optional_path(&value, "stagedProject")?;
		if target.exists() {
			ensure!(
				project.as_ref().is_none_or(|path| path.exists()),
				"cannot recover promoted script project"
			);
		} else if staged.exists() {
			if let (Some(project), Some(backup), Some(staged_project)) = (&project, &project_backup, &staged_project) {
				if staged_project.exists() {
					if project.exists() && !backup.exists() {
						fs::rename(project, backup)?;
					}
					fs::rename(staged_project, project)?;
				}
				ensure!(project.exists(), "cannot recover promoted script project");
			}
			fs::rename(&staged, &target)?;
		} else if source_backup.exists() {
			if let (Some(project), Some(backup)) = (&project, &project_backup) {
				if backup.exists() {
					remove_any(project)?;
					fs::rename(backup, project)?;
				}
			}
			if !source.exists() {
				fs::rename(&source_backup, &source)?;
			}
		} else {
			bail!("Carbon script-promotion transaction cannot be recovered");
		}
		remove_any(&source_backup)?;
		if let Some(backup) = project_backup {
			remove_any(&backup)?;
		}
		remove_any(&staged)?;
		if let Some(stage) = staged.parent() {
			remove_any(stage)?;
		}
		fs::remove_file(journal)?;
		return Ok(());
	}
	ensure!(
		matches!(
			value["mode"].as_str(),
			Some("replaceData" | "replacePath" | "replaceFile")
		),
		"unsupported Carbon recovery journal mode"
	);
	let target = json_path(&value, "target")?;
	let backup = json_path(&value, "backup")?;
	let staged = json_path(&value, "staged")?;
	if target.exists() {
		remove_any(&backup)?;
		remove_any(&staged)?;
	} else if staged.exists() {
		fs::rename(staged, target)?;
		remove_any(&backup)?;
	} else if backup.exists() {
		fs::rename(backup, target)?;
	} else {
		bail!("Carbon transaction cannot be recovered: all transaction domains are missing");
	}
	fs::remove_file(journal)?;
	Ok(())
}

fn json_path(value: &Value, key: &str) -> Result<PathBuf> {
	Ok(PathBuf::from(
		value[key]
			.as_str()
			.with_context(|| format!("recovery journal is missing {key}"))?,
	))
}

fn json_optional_path(value: &Value, key: &str) -> Result<Option<PathBuf>> {
	match value.get(key) {
		None | Some(Value::Null) => Ok(None),
		Some(Value::String(path)) => Ok(Some(PathBuf::from(path))),
		Some(_) => bail!("recovery journal has invalid {key}"),
	}
}

fn transaction_stage(project_path: &Path) -> Result<PathBuf> {
	Ok(project_path
		.parent()
		.unwrap_or_else(|| Path::new("."))
		.join(format!(".carbon-stage-{}", Uuid::new_v4().simple())))
}

fn transaction_journal(project_path: &Path) -> Result<PathBuf> {
	Ok(project_path
		.parent()
		.unwrap_or_else(|| Path::new("."))
		.join(format!(".{}.transaction.json", project_path.get_name())))
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent)?;
	}
	let mut writer = BufWriter::new(File::create(path)?);
	serde_json::to_writer_pretty(&mut writer, value)?;
	writer.write_all(b"\n")?;
	writer.flush()?;
	writer.get_ref().sync_all()?;
	Ok(())
}

fn write_text(path: &Path, contents: &str) -> Result<()> {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent)?;
	}
	let mut file = File::create(path)?;
	file.write_all(contents.as_bytes())?;
	file.sync_all()?;
	Ok(())
}

fn artifact_store_report(manifest_path: &Path) -> Result<ExtractReport> {
	let artifact = artifact_store::inspect(manifest_path)?;
	Ok(ExtractReport {
		instances: artifact.instances,
		properties: artifact.properties,
		blobs: artifact.blobs,
		artifacts: 1,
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Read;
	use std::time::{SystemTime, UNIX_EPOCH};

	fn temp(name: &str) -> PathBuf {
		let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
		std::env::temp_dir().join(format!("carbon-project-{name}-{unique}"))
	}

	fn copy_tree(source: &Path, destination: &Path) {
		fs::create_dir_all(destination).unwrap();
		let mut entries = fs::read_dir(source)
			.unwrap()
			.collect::<std::io::Result<Vec<_>>>()
			.unwrap();
		entries.sort_by_key(|entry| entry.file_name());
		for entry in entries {
			let source = entry.path();
			let destination = destination.join(entry.file_name());
			if entry.file_type().unwrap().is_dir() {
				copy_tree(&source, &destination);
			} else {
				fs::copy(source, destination).unwrap();
			}
		}
	}

	#[test]
	fn merge_stress_regression_identical_worktrees_build_identical_sibling_order() {
		let root = temp("portable-worktree-build");
		let first = root.join("first");
		let second = root.join("second");
		fs::create_dir_all(&first).unwrap();
		let first_project = first.join("game.carbon.json");
		initialize(&first_project, "Game".to_owned()).unwrap();
		for index in 0..32 {
			write_text(
				&first.join(format!("src/ReplicatedStorage/Shared/Portable{index:02}.luau")),
				"return true\n",
			)
			.unwrap();
		}
		copy_tree(&first, &second);
		let second_project = second.join("game.carbon.json");
		assert_eq!(
			fs::read(data_artifact(&first_project).unwrap()).unwrap(),
			fs::read(data_artifact(&second_project).unwrap()).unwrap()
		);

		let first_output = root.join("first.rbxl");
		let second_output = root.join("second.rbxl");
		compile(&first_project, &first_output, None).unwrap();
		compile(&second_project, &second_output, None).unwrap();

		assert_eq!(
			fs::read(first_output).unwrap(),
			fs::read(second_output).unwrap(),
			"byte-identical projects in separate worktrees must build byte-identical places"
		);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn parent_relative_mappings_materialize_and_watch_external_source() {
		let root = temp("parent-relative-mapping");
		let place = root.join("place");
		let shared = root.join("shared");
		fs::create_dir_all(&shared).unwrap();
		fs::write(shared.join("Example.luau"), "return true\n").unwrap();
		let project_path = place.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		write_json(
			&project_path,
			&serde_json::json!({
				"name": "Game",
				"tree": {
					"$className": "DataModel",
					"ReplicatedStorage": {
						"Shared": { "$path": "../shared" }
					}
				}
			}),
		)
		.unwrap();

		let materialized = materialize(&project_path).unwrap();
		assert_eq!(
			materialized.mapped_watch_roots,
			vec![fs::canonicalize(&shared).unwrap()]
		);
		let shared = materialized
			.snapshot
			.children
			.iter()
			.find(|child| child.name == "ReplicatedStorage")
			.unwrap()
			.children
			.iter()
			.find(|child| child.name == "Shared")
			.unwrap();
		assert_eq!(
			shared
				.children
				.iter()
				.map(|child| (child.name.as_str(), child.class.as_str()))
				.collect::<Vec<_>>(),
			vec![("Example", "ModuleScript")]
		);

		fs::remove_dir_all(materialized.directory).unwrap();
		fs::remove_dir_all(root).unwrap();
	}

	fn install_build_parity_fixture(project_path: &Path) -> Ref {
		initialize(project_path, "Game".to_owned()).unwrap();
		let project_root = project_path.parent().unwrap();
		let mapped_identity = "build-parity-mapped-shared";
		write_json(
			&project_root.join("src/ReplicatedStorage/Shared/meta.json"),
			&serde_json::json!({"id": mapped_identity}),
		)
		.unwrap();
		let mapped_ref = identity_ref(mapped_identity);
		let mut lighting_attributes = Attributes::new();
		lighting_attributes.insert("RBX_LightingTechnologyUnifiedMigration".to_owned(), Variant::Bool(true));
		lighting_attributes.insert("RBX_OriginalTechnologyOnFileLoad".to_owned(), Variant::Int32(2));
		let mut studio = empty_studio_snapshot();
		studio.children.push(
			Snapshot::new()
				.with_id(stable_ref("build-parity:lighting"))
				.with_name("Lighting")
				.with_class("Lighting")
				.with_properties(UstrMap::from_iter([(
					Ustr::from("Attributes"),
					Variant::Attributes(lighting_attributes),
				)])),
		);
		let workspace = studio
			.children
			.iter_mut()
			.find(|child| child.name == "Workspace")
			.unwrap();
		workspace.children.extend([
			Snapshot::new()
				.with_id(stable_ref("build-parity:sibling-z"))
				.with_name("ZetaSibling")
				.with_class("Folder"),
			Snapshot::new()
				.with_id(stable_ref("build-parity:external-source"))
				.with_name("ExternalSource")
				.with_class("ModuleScript")
				.with_properties(UstrMap::from_iter([(
					Ustr::from("Source"),
					Variant::String("return 'external-blob'\n".repeat(1024)),
				)])),
			Snapshot::new()
				.with_id(stable_ref("build-parity:ref-holder"))
				.with_name("RefHolder")
				.with_class("ObjectValue")
				.with_properties(UstrMap::from_iter([(Ustr::from("Value"), Variant::Ref(mapped_ref))])),
			Snapshot::new()
				.with_id(stable_ref("build-parity:content-holder"))
				.with_name("ContentHolder")
				.with_class("AdGui")
				.with_properties(UstrMap::from_iter([(
					Ustr::from("FallbackImageContent"),
					Variant::Content(Content::from_referent(mapped_ref)),
				)])),
			Snapshot::new()
				.with_id(stable_ref("build-parity:sibling-a"))
				.with_name("AlphaSibling")
				.with_class("Folder"),
		]);
		install_data_store(project_path, &studio, "Game").unwrap();
		mapped_ref
	}

	#[test]
	fn hybrid_build_reuses_written_composite_tree_without_changing_output() {
		for managed in [false, true] {
			let root = temp(if managed {
				"build-tree-parity-managed"
			} else {
				"build-tree-parity-unmanaged"
			});
			fs::create_dir_all(&root).unwrap();
			let project_path = root.join("game.carbon.json");
			let mapped_ref = install_build_parity_fixture(&project_path);
			let reference_output = root.join("reference.rbxl");
			let actual_output = root.join("actual.rbxl");
			let cached_output = root.join("cached.rbxl");
			let changed_output = root.join("changed.rbxl");
			let materialized = materialize(&project_path).unwrap();
			assert!(materialized.mapped_refs.contains(&mapped_ref));
			assert!(materialized.identity_exclusions.contains(&mapped_ref));
			let contract = WorktreeContract {
				endpoint: "http://127.0.0.1:34872".to_owned(),
				project: "Game".to_owned(),
				worktree_id: "build-parity-worktree".to_owned(),
				session_token: "build-parity-session".to_owned(),
				identity_exclusions: HashSet::new(),
			};
			if managed {
				let mut reference_contract = contract.clone();
				reference_contract.identity_exclusions = materialized.identity_exclusions.clone();
				artifact_store::compile_worktree(&materialized.manifest_path, &reference_output, &reference_contract)
					.unwrap();
			} else {
				artifact_store::compile(&materialized.manifest_path, &reference_output).unwrap();
			}

			let (result, tree_loads) = artifact_store::count_tree_loads(|| {
				compile(&project_path, &actual_output, managed.then_some(&contract))
			});
			result.unwrap();
			assert_eq!(
				fs::read(&actual_output).unwrap(),
				fs::read(&reference_output).unwrap(),
				"hybrid build output changed for managed={managed}"
			);
			assert_eq!(
				tree_loads, 1,
				"a stable hybrid build should load only the canonical Studio artifact"
			);

			let (result, cached_tree_loads) = artifact_store::count_tree_loads(|| {
				compile(&project_path, &cached_output, managed.then_some(&contract))
			});
			result.unwrap();
			assert_eq!(
				cached_tree_loads, 0,
				"an unchanged build should use the validated output cache"
			);
			assert_eq!(
				fs::read(&cached_output).unwrap(),
				fs::read(&actual_output).unwrap(),
				"the validated cache changed build output for managed={managed}"
			);

			let project = load_project(&project_path).unwrap();
			let cache_key = build_cache_key(&project_path, &project, true, managed.then_some(&contract)).unwrap();
			let cache_output = build_cache_root(&project_path)
				.unwrap()
				.join(cache_key)
				.join("output.rbxl");
			fs::write(&cache_output, b"corrupt cached output").unwrap();
			let repaired_output = root.join("repaired.rbxl");
			let (result, repaired_tree_loads) = artifact_store::count_tree_loads(|| {
				compile(&project_path, &repaired_output, managed.then_some(&contract))
			});
			result.unwrap();
			assert_eq!(
				repaired_tree_loads,
				usize::from(managed),
				"a corrupt exact cache should use a healthy source layer when available"
			);
			assert_eq!(
				fs::read(&repaired_output).unwrap(),
				fs::read(&actual_output).unwrap(),
				"cache corruption recovery changed build output for managed={managed}"
			);
			if !managed {
				fs::write(&cache_output, b"corrupt exact cache again").unwrap();
				let source = mapped_source_state(&project_path, &project).unwrap();
				let layer_key = build_layer_key(&project_path, &project, &source).unwrap();
				let layer_output = build_cache_root(&project_path)
					.unwrap()
					.join("layers")
					.join(layer_key)
					.join("output.rbxl");
				fs::write(&layer_output, b"corrupt source layer").unwrap();
				let fully_repaired_output = root.join("fully-repaired.rbxl");
				let (result, fully_repaired_tree_loads) =
					artifact_store::count_tree_loads(|| compile(&project_path, &fully_repaired_output, None));
				result.unwrap();
				assert_eq!(
					fully_repaired_tree_loads, 1,
					"corrupt exact and layered caches must fall back to validated source"
				);
				assert_eq!(
					fs::read(&fully_repaired_output).unwrap(),
					fs::read(&actual_output).unwrap(),
					"layer corruption recovery changed build output"
				);
			}

			if managed {
				let mut changed_contract = contract.clone();
				changed_contract.session_token = "build-parity-session-changed".to_owned();
				let contract_output = root.join("contract-changed.rbxl");
				let contract_reference_output = root.join("contract-changed-reference.rbxl");
				let mut reference_contract = changed_contract.clone();
				reference_contract.identity_exclusions = materialized.identity_exclusions.clone();
				artifact_store::compile_worktree(
					&materialized.manifest_path,
					&contract_reference_output,
					&reference_contract,
				)
				.unwrap();
				let (result, contract_tree_loads) = artifact_store::count_tree_loads(|| {
					compile(&project_path, &contract_output, Some(&changed_contract))
				});
				result.unwrap();
				assert_eq!(
					contract_tree_loads, 0,
					"transport-only managed worktree changes must reuse the validated place payload"
				);
				assert_ne!(
					fs::read(&contract_output).unwrap(),
					fs::read(&actual_output).unwrap(),
					"a managed worktree contract change reused stale output"
				);
				assert_eq!(
					fs::read(&contract_output).unwrap(),
					fs::read(&contract_reference_output).unwrap(),
					"transport rewriting diverged from a full managed build"
				);
			}

			fs::write(
				root.join("src/ServerScriptService/Server.server.luau"),
				"print('cache invalidated by mapped source')\n",
			)
			.unwrap();
			let (result, changed_tree_loads) = artifact_store::count_tree_loads(|| {
				compile(&project_path, &changed_output, managed.then_some(&contract))
			});
			result.unwrap();
			assert_eq!(
				changed_tree_loads,
				usize::from(managed),
				"a source-only mapped change must patch the validated place payload"
			);
			assert_ne!(
				fs::read(&changed_output).unwrap(),
				fs::read(&actual_output).unwrap(),
				"a mapped-source change reused stale build output for managed={managed}"
			);
			let changed_reference_output = root.join("changed-reference.rbxl");
			let changed_materialized = materialize(&project_path).unwrap();
			if managed {
				let mut reference_contract = contract.clone();
				reference_contract.identity_exclusions = changed_materialized.identity_exclusions.clone();
				artifact_store::compile_worktree(
					&changed_materialized.manifest_path,
					&changed_reference_output,
					&reference_contract,
				)
				.unwrap();
			} else {
				artifact_store::compile(&changed_materialized.manifest_path, &changed_reference_output).unwrap();
			}
			assert_eq!(
				fs::read(&changed_output).unwrap(),
				fs::read(&changed_reference_output).unwrap(),
				"source patching diverged from a full build for managed={managed}"
			);
			fs::remove_dir_all(changed_materialized.directory).unwrap();
			fs::remove_dir_all(materialized.directory).unwrap();
			fs::remove_dir_all(root).unwrap();
		}
	}

	#[test]
	fn hybrid_build_preserves_capture_attestation_and_seeds_managed_cache() {
		let root = temp("build-capture-attestation-cache");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		let artifact = data_artifact(&project_path).unwrap();
		let metadata = BTreeMap::from([
			(
				artifact_store::CAPTURE_FINGERPRINT_METADATA_KEY.to_owned(),
				"capture-fingerprint".to_owned(),
			),
			(
				artifact_store::CAPTURE_PROJECT_GENERATION_METADATA_KEY.to_owned(),
				"served-generation".to_owned(),
			),
		]);
		artifact_store::extract_snapshot_with_metadata(
			empty_studio_snapshot(),
			"Game".to_owned(),
			metadata.clone(),
			&artifact,
		)
		.unwrap();
		let contract = WorktreeContract {
			endpoint: "http://127.0.0.1:34872".to_owned(),
			project: "Game".to_owned(),
			worktree_id: "capture-attestation-worktree".to_owned(),
			session_token: "capture-attestation-session-a".to_owned(),
			identity_exclusions: HashSet::new(),
		};

		compile(&project_path, &root.join("first.rbxl"), Some(&contract)).unwrap();
		assert_eq!(
			artifact_store::validated_artifact_receipt(&artifact)
				.unwrap()
				.metadata(),
			&metadata,
			"mapping-barrier pruning must preserve capture attestation metadata"
		);

		let mut changed_contract = contract;
		changed_contract.session_token = "capture-attestation-session-b".to_owned();
		let (result, tree_loads) = artifact_store::count_tree_loads(|| {
			compile(&project_path, &root.join("second.rbxl"), Some(&changed_contract))
		});
		result.unwrap();
		assert_eq!(
			tree_loads, 0,
			"the first build after capture must seed the managed output cache"
		);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn first_hybrid_build_seeds_the_post_canonical_cache_key() {
		let root = temp("build-post-canonical-cache");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		let studio = Snapshot::new()
			.with_id(stable_ref("build-post-canonical-cache:DataModel"))
			.with_name("DataModel")
			.with_class("DataModel")
			.with_children(vec![Snapshot::new()
				.with_id(stable_ref("build-post-canonical-cache:Workspace"))
				.with_name("Workspace")
				.with_class("Workspace")]);
		install_data_store(&project_path, &studio, "Game").unwrap();

		let (result, artifact_writes) =
			artifact_store::count_artifact_writes(|| compile(&project_path, &root.join("first.rbxl"), None));
		result.unwrap();
		assert_eq!(
			artifact_writes, 1,
			"a build must write only the canonical Studio complement, not a temporary composite artifact"
		);
		let (result, tree_loads) =
			artifact_store::count_tree_loads(|| compile(&project_path, &root.join("second.rbxl"), None));
		result.unwrap();
		assert_eq!(
			tree_loads, 0,
			"the first build must cache the output under the canonical Studio generation it persists"
		);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn capture_attestation_and_nonauthored_order_do_not_invalidate_managed_build_cache() {
		let root = temp("build-capture-nonauthored-cache");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		let artifact = data_artifact(&project_path).unwrap();
		let mut studio = empty_studio_snapshot();
		let metadata = |fingerprint: &str, generation: &str| {
			BTreeMap::from([
				(
					artifact_store::CAPTURE_FINGERPRINT_METADATA_KEY.to_owned(),
					fingerprint.to_owned(),
				),
				(
					artifact_store::CAPTURE_PROJECT_GENERATION_METADATA_KEY.to_owned(),
					generation.to_owned(),
				),
			])
		};
		artifact_store::extract_snapshot_with_metadata(
			studio.clone(),
			"Game".to_owned(),
			metadata("capture-a", "project-a"),
			&artifact,
		)
		.unwrap();
		let mut contract = WorktreeContract {
			endpoint: "http://127.0.0.1:34872".to_owned(),
			project: "Game".to_owned(),
			worktree_id: "capture-nonauthored-worktree".to_owned(),
			session_token: "capture-nonauthored-session-a".to_owned(),
			identity_exclusions: HashSet::new(),
		};
		compile(&project_path, &root.join("first.rbxl"), Some(&contract)).unwrap();

		studio.children.reverse();
		artifact_store::extract_snapshot_with_metadata(
			studio.clone(),
			"Game".to_owned(),
			metadata("capture-b", "project-b"),
			&artifact,
		)
		.unwrap();
		contract.session_token = "capture-nonauthored-session-b".to_owned();
		let (result, tree_loads) =
			artifact_store::count_tree_loads(|| compile(&project_path, &root.join("second.rbxl"), Some(&contract)));
		result.unwrap();
		assert_eq!(
			tree_loads, 0,
			"capture attestation and raw sibling order are not authored build inputs"
		);

		let mut custom_metadata = metadata("capture-c", "project-c");
		custom_metadata.insert("AuthoredSetting".to_owned(), "changed".to_owned());
		artifact_store::extract_snapshot_with_metadata(studio, "Game".to_owned(), custom_metadata, &artifact).unwrap();
		let (result, tree_loads) = artifact_store::count_tree_loads(|| {
			compile(&project_path, &root.join("custom-metadata.rbxl"), Some(&contract))
		});
		result.unwrap();
		assert_eq!(
			tree_loads, 1,
			"authored custom metadata must invalidate the build cache"
		);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn projected_realization_generation_ignores_nonauthored_sibling_order() {
		let first = empty_studio_snapshot();
		let mut reordered = first.clone();
		reordered.children.reverse();
		let baseline = projected_realization_generation(first.clone(), Vec::new()).unwrap();
		assert_eq!(
			baseline,
			projected_realization_generation(reordered, Vec::new()).unwrap()
		);

		let mut renamed = first;
		renamed.children[0].name.push_str("Changed");
		assert_ne!(
			baseline,
			projected_realization_generation(renamed, Vec::new()).unwrap(),
			"authored hierarchy changes must alter project realization generation"
		);
	}

	#[test]
	fn direct_build_cache_is_invalidated_by_the_validated_artifact_generation() {
		let root = temp("direct-build-cache");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		write_json(
			&project_path,
			&serde_json::json!({
				"name": "DirectBuildCache",
				"tree": {"$className": "DataModel"}
			}),
		)
		.unwrap();
		let mut studio = empty_studio_snapshot();
		studio.children.push(
			Snapshot::new()
				.with_id(stable_ref("direct-build-cache:first"))
				.with_name("First")
				.with_class("Folder"),
		);
		install_data_store(&project_path, &studio, "DirectBuildCache").unwrap();

		let first_output = root.join("first.rbxl");
		let (result, first_loads) = artifact_store::count_tree_loads(|| compile(&project_path, &first_output, None));
		result.unwrap();
		assert_eq!(first_loads, 1, "a cold direct build must validate and load its source");

		let cached_output = root.join("cached.rbxl");
		let (result, cached_loads) = artifact_store::count_tree_loads(|| compile(&project_path, &cached_output, None));
		result.unwrap();
		assert_eq!(
			cached_loads, 0,
			"an unchanged direct build should use the validated cache"
		);
		assert_eq!(fs::read(&cached_output).unwrap(), fs::read(&first_output).unwrap());

		studio.children.push(
			Snapshot::new()
				.with_id(stable_ref("direct-build-cache:second"))
				.with_name("Second")
				.with_class("Folder"),
		);
		install_data_store(&project_path, &studio, "DirectBuildCache").unwrap();
		let changed_output = root.join("changed.rbxl");
		let (result, changed_loads) =
			artifact_store::count_tree_loads(|| compile(&project_path, &changed_output, None));
		result.unwrap();
		assert_eq!(
			changed_loads, 1,
			"a changed validated artifact generation must invalidate the direct-build cache"
		);
		assert_ne!(fs::read(&changed_output).unwrap(), fs::read(&first_output).unwrap());
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn managed_build_identity_exclusions_cover_studio_rehydrated_instances() {
		let root = Ref::new();
		let workspace = Ref::new();
		let edit_camera = Ref::new();
		let edit_camera_humanoid = Ref::new();
		let edit_camera_status = Ref::new();
		let edit_camera_status_child = Ref::new();
		let character = Ref::new();
		let humanoid = Ref::new();
		let status = Ref::new();
		let accessory = Ref::new();
		let handle = Ref::new();
		let accessory_weld = Ref::new();
		let head = Ref::new();
		let head_weld = Ref::new();
		let safe_part = Ref::new();
		let safe_weld = Ref::new();
		let configure = Ref::new();
		let configure_child = Ref::new();
		let filtered_selection = Ref::new();
		let mapped = Ref::new();
		let mapped_child = Ref::new();
		let snapshot = Snapshot::new()
			.with_id(root)
			.with_class("DataModel")
			.with_children(vec![
				Snapshot::new()
					.with_id(workspace)
					.with_class("Workspace")
					.with_properties(UstrMap::from_iter([(
						Ustr::from("CurrentCamera"),
						Variant::Ref(edit_camera),
					)]))
					.with_children(vec![
						Snapshot::new()
							.with_id(edit_camera)
							.with_class("Camera")
							.with_children(vec![Snapshot::new()
								.with_id(edit_camera_humanoid)
								.with_class("Humanoid")
								.with_children(vec![Snapshot::new()
									.with_id(edit_camera_status)
									.with_class("Status")
									.with_children(vec![Snapshot::new()
										.with_id(edit_camera_status_child)
										.with_class("Folder")])])]),
						Snapshot::new()
							.with_id(character)
							.with_class("Model")
							.with_children(vec![
								Snapshot::new()
									.with_id(humanoid)
									.with_class("Humanoid")
									.with_children(vec![Snapshot::new().with_id(status).with_class("Status")]),
								Snapshot::new()
									.with_id(accessory)
									.with_class("Accessory")
									.with_children(vec![Snapshot::new()
										.with_id(handle)
										.with_class("Part")
										.with_name("Handle")
										.with_children(vec![Snapshot::new()
											.with_id(accessory_weld)
											.with_class("Weld")
											.with_name("AccessoryWeld")])]),
								Snapshot::new()
									.with_id(head)
									.with_class("Part")
									.with_name("Head")
									.with_children(vec![Snapshot::new()
										.with_id(head_weld)
										.with_class("Weld")
										.with_name("HeadWeld")
										.with_properties(UstrMap::from_iter([(
											Ustr::from("Part1"),
											Variant::Ref(handle),
										)]))]),
							]),
						Snapshot::new()
							.with_id(safe_part)
							.with_class("Part")
							.with_name("Handle")
							.with_children(vec![Snapshot::new()
								.with_id(safe_weld)
								.with_class("Weld")
								.with_name("AccessoryWeld")]),
					]),
				Snapshot::new()
					.with_id(configure)
					.with_class("ConfigureServerService")
					.with_children(vec![Snapshot::new().with_id(configure_child).with_class("Folder")]),
				Snapshot::new()
					.with_id(filtered_selection)
					.with_class("Instance")
					.with_name("FilteredSelection"),
				Snapshot::new()
					.with_id(mapped)
					.with_class("Folder")
					.with_children(vec![Snapshot::new().with_id(mapped_child).with_class("ModuleScript")]),
			]);

		let exclusions = managed_build_identity_exclusions(&snapshot, &HashSet::from([mapped, mapped_child]));
		assert_eq!(
			exclusions,
			HashSet::from([
				edit_camera,
				edit_camera_humanoid,
				edit_camera_status,
				edit_camera_status_child,
				status,
				accessory_weld,
				head_weld,
				configure,
				configure_child,
				filtered_selection,
				mapped,
				mapped_child,
			])
		);
		assert!(!exclusions.contains(&safe_weld));
		let rebindings = managed_build_identity_rebindings(&snapshot, &HashSet::from([mapped, mapped_child]));
		assert_eq!(
			rebindings
				.iter()
				.map(|rebinding| (rebinding.source_id, rebinding.kind, rebinding.related_source_id))
				.collect::<Vec<_>>(),
			vec![
				(status, ManagedIdentityRebindingKind::HumanoidStatus, None),
				(accessory_weld, ManagedIdentityRebindingKind::AccessoryWeld, None),
				(head_weld, ManagedIdentityRebindingKind::HeadWeld, Some(handle)),
				(configure, ManagedIdentityRebindingKind::ConfigureServerService, None),
				(configure_child, ManagedIdentityRebindingKind::Descendant, None),
				(
					filtered_selection,
					ManagedIdentityRebindingKind::FilteredSelection,
					None
				),
			]
		);
		assert!(rebindings.iter().all(|rebinding| ![
			edit_camera,
			edit_camera_humanoid,
			edit_camera_status,
			edit_camera_status_child,
			mapped,
			mapped_child,
		]
		.contains(&rebinding.source_id)));
	}

	#[test]
	fn project_schema_field_is_removed() {
		assert!(starter_project_json("Game").get("$schema").is_none());
		assert!(project_json("Game", &[]).get("$schema").is_none());

		let error = parse_project(
			Path::new("game.carbon.json"),
			br#"{"$schema":"unused","name":"Game","tree":{"$className":"DataModel"}}"#,
		)
		.unwrap_err();
		assert!(error.to_string().contains("unsupported project field $schema"));
	}

	#[test]
	fn new_capture_service_identity_is_stable_and_ambiguity_blocks() {
		let root_id = stable_ref("capture-service-test-root");
		let mut canonical = Snapshot::new()
			.with_id(root_id)
			.with_name("Game")
			.with_class("DataModel");
		let class = "NonReplicatedCSGDictionaryService";
		let name = "NonReplicatedCSGDictionaryService";

		let first = capture_service_anchor(&canonical, class, name).unwrap();
		assert_eq!(first, capture_service_anchor(&canonical, class, name).unwrap());
		assert_ne!(first, root_id);

		canonical
			.children
			.push(Snapshot::new().with_id(first).with_name(name).with_class(class));
		assert_eq!(capture_service_anchor(&canonical, class, name).unwrap(), first);

		canonical.children.push(
			Snapshot::new()
				.with_id(stable_ref("capture-service-duplicate"))
				.with_name(name)
				.with_class(class),
		);
		let error = capture_service_anchor(&canonical, class, name).unwrap_err();
		assert!(error.to_string().contains("has no unique canonical anchor"));
	}

	#[test]
	fn init_creates_one_binary_artifact_and_templates() {
		let root = temp("init");
		fs::create_dir_all(&root).unwrap();
		let project = root.join("game.carbon.json");
		initialize(&project, "Game".to_owned()).unwrap();
		assert!(project.is_file());
		let data = root.join("game.carbon.data");
		let artifact = data.join("state.carbon");
		assert!(artifact.is_file());
		assert!(fs::read(&artifact).unwrap().starts_with(b"CARBONRB"));
		assert!(!data.join("manifest.json").exists());
		assert!(!data.join("instances").exists());
		assert!(root.join("src/ReplicatedStorage/Shared/Example.luau").is_file());
		assert!(root.join("src/ServerScriptService/Server.server.luau").is_file());
		assert!(root
			.join("src/StarterPlayer/StarterPlayerScripts/Client.client.luau")
			.is_file());
		let materialized = materialize(&project).unwrap();
		assert_eq!(materialized.mapped_roots.len(), 3);
		assert!(!materialized.mapped_refs.is_empty());
		fs::remove_dir_all(&root).unwrap();
	}

	#[test]
	fn unmarked_empty_directories_do_not_change_mapped_source() {
		let root = temp("unmarked-empty-directory");
		fs::create_dir_all(&root).unwrap();
		fs::write(root.join("Entry.luau"), "return {}\n").unwrap();

		let before = read_directory_instance(&root, "Root", Path::new("/not-data")).unwrap();
		fs::create_dir_all(root.join("Empty/Nested/Leaf")).unwrap();
		let after = read_directory_instance(&root, "Root", Path::new("/not-data")).unwrap();

		let child_names = |snapshot: &Snapshot| {
			snapshot
				.children
				.iter()
				.map(|child| child.name.clone())
				.collect::<Vec<_>>()
		};
		assert_eq!(child_names(&before), vec!["Entry".to_owned()]);
		assert_eq!(
			child_names(&after),
			child_names(&before),
			"an unmarked empty directory cannot be preserved by a fresh Git checkout"
		);

		fs::write(root.join("Empty/README.md"), "not mapped source\n").unwrap();
		let ignored_file = read_directory_instance(&root, "Root", Path::new("/not-data")).unwrap();
		assert_eq!(
			child_names(&ignored_file),
			child_names(&before),
			"an ignored file is not a mapped Folder marker"
		);
		write_json(&root.join("Empty/meta.json"), &serde_json::json!({})).unwrap();
		let marked = read_directory_instance(&root, "Root", Path::new("/not-data")).unwrap();
		assert_eq!(
			child_names(&marked),
			vec!["Empty".to_owned(), "Entry".to_owned()],
			"meta.json explicitly preserves an intentionally empty Folder"
		);
		assert!(
			marked
				.children
				.iter()
				.find(|child| child.name == "Empty")
				.unwrap()
				.children
				.is_empty(),
			"unmarked empty descendants remain inert"
		);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn mapped_directory_matches_rojo_for_pesde_source_forms() {
		let root = temp("pesde-source-forms");
		fs::create_dir_all(root.join("Library")).unwrap();
		fs::write(root.join("Dependency.lua"), "return \"legacy Lua\"\n").unwrap();
		fs::write(root.join("Server.server.lua"), "print(\"server\")\n").unwrap();
		fs::write(root.join("Client.client.lua"), "print(\"client\")\n").unwrap();
		fs::write(root.join("Library/init.lua"), "return {}\n").unwrap();
		fs::write(
			root.join("pesde.toml"),
			concat!(
				"name = \"fixture\"\n",
				"enabled = true\n",
				"targets = [\"client\", \"server\"]\n",
				"\n",
				"[dependencies]\n",
				"foo = \"1.2.3\"\n",
				"\"hyphen-key\" = 42\n",
			),
		)
		.unwrap();
		fs::write(root.join("README.md"), "# ignored by Rojo\n").unwrap();

		let snapshot = read_directory_instance(&root, "Root", Path::new("/not-data")).unwrap();
		assert_eq!(
			snapshot
				.children
				.iter()
				.map(|child| (child.name.as_str(), child.class.as_str()))
				.collect::<Vec<_>>(),
			vec![
				("Client", "LocalScript"),
				("Dependency", "ModuleScript"),
				("Library", "ModuleScript"),
				("Server", "Script"),
				("pesde", "ModuleScript"),
			]
		);
		let pesde = snapshot.children.iter().find(|child| child.name == "pesde").unwrap();
		assert_eq!(
			pesde.properties.get(&Ustr::from("Source")),
			Some(&Variant::String(
				concat!(
					"return {\n",
					"\tdependencies = {\n",
					"\t\tfoo = \"1.2.3\",\n",
					"\t\t[\"hyphen-key\"] = 42,\n",
					"\t},\n",
					"\tenabled = true,\n",
					"\tname = \"fixture\",\n",
					"\ttargets = {\"client\", \"server\"},\n",
					"}",
				)
				.to_owned()
			))
		);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn mapped_directory_ignores_pesde_type_declarations() {
		let root = temp("pesde-type-declarations");
		let package = root.join(".pesde/example_ui-labs@1.0.0/ui-labs");
		let package_source = package.join("src");
		fs::create_dir_all(&package_source).unwrap();
		fs::write(root.join("ui_labs.luau"), "return require(script.Parent[\".pesde\"])\n").unwrap();
		fs::write(package_source.join("init.luau"), "return {}\n").unwrap();
		fs::write(package_source.join("index.d.ts"), "export = {}\n").unwrap();
		fs::write(package.join("default.project.json"), "{\"tree\":{\"$path\":\"src\"}}\n").unwrap();
		fs::write(package_source.join("package.json"), "{}\n").unwrap();
		fs::write(package_source.join("LICENSE"), "fixture license\n").unwrap();
		fs::write(package_source.join("test-asset.png"), b"not a runtime instance").unwrap();

		let snapshot = read_directory_instance(&root, "Packages", Path::new("/not-data")).unwrap();
		assert_eq!(
			snapshot
				.children
				.iter()
				.map(|child| (child.name.as_str(), child.class.as_str()))
				.collect::<Vec<_>>(),
			vec![(".pesde", "Folder"), ("ui_labs", "ModuleScript")]
		);
		let package = &snapshot.children[0].children[0].children[0];
		assert_eq!(
			(package.name.as_str(), package.class.as_str()),
			("ui-labs", "ModuleScript")
		);
		assert!(package.children.is_empty());

		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn mapped_directory_honors_pesde_rojo_runtime_root() {
		let root = temp("pesde-rojo-runtime-root");
		let package = root.join(".pesde/elttob_fusion@0.3.0/fusion");
		let source = package.join("src");
		fs::create_dir_all(&source).unwrap();
		fs::create_dir_all(package.join("roblox_packages")).unwrap();
		fs::write(
			root.join("fusion.luau"),
			"return require(script.Parent[\".pesde\"][\"elttob_fusion@0.3.0\"].fusion)\n",
		)
		.unwrap();
		fs::write(source.join("init.luau"), "return { runtime = true }\n").unwrap();
		fs::write(source.join("Child.luau"), "return { child = true }\n").unwrap();
		fs::write(
			package.join("roblox_packages/Dependency.luau"),
			"return { dependency = true }\n",
		)
		.unwrap();
		fs::write(
			package.join("default.project.json"),
			serde_json::to_vec_pretty(&serde_json::json!({
				"name": "fusion",
				"tree": {
					"$path": "src",
					"roblox_packages": {
						"$path": { "optional": "roblox_packages" }
					},
					"roblox_server_packages": {
						"$path": { "optional": "roblox_server_packages" }
					}
				}
			}))
			.unwrap(),
		)
		.unwrap();

		let snapshot = read_directory_instance(&root, "Packages", Path::new("/not-data")).unwrap();
		let pesde = snapshot.children.iter().find(|child| child.name == ".pesde").unwrap();
		let version = pesde
			.children
			.iter()
			.find(|child| child.name == "elttob_fusion@0.3.0")
			.unwrap();
		let fusion = version.children.iter().find(|child| child.name == "fusion").unwrap();

		assert_eq!(fusion.class, "ModuleScript");
		assert_eq!(
			fusion.properties.get(&Ustr::from("Source")),
			Some(&Variant::String("return { runtime = true }\n".to_owned()))
		);
		assert_eq!(
			fusion
				.children
				.iter()
				.map(|child| (child.name.as_str(), child.class.as_str()))
				.collect::<Vec<_>>(),
			vec![("Child", "ModuleScript"), ("roblox_packages", "Folder")]
		);
		assert!(fusion
			.children
			.iter()
			.all(|child| child.name != "roblox_server_packages"));

		let mut paths = HashMap::new();
		collect_directory_script_paths(
			&root,
			Path::new("roblox_packages"),
			&mut Vec::new(),
			&mut paths,
			&mut MappedTraversal::default(),
			&mut Vec::new(),
		)
		.unwrap();
		let fusion_route = vec![
			".pesde".to_owned(),
			"elttob_fusion@0.3.0".to_owned(),
			"fusion".to_owned(),
		];
		assert_eq!(
			paths.get(&fusion_route),
			Some(&PathBuf::from(
				"roblox_packages/.pesde/elttob_fusion@0.3.0/fusion/src/init.luau"
			))
		);
		assert!(!paths.contains_key(&[fusion_route, vec!["src".to_owned()],].concat()));

		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn mapped_directory_honors_generated_pesde_project_children() {
		let root = temp("pesde-generated-project-children");
		let package = root.join(".pesde/acme_library@1.0.0/library");
		fs::create_dir_all(package.join("src")).unwrap();
		fs::write(package.join("src/Library.luau"), "return { library = true }\n").unwrap();
		fs::write(package.join("src/TestOnly.luau"), "error(\"not selected\")\n").unwrap();
		fs::write(
			package.join("default.project.json"),
			serde_json::to_vec_pretty(&serde_json::json!({
				"name": "library",
				"tree": {
					"$className": "Folder",
					"Library": { "$path": "src/Library.luau" },
					"roblox_packages": {
						"$path": { "optional": "roblox_packages" }
					}
				}
			}))
			.unwrap(),
		)
		.unwrap();

		let snapshot = read_directory_instance(&root, "Packages", Path::new("/not-data")).unwrap();
		let package = &snapshot.children[0].children[0].children[0];
		assert_eq!((package.name.as_str(), package.class.as_str()), ("library", "Folder"));
		assert_eq!(
			package
				.children
				.iter()
				.map(|child| (child.name.as_str(), child.class.as_str()))
				.collect::<Vec<_>>(),
			vec![("Library", "ModuleScript")]
		);
		let mut locations = Vec::new();
		find_path_identity_locations(&root, package.id, None, None, &mut locations).unwrap();
		assert!(matches!(locations.as_slice(), [IdentityLocation::StableLinked]));

		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn mapped_directory_still_rejects_adjacent_script_metadata() {
		let root = temp("adjacent-script-metadata");
		fs::create_dir_all(&root).unwrap();
		fs::write(root.join("Module.luau"), "return {}\n").unwrap();
		fs::write(root.join("Module.meta.json"), "{}\n").unwrap();

		let error = read_directory_instance(&root, "Root", Path::new("/not-data"))
			.unwrap_err()
			.to_string();
		assert!(error.contains("adjacent per-script metadata is unsupported"));

		fs::remove_dir_all(root).unwrap();
	}

	#[cfg(unix)]
	#[test]
	fn mapped_directory_follows_pesde_path_dependency_symlinks() {
		use std::os::unix::fs::symlink;

		let root = temp("pesde-symlinks");
		let dependency = temp("pesde-path-dependency");
		fs::create_dir_all(&root).unwrap();
		fs::create_dir_all(&dependency).unwrap();
		fs::write(root.join("Runtime.luau"), "return {}\n").unwrap();
		fs::write(dependency.join("index.d.ts"), "export = {}\n").unwrap();
		fs::write(dependency.join("init.luau"), "return {}\n").unwrap();
		fs::write(dependency.join("Child.luau"), "return {}\n").unwrap();
		fs::write(dependency.join("Linked.luau"), "return {}\n").unwrap();
		symlink(dependency.join("index.d.ts"), root.join("index.d.ts")).unwrap();
		symlink(dependency.join("Linked.luau"), root.join("linked_file.luau")).unwrap();
		symlink(&dependency, root.join("path_dependency")).unwrap();
		symlink(&dependency, root.join("path_dependency_alias")).unwrap();

		let mut traversal = MappedTraversal::default();
		let snapshot =
			read_directory_instance_tracked(&root, "Packages", Path::new("/not-data"), &mut traversal).unwrap();
		assert_eq!(
			snapshot
				.children
				.iter()
				.map(|child| (child.name.as_str(), child.class.as_str()))
				.collect::<Vec<_>>(),
			vec![
				("Runtime", "ModuleScript"),
				("linked_file", "ModuleScript"),
				("path_dependency", "ModuleScript"),
				("path_dependency_alias", "ModuleScript"),
			]
		);
		assert_eq!(
			snapshot.children[2]
				.children
				.iter()
				.map(|child| (child.name.as_str(), child.class.as_str()))
				.collect::<Vec<_>>(),
			vec![("Child", "ModuleScript"), ("Linked", "ModuleScript")]
		);
		assert_ne!(snapshot.children[2].id, snapshot.children[3].id);
		assert_eq!(
			traversal.watch_roots,
			BTreeSet::from([fs::canonicalize(&dependency).unwrap()])
		);

		let mut paths = HashMap::new();
		collect_directory_script_paths(
			&root,
			Path::new("packages"),
			&mut Vec::new(),
			&mut paths,
			&mut MappedTraversal::default(),
			&mut Vec::new(),
		)
		.unwrap();
		assert_eq!(
			paths.get(&vec!["path_dependency".to_owned()]),
			Some(&PathBuf::from("packages/path_dependency/init.luau"))
		);
		assert_eq!(
			paths.get(&vec!["linked_file".to_owned()]),
			Some(&PathBuf::from("packages/linked_file.luau"))
		);

		fs::remove_dir_all(root).unwrap();
		fs::remove_dir_all(dependency).unwrap();
	}

	#[cfg(unix)]
	#[test]
	fn mapped_directory_reports_broken_pesde_symlink_targets() {
		use std::os::unix::fs::symlink;

		let root = temp("mapped-broken-symlink");
		fs::create_dir_all(&root).unwrap();
		symlink(root.join("missing.luau"), root.join("Broken.luau")).unwrap();

		let error = read_directory_instance(&root, "Root", Path::new("/not-data"))
			.unwrap_err()
			.to_string();
		assert!(error.contains("mapped symlink target is unavailable"));

		fs::remove_dir_all(root).unwrap();
	}

	#[cfg(unix)]
	#[test]
	fn mapped_directory_rejects_symlink_cycles() {
		use std::os::unix::fs::symlink;

		let root = temp("mapped-symlink-cycle");
		fs::create_dir_all(&root).unwrap();
		symlink(&root, root.join("cycle")).unwrap();

		let error = read_directory_instance(&root, "Root", Path::new("/not-data"))
			.unwrap_err()
			.to_string();
		assert!(error.contains("mapped source symlink cycle"));

		fs::remove_dir_all(root).unwrap();
	}

	#[cfg(unix)]
	#[test]
	fn mapped_pesde_dependencies_keep_capture_identity_persistence_read_only() {
		use std::os::unix::fs::symlink;

		let root = temp("mapped-symlink-capture");
		let dependency = temp("mapped-symlink-capture-dependency");
		fs::create_dir_all(&root).unwrap();
		fs::create_dir_all(dependency.join("src")).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		fs::write(dependency.join("src/init.luau"), "return {}\n").unwrap();
		fs::write(
			dependency.join("default.project.json"),
			"{\"tree\":{\"$path\":\"src\"}}\n",
		)
		.unwrap();
		let packages = root.join("roblox_packages");
		fs::create_dir_all(&packages).unwrap();
		symlink(&dependency, packages.join("path_dependency")).unwrap();
		write_json(
			&project_path,
			&serde_json::json!({
				"name": "Game",
				"tree": {
					"$className": "DataModel",
					"ReplicatedStorage": {
						"Packages": { "$path": "roblox_packages" }
					}
				}
			}),
		)
		.unwrap();

		let materialized = materialize_for_capture(&project_path).unwrap();
		assert_eq!(
			materialized.mapped_watch_roots,
			vec![fs::canonicalize(&dependency).unwrap()]
		);
		let target = materialized
			.snapshot
			.children
			.iter()
			.find(|child| child.name == "ReplicatedStorage")
			.unwrap()
			.children
			.iter()
			.find(|child| child.name == "Packages")
			.unwrap()
			.children
			.first()
			.unwrap();
		assert_eq!(target.class, "ModuleScript");
		assert_eq!(
			target.properties.get(&Ustr::from("Source")),
			Some(&Variant::String("return {}\n".to_owned()))
		);
		let target = target.id;
		let policy = live_policy(&project_path, &materialized);
		let identities = stage_mapped_identities(&policy, &HashSet::from([target]), &|| false).unwrap();
		assert!(identities.domains.is_empty());
		assert!(!dependency.join("meta.json").exists());
		assert!(!dependency.join("src/meta.json").exists());

		drop(identities);
		fs::remove_dir_all(materialized.directory).unwrap();
		fs::remove_dir_all(root).unwrap();
		fs::remove_dir_all(dependency).unwrap();
	}

	#[test]
	fn strict_project_rejects_unknown_fields_and_lua() {
		let root = temp("strict");
		fs::create_dir_all(root.join("src")).unwrap();
		fs::write(root.join("src/Bad.lua"), "return nil").unwrap();
		fs::write(
			root.join("game.carbon.json"),
			r#"{"name":"Game","tree":{"$className":"DataModel","ReplicatedStorage":{"Bad":{"$path":"src/Bad.lua","$ignoreUnknownInstances":true}}}}"#,
		)
		.unwrap();
		assert!(load_project(&root.join("game.carbon.json"))
			.unwrap_err()
			.to_string()
			.contains("unsupported project node field"));
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn portable_collisions_are_rejected() {
		let root = temp("collision");
		fs::create_dir_all(&root).unwrap();
		fs::write(root.join("Thing.luau"), "return 1").unwrap();
		fs::write(root.join("thing.server.luau"), "").unwrap();
		let error = read_directory_instance(&root, "Root", Path::new("/not-data"))
			.unwrap_err()
			.to_string();
		assert!(error.contains("collision"));
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn snapshot_diff_is_sparse_and_ignores_explicit_reflection_defaults() {
		let id = Ref::new();
		let old = Snapshot::new()
			.with_id(id)
			.with_name("Folder")
			.with_class("Folder")
			.with_properties(UstrMap::from_iter([(Ustr::from("Archivable"), Variant::Bool(true))]));
		let new = Snapshot::new()
			.with_id(id)
			.with_name("Folder")
			.with_class("Folder")
			.with_properties(UstrMap::from_iter([
				(Ustr::from("Archivable"), Variant::Bool(false)),
				(Ustr::from("Attributes"), Variant::Attributes(Attributes::new())),
			]));

		let changes = diff_snapshots(&old, &new).unwrap();
		assert_eq!(changes.updates.len(), 1);
		let update = &changes.updates[0];
		assert_eq!(
			update.properties,
			Some(UstrMap::from_iter([(Ustr::from("Archivable"), Variant::Bool(false),)]))
		);
		assert!(update.removed_properties.is_empty());
	}

	#[test]
	fn projected_realization_ignores_engine_owned_state_removed_from_mapped_scripts() {
		let id = Ref::new();
		let old = Snapshot::new()
			.with_id(id)
			.with_name("Script")
			.with_class("ModuleScript")
			.with_properties(UstrMap::from_iter([
				(Ustr::from("Capabilities"), Variant::String("engine".to_owned())),
				(Ustr::from("LinkedSource"), Variant::String("asset".to_owned())),
				(Ustr::from("Sandboxed"), Variant::Bool(true)),
				(Ustr::from("SourceAssetId"), Variant::Int64(42)),
			]));
		let new = Snapshot::new()
			.with_id(id)
			.with_name("Script")
			.with_class("ModuleScript");
		let mut changes = diff_snapshots(&old, &new).unwrap();

		assert_eq!(changes.updates.len(), 1);
		assert!(!projected_realization_changes_pending(&changes, &HashSet::from([id])));
		assert!(projected_realization_changes_pending(&changes, &HashSet::new()));

		changes.updates[0].removed_properties.push(Ustr::from("Archivable"));
		assert!(projected_realization_changes_pending(&changes, &HashSet::from([id])));
	}

	#[test]
	fn snapshot_diff_treats_utf8_attribute_wire_bytes_as_the_same_string() {
		let id = Ref::new();
		let properties = |value| {
			let mut attributes = Attributes::new();
			attributes.insert("Label".to_owned(), value);
			UstrMap::from_iter([(Ustr::from("Attributes"), Variant::Attributes(attributes))])
		};
		let snapshot = |properties| {
			Snapshot::new()
				.with_id(id)
				.with_name("Folder")
				.with_class("Folder")
				.with_properties(properties)
		};
		let binary = snapshot(properties(Variant::BinaryString(b"value".to_vec().into())));
		let text = snapshot(properties(Variant::String("value".to_owned())));

		assert!(diff_snapshots(&binary, &text).unwrap().is_empty());
	}

	#[test]
	fn snapshot_diff_ignores_studio_cframe_rotation_rounding() {
		use rbx_dom_weak::types::{CFrame, Matrix3, Vector3};

		let id = Ref::new();
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
		let snapshot = |value| {
			Snapshot::new()
				.with_id(id)
				.with_name("RightWrist")
				.with_class("AnimationConstraint")
				.with_properties(UstrMap::from_iter([(Ustr::from("Transform"), Variant::CFrame(value))]))
		};
		let original = cframe([
			[0.86990005, -0.19090259, 0.45478573],
			[0.26061186, 0.96073776, -0.09520735],
			[-0.4187545, 0.20134343, 0.88549733],
		]);
		let first_studio_roundtrip = cframe([
			[0.86990005, -0.1909026, 0.4547858],
			[0.2606119, 0.9607377, -0.095207356],
			[-0.41875455, 0.20134348, 0.8854973],
		]);
		let second_studio_roundtrip = cframe([
			[0.86990005, -0.19090259, 0.45478582],
			[0.2606119, 0.96073776, -0.095207356],
			[-0.41875458, 0.20134348, 0.8854973],
		]);

		assert!(diff_snapshots(&snapshot(original), &snapshot(first_studio_roundtrip))
			.unwrap()
			.is_empty());
		assert!(
			diff_snapshots(&snapshot(first_studio_roundtrip), &snapshot(second_studio_roundtrip))
				.unwrap()
				.is_empty()
		);

		let authored_rotation = CFrame::new(position, Matrix3::identity());
		assert_eq!(
			diff_snapshots(&snapshot(original), &snapshot(authored_rotation))
				.unwrap()
				.updates
				.len(),
			1
		);
	}

	#[test]
	fn live_path_replacement_does_not_echo_unrelated_studio_properties() {
		let root = temp("live-path-replacement");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		fs::write(
			root.join("src/ServerScriptService/Alternate.server.luau"),
			"print('alternate')\n",
		)
		.unwrap();
		let materialized = materialize_for_capture(&project_path).unwrap();
		let (mut baseline, _) = reevaluate(&project_path).unwrap();
		for service in &mut baseline.children {
			if matches!(service.class.as_str(), "Workspace" | "ServerStorage") {
				service.properties.remove(&Ustr::from("Attributes"));
			}
		}
		let mut value: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
		value["tree"]["ServerScriptService"]["Server"]["$path"] =
			Value::String("src/ServerScriptService/Alternate.server.luau".to_owned());
		write_json(&project_path, &value).unwrap();
		let (replacement, _) = reevaluate(&project_path).unwrap();

		let changes = diff_snapshots(&baseline, &replacement).unwrap();
		assert_eq!(changes.additions.len(), 1);
		assert_eq!(changes.removals.len(), 1);
		assert!(
			changes.updates.is_empty(),
			"a path-only mapping replacement echoed unrelated Studio properties: {:?}",
			changes.updates
		);

		fs::remove_dir_all(materialized.directory).unwrap();
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn projected_watcher_edit_never_materializes_or_touches_opaque_siblings() {
		fn find(snapshot: &Snapshot, id: Ref) -> Option<&Snapshot> {
			if snapshot.id == id {
				return Some(snapshot);
			}
			snapshot.children.iter().find_map(|child| find(child, id))
		}
		fn collect_addition_ids(snapshot: &Snapshot, ids: &mut HashSet<Ref>) {
			ids.insert(snapshot.id);
			for child in &snapshot.children {
				collect_addition_ids(child, ids);
			}
		}

		let root = temp("projected-watcher-opaque-sibling");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		let mut studio = empty_studio_snapshot();
		let opaque = stable_ref("projected-watcher:opaque-studio-sibling");
		studio
			.children
			.iter_mut()
			.find(|child| child.name == "ServerScriptService")
			.unwrap()
			.children
			.push(
				Snapshot::new()
					.with_id(opaque)
					.with_name("OpaqueStudioFolder")
					.with_class("Folder")
					.with_properties(UstrMap::from_iter([(Ustr::from("Archivable"), Variant::Bool(false))])),
			);
		install_data_store(&project_path, &studio, "Game").unwrap();

		let materialized = materialize_for_capture(&project_path).unwrap();
		let policy = live_policy(&project_path, &materialized);
		let projected =
			artifact_store::load_projected_live(&materialized.manifest_path, &policy.mapped_refs, &policy.routing_refs)
				.unwrap();
		let previous = snapshot_from_tree(&projected.tree).unwrap();
		assert!(
			find(&previous, opaque).is_none(),
			"opaque sibling leaked into projection"
		);
		let server_id = *policy
			.mapped_refs
			.iter()
			.find(|id| find(&previous, **id).is_some_and(|node| node.name == "Server"))
			.unwrap();
		let before_server = find(&previous, server_id).unwrap();
		assert!(before_server.properties.contains_key(&Ustr::from("Source")));

		fs::write(
			root.join("src/ServerScriptService/Server.server.luau"),
			"print('projected watcher edit')\n",
		)
		.unwrap();
		let frozen = frozen_project_document(&project_path).unwrap();
		let (candidate, routes) =
			reevaluate_projected_frozen(&project_path, &frozen, &previous, &policy.mapped_refs).unwrap();
		assert!(
			find(&candidate, opaque).is_none(),
			"opaque sibling was materialized by reevaluation"
		);
		let candidate_tree = Tree::new(candidate.clone());
		let (candidate_mapped, _) = refs_for_routes(&candidate_tree, &routes).unwrap();
		let after_server = find(&candidate, server_id).unwrap();
		assert_eq!(
			after_server.properties.get(&Ustr::from("Source")),
			Some(&Variant::String("print('projected watcher edit')\n".to_owned())),
			"mapped source properties were reduced to hierarchy-only data"
		);
		for (name, value) in &before_server.properties {
			if name.as_str() != "Source" {
				assert!(mapped_property_equal(
					after_server.class.as_str(),
					name.as_str(),
					Some(value),
					after_server.properties.get(name),
				));
			}
		}

		let changes = diff_snapshots(&previous, &candidate).unwrap();
		let allowed = policy
			.mapped_refs
			.union(&candidate_mapped)
			.copied()
			.collect::<HashSet<_>>();
		let mut changed = changes.updates.iter().map(|update| update.id).collect::<HashSet<_>>();
		changed.extend(changes.removals.iter().copied());
		for addition in &changes.additions {
			changed.insert(addition.id);
			for child in &addition.children {
				collect_addition_ids(child, &mut changed);
			}
		}
		assert!(!changed.contains(&opaque));
		assert!(changed.iter().all(|id| allowed.contains(id)));

		fs::remove_dir_all(materialized.directory).unwrap();
		fs::remove_dir_all(root).unwrap();
	}

	fn capture_promotion_fixture(label: &str) -> (PathBuf, PathBuf, LivePolicy, Snapshot) {
		let project_root = temp(&format!("{label}-project"));
		let composite_root = temp(&format!("{label}-composite"));
		fs::create_dir_all(&project_root).unwrap();
		fs::create_dir_all(&composite_root).unwrap();
		let project_path = project_root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		let materialized = materialize_for_capture(&project_path).unwrap();
		let snapshot = materialized.snapshot.clone();
		let mut policy = live_policy(&project_path, &materialized);
		let composite = composite_root.join("composite.carbon.json");
		artifact_store::extract_snapshot(snapshot.clone(), "Game".to_owned(), &composite).unwrap();
		policy.composite_manifest = composite;
		persist_studio_domain(&policy).unwrap();
		fs::remove_dir_all(materialized.directory).unwrap();
		(project_root, composite_root, policy, snapshot)
	}

	fn zero_mapping_capture_promotion_fixture(label: &str) -> (PathBuf, PathBuf, LivePolicy, Snapshot) {
		let project_root = temp(&format!("{label}-project"));
		let composite_root = temp(&format!("{label}-composite"));
		fs::create_dir_all(&project_root).unwrap();
		fs::create_dir_all(&composite_root).unwrap();
		let project_path = project_root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		write_json(
			&project_path,
			&serde_json::json!({
				"name": "Game",
				"tree": { "$className": "DataModel" }
			}),
		)
		.unwrap();
		let materialized = materialize_for_capture(&project_path).unwrap();
		let snapshot = materialized.snapshot.clone();
		let mut policy = live_policy(&project_path, &materialized);
		assert!(policy.mapped_refs.is_empty());
		let composite = composite_root.join("composite.carbon.json");
		artifact_store::extract_snapshot(snapshot.clone(), "Game".to_owned(), &composite).unwrap();
		policy.composite_manifest = composite;
		persist_studio_domain(&policy).unwrap();
		fs::remove_dir_all(materialized.directory).unwrap();
		(project_root, composite_root, policy, snapshot)
	}

	#[test]
	fn capture_atomically_appends_a_mapped_directory_identity_to_existing_meta() {
		let root = temp("capture-directory-identity");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		let meta_path = root.join("src/ReplicatedStorage/Shared/meta.json");
		write_json(
			&meta_path,
			&serde_json::json!({"attributes": {"IdentityRegression": "preserved"}}),
		)
		.unwrap();
		let materialized = materialize_for_capture(&project_path).unwrap();
		let target = materialized
			.snapshot
			.children
			.iter()
			.find(|child| child.name == "ReplicatedStorage")
			.unwrap()
			.children
			.iter()
			.find(|child| child.name == "Shared")
			.unwrap()
			.id;
		let policy = live_policy(&project_path, &materialized);
		let composite = artifact_store::stage_snapshot_capture(
			&materialized.snapshot,
			"Game".to_owned(),
			&policy.mapped_refs,
			&materialized.manifest_path,
			&|| false,
		)
		.unwrap();
		let studio = stage_captured_studio_domain(&policy, &composite, &|| false).unwrap();
		let identities = stage_mapped_identities(&policy, &HashSet::from([target]), &|| false).unwrap();
		assert!(serde_json::from_slice::<Value>(&fs::read(&meta_path).unwrap()).unwrap()["id"].is_null());
		let promotion = prepare_capture_promotion_with_identities(composite, studio, identities).unwrap();
		drop(promote_capture_domains(&promotion).unwrap());
		drop(promotion);

		let meta: Value = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
		assert_eq!(meta["id"], target.to_string());
		assert_eq!(meta["attributes"]["IdentityRegression"], "preserved");
		let rematerialized = materialize(&project_path).unwrap();
		assert!(rematerialized.mapped_refs.contains(&target));
		fs::remove_dir_all(rematerialized.directory).unwrap();
		fs::remove_dir_all(materialized.directory).unwrap();
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn capture_promotes_a_referenced_script_and_missing_id_fails_future_builds() {
		let root = temp("capture-script-identity");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		let materialized = materialize_for_capture(&project_path).unwrap();
		let target = materialized
			.snapshot
			.children
			.iter()
			.find(|child| child.name == "ServerScriptService")
			.unwrap()
			.children
			.iter()
			.find(|child| child.name == "Server")
			.unwrap()
			.id;
		let policy = live_policy(&project_path, &materialized);
		let mut loaded = artifact_store::load_live(&materialized.manifest_path).unwrap();
		let workspace = loaded
			.tree
			.root()
			.children()
			.iter()
			.copied()
			.find(|id| loaded.tree.get_instance(*id).unwrap().class.as_str() == "Workspace")
			.unwrap();
		let holder = Ref::new();
		let mut changes = Changes::new();
		changes.add(
			Snapshot::new()
				.with_id(holder)
				.with_name("MappedScriptReference")
				.with_class("ObjectValue")
				.with_properties(UstrMap::from_iter([(Ustr::from("Value"), Variant::Ref(target))])),
			workspace,
		);
		loaded.store.apply(&mut loaded.tree, changes).unwrap();
		let composite = artifact_store::stage_compiled_noop(&materialized.manifest_path).unwrap();
		let studio = stage_captured_studio_domain(&policy, &composite, &|| false).unwrap();
		let identities = stage_mapped_identities(&policy, &HashSet::from([target]), &|| false).unwrap();
		let promotion = prepare_capture_promotion_with_identities(composite, studio, identities).unwrap();
		let failure_after_last_staged_domain = promotion.domains.len() - 1;
		let error = promote_capture_domains_impl(&promotion, Some(failure_after_last_staged_domain)).unwrap_err();
		assert!(error.to_string().contains("synchronous roll-forward completed"));
		drop(promotion);

		let old_path = root.join("src/ServerScriptService/Server.server.luau");
		let promoted = root.join("src/ServerScriptService/Server");
		assert!(!old_path.exists());
		assert!(promoted.join("init.server.luau").is_file());
		let meta: Value = serde_json::from_slice(&fs::read(promoted.join("meta.json")).unwrap()).unwrap();
		assert_eq!(meta["id"], target.to_string());
		let project: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
		assert_eq!(
			project["tree"]["ServerScriptService"]["Server"]["$path"],
			"src/ServerScriptService/Server"
		);
		let built = materialize(&project_path).unwrap();
		fs::remove_dir_all(built.directory).unwrap();

		write_json(&promoted.join("meta.json"), &serde_json::json!({})).unwrap();
		let error = materialize(&project_path).unwrap_err().to_string();
		assert!(error.contains(&target.to_string()));
		assert!(error.contains("modify the relevant manifest reference"));
		fs::remove_dir_all(materialized.directory).unwrap();
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn prepared_noop_capture_skips_all_promotions_and_preserves_domain_mtimes() {
		let (project_root, composite_root, policy, snapshot) = capture_promotion_fixture("capture-noop");
		let studio_manifest = data_dir(&policy.project_path).unwrap().join(DATA_ARTIFACT);
		let composite_bytes = fs::read(&policy.composite_manifest).unwrap();
		let studio_bytes = fs::read(&studio_manifest).unwrap();
		let composite_mtime = fs::metadata(&policy.composite_manifest).unwrap().modified().unwrap();
		let studio_mtime = fs::metadata(&studio_manifest).unwrap().modified().unwrap();

		let composite = artifact_store::stage_snapshot_capture(
			&snapshot,
			"Game".to_owned(),
			&policy.mapped_refs,
			&policy.composite_manifest,
			&|| false,
		)
		.unwrap();
		let studio = stage_captured_studio_domain(&policy, &composite, &|| false).unwrap();
		let promotion = prepare_capture_promotion(composite, studio).unwrap();
		assert!(
			promotion.domains.is_empty(),
			"exact no-op unexpectedly planned promotion"
		);
		drop(promote_capture_domains(&promotion).unwrap());
		drop(promotion);

		assert_eq!(fs::read(&policy.composite_manifest).unwrap(), composite_bytes);
		assert_eq!(fs::read(&studio_manifest).unwrap(), studio_bytes);
		assert_eq!(
			fs::metadata(&policy.composite_manifest).unwrap().modified().unwrap(),
			composite_mtime
		);
		assert_eq!(
			fs::metadata(&studio_manifest).unwrap().modified().unwrap(),
			studio_mtime
		);
		fs::remove_dir_all(project_root).unwrap();
		fs::remove_dir_all(composite_root).unwrap();
	}

	#[test]
	fn zero_mapping_noop_compares_only_canonical_manifests_and_preserves_mtimes() {
		let (project_root, composite_root, policy, snapshot) =
			zero_mapping_capture_promotion_fixture("capture-unfiltered-noop");
		let studio_manifest = data_dir(&policy.project_path).unwrap().join(DATA_ARTIFACT);
		let composite_mtime = fs::metadata(&policy.composite_manifest).unwrap().modified().unwrap();
		let studio_mtime = fs::metadata(&studio_manifest).unwrap().modified().unwrap();

		let composite = artifact_store::stage_snapshot_capture(
			&snapshot,
			"Game".to_owned(),
			&policy.mapped_refs,
			&policy.composite_manifest,
			&|| false,
		)
		.unwrap();
		let studio = stage_captured_studio_domain(&policy, &composite, &|| false).unwrap();
		assert!(studio.staged().is_none());
		let promotion = prepare_capture_promotion(composite, studio).unwrap();
		assert!(promotion.domains.is_empty());
		drop(promote_capture_domains(&promotion).unwrap());
		drop(promotion);

		assert_eq!(
			fs::metadata(&policy.composite_manifest).unwrap().modified().unwrap(),
			composite_mtime
		);
		assert_eq!(
			fs::metadata(&studio_manifest).unwrap().modified().unwrap(),
			studio_mtime
		);
		fs::remove_dir_all(project_root).unwrap();
		fs::remove_dir_all(composite_root).unwrap();
	}

	#[test]
	fn zero_mapping_capture_hard_links_the_staged_artifact() {
		let (project_root, composite_root, policy, mut snapshot) =
			zero_mapping_capture_promotion_fixture("capture-unfiltered");
		snapshot
			.children
			.iter_mut()
			.find(|child| child.name == "Workspace")
			.unwrap()
			.properties
			.insert(Ustr::from("Archivable"), Variant::Bool(false));
		let composite = artifact_store::stage_snapshot_capture(
			&snapshot,
			"Game".to_owned(),
			&policy.mapped_refs,
			&policy.composite_manifest,
			&|| false,
		)
		.unwrap();
		let source_artifact = composite.artifact().to_owned();
		let studio = stage_captured_studio_domain(&policy, &composite, &|| false).unwrap();
		let studio_data = studio.staged().expect("changed Studio domain was not staged");
		let staged_artifact = studio_data.join(DATA_ARTIFACT);
		assert_eq!(fs::read(&staged_artifact).unwrap(), fs::read(&source_artifact).unwrap());
		#[cfg(unix)]
		{
			use std::os::unix::fs::MetadataExt;
			let source = fs::metadata(&source_artifact).unwrap();
			let staged = fs::metadata(&staged_artifact).unwrap();
			assert_eq!((source.dev(), source.ino()), (staged.dev(), staged.ino()));
		}

		drop(studio);
		drop(composite);
		fs::remove_dir_all(project_root).unwrap();
		fs::remove_dir_all(composite_root).unwrap();
	}

	#[test]
	fn artifact_script_capture_preserves_nested_hierarchy_and_content_addressed_source() {
		let (project_root, composite_root, policy, mut snapshot) = capture_promotion_fixture("capture-manifest-script");
		let workspace = snapshot
			.children
			.iter_mut()
			.find(|child| child.class.as_str() == "Workspace")
			.unwrap();
		let script_source = format!("-- captured from Studio\nreturn {:?}\n", "x".repeat(20 * 1024));
		workspace.children.push(
			Snapshot::new()
				.with_id(Ref::new())
				.with_name("ManifestParent")
				.with_class("Part")
				.with_children(vec![Snapshot::new()
					.with_id(Ref::new())
					.with_name("NestedScript")
					.with_class("ModuleScript")
					.with_properties(UstrMap::from_iter([(
						Ustr::from("Source"),
						Variant::String(script_source.clone()),
					)]))]),
		);

		let composite = artifact_store::stage_snapshot_capture(
			&snapshot,
			"Game".to_owned(),
			&policy.mapped_refs,
			&policy.composite_manifest,
			&|| false,
		)
		.unwrap();
		let studio = stage_captured_studio_domain(&policy, &composite, &|| false).unwrap();
		let promotion = prepare_capture_promotion(composite, studio).unwrap();
		drop(promote_capture_domains(&promotion).unwrap());
		drop(promotion);

		let studio_manifest = data_dir(&policy.project_path).unwrap().join(DATA_ARTIFACT);
		let loaded = artifact_store::load_tree(&studio_manifest).unwrap();
		let workspace = loaded
			.tree
			.root()
			.children()
			.iter()
			.map(|id| loaded.tree.get_instance(*id).unwrap())
			.find(|instance| instance.class.as_str() == "Workspace")
			.unwrap();
		let parent = loaded
			.tree
			.get_instance(
				*workspace
					.children()
					.iter()
					.find(|id| loaded.tree.get_instance(**id).unwrap().name == "ManifestParent")
					.unwrap(),
			)
			.unwrap();
		let script = loaded.tree.get_instance(parent.children()[0]).unwrap();
		assert_eq!(script.class.as_str(), "ModuleScript");
		assert_eq!(
			script.properties.get(&Ustr::from("Source")),
			Some(&Variant::String(script_source.to_owned()))
		);

		let studio_data = studio_manifest.parent().unwrap();
		let blobs = studio_data.join("blobs");
		let blob = fs::read_dir(&blobs).unwrap().next().unwrap().unwrap().path();
		assert_eq!(blob.extension().and_then(|value| value.to_str()), Some("zst"));
		assert!(fs::read_dir(&blobs).unwrap().nth(1).is_none());
		let mut stored_source = String::new();
		zstd::Decoder::new(BufReader::new(File::open(blob).unwrap()))
			.unwrap()
			.read_to_string(&mut stored_source)
			.unwrap();
		assert_eq!(stored_source, script_source);
		let rematerialized = materialize(&policy.project_path).unwrap();
		let rebuilt_script = rematerialized
			.snapshot
			.children
			.iter()
			.flat_map(|service| &service.children)
			.flat_map(|instance| &instance.children)
			.find(|instance| instance.name == "NestedScript")
			.unwrap();
		assert_eq!(
			rebuilt_script.properties.get(&Ustr::from("Source")),
			Some(&Variant::String(script_source.clone()))
		);
		let fixed_point = artifact_store::stage_snapshot_capture(
			&rematerialized.snapshot,
			"Game".to_owned(),
			&policy.mapped_refs,
			&policy.composite_manifest,
			&|| false,
		)
		.unwrap();
		let fixed_studio = stage_captured_studio_domain(&policy, &fixed_point, &|| false).unwrap();
		let fixed_promotion = prepare_capture_promotion(fixed_point, fixed_studio).unwrap();
		assert!(
			fixed_promotion.domains.is_empty(),
			"unchanged manifest script capture did not reach a fixed point"
		);
		drop(fixed_promotion);
		fs::remove_dir_all(rematerialized.directory).unwrap();

		let output = project_root.join("rebuilt.rbxl");
		compile(&policy.project_path, &output, None).unwrap();
		let rebuilt = rbx_binary::from_reader_with_database(
			BufReader::new(File::open(&output).unwrap()),
			crate::util::get_reflection_database(),
		)
		.unwrap();
		let rebuilt_script = rebuilt
			.descendants()
			.find(|instance| instance.name == "NestedScript")
			.unwrap();
		assert_eq!(
			rebuilt_script.properties.get(&Ustr::from("Source")),
			Some(&Variant::String(script_source))
		);

		fs::remove_dir_all(project_root).unwrap();
		fs::remove_dir_all(composite_root).unwrap();
	}

	#[test]
	fn cancelled_zero_mapping_clone_removes_partial_studio_stage() {
		let (project_root, composite_root, policy, mut snapshot) =
			zero_mapping_capture_promotion_fixture("capture-unfiltered-cancel");
		snapshot
			.children
			.iter_mut()
			.find(|child| child.name == "Workspace")
			.unwrap()
			.properties
			.insert(Ustr::from("Archivable"), Variant::Bool(false));
		let composite = artifact_store::stage_snapshot_capture(
			&snapshot,
			"Game".to_owned(),
			&policy.mapped_refs,
			&policy.composite_manifest,
			&|| false,
		)
		.unwrap();
		let error = stage_captured_studio_domain(&policy, &composite, &|| true).unwrap_err();
		assert!(error.to_string().contains("cancelled"));
		assert!(!fs::read_dir(&project_root)
			.unwrap()
			.filter_map(|entry| entry.ok())
			.any(|entry| entry.file_name().to_string_lossy().starts_with(".carbon-stage-")));

		drop(composite);
		fs::remove_dir_all(project_root).unwrap();
		fs::remove_dir_all(composite_root).unwrap();
	}

	#[test]
	fn partial_capture_promotion_rolls_forward_every_domain_before_stage_cleanup() {
		for fail_after in 0..2 {
			let (project_root, composite_root, policy, mut snapshot) =
				capture_promotion_fixture(&format!("capture-roll-forward-{fail_after}"));
			snapshot
				.children
				.iter_mut()
				.find(|child| child.name == "Workspace")
				.unwrap()
				.properties
				.insert(Ustr::from("Archivable"), Variant::Bool(false));
			let composite = artifact_store::stage_snapshot_capture(
				&snapshot,
				"Game".to_owned(),
				&policy.mapped_refs,
				&policy.composite_manifest,
				&|| false,
			)
			.unwrap();
			let expected_composite = fs::read(composite.artifact()).unwrap();
			let studio = stage_captured_studio_domain(&policy, &composite, &|| false).unwrap();
			let expected_studio = fs::read(studio.staged().unwrap().join(DATA_ARTIFACT)).unwrap();
			let promotion = prepare_capture_promotion(composite, studio).unwrap();
			assert_eq!(promotion.domains.len(), 2);
			for (_, target, backup) in &promotion.domains {
				assert_eq!(
					target.parent(),
					backup.parent(),
					"backup must be same-filesystem sibling"
				);
			}
			let error = promote_capture_domains_impl(&promotion, Some(fail_after)).unwrap_err();
			assert!(error.to_string().contains("synchronous roll-forward completed"));
			assert_eq!(fs::read(&policy.composite_manifest).unwrap(), expected_composite);
			assert_eq!(
				fs::read(data_dir(&policy.project_path).unwrap().join(DATA_ARTIFACT)).unwrap(),
				expected_studio
			);
			assert!(!transaction_journal(&policy.project_path).unwrap().exists());
			assert!(promotion
				.domains
				.iter()
				.all(|(_, target, backup)| target.exists() && !backup.exists()));
			drop(promotion);
			fs::remove_dir_all(project_root).unwrap();
			fs::remove_dir_all(composite_root).unwrap();
		}
	}

	#[test]
	fn cancelled_studio_stage_preserves_both_domains_and_removes_transaction_stage() {
		let (project_root, composite_root, policy, _) = capture_promotion_fixture("capture-stage-cancel");
		let studio_manifest = data_dir(&policy.project_path).unwrap().join(DATA_ARTIFACT);
		let before_composite = (
			fs::read(&policy.composite_manifest).unwrap(),
			fs::metadata(&policy.composite_manifest).unwrap().modified().unwrap(),
		);
		let before_studio = (
			fs::read(&studio_manifest).unwrap(),
			fs::metadata(&studio_manifest).unwrap().modified().unwrap(),
		);
		let error = stage_studio_domain(&policy, &policy.composite_manifest, &|| true).unwrap_err();
		assert!(error.to_string().contains("cancelled"));
		assert_eq!(fs::read(&policy.composite_manifest).unwrap(), before_composite.0);
		assert_eq!(
			fs::metadata(&policy.composite_manifest).unwrap().modified().unwrap(),
			before_composite.1
		);
		assert_eq!(fs::read(&studio_manifest).unwrap(), before_studio.0);
		assert_eq!(
			fs::metadata(&studio_manifest).unwrap().modified().unwrap(),
			before_studio.1
		);
		assert!(!fs::read_dir(&project_root)
			.unwrap()
			.filter_map(|entry| entry.ok())
			.any(|entry| entry.file_name().to_string_lossy().starts_with(".carbon-stage-")));
		fs::remove_dir_all(project_root).unwrap();
		fs::remove_dir_all(composite_root).unwrap();
	}

	#[test]
	fn mapped_topology_change_after_staging_aborts_before_promotion_without_active_writes() {
		let (project_root, composite_root, policy, snapshot) = capture_promotion_fixture("capture-topology-race");
		let projected =
			artifact_store::load_projected_live(&policy.composite_manifest, &policy.mapped_refs, &policy.routing_refs)
				.unwrap();
		let previous = snapshot_from_tree(&projected.tree).unwrap();
		let baseline_generation = exact_projected_realization_generation(
			&policy.project_path,
			&policy.project_document,
			&previous,
			&policy.mapped_refs,
		)
		.unwrap();
		let studio_manifest = data_dir(&policy.project_path).unwrap().join(DATA_ARTIFACT);
		let before_composite = (
			fs::read(&policy.composite_manifest).unwrap(),
			fs::metadata(&policy.composite_manifest).unwrap().modified().unwrap(),
		);
		let before_studio = (
			fs::read(&studio_manifest).unwrap(),
			fs::metadata(&studio_manifest).unwrap().modified().unwrap(),
		);
		let composite = artifact_store::stage_snapshot_capture(
			&snapshot,
			"Game".to_owned(),
			&policy.mapped_refs,
			&policy.composite_manifest,
			&|| false,
		)
		.unwrap();
		let studio = stage_captured_studio_domain(&policy, &composite, &|| false).unwrap();
		let promotion = prepare_capture_promotion(composite, studio).unwrap();
		fs::write(
			project_root.join("src/ReplicatedStorage/Shared/AddedDuringCapture.luau"),
			"return 'new mapped child'\n",
		)
		.unwrap();
		let error = exact_projected_realization_generation(
			&policy.project_path,
			&policy.project_document,
			&previous,
			&policy.mapped_refs,
		)
		.unwrap_err();
		assert!(error.to_string().contains("pending changes"));
		assert!(!baseline_generation.is_empty());
		drop(promotion);
		assert_eq!(fs::read(&policy.composite_manifest).unwrap(), before_composite.0);
		assert_eq!(
			fs::metadata(&policy.composite_manifest).unwrap().modified().unwrap(),
			before_composite.1
		);
		assert_eq!(fs::read(&studio_manifest).unwrap(), before_studio.0);
		assert_eq!(
			fs::metadata(&studio_manifest).unwrap().modified().unwrap(),
			before_studio.1
		);
		assert!(!fs::read_dir(&project_root)
			.unwrap()
			.filter_map(|entry| entry.ok())
			.any(|entry| entry.file_name().to_string_lossy().starts_with(".carbon-stage-")));
		assert!(!fs::read_dir(&composite_root)
			.unwrap()
			.filter_map(|entry| entry.ok())
			.any(|entry| entry
				.file_name()
				.to_string_lossy()
				.starts_with(".carbon-capture-stage-")));
		fs::remove_dir_all(project_root).unwrap();
		fs::remove_dir_all(composite_root).unwrap();
	}

	#[test]
	fn mapped_mutations_are_rejected_while_manifest_scripts_are_allowed() {
		let root = temp("live-policy");
		fs::create_dir_all(&root).unwrap();
		let project = root.join("game.carbon.json");
		initialize(&project, "Game".to_owned()).unwrap();
		let materialized = materialize(&project).unwrap();
		let policy = live_policy(&project, &materialized);
		let mapped_parent = *policy.mapped_roots.first().unwrap();
		let mut mapped = Changes::new();
		mapped.additions.push(AddedSnapshot {
			id: Ref::new(),
			parent: mapped_parent,
			name: "Part".to_owned(),
			raw_name: None,
			class: Ustr::from("Part"),
			properties: UstrMap::default(),
			children: Vec::new(),
		});
		assert!(policy
			.reject_studio_changes(&materialized.snapshot, &mut mapped)
			.unwrap_err()
			.to_string()
			.contains("mapped project source is authoritative"));

		let manifest_parent = materialized
			.snapshot
			.children
			.iter()
			.find(|child| child.class.as_str() == "Workspace")
			.unwrap()
			.id;
		let mut script = Changes::new();
		script.additions.push(AddedSnapshot {
			id: Ref::new(),
			parent: manifest_parent,
			name: "ManifestScript".to_owned(),
			raw_name: None,
			class: Ustr::from("Script"),
			properties: UstrMap::default(),
			children: Vec::new(),
		});
		policy
			.reject_studio_changes(&materialized.snapshot, &mut script)
			.unwrap();
		assert_eq!(script.additions.len(), 1);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn canonical_mapped_sync_echoes_are_not_user_mutations() {
		fn find(snapshot: &Snapshot, id: Ref) -> Option<&Snapshot> {
			if snapshot.id == id {
				return Some(snapshot);
			}
			snapshot.children.iter().find_map(|child| find(child, id))
		}

		let root = temp("mapped-sync-echo");
		fs::create_dir_all(&root).unwrap();
		let project = root.join("game.carbon.json");
		initialize(&project, "Game".to_owned()).unwrap();
		let materialized = materialize(&project).unwrap();
		let policy = live_policy(&project, &materialized);
		let (mapped_parent, mapped) = policy
			.mapped_roots
			.iter()
			.find_map(|id| {
				let mapped = find(&materialized.snapshot, *id)?;
				(!mapped.children.is_empty()).then_some((*id, mapped))
			})
			.unwrap();

		let mut default_echo = Changes::new();
		let mut update = UpdatedSnapshot::new(mapped_parent);
		update.properties = Some(UstrMap::from_iter([(Ustr::from("Archivable"), Variant::Bool(true))]));
		default_echo.updates.push(update);
		policy
			.reject_studio_changes(&materialized.snapshot, &mut default_echo)
			.unwrap();
		assert!(default_echo.is_empty());

		let child = mapped.children.first().unwrap();
		let mut addition_echo = Changes::new();
		addition_echo.additions.push(AddedSnapshot {
			id: child.id,
			parent: mapped_parent,
			name: child.name.clone(),
			raw_name: child.raw_name.clone(),
			class: child.class,
			properties: child.properties.clone(),
			children: child.children.clone(),
		});
		policy
			.reject_studio_changes(&materialized.snapshot, &mut addition_echo)
			.unwrap();
		assert!(addition_echo.is_empty());

		let mut script_default_echo = Changes::new();
		let mut update = UpdatedSnapshot::new(child.id);
		update.properties = Some(UstrMap::from_iter([(
			Ustr::from("ScriptGuid"),
			Variant::String(String::new()),
		)]));
		script_default_echo.updates.push(update);
		policy
			.reject_studio_changes(&materialized.snapshot, &mut script_default_echo)
			.unwrap();
		assert!(
			script_default_echo.is_empty(),
			"ScriptGuid default echo must not be rejected"
		);

		let mut attribute_bytes = Vec::new();
		Attributes::new().to_writer(&mut attribute_bytes).unwrap();
		let mut exact_attribute_echo = Changes::new();
		let mut update = UpdatedSnapshot::new(child.id);
		update.properties = Some(UstrMap::from_iter([(
			Ustr::from("Attributes"),
			Variant::BinaryString(attribute_bytes.into()),
		)]));
		exact_attribute_echo.updates.push(update);
		policy
			.reject_studio_changes(&materialized.snapshot, &mut exact_attribute_echo)
			.unwrap();
		assert!(
			exact_attribute_echo.is_empty(),
			"exact serialized Attributes must compare equal to canonical Attributes"
		);

		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn retired_mapping_events_never_enter_the_studio_manifest_transaction() {
		let root = temp("retired-mapped-echo");
		fs::create_dir_all(&root).unwrap();
		let project = root.join("game.carbon.json");
		initialize(&project, "Game".to_owned()).unwrap();
		let materialized = materialize(&project).unwrap();
		let mut policy = live_policy(&project, &materialized);
		let retired = materialized
			.snapshot
			.children
			.iter()
			.flat_map(|service| &service.children)
			.flat_map(|mapped| std::iter::once(mapped.id).chain(mapped.children.iter().map(|child| child.id)))
			.find(|id| policy.mapped_refs.contains(id))
			.unwrap();
		policy.mapped_refs.remove(&retired);
		policy.retired_mapped_refs.insert(retired);

		let mut changes = Changes::new();
		let mut update = UpdatedSnapshot::new(retired);
		update.properties = Some(UstrMap::from_iter([(
			Ustr::from("Source"),
			Variant::String("stale mapped echo".to_owned()),
		)]));
		changes.updates.push(update);
		changes.removals.push(retired);
		changes.additions.push(AddedSnapshot {
			id: Ref::new(),
			parent: retired,
			name: "StaleChild".to_owned(),
			raw_name: None,
			class: Ustr::from("ModuleScript"),
			properties: UstrMap::default(),
			children: Vec::new(),
		});

		policy
			.reject_studio_changes(&materialized.snapshot, &mut changes)
			.unwrap();
		assert!(changes.is_empty());
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn source_replacement_retires_removed_studio_identities() {
		let previous_mapping = Ref::new();
		let removed_studio_identity = Ref::new();
		let previous = HashSet::from([previous_mapping]);
		let next = HashSet::new();
		let retired = retired_source_refs(&previous, &next, &[removed_studio_identity]);
		assert!(retired.contains(&previous_mapping));
		assert!(retired.contains(&removed_studio_identity));
	}

	#[test]
	fn unchanged_studio_data_install_preserves_artifact_mtime() {
		let root = temp("studio-data-noop-mtimes");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		let snapshot = empty_studio_snapshot();
		install_data_store(&project_path, &snapshot, "Game").unwrap();
		let manifest_path = data_artifact(&project_path).unwrap();
		let manifest_mtime = fs::metadata(&manifest_path).unwrap().modified().unwrap();

		install_data_store(&project_path, &snapshot, "Game").unwrap();

		assert_eq!(
			manifest_mtime,
			fs::metadata(&manifest_path).unwrap().modified().unwrap()
		);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn mapping_barrier_prunes_stale_manifest_without_writing_source() {
		let root = temp("ownership-transition");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		let mut studio = empty_studio_snapshot();
		studio
			.children
			.iter_mut()
			.find(|child| child.name == "ReplicatedStorage")
			.unwrap()
			.children
			.push(
				Snapshot::new()
					.with_id(stable_ref("transition:SharedData"))
					.with_name("SharedData")
					.with_class("Folder")
					.with_children(vec![Snapshot::new()
						.with_id(stable_ref("transition:Nested"))
						.with_name("Nested")
						.with_class("Folder")]),
			);
		install_data_store(&project_path, &studio, "Game").unwrap();
		fs::create_dir_all(root.join("src/ReplicatedStorage/SharedData")).unwrap();
		let mut project: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
		project["tree"]["ReplicatedStorage"]["SharedData"] =
			serde_json::json!({"$path": "src/ReplicatedStorage/SharedData"});
		write_json(&project_path, &project).unwrap();
		let staged = materialize(&project_path).unwrap();
		fs::remove_dir_all(staged.directory).unwrap();
		let materialized = materialize_for_capture(&project_path).unwrap();
		assert!(!root.join("src/ReplicatedStorage/SharedData/Nested").exists());
		assert!(!root.join("src/ReplicatedStorage/SharedData/meta.json").exists());
		let replicated = materialized
			.snapshot
			.children
			.iter()
			.find(|child| child.name == "ReplicatedStorage")
			.unwrap();
		let shared_data = replicated
			.children
			.iter()
			.find(|child| child.name == "SharedData")
			.unwrap();
		assert!(
			shared_data.children.is_empty(),
			"filesystem barrier must replace stale manifest descendants"
		);
		let studio = artifact_store::load_tree(&data_artifact(&project_path).unwrap()).unwrap();
		assert!(!tree_snapshot(&studio.tree, studio.tree.root_ref())
			.unwrap()
			.children
			.iter()
			.flat_map(|service| &service.children)
			.any(|child| child.name == "SharedData"));
		fs::remove_dir_all(materialized.directory).unwrap();
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn ownership_transfer_is_stable_after_manifest_rebuild() {
		let root = temp("ownership-transition-sibling-slots");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		let mut studio = empty_studio_snapshot();
		let replicated = studio
			.children
			.iter_mut()
			.find(|child| child.name == "ReplicatedStorage")
			.unwrap();
		replicated.children = vec![
			Snapshot::new()
				.with_id(stable_ref("transition:Before"))
				.with_name("Before")
				.with_class("Folder"),
			Snapshot::new()
				.with_id(stable_ref("transition:SharedData"))
				.with_name("SharedData")
				.with_class("Folder")
				.with_children(vec![Snapshot::new()
					.with_id(stable_ref("transition:Nested"))
					.with_name("Nested")
					.with_class("Folder")]),
			Snapshot::new()
				.with_id(stable_ref("transition:After"))
				.with_name("After")
				.with_class("Folder"),
		];
		install_data_store(&project_path, &studio, "Game").unwrap();
		fs::create_dir_all(root.join("src/ReplicatedStorage/SharedData")).unwrap();
		let mut project: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
		project["tree"]["ReplicatedStorage"]["SharedData"] =
			serde_json::json!({"$path": "src/ReplicatedStorage/SharedData"});
		write_json(&project_path, &project).unwrap();

		let first = materialize_for_capture(&project_path).unwrap();
		let first_order = first
			.snapshot
			.children
			.iter()
			.find(|child| child.name == "ReplicatedStorage")
			.unwrap()
			.children
			.iter()
			.map(|child| child.name.clone())
			.collect::<Vec<_>>();
		assert_eq!(
			first_order.iter().cloned().collect::<BTreeSet<_>>(),
			BTreeSet::from([
				"Before".to_owned(),
				"Shared".to_owned(),
				"After".to_owned(),
				"SharedData".to_owned(),
			])
		);
		fs::remove_dir_all(first.directory).unwrap();

		let second = materialize_for_capture(&project_path).unwrap();
		let second_order = second
			.snapshot
			.children
			.iter()
			.find(|child| child.name == "ReplicatedStorage")
			.unwrap()
			.children
			.iter()
			.map(|child| child.name.clone())
			.collect::<Vec<_>>();
		assert_eq!(
			second_order, first_order,
			"manifest persistence must keep canonical membership stable"
		);
		fs::remove_dir_all(second.directory).unwrap();
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn ownership_transfer_is_stable_without_sibling_slots() {
		let root = temp("ownership-transition-declared-order");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		let mut studio = empty_studio_snapshot();
		let replicated = studio
			.children
			.iter_mut()
			.find(|child| child.name == "ReplicatedStorage")
			.unwrap();
		replicated.children = vec![
			Snapshot::new()
				.with_id(stable_ref("declared:StudioOwned"))
				.with_name("StudioOwned")
				.with_class("Folder"),
			Snapshot::new()
				.with_id(stable_ref("declared:PrefixOwned"))
				.with_name("PrefixOwned")
				.with_class("Folder"),
			Snapshot::new()
				.with_id(stable_ref("declared:EqualRoot"))
				.with_name("EqualRoot")
				.with_class("Folder")
				.with_children(vec![Snapshot::new()
					.with_id(stable_ref("declared:EqualChild"))
					.with_name("EqualChild")
					.with_class("Folder")]),
			Snapshot::new()
				.with_id(stable_ref("declared:SuffixOwned"))
				.with_name("SuffixOwned")
				.with_class("Folder"),
		];
		install_data_store(&project_path, &studio, "Game").unwrap();
		fs::create_dir_all(root.join("src/ReplicatedStorage/EqualRoot")).unwrap();
		let project = serde_json::json!({
			"name": "Game",
			"tree": {
				"$className": "DataModel",
				"ReplicatedStorage": {
					"Shared": { "$path": "src/ReplicatedStorage/Shared" },
					"EqualRoot": { "$path": "src/ReplicatedStorage/EqualRoot" }
				},
				"ServerScriptService": {
					"Server": { "$path": "src/ServerScriptService/Server.server.luau" }
				},
				"StarterPlayer": {
					"StarterPlayerScripts": {
						"Client": { "$path": "src/StarterPlayer/StarterPlayerScripts/Client.client.luau" }
					}
				}
			}
		});
		write_json(&project_path, &project).unwrap();

		let materialized = materialize_for_capture(&project_path).unwrap();
		let names = materialized
			.snapshot
			.children
			.iter()
			.find(|child| child.name == "ReplicatedStorage")
			.unwrap()
			.children
			.iter()
			.map(|child| child.name.as_str())
			.collect::<Vec<_>>();
		assert_eq!(
			names.into_iter().collect::<BTreeSet<_>>(),
			BTreeSet::from(["Shared", "StudioOwned", "PrefixOwned", "EqualRoot", "SuffixOwned",])
		);
		fs::remove_dir_all(materialized.directory).unwrap();
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn studio_owned_changes_persist_without_mapped_records() {
		let root = temp("studio-persist");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		let materialized = materialize(&project_path).unwrap();
		let policy = live_policy(&project_path, &materialized);
		let mut loaded = artifact_store::load_live(&materialized.manifest_path).unwrap();
		let workspace = loaded
			.tree
			.root()
			.children()
			.iter()
			.copied()
			.find(|id| loaded.tree.get_instance(*id).unwrap().class.as_str() == "Workspace")
			.unwrap();
		let authored = Ref::new();
		let mut changes = Changes::new();
		changes.add(
			Snapshot::new()
				.with_id(authored)
				.with_name("StudioOwned")
				.with_class("Folder"),
			workspace,
		);
		loaded.store.apply(&mut loaded.tree, changes).unwrap();
		persist_studio_domain(&policy).unwrap();
		let studio = artifact_store::load_tree(&data_artifact(&project_path).unwrap()).unwrap();
		assert!(studio.tree.exists(authored));
		for mapped in &policy.mapped_refs {
			assert!(!studio.tree.exists(*mapped));
		}
		fs::remove_dir_all(materialized.directory).unwrap();
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn inline_properties_attributes_tags_and_script_children_hydrate() {
		let root = temp("inline-hydration");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		let mut value: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
		value["tree"]["Workspace"] = serde_json::json!({
			"$properties": {"Gravity": 100},
			"Mapped": {
				"$className": "Folder",
				"$properties": {"Archivable": false, "Tags": ["One", "Two"]},
				"$attributes": {"Mode": "Test", "Count": 2},
				"Nested": {"$className": "ModuleScript"}
			}
		});
		write_json(&project_path, &value).unwrap();
		let materialized = materialize(&project_path).unwrap();
		let workspace = materialized
			.snapshot
			.children
			.iter()
			.find(|child| child.name == "Workspace")
			.unwrap();
		assert_eq!(workspace.properties[&Ustr::from("Gravity")], Variant::Float32(100.0));
		let mapped = workspace.children.iter().find(|child| child.name == "Mapped").unwrap();
		assert_eq!(mapped.properties[&Ustr::from("Archivable")], Variant::Bool(false));
		assert!(matches!(mapped.properties[&Ustr::from("Tags")], Variant::Tags(_)));
		assert!(matches!(
			mapped.properties[&Ustr::from("Attributes")],
			Variant::Attributes(_)
		));
		assert_eq!(mapped.children[0].class.as_str(), "ModuleScript");
		fs::remove_dir_all(materialized.directory).unwrap();
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn mapped_snapshot_export_preserves_attributes_and_tags() {
		let root = temp("mapped-metadata-export");
		let output = root.join("Shared");
		fs::create_dir_all(&root).unwrap();
		let mut attributes = Attributes::new();
		attributes.insert("Alpha".to_owned(), Variant::Int32(42));
		attributes.insert("Enabled".to_owned(), Variant::Bool(true));
		let mut properties = UstrMap::default();
		properties.insert(Ustr::from("Attributes"), Variant::Attributes(attributes));
		properties.insert(
			Ustr::from("Tags"),
			Variant::Tags(rbx_dom_weak::types::Tags::from(vec!["One".to_owned()])),
		);
		let snapshot = Snapshot::new()
			.with_id(Ref::new())
			.with_name("Shared")
			.with_class("Folder")
			.with_properties(properties);
		write_mapped_snapshot(&output, &snapshot, &HashSet::new()).unwrap();
		let restored = read_directory_instance(&output, "Shared", Path::new("/not-data")).unwrap();
		assert_eq!(
			restored.properties[&Ustr::from("Attributes")],
			snapshot.properties[&Ustr::from("Attributes")]
		);
		assert_eq!(
			restored.properties[&Ustr::from("Tags")],
			snapshot.properties[&Ustr::from("Tags")]
		);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn mapped_snapshot_export_marks_empty_folders_portably() {
		let root = temp("mapped-empty-folder-export");
		let output = root.join("Shared");
		fs::create_dir_all(&root).unwrap();
		let snapshot = Snapshot::new()
			.with_id(Ref::new())
			.with_name("Shared")
			.with_class("Folder")
			.with_children(vec![Snapshot::new()
				.with_id(Ref::new())
				.with_name("Empty")
				.with_class("Folder")]);

		write_mapped_snapshot(&output, &snapshot, &HashSet::new()).unwrap();

		let marker = output.join("Empty/meta.json");
		assert!(marker.is_file(), "an empty mapped Folder needs a Git-trackable marker");
		assert_eq!(
			serde_json::from_slice::<Value>(&fs::read(marker).unwrap()).unwrap(),
			serde_json::json!({})
		);
		let restored = read_directory_instance(&output, "Shared", Path::new("/not-data")).unwrap();
		assert_eq!(
			restored
				.children
				.iter()
				.map(|child| (child.name.as_str(), child.class.as_str()))
				.collect::<Vec<_>>(),
			vec![("Empty", "Folder")]
		);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn mapped_snapshot_export_normalizes_utf8_attribute_binary_strings() {
		let root = temp("mapped-string-attribute-export");
		let output = root.join("Shared");
		fs::create_dir_all(&root).unwrap();
		let mut attributes = Attributes::new();
		attributes.insert("Label".to_owned(), Variant::BinaryString(b"value".to_vec().into()));
		let snapshot = Snapshot::new()
			.with_id(Ref::new())
			.with_name("Shared")
			.with_class("Folder")
			.with_properties(UstrMap::from_iter([(
				Ustr::from("Attributes"),
				Variant::Attributes(attributes),
			)]));
		write_mapped_snapshot(&output, &snapshot, &HashSet::new()).unwrap();
		let restored = read_directory_instance(&output, "Shared", Path::new("/not-data")).unwrap();
		let Variant::Attributes(restored) = &restored.properties[&Ustr::from("Attributes")] else {
			panic!("restored attributes are unavailable");
		};
		assert_eq!(restored.get("Label"), Some(&Variant::String("value".to_owned())));
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn rejected_studio_changes_produce_canonical_corrections() {
		let mapped = Ref::new();
		let script = Ref::new();
		let mut properties = UstrMap::default();
		properties.insert(Ustr::from("Source"), Variant::String("return true".to_owned()));
		let canonical = Snapshot::new()
			.with_class("DataModel")
			.with_children(vec![Snapshot::new()
				.with_id(mapped)
				.with_name("Mapped")
				.with_class("Folder")
				.with_children(vec![Snapshot::new()
					.with_id(script)
					.with_name("Entry")
					.with_class("ModuleScript")
					.with_properties(properties)])]);
		let tree = Tree::new(canonical);
		let mut attempted = Changes::new();
		let mut update = UpdatedSnapshot::new(script);
		update.properties = Some(UstrMap::from_iter([(
			Ustr::from("Source"),
			Variant::String("return false".to_owned()),
		)]));
		attempted.updates.push(update);
		let correction = corrective_changes(&tree, &attempted).unwrap();
		assert_eq!(correction.updates.len(), 1);
		assert_eq!(
			correction.updates[0].properties.as_ref().unwrap()[&Ustr::from("Source")],
			Variant::String("return true".to_owned())
		);

		let forbidden = Ref::new();
		let mut attempted = Changes::new();
		attempted.additions.push(AddedSnapshot {
			id: forbidden,
			parent: mapped,
			name: "Forbidden".to_owned(),
			raw_name: None,
			class: Ustr::from("Part"),
			properties: UstrMap::default(),
			children: Vec::new(),
		});
		let correction = corrective_changes(&tree, &attempted).unwrap();
		assert_eq!(correction.removals, vec![forbidden]);
	}

	#[test]
	fn combined_forbidden_structural_changes_produce_one_canonical_correction() {
		let root = Ref::new();
		let mapped = Ref::new();
		let example = Ref::new();
		let studio_owned = Ref::new();
		let barrier_part = Ref::new();
		let manifest_script = Ref::new();
		let canonical = Snapshot::new()
			.with_id(root)
			.with_class("DataModel")
			.with_children(vec![
				Snapshot::new()
					.with_id(mapped)
					.with_name("Shared")
					.with_class("Folder")
					.with_children(vec![Snapshot::new()
						.with_id(example)
						.with_name("Example")
						.with_class("ModuleScript")]),
				Snapshot::new()
					.with_id(studio_owned)
					.with_name("StudioOwned")
					.with_class("Folder"),
			]);
		let mut attempted = Changes::new();
		attempted.additions.push(AddedSnapshot {
			id: barrier_part,
			parent: mapped,
			name: "BarrierPart".to_owned(),
			raw_name: None,
			class: Ustr::from("Part"),
			properties: UstrMap::default(),
			children: Vec::new(),
		});
		attempted.additions.push(AddedSnapshot {
			id: manifest_script,
			parent: studio_owned,
			name: "ManifestScript".to_owned(),
			raw_name: None,
			class: Ustr::from("ModuleScript"),
			properties: UstrMap::default(),
			children: Vec::new(),
		});
		let mut moved = UpdatedSnapshot::new(example);
		moved.parent = Some(studio_owned);
		attempted.updates.push(moved);

		let correction = corrective_changes_from_snapshot(&canonical, &attempted).unwrap();
		assert_eq!(correction.removals.len(), 2);
		assert!(correction.removals.contains(&barrier_part));
		assert!(correction.removals.contains(&manifest_script));
		let restored = correction.updates.iter().find(|update| update.id == example).unwrap();
		assert_eq!(restored.parent, Some(mapped));
	}

	#[test]
	fn studio_commit_refresh_preserves_the_unapplied_filesystem_baseline() {
		let root = Ref::new();
		let mapped = Ref::new();
		let studio = Ref::new();
		let property = Ustr::from("Value");
		let previous = Snapshot::new()
			.with_id(root)
			.with_class("DataModel")
			.with_children(vec![
				Snapshot::new()
					.with_id(mapped)
					.with_name("Mapped")
					.with_class("Folder")
					.with_properties(UstrMap::from_iter([(
						property,
						Variant::String("filesystem-baseline".to_owned()),
					)])),
				Snapshot::new()
					.with_id(studio)
					.with_name("Studio")
					.with_class("Folder")
					.with_properties(UstrMap::from_iter([(
						property,
						Variant::String("studio-before".to_owned()),
					)])),
			]);
		let committed = Snapshot::new()
			.with_id(root)
			.with_class("DataModel")
			.with_children(vec![
				Snapshot::new()
					.with_id(mapped)
					.with_name("Mapped")
					.with_class("Folder")
					.with_properties(UstrMap::from_iter([(
						property,
						Variant::BinaryString(b"transport-form".to_vec().into()),
					)])),
				Snapshot::new()
					.with_id(studio)
					.with_name("Studio")
					.with_class("Folder")
					.with_properties(UstrMap::from_iter([(
						property,
						Variant::String("studio-after".to_owned()),
					)])),
			]);
		let merged = canonical_after_studio_commit(&previous, &committed, &HashSet::from([mapped])).unwrap();
		let nodes = flatten_snapshot(&merged);
		assert_eq!(
			nodes[&mapped].snapshot.properties[&property],
			Variant::String("filesystem-baseline".to_owned())
		);
		assert_eq!(
			nodes[&studio].snapshot.properties[&property],
			Variant::String("studio-after".to_owned())
		);
	}

	#[test]
	fn mapped_identity_metadata_preserves_directory_identity_and_allows_manifest_references() {
		let root = temp("mapped-identity-metadata");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		let identity = Ref::new();
		write_json(
			&root.join("src/ReplicatedStorage/Shared/meta.json"),
			&serde_json::json!({"id": identity.to_string()}),
		)
		.unwrap();

		let first = materialize(&project_path).unwrap();
		let target = first
			.snapshot
			.children
			.iter()
			.find(|child| child.name == "ReplicatedStorage")
			.unwrap()
			.children
			.iter()
			.find(|child| child.name == "Shared")
			.unwrap()
			.id;
		assert_eq!(target, identity);
		let policy = live_policy(&project_path, &first);
		let mut loaded = artifact_store::load_live(&first.manifest_path).unwrap();
		let workspace = loaded
			.tree
			.root()
			.children()
			.iter()
			.copied()
			.find(|id| loaded.tree.get_instance(*id).unwrap().class.as_str() == "Workspace")
			.unwrap();
		let holder = Ref::new();
		let mut changes = Changes::new();
		changes.add(
			Snapshot::new()
				.with_id(holder)
				.with_name("MappedReference")
				.with_class("ObjectValue")
				.with_properties(UstrMap::from_iter([(Ustr::from("Value"), Variant::Ref(target))])),
			workspace,
		);
		loaded.store.apply(&mut loaded.tree, changes).unwrap();
		persist_studio_domain(&policy).unwrap();

		let second = materialize(&project_path).unwrap();
		let nodes = flatten_snapshot(&second.snapshot);
		assert_eq!(
			nodes[&holder].snapshot.properties[&Ustr::from("Value")],
			Variant::Ref(identity)
		);
		fs::remove_dir_all(first.directory).unwrap();
		fs::remove_dir_all(second.directory).unwrap();
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn project_and_meta_identity_fields_are_loaded_as_persistent_mapped_ids() {
		let root = temp("identity-fields-loaded");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		let original_source = fs::read_to_string(root.join("src/ServerScriptService/Server.server.luau")).unwrap();
		let mut project: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
		project["tree"]["ServerScriptService"]["Server"]["$id"] = Value::String("legacy".to_owned());
		write_json(&project_path, &project).unwrap();
		let inline_identity = identity_ref("legacy");
		let materialized = materialize(&project_path).unwrap();
		assert!(materialized.mapped_refs.contains(&inline_identity));
		fs::remove_dir_all(materialized.directory).unwrap();
		assert_eq!(
			fs::read_to_string(root.join("src/ServerScriptService/Server.server.luau")).unwrap(),
			original_source
		);
		project["tree"]["ServerScriptService"]["Server"]
			.as_object_mut()
			.unwrap()
			.remove("$id");
		write_json(&project_path, &project).unwrap();
		write_json(
			&root.join("src/ReplicatedStorage/Shared/meta.json"),
			&serde_json::json!({"id": "legacy"}),
		)
		.unwrap();
		let materialized = materialize(&project_path).unwrap();
		assert!(materialized.mapped_refs.contains(&inline_identity));
		fs::remove_dir_all(materialized.directory).unwrap();
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn studio_reference_to_mapped_identity_is_retained_in_the_manifest() {
		let root = temp("cross-domain-reference");
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		initialize(&project_path, "Game".to_owned()).unwrap();
		let materialized = materialize(&project_path).unwrap();
		let target = materialized
			.snapshot
			.children
			.iter()
			.find(|child| child.name == "ReplicatedStorage")
			.unwrap()
			.children
			.iter()
			.find(|child| child.name == "Shared")
			.unwrap()
			.id;
		let policy = live_policy(&project_path, &materialized);
		let mut loaded = artifact_store::load_live(&materialized.manifest_path).unwrap();
		let workspace = loaded
			.tree
			.root()
			.children()
			.iter()
			.copied()
			.find(|id| loaded.tree.get_instance(*id).unwrap().class.as_str() == "Workspace")
			.unwrap();
		let holder = Ref::new();
		let mut properties = UstrMap::default();
		properties.insert(Ustr::from("Value"), Variant::Ref(target));
		let mut changes = Changes::new();
		changes.add(
			Snapshot::new()
				.with_id(holder)
				.with_name("MappedReference")
				.with_class("ObjectValue")
				.with_properties(properties),
			workspace,
		);
		loaded.store.apply(&mut loaded.tree, changes).unwrap();
		persist_studio_domain(&policy).unwrap();
		let rematerialized = materialize(&project_path).unwrap();
		let nodes = flatten_snapshot(&rematerialized.snapshot);
		assert_eq!(
			nodes[&holder].snapshot.properties[&Ustr::from("Value")],
			Variant::Ref(target)
		);
		fs::remove_dir_all(rematerialized.directory).unwrap();
		fs::remove_dir_all(materialized.directory).unwrap();
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn capture_preflight_allows_all_manifest_references_to_mapped_targets_but_rejects_reverse_refs() {
		use rbx_dom_weak::types::Content;

		let root = Ref::new();
		let replicated_storage = Ref::new();
		let mapped = Ref::new();
		let mapped_target = Ref::new();
		let studio_owned = Ref::new();
		let canonical = Snapshot::new()
			.with_id(root)
			.with_name("DataModel")
			.with_class("DataModel")
			.with_children(vec![Snapshot::new()
				.with_id(replicated_storage)
				.with_name("ReplicatedStorage")
				.with_class("ReplicatedStorage")
				.with_children(vec![
					Snapshot::new()
						.with_id(mapped)
						.with_name("Shared")
						.with_class("Folder")
						.with_children(vec![Snapshot::new()
							.with_id(mapped_target)
							.with_name("Example")
							.with_class("ModuleScript")]),
					Snapshot::new()
						.with_id(studio_owned)
						.with_name("StudioOwned")
						.with_class("Folder"),
				])]);
		let mapped_refs = HashSet::from([mapped, mapped_target]);

		for (property, value) in [
			("Value", Variant::Ref(mapped_target)),
			(
				"FallbackImageContent",
				Variant::Content(Content::from_referent(mapped_target)),
			),
		] {
			let mut changes = Changes::new();
			changes.additions.push(AddedSnapshot {
				id: Ref::new(),
				parent: studio_owned,
				name: "CrossDomain".to_owned(),
				raw_name: None,
				class: Ustr::from("ObjectValue"),
				properties: UstrMap::from_iter([(Ustr::from(property), value)]),
				children: Vec::new(),
			});

			validate_capture_cross_domain_references(&canonical, &changes, &mapped_refs).unwrap();
		}

		let mut changes = Changes::new();
		let mut mapped_update = UpdatedSnapshot::new(mapped_target);
		mapped_update.properties = Some(UstrMap::from_iter([(Ustr::from("Value"), Variant::Ref(studio_owned))]));
		changes.updates.push(mapped_update);
		let error = validate_capture_cross_domain_references(&canonical, &changes, &mapped_refs)
			.unwrap_err()
			.to_string();
		assert!(error.contains("game.ReplicatedStorage.Shared.Example.Value"));
		assert!(error.contains("game.ReplicatedStorage.StudioOwned"));
	}

	#[test]
	fn capture_identity_reconciliation_preserves_unambiguous_rename_and_reparent() {
		let root_id = Ref::new();
		let prior_id = Ref::new();
		let target_parent = Ref::new();
		let property = Ustr::from("Archivable");
		let canonical = Snapshot::new()
			.with_id(root_id)
			.with_name("DataModel")
			.with_class("DataModel")
			.with_children(vec![
				Snapshot::new()
					.with_id(prior_id)
					.with_name("Before")
					.with_class("Folder")
					.with_properties(UstrMap::from_iter([(property, Variant::Bool(true))])),
				Snapshot::new()
					.with_id(target_parent)
					.with_name("Workspace")
					.with_class("Workspace"),
			]);
		let transient = Ref::new();
		let mut changes = Changes::new();
		changes.removals.push(prior_id);
		changes.additions.push(AddedSnapshot {
			id: transient,
			parent: target_parent,
			name: "After".to_owned(),
			raw_name: None,
			class: Ustr::from("Folder"),
			properties: UstrMap::from_iter([(property, Variant::Bool(false))]),
			children: Vec::new(),
		});

		reconcile_capture_identities(&canonical, &mut changes).unwrap();
		assert_eq!(changes.additions[0].id, prior_id);
		assert_eq!(changes.additions[0].parent, target_parent);
	}

	#[test]
	fn capture_identity_reconciliation_rejects_ambiguous_prior_objects() {
		let first = Ref::new();
		let second = Ref::new();
		let canonical = Snapshot::new()
			.with_id(Ref::new())
			.with_name("DataModel")
			.with_class("DataModel")
			.with_children(vec![
				Snapshot::new().with_id(first).with_name("One").with_class("Folder"),
				Snapshot::new().with_id(second).with_name("Two").with_class("Folder"),
			]);
		let mut changes = Changes::new();
		changes.removals.extend([first, second]);
		changes.additions.push(AddedSnapshot {
			id: Ref::new(),
			parent: canonical.id,
			name: "Renamed".to_owned(),
			raw_name: None,
			class: Ustr::from("Folder"),
			properties: UstrMap::new(),
			children: Vec::new(),
		});

		let error = reconcile_capture_identities(&canonical, &mut changes)
			.unwrap_err()
			.to_string();
		assert!(error.contains("identity-ambiguity blocker"));
	}

	#[test]
	fn capture_identity_reconciliation_indexes_thousands_of_unique_roots() {
		let root_id = Ref::new();
		let marker = Ustr::from("Marker");
		let mut prior = Vec::with_capacity(5_000);
		let mut changes = Changes::new();
		let mut expected = Vec::with_capacity(5_000);
		for index in 0..5_000 {
			let prior_id = Ref::new();
			expected.push(prior_id);
			prior.push(
				Snapshot::new()
					.with_id(prior_id)
					.with_name(&format!("Before{index}"))
					.with_class("Folder")
					.with_properties(UstrMap::from_iter([(marker, Variant::String(index.to_string()))])),
			);
			changes.removals.push(prior_id);
			changes.additions.push(AddedSnapshot {
				id: Ref::new(),
				parent: root_id,
				name: format!("After{index}"),
				raw_name: None,
				class: Ustr::from("Folder"),
				properties: UstrMap::from_iter([(marker, Variant::String(index.to_string()))]),
				children: Vec::new(),
			});
		}
		let canonical = Snapshot::new()
			.with_id(root_id)
			.with_name("DataModel")
			.with_class("DataModel")
			.with_children(prior);

		reconcile_capture_identities(&canonical, &mut changes).unwrap();

		assert_eq!(
			changes.additions.iter().map(|addition| addition.id).collect::<Vec<_>>(),
			expected
		);
	}

	#[test]
	fn capture_identity_reconciliation_assigns_a_structured_identity_to_a_genuinely_new_instance() {
		let root_id = Ref::new();
		let canonical = Snapshot::new()
			.with_id(root_id)
			.with_name("DataModel")
			.with_class("DataModel");
		let incoming = Ref::new();
		let mut changes = Changes::new();
		changes.additions.push(AddedSnapshot {
			id: incoming,
			parent: root_id,
			name: "New".to_owned(),
			raw_name: None,
			class: Ustr::from("Folder"),
			properties: UstrMap::new(),
			children: Vec::new(),
		});

		reconcile_capture_identities(&canonical, &mut changes).unwrap();
		let assigned = u128::from_str_radix(&changes.additions[0].id.to_string(), 16).unwrap();
		let bytes = assigned.to_be_bytes();
		assert_ne!(changes.additions[0].id, incoming);
		assert_ne!(
			&bytes[..crate::manifest_identity::PREFIX_BYTES],
			&[0; crate::manifest_identity::PREFIX_BYTES]
		);
		assert_eq!(&bytes[crate::manifest_identity::PREFIX_BYTES..], &[0, 0]);
	}

	#[test]
	fn imported_snapshot_remaps_ref_and_object_content_to_structured_identities() {
		let source_root = Ref::new();
		let source_target = Ref::new();
		let source_holder = Ref::new();
		let mut snapshot = Snapshot::new()
			.with_id(source_root)
			.with_name("Game")
			.with_class("DataModel")
			.with_children(vec![
				Snapshot::new()
					.with_id(source_target)
					.with_name("Target")
					.with_class("Folder"),
				Snapshot::new()
					.with_id(source_holder)
					.with_name("Holder")
					.with_class("AdGui")
					.with_properties(UstrMap::from_iter([
						(Ustr::from("Ref"), Variant::Ref(source_target)),
						(
							Ustr::from("Content"),
							Variant::Content(Content::from_referent(source_target)),
						),
					])),
			]);

		stabilize_snapshot_ids(&mut snapshot).unwrap();

		let target = snapshot.children[0].id;
		let holder = &snapshot.children[1];
		assert_ne!(snapshot.id, source_root);
		assert_ne!(target, source_target);
		assert_ne!(holder.id, source_holder);
		assert_eq!(holder.properties[&Ustr::from("Ref")], Variant::Ref(target));
		assert_eq!(
			holder.properties[&Ustr::from("Content")],
			Variant::Content(Content::from_referent(target))
		);
		let prefixes = [&snapshot.id, &target, &holder.id]
			.into_iter()
			.map(|id| id.to_string()[..crate::manifest_identity::PREFIX_BYTES * 2].to_owned())
			.collect::<HashSet<_>>();
		assert_eq!(prefixes.len(), 1);
	}

	#[test]
	fn capture_reconciliation_remaps_ref_and_object_content_to_new_identities() {
		let root = Ref::new();
		let source_target = Ref::new();
		let source_holder = Ref::new();
		let canonical = Snapshot::new().with_id(root).with_name("Game").with_class("DataModel");
		let mut changes = Changes::new();
		changes.additions.push(AddedSnapshot {
			id: source_target,
			parent: root,
			name: "Target".to_owned(),
			raw_name: None,
			class: Ustr::from("Folder"),
			properties: UstrMap::new(),
			children: Vec::new(),
		});
		changes.additions.push(AddedSnapshot {
			id: source_holder,
			parent: root,
			name: "Holder".to_owned(),
			raw_name: None,
			class: Ustr::from("AdGui"),
			properties: UstrMap::from_iter([
				(Ustr::from("Ref"), Variant::Ref(source_target)),
				(
					Ustr::from("Content"),
					Variant::Content(Content::from_referent(source_target)),
				),
			]),
			children: Vec::new(),
		});

		reconcile_capture_identities(&canonical, &mut changes).unwrap();

		let target = changes
			.additions
			.iter()
			.find(|addition| addition.name == "Target")
			.unwrap()
			.id;
		let holder = changes
			.additions
			.iter()
			.find(|addition| addition.name == "Holder")
			.unwrap();
		assert_ne!(target, source_target);
		assert_ne!(holder.id, source_holder);
		assert_eq!(holder.properties[&Ustr::from("Ref")], Variant::Ref(target));
		assert_eq!(
			holder.properties[&Ustr::from("Content")],
			Variant::Content(Content::from_referent(target))
		);
	}
}
