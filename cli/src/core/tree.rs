use anyhow::{ensure, Context, Result};
use rbx_dom_weak::{
	types::{Ref, Variant},
	Ustr, UstrMap,
};
use std::collections::HashMap;

use super::snapshot::{HierarchySnapshot, Snapshot, SnapshotPage, UpdatedSnapshot};

/// Dense mutable hierarchy used for reconciliation and canonical ownership.
#[derive(Clone, Debug)]
pub struct Tree {
	nodes: Vec<TreeNode>,
	by_ref: HashMap<Ref, usize>,
	root_ref: Ref,
}

#[derive(Clone, Debug)]
pub struct TreeNode {
	referent: Ref,
	parent: Ref,
	pub class: Ustr,
	pub name: String,
	pub properties: UstrMap<Variant>,
	children: Vec<Ref>,
	live_available: bool,
	unavailable_root: bool,
}

impl TreeNode {
	pub fn referent(&self) -> Ref {
		self.referent
	}

	pub fn parent(&self) -> Ref {
		self.parent
	}

	pub fn children(&self) -> &[Ref] {
		&self.children
	}
}

impl Tree {
	fn snapshot_properties(snapshot: &mut Snapshot) -> UstrMap<Variant> {
		let mut properties = std::mem::take(&mut snapshot.properties);
		if let Some(raw_name) = snapshot.raw_name.take() {
			properties.insert(
				Ustr::from("__CarbonRawName"),
				Variant::BinaryString(raw_name.into_vec().into()),
			);
		}
		properties
	}

	pub(crate) fn new_detached(snapshot: Snapshot, capacity: usize) -> Result<Self> {
		ensure!(snapshot.id.is_some(), "tree root has no stable id");
		let root_ref = snapshot.id;
		let mut tree = Self {
			nodes: Vec::with_capacity(capacity),
			by_ref: HashMap::with_capacity(capacity),
			root_ref,
		};
		tree.insert_detached(snapshot, Ref::none())?;
		Ok(tree)
	}

	pub(crate) fn insert_detached(&mut self, mut snapshot: Snapshot, parent: Ref) -> Result<()> {
		ensure!(snapshot.id.is_some(), "tree node has no stable id");
		let index = self.nodes.len();
		ensure!(
			self.by_ref.insert(snapshot.id, index).is_none(),
			"duplicate tree referent {}",
			snapshot.id
		);
		let properties = Self::snapshot_properties(&mut snapshot);
		self.nodes.push(TreeNode {
			referent: snapshot.id,
			parent,
			class: snapshot.class,
			name: snapshot.name,
			properties,
			children: Vec::new(),
			live_available: true,
			unavailable_root: false,
		});
		Ok(())
	}

	pub(crate) fn finish_detached(&mut self) -> Result<()> {
		let mut children: Vec<Vec<Ref>> = (0..self.nodes.len()).map(|_| Vec::new()).collect();
		for node in &self.nodes {
			if node.parent.is_none() {
				ensure!(node.referent == self.root_ref, "tree has multiple roots");
				continue;
			}
			let parent = *self.by_ref.get(&node.parent).context("tree parent is missing")?;
			children[parent].push(node.referent);
		}
		for (node, mut entries) in self.nodes.iter_mut().zip(children) {
			entries.sort_unstable_by_key(ToString::to_string);
			node.children = entries;
		}
		let root = self.root_ref;
		let mut stack = vec![(root, true)];
		while let Some((id, parent_available)) = stack.pop() {
			let index = *self.by_ref.get(&id).context("tree traversal instance is missing")?;
			// Root availability depends on the active Studio build and plugin
			// creation capability. The Studio client now probes each root instead of
			// baking a class-name deny-list into source loading.
			let unavailable_root = false;
			let live_available = parent_available;
			self.nodes[index].live_available = live_available;
			self.nodes[index].unavailable_root = unavailable_root;
			for child in self.nodes[index].children.iter().rev() {
				stack.push((*child, live_available));
			}
		}
		Ok(())
	}

	pub fn new(mut snapshot: Snapshot) -> Self {
		if snapshot.id.is_none() {
			snapshot.id = Ref::new();
		}
		let children = std::mem::take(&mut snapshot.children);
		let root_ref = snapshot.id;
		let properties = Self::snapshot_properties(&mut snapshot);
		let mut tree = Self {
			nodes: vec![TreeNode {
				referent: root_ref,
				parent: Ref::none(),
				class: snapshot.class,
				name: snapshot.name,
				properties,
				children: Vec::new(),
				live_available: true,
				unavailable_root: false,
			}],
			by_ref: HashMap::from([(root_ref, 0)]),
			root_ref,
		};
		for child in children {
			tree.insert_instance_recursive_unsorted(child, root_ref);
		}
		tree.nodes[0].children.sort_unstable_by_key(ToString::to_string);
		tree
	}

	fn insert_instance_recursive_unsorted(&mut self, mut snapshot: Snapshot, parent: Ref) -> Ref {
		if snapshot.id.is_none() {
			snapshot.id = Ref::new();
		}
		let children = std::mem::take(&mut snapshot.children);
		let id = snapshot.id;
		let index = self.nodes.len();
		let properties = Self::snapshot_properties(&mut snapshot);
		self.nodes.push(TreeNode {
			referent: id,
			parent,
			class: snapshot.class,
			name: snapshot.name,
			properties,
			children: Vec::new(),
			live_available: true,
			unavailable_root: false,
		});
		assert!(self.by_ref.insert(id, index).is_none(), "duplicate tree referent {id}");
		let parent_node = self.get_instance_mut(parent).expect("parent is missing");
		parent_node.children.push(id);
		for child in children {
			self.insert_instance_recursive_unsorted(child, id);
		}
		self.nodes[index].children.sort_unstable_by_key(ToString::to_string);
		id
	}

	pub fn insert_instance_recursive(&mut self, snapshot: Snapshot, parent: Ref) -> Ref {
		let id = self.insert_instance_recursive_unsorted(snapshot, parent);
		self.get_instance_mut(parent)
			.expect("parent is missing")
			.children
			.sort_unstable_by_key(ToString::to_string);
		id
	}

	pub fn remove_instance(&mut self, id: Ref) -> Vec<Ref> {
		if id == self.root_ref {
			return Vec::new();
		}
		let Some(node) = self.get_instance(id) else {
			return Vec::new();
		};
		let parent = node.parent;
		let mut to_remove = vec![id];
		let mut cursor = 0;
		while cursor < to_remove.len() {
			let current = to_remove[cursor];
			to_remove.extend_from_slice(self.get_instance(current).unwrap().children());
			cursor += 1;
		}
		let removed = to_remove.clone();
		self.get_instance_mut(parent)
			.unwrap()
			.children
			.retain(|child| *child != id);
		for id in to_remove.into_iter().rev() {
			let index = self.by_ref.remove(&id).unwrap();
			self.nodes.swap_remove(index);
			if let Some(moved) = self.nodes.get(index) {
				self.by_ref.insert(moved.referent, index);
			}
		}
		removed
	}

	pub fn reparent_instance(&mut self, id: Ref, parent: Ref) {
		self.move_instance(id, parent).unwrap();
	}

	fn move_instance(&mut self, id: Ref, parent: Ref) -> Result<()> {
		let old_parent = self.get_instance(id).context("moved instance is missing")?.parent;
		self.get_instance_mut(old_parent)
			.context("old parent is missing")?
			.children
			.retain(|child| *child != id);
		let children = &mut self
			.get_instance_mut(parent)
			.context("destination parent is missing")?
			.children;
		children.push(id);
		children.sort_unstable_by_key(ToString::to_string);
		self.get_instance_mut(id).unwrap().parent = parent;
		Ok(())
	}

	pub fn apply_update(&mut self, snapshot: UpdatedSnapshot) -> Result<()> {
		let id = snapshot.id;
		if let Some(properties) = &snapshot.properties {
			for property in &snapshot.removed_properties {
				ensure!(
					!properties.contains_key(property),
					"property {property} cannot be both set and removed"
				);
			}
		}
		if snapshot.parent.is_some() {
			let current_parent = self.get_instance(id).context("updated instance is missing")?.parent;
			let parent = snapshot.parent.unwrap_or(current_parent);
			ensure!(id != self.root_ref, "cannot reparent the root instance");
			ensure!(self.exists(parent), "destination parent {parent} is missing");
			let mut ancestor = parent;
			while ancestor.is_some() {
				ensure!(ancestor != id, "cannot reparent an instance beneath itself");
				ancestor = self
					.get_instance(ancestor)
					.map(TreeNode::parent)
					.unwrap_or_else(Ref::none);
			}
			self.move_instance(id, parent)?;
		}
		let instance = self.get_instance_mut(id).context("updated instance is missing")?;
		if let Some(name) = snapshot.name {
			instance.name = name;
			if let Some(raw_name) = snapshot.raw_name {
				instance.properties.insert(
					Ustr::from("__CarbonRawName"),
					Variant::BinaryString(raw_name.into_vec().into()),
				);
			} else {
				instance.properties.remove(&Ustr::from("__CarbonRawName"));
			}
		}
		if let Some(class) = snapshot.class {
			instance.class = class;
		}
		if let Some(properties) = snapshot.properties {
			instance.properties.extend(properties);
		}
		for property in snapshot.removed_properties {
			instance.properties.remove(&property);
		}
		Ok(())
	}

	pub fn subtree_refs(&self, id: Ref) -> Result<Vec<Ref>> {
		ensure!(self.exists(id), "instance {id} is missing");
		let mut refs = Vec::new();
		let mut stack = vec![id];
		while let Some(id) = stack.pop() {
			let node = self.get_instance(id).context("subtree instance is missing")?;
			refs.push(id);
			stack.extend(node.children().iter().rev().copied());
		}
		Ok(refs)
	}

	pub fn get_instance(&self, id: Ref) -> Option<&TreeNode> {
		self.nodes.get(*self.by_ref.get(&id)?)
	}

	pub fn get_instance_mut(&mut self, id: Ref) -> Option<&mut TreeNode> {
		let index = *self.by_ref.get(&id)?;
		self.nodes.get_mut(index)
	}

	pub fn exists(&self, id: Ref) -> bool {
		self.by_ref.contains_key(&id)
	}

	pub fn root(&self) -> &TreeNode {
		self.get_instance(self.root_ref).unwrap()
	}

	pub fn root_ref(&self) -> Ref {
		self.root_ref
	}

	pub fn place_root_refs(&self) -> &[Ref] {
		self.root().children()
	}

	/// Canonicalize non-gameplay sibling order for emitted place bytes.
	///
	/// Mapped source identities include source-location provenance and may vary
	/// between separately materialized worktrees. Names and classes are the
	/// portable authored hierarchy; stable identity remains the final tie-breaker
	/// for duplicate Studio-owned siblings.
	pub(crate) fn canonicalize_output_order(&mut self) {
		let keys = self
			.nodes
			.iter()
			.map(|node| (node.referent, (node.class, node.name.clone())))
			.collect::<HashMap<_, _>>();
		for node in &mut self.nodes {
			node.children.sort_unstable_by(|left, right| {
				keys[left]
					.cmp(&keys[right])
					.then_with(|| left.to_string().cmp(&right.to_string()))
			});
		}
	}

	/// Consume the dense arena into a recursive snapshot without cloning node
	/// names or property maps.
	pub(crate) fn into_snapshot(self) -> Result<Snapshot> {
		fn take(id: Ref, nodes: &mut [Option<TreeNode>], by_ref: &HashMap<Ref, usize>) -> Result<Snapshot> {
			let index = *by_ref.get(&id).context("snapshot instance is missing")?;
			let mut node = nodes[index]
				.take()
				.context("snapshot hierarchy contains a duplicate instance")?;
			let raw_name = node
				.properties
				.remove(&Ustr::from("__CarbonRawName"))
				.and_then(|value| match value {
					Variant::BinaryString(value) => Some(serde_bytes::ByteBuf::from(value.into_vec())),
					_ => None,
				});
			let children = node
				.children
				.into_iter()
				.map(|child| take(child, nodes, by_ref))
				.collect::<Result<Vec<_>>>()?;
			Ok(Snapshot {
				id: node.referent,
				name: node.name,
				raw_name,
				class: node.class,
				properties: node.properties,
				children,
			})
		}

		let root = self.root_ref;
		let mut nodes = self.nodes.into_iter().map(Some).collect::<Vec<_>>();
		take(root, &mut nodes, &self.by_ref)
	}

	pub fn snapshot_page(
		&self,
		instance: Ref,
		mut cursor: Vec<Ref>,
		max_instances: usize,
		max_bytes: usize,
	) -> Result<Option<SnapshotPage>> {
		if cursor.is_empty() {
			let root = if instance.is_some() { instance } else { self.root_ref };
			if !self.exists(root) {
				return Ok(None);
			}
			cursor.push(root);
		}
		let mut instances = Vec::with_capacity(max_instances.min(cursor.len().max(1)));
		let mut encoded_bytes = 0;
		while instances.len() < max_instances {
			let Some(id) = cursor.pop() else { break };
			let node = self
				.get_instance(id)
				.context("snapshot cursor contains an unknown instance")?;
			let snapshot = HierarchySnapshot {
				id,
				parent: node.parent,
				name: node.name.clone(),
				raw_name: node
					.properties
					.get(&Ustr::from("__CarbonRawName"))
					.and_then(|value| match value {
						Variant::BinaryString(value) => Some(serde_bytes::ByteBuf::from(value.clone().into_vec())),
						_ => None,
					}),
				class: node.class,
				unavailable: node.unavailable_root,
			};
			let instance_bytes = 96 + snapshot.name.len() + snapshot.class.as_str().len();
			if !instances.is_empty() && encoded_bytes + instance_bytes > max_bytes {
				cursor.push(id);
				break;
			}
			encoded_bytes += instance_bytes;
			instances.push(snapshot);
			if node.live_available {
				cursor.extend(node.children.iter().rev());
			}
		}
		Ok(Some(SnapshotPage {
			done: cursor.is_empty(),
			instances,
			cursor,
			encoded_bytes,
		}))
	}
}

impl rbx_binary::InstanceSource for Tree {
	fn get_by_ref<'a>(&'a self, referent: Ref) -> Option<rbx_binary::InstanceView<'a>> {
		let node = self.get_instance(referent)?;
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn tree_construction_moves_property_storage_into_arena() {
		let payload = String::from("property storage must not be cloned");
		let payload_ptr = payload.as_ptr();
		let mut properties = UstrMap::default();
		properties.insert(Ustr::from("Source"), Variant::String(payload));
		let tree = Tree::new(Snapshot::new().with_class("ModuleScript").with_properties(properties));
		let Some(Variant::String(actual)) = tree.root().properties.get(&Ustr::from("Source")) else {
			panic!("Source property was not preserved");
		};
		assert_eq!(actual.as_ptr(), payload_ptr);
	}

	#[test]
	fn snapshot_conversion_moves_property_storage_out_of_arena() {
		let payload = String::from("property storage must still not be cloned");
		let payload_ptr = payload.as_ptr();
		let mut properties = UstrMap::default();
		properties.insert(Ustr::from("Source"), Variant::String(payload));
		let snapshot = Tree::new(Snapshot::new().with_class("ModuleScript").with_properties(properties))
			.into_snapshot()
			.unwrap();
		let Some(Variant::String(actual)) = snapshot.properties.get(&Ustr::from("Source")) else {
			panic!("Source property was not preserved");
		};
		assert_eq!(actual.as_ptr(), payload_ptr);
	}

	#[test]
	fn siblings_are_canonicalized_by_stable_referent() {
		let root = "00000000000000000000000000000010".parse::<Ref>().unwrap();
		let low = "00000000000000000000000000000001".parse::<Ref>().unwrap();
		let high = "00000000000000000000000000000002".parse::<Ref>().unwrap();
		let mut tree = Tree::new_detached(Snapshot::new().with_id(root), 3).unwrap();
		tree.insert_detached(Snapshot::new().with_id(high), root).unwrap();
		tree.insert_detached(Snapshot::new().with_id(low), root).unwrap();
		tree.finish_detached().unwrap();
		assert_eq!(tree.root().children(), &[low, high]);
	}

	#[test]
	fn arena_removal_repairs_dense_indexes() {
		let removed_id = Ref::new();
		let survivor_id = Ref::new();
		let descendant_id = Ref::new();
		let mut tree = Tree::new(Snapshot::new().with_class("DataModel").with_children(vec![
			Snapshot::new().with_id(removed_id).with_children(vec![Snapshot::new().with_id(descendant_id)]),
			Snapshot::new().with_id(survivor_id).with_name("Survivor"),
		]));
		tree.remove_instance(removed_id);
		assert!(!tree.exists(removed_id));
		assert!(!tree.exists(descendant_id));
		assert_eq!(tree.root().children(), &[survivor_id]);
		assert_eq!(tree.get_instance(survivor_id).unwrap().name, "Survivor");
	}

	#[test]
	fn arena_reparent_updates_both_parent_child_lists() {
		let old_parent = Ref::new();
		let new_parent = Ref::new();
		let child = Ref::new();
		let mut tree = Tree::new(Snapshot::new().with_children(vec![
			Snapshot::new().with_id(old_parent).with_children(vec![Snapshot::new().with_id(child)]),
			Snapshot::new().with_id(new_parent),
		]));
		tree.reparent_instance(child, new_parent);
		assert!(tree.get_instance(old_parent).unwrap().children().is_empty());
		assert_eq!(tree.get_instance(new_parent).unwrap().children(), &[child]);
		assert_eq!(tree.get_instance(child).unwrap().parent(), new_parent);
	}

	#[test]
	fn hierarchy_pages_never_clone_properties() {
		let mut properties = UstrMap::default();
		properties.insert(Ustr::from("Source"), Variant::String("large payload".repeat(100)));
		let tree = Tree::new(Snapshot::new().with_name("Root").with_children(vec![
			Snapshot::new().with_name("A").with_properties(properties).with_children(vec![Snapshot::new().with_name("A1")]),
			Snapshot::new().with_name("B"),
		]));
		let mut cursor = Vec::new();
		let mut names = Vec::new();
		loop {
			let page = tree.snapshot_page(Ref::none(), cursor, 2, 1).unwrap().unwrap();
			assert_eq!(page.instances.len(), 1);
			names.push(page.instances[0].name.clone());
			if page.done {
				break;
			}
			cursor = page.cursor;
		}
		names.sort();
		assert_eq!(names, ["A", "A1", "B", "Root"]);
	}

	#[test]
	fn sparse_property_updates_preserve_unmentioned_canonical_properties() {
		let target = Ref::new();
		let mut tree = Tree::new(Snapshot::new().with_children(vec![
			Snapshot::new()
				.with_id(target)
				.with_class("WrapTarget")
				.with_properties(UstrMap::from_iter([
					(Ustr::from("HSRAssetId"), Variant::Int64(46)),
					(Ustr::from("Stiffness"), Variant::Float32(0.0)),
				])),
		]));
		let mut update = UpdatedSnapshot::new(target);
		update.properties = Some(UstrMap::from_iter([(Ustr::from("Stiffness"), Variant::Float32(0.25))]));

		tree.apply_update(update).unwrap();

		let properties = &tree.get_instance(target).unwrap().properties;
		assert_eq!(properties.get(&Ustr::from("HSRAssetId")), Some(&Variant::Int64(46)));
		assert_eq!(properties.get(&Ustr::from("Stiffness")), Some(&Variant::Float32(0.25)));
	}

	#[test]
	fn sparse_property_updates_remove_explicit_defaults_only() {
		let target = Ref::new();
		let mut tree = Tree::new(Snapshot::new().with_children(vec![
			Snapshot::new()
				.with_id(target)
				.with_class("WrapTarget")
				.with_properties(UstrMap::from_iter([
					(Ustr::from("HSRAssetId"), Variant::Int64(46)),
					(Ustr::from("Stiffness"), Variant::Float32(0.25)),
				])),
		]));
		let mut update = UpdatedSnapshot::new(target);
		update.removed_properties.push(Ustr::from("Stiffness"));

		tree.apply_update(update).unwrap();

		let properties = &tree.get_instance(target).unwrap().properties;
		assert_eq!(properties.get(&Ustr::from("HSRAssetId")), Some(&Variant::Int64(46)));
		assert!(!properties.contains_key(&Ustr::from("Stiffness")));
	}
}
