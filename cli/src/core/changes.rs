use rbx_dom_weak::types::Ref;
use serde::{Deserialize, Serialize};

use super::snapshot::{AddedSnapshot, Snapshot, UpdatedSnapshot};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Changes {
	pub additions: Vec<AddedSnapshot>,
	pub updates: Vec<UpdatedSnapshot>,
	pub removals: Vec<Ref>,
}

impl Changes {
	pub fn new() -> Self {
		Self {
			additions: Vec::new(),
			updates: Vec::new(),
			removals: Vec::new(),
		}
	}

	pub fn add(&mut self, snapshot: Snapshot, parent: Ref) {
		let mut properties = snapshot.properties;
		let raw_name = snapshot.raw_name.or_else(|| {
			properties
				.remove(&rbx_dom_weak::Ustr::from("__CarbonRawName"))
				.and_then(|value| match value {
					rbx_dom_weak::types::Variant::BinaryString(value) => {
						Some(serde_bytes::ByteBuf::from(value.into_vec()))
					}
					_ => None,
				})
		});
		self.additions.push(AddedSnapshot {
			id: snapshot.id,
			parent,
			name: snapshot.name,
			raw_name,
			class: snapshot.class,
			properties,
			children: snapshot.children,
		});
	}

	pub fn update(&mut self, modified_snapshot: UpdatedSnapshot) {
		self.updates.push(modified_snapshot);
	}

	pub fn remove(&mut self, id: Ref) {
		self.removals.push(id);
	}

	pub fn extend(&mut self, changes: Self) {
		self.additions.extend(changes.additions);
		self.updates.extend(changes.updates);
		self.removals.extend(changes.removals);
	}

	pub fn is_empty(&self) -> bool {
		self.additions.is_empty() && self.updates.is_empty() && self.removals.is_empty()
	}

	pub fn total(&self) -> usize {
		self.additions.len() + self.updates.len() + self.removals.len()
	}
}
