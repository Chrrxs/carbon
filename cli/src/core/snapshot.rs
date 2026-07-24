use rbx_dom_weak::{types::Ref, Ustr};
use serde::{ser::SerializeStruct, Deserialize, Serialize, Serializer};
use serde_bytes::ByteBuf;

use crate::Properties;

fn is_false(value: &bool) -> bool {
	!*value
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
	pub id: Ref,
	pub name: String,
	#[serde(default, rename = "rawName", skip_serializing_if = "Option::is_none")]
	pub raw_name: Option<ByteBuf>,
	pub class: Ustr,
	pub properties: Properties,
	pub children: Vec<Snapshot>,
}

impl Serialize for Snapshot {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		let mut snapshot = serializer.serialize_struct("Snapshot", if self.raw_name.is_some() { 6 } else { 5 })?;
		snapshot.serialize_field("id", &self.id)?;
		snapshot.serialize_field("name", &self.name)?;
		if let Some(raw_name) = &self.raw_name {
			snapshot.serialize_field("rawName", raw_name)?;
		}
		snapshot.serialize_field("class", &self.class)?;
		snapshot.serialize_field("properties", &crate::artifact_store::WireProperties(&self.properties))?;
		snapshot.serialize_field("children", &self.children)?;
		snapshot.end()
	}
}

impl Snapshot {
	pub fn new() -> Self {
		Self {
			id: Ref::none(),
			name: String::new(),
			raw_name: None,
			class: Ustr::from("Folder"),
			properties: Properties::default(),
			children: Vec::new(),
		}
	}

	pub fn with_id(mut self, id: Ref) -> Self {
		self.id = id;
		self
	}

	pub fn with_name(mut self, name: &str) -> Self {
		self.name = name.to_owned();
		self
	}

	pub fn with_class(mut self, class: &str) -> Self {
		self.class = Ustr::from(class);
		self
	}

	pub fn with_properties(mut self, properties: Properties) -> Self {
		self.properties = properties;
		self
	}

	pub fn with_children(mut self, children: Vec<Snapshot>) -> Self {
		self.children = children;
		self
	}
}

impl Default for Snapshot {
	fn default() -> Self {
		Self::new()
	}
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddedSnapshot {
	pub id: Ref,
	pub parent: Ref,
	pub name: String,
	#[serde(default, rename = "rawName", skip_serializing_if = "Option::is_none")]
	pub raw_name: Option<ByteBuf>,
	pub class: Ustr,
	pub properties: Properties,
	pub children: Vec<Snapshot>,
}

impl Serialize for AddedSnapshot {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		let mut snapshot = serializer.serialize_struct("AddedSnapshot", 6 + usize::from(self.raw_name.is_some()))?;
		snapshot.serialize_field("id", &self.id)?;
		snapshot.serialize_field("parent", &self.parent)?;
		snapshot.serialize_field("name", &self.name)?;
		if let Some(raw_name) = &self.raw_name {
			snapshot.serialize_field("rawName", raw_name)?;
		}
		snapshot.serialize_field("class", &self.class)?;
		snapshot.serialize_field("properties", &crate::artifact_store::WireProperties(&self.properties))?;
		snapshot.serialize_field("children", &self.children)?;
		snapshot.end()
	}
}

/// Bounded, flat pre-order hierarchy page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HierarchySnapshot {
	pub id: Ref,
	pub parent: Ref,
	pub name: String,
	#[serde(default, rename = "rawName", skip_serializing_if = "Option::is_none")]
	pub raw_name: Option<ByteBuf>,
	pub class: Ustr,
	#[serde(default, skip_serializing_if = "is_false")]
	pub unavailable: bool,
}

/// Bounded, flat pre-order hierarchy page.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotPage {
	pub instances: Vec<HierarchySnapshot>,
	pub cursor: Vec<Ref>,
	pub done: bool,
	pub encoded_bytes: usize,
}

impl From<AddedSnapshot> for Snapshot {
	fn from(snapshot: AddedSnapshot) -> Self {
		Self {
			id: snapshot.id,
			name: snapshot.name,
			raw_name: snapshot.raw_name,
			class: snapshot.class,
			properties: snapshot.properties,
			children: snapshot.children,
		}
	}
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatedSnapshot {
	pub id: Ref,
	pub parent: Option<Ref>,
	pub name: Option<String>,
	#[serde(default, rename = "rawName", skip_serializing_if = "Option::is_none")]
	pub raw_name: Option<ByteBuf>,
	pub class: Option<Ustr>,
	/// Sparse property values to set. Properties omitted from this map are
	/// intentionally left untouched.
	pub properties: Option<Properties>,
	/// Canonical property names to reset to their reflection defaults.
	#[serde(default, rename = "removedProperties", skip_serializing_if = "Vec::is_empty")]
	pub removed_properties: Vec<Ustr>,
}

impl Serialize for UpdatedSnapshot {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		let mut snapshot = serializer.serialize_struct(
			"UpdatedSnapshot",
			5 + usize::from(self.raw_name.is_some()) + usize::from(!self.removed_properties.is_empty()),
		)?;
		snapshot.serialize_field("id", &self.id)?;
		snapshot.serialize_field("parent", &self.parent)?;
		snapshot.serialize_field("name", &self.name)?;
		if let Some(raw_name) = &self.raw_name {
			snapshot.serialize_field("rawName", raw_name)?;
		}
		snapshot.serialize_field("class", &self.class)?;
		match &self.properties {
			Some(properties) => {
				snapshot.serialize_field("properties", &Some(crate::artifact_store::WireProperties(properties)))?
			}
			None => snapshot.serialize_field("properties", &Option::<u8>::None)?,
		}
		if !self.removed_properties.is_empty() {
			snapshot.serialize_field("removedProperties", &self.removed_properties)?;
		}
		snapshot.end()
	}
}

impl UpdatedSnapshot {
	pub fn new(id: Ref) -> Self {
		Self {
			id,
			parent: None,
			name: None,
			raw_name: None,
			class: None,
			properties: None,
			removed_properties: Vec::new(),
		}
	}

	pub fn is_empty(&self) -> bool {
		self.parent.is_none()
			&& self.name.is_none()
			&& self.class.is_none()
			&& self.properties.is_none()
			&& self.removed_properties.is_empty()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn property_removals_use_the_protocol_v2_camel_case_wire_key() {
		let mut update = UpdatedSnapshot::new(Ref::new());
		update.removed_properties.push(Ustr::from("Stiffness"));
		let value = serde_json::to_value(update).unwrap();
		assert_eq!(value["removedProperties"], serde_json::json!(["Stiffness"]));
		assert!(value.get("removed_properties").is_none());
	}
}
