mod error;
mod header;
mod state;

use std::{
	collections::{BTreeMap, HashMap},
	io::Read,
	str,
	sync::LazyLock,
};

use rbx_dom_weak::{
	types::{Ref, Variant},
	InstanceBuilder, Ustr, UstrMap, WeakDom,
};
use rbx_reflection::ReflectionDatabase;

use self::{error::InnerError, state::DeserializerState};

#[cfg(any(test, feature = "unstable_text_format"))]
pub(crate) use self::header::FileHeader;

pub use self::error::Error;

use crate::{InstanceSource, InstanceView};

/// Receives decoded properties as soon as each binary PROP column is read.
///
/// A sink allows large extractors to spool variable payloads instead of
/// retaining every property in the decoded arena.
pub trait DecodeSink {
	/// Consume one canonical property value for an instance.
	fn property(&mut self, referent: Ref, name: Ustr, value: Variant) -> Result<(), String>;
}

/// One owned instance from a compact decoded arena.
///
/// Consuming these records lets downstream canonical stores move decoded
/// names and properties into their own arenas without cloning a place-wide
/// property graph.
pub struct DecodedInstance {
	/// File-local referent assigned by the decoder.
	pub referent: Ref,
	/// File-local parent referent, or [`Ref::none`] for the synthetic root.
	pub parent: Ref,
	/// Roblox class name.
	pub class: Ustr,
	/// Decoded UTF-8 instance name.
	pub name: String,
	/// Explicit properties retained from the payload.
	pub properties: UstrMap<Variant>,
	/// File-local child referents in payload hierarchy order.
	pub children: Vec<Ref>,
}

pub(super) struct StructureNode {
	pub referent: Ref,
	pub parent: Ref,
	pub class: Ustr,
	pub name: String,
	pub children: Vec<Ref>,
}

/// Compact hierarchy-only representation of a binary place.
///
/// Unlike [`DecodedArena`], this type has no per-node property storage.
pub struct DecodedStructure {
	pub(super) nodes: Vec<StructureNode>,
	by_ref: HashMap<Ref, usize>,
	root: Ref,
	metadata: BTreeMap<String, String>,
}

impl DecodedStructure {
	pub(super) fn new(nodes: Vec<StructureNode>, root: Ref, metadata: BTreeMap<String, String>) -> Self {
		let by_ref = nodes
			.iter()
			.enumerate()
			.map(|(index, node)| (node.referent, index))
			.collect();
		Self {
			nodes,
			by_ref,
			root,
			metadata,
		}
	}

	/// Synthetic DataModel root containing every serialized root.
	pub fn root_ref(&self) -> Ref {
		self.root
	}

	/// File-level key/value metadata decoded from META chunks.
	pub fn metadata(&self) -> &BTreeMap<String, String> {
		&self.metadata
	}
}

impl InstanceSource for DecodedStructure {
	fn get_by_ref<'a>(&'a self, referent: Ref) -> Option<InstanceView<'a>> {
		static EMPTY_PROPERTIES: LazyLock<UstrMap<Variant>> = LazyLock::new(UstrMap::default);
		let node = &self.nodes[*self.by_ref.get(&referent)?];
		Some(InstanceView {
			referent: node.referent,
			parent: node.parent,
			class: node.class,
			name: &node.name,
			raw_name: None,
			properties: &EMPTY_PROPERTIES,
			children: &node.children,
		})
	}
}

/// Compact decoded binary place representation.
///
/// Instances and properties are decoded exactly once and can be consumed by
/// source extractors or the binary serializer without constructing a
/// [`WeakDom`].
pub struct DecodedArena {
	pub(super) nodes: Vec<DecodedInstance>,
	by_ref: HashMap<Ref, usize>,
	root: Ref,
	metadata: BTreeMap<String, String>,
}

impl DecodedArena {
	pub(super) fn new(nodes: Vec<DecodedInstance>, root: Ref, metadata: BTreeMap<String, String>) -> Self {
		let by_ref = nodes
			.iter()
			.enumerate()
			.map(|(index, node)| (node.referent, index))
			.collect();
		Self {
			nodes,
			by_ref,
			root,
			metadata,
		}
	}

	/// Synthetic DataModel root containing every root serialized in the file.
	pub fn root_ref(&self) -> Ref {
		self.root
	}

	/// File-level key/value metadata decoded from META chunks.
	pub fn metadata(&self) -> &BTreeMap<String, String> {
		&self.metadata
	}

	/// Consume the arena into owned instance records.
	pub fn into_instances(self) -> Vec<DecodedInstance> {
		self.nodes
	}

	/// Convert into the legacy DOM representation.
	pub fn into_dom(mut self) -> WeakDom {
		let root_index = self.by_ref[&self.root];
		let root = self.nodes.remove(root_index);
		let mut dom = WeakDom::new(
			InstanceBuilder::new(root.class)
				.with_referent(root.referent)
				.with_name(root.name)
				.with_properties(root.properties),
		);
		for node in self.nodes {
			dom.insert(
				node.parent,
				InstanceBuilder::new(node.class)
					.with_referent(node.referent)
					.with_name(node.name)
					.with_properties(node.properties),
			);
		}
		dom
	}
}

impl InstanceSource for DecodedArena {
	fn get_by_ref<'a>(&'a self, referent: Ref) -> Option<InstanceView<'a>> {
		let node = &self.nodes[*self.by_ref.get(&referent)?];
		Some(InstanceView {
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

/// A configurable deserializer for Roblox binary models and places.
///
/// ## Example
/// ```no_run
/// use std::fs::File;
/// use std::io::BufReader;
///
/// use rbx_binary::Deserializer;
/// use rbx_reflection::ReflectionDatabase;
///
/// let input = BufReader::new(File::open("File.rbxm")?);
///
/// let database = ReflectionDatabase::new();
/// let deserializer = Deserializer::new(&database);
/// let dom = deserializer.deserialize(input)?;
///
/// // rbx_binary always returns a DOM with a DataModel at the top level.
/// // To get to the instances from our file, we need to go one level deeper.
///
/// println!("Root instances in file:");
/// for &referent in dom.root().children() {
///     let instance = dom.get_by_ref(referent).unwrap();
///     println!("- {}", instance.name);
/// }
///
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// ## Configuration
///
/// A custom [`ReflectionDatabase`][ReflectionDatabase] can be specified via
/// [`reflection_database`][reflection_database].
///
/// [ReflectionDatabase]: rbx_reflection::ReflectionDatabase
/// [reflection_database]: Deserializer#method.reflection_database
static EMPTY_DATABASE: std::sync::LazyLock<ReflectionDatabase<'static>> =
	std::sync::LazyLock::new(ReflectionDatabase::new);

/// Configures deserialization using an explicit reflection database.
pub struct Deserializer<'db> {
	database: &'db ReflectionDatabase<'db>,
	strict: bool,
	skip_known_non_serializing_properties: bool,
}

impl<'db> Deserializer<'db> {
	/// Create a new `Deserializer` using the provided reflection database.
	pub fn new(database: &'db ReflectionDatabase<'db>) -> Self {
		Self {
			database,
			strict: false,
			skip_known_non_serializing_properties: false,
		}
	}

	/// Create a new `Deserializer` with an empty reflection database.
	pub fn new_empty() -> Deserializer<'static> {
		Deserializer {
			database: &EMPTY_DATABASE,
			strict: false,
			skip_known_non_serializing_properties: false,
		}
	}

	/// Sets what reflection database for the deserializer to use.
	#[inline]
	pub fn reflection_database(mut self, database: &'db ReflectionDatabase<'db>) -> Self {
		self.database = database;
		self
	}

	/// Reject every property or chunk that would otherwise be skipped or
	/// preserved through a compatibility fallback.
	///
	/// This is intended for authoritative capture paths that must fail instead
	/// of silently accepting data the current decoder cannot represent. It is
	/// disabled by default so ordinary imports retain their compatibility
	/// behavior.
	#[inline]
	pub fn strict(mut self, strict: bool) -> Self {
		self.strict = strict;
		self
	}

	/// Allows strict capture decoding to omit properties that the current
	/// reflection schema knows Studio cannot load back from a saved place.
	///
	/// Studio may still emit save-only properties in native capture chunks.
	/// This exception remains narrow: unknown properties and malformed chunks
	/// retain strict failure behavior.
	#[inline]
	pub fn skip_known_non_serializing_properties(mut self, skip: bool) -> Self {
		self.skip_known_non_serializing_properties = skip;
		self
	}

	/// Deserialize a Roblox binary model or place from the given stream using
	/// this deserializer.
	pub fn deserialize<R: Read>(&self, reader: R) -> Result<WeakDom, Error> {
		Ok(self.deserialize_source(reader)?.into_dom())
	}

	/// Deserialize into a compact arena without constructing a [`WeakDom`].
	pub fn deserialize_source<R: Read>(&self, reader: R) -> Result<DecodedArena, Error> {
		self.deserialize_source_inner(reader, None, false)
	}

	/// Deserialize into a compact retained arena without reserving every
	/// reflected default property on every instance.
	///
	/// This is intended for large canonical stores that retain only properties
	/// explicitly present in the payload and then consume the arena directly.
	pub fn deserialize_compact_source<R: Read>(&self, reader: R) -> Result<DecodedArena, Error> {
		self.deserialize_source_inner(reader, None, true)
	}

	/// Deserialize only classes, names, parentage, and sibling order.
	///
	/// This is intended for extractors that make a second property pass and
	/// need stable routing without retaining place-wide property payloads.
	pub fn deserialize_structure<R: Read>(&self, reader: R) -> Result<DecodedStructure, Error> {
		profiling::scope!("rbx_binary::deserialize_structure");
		let mut deserializer = DeserializerState::new(self, reader, None, true, false)?;
		loop {
			let chunk = deserializer.next_chunk()?;
			match &chunk.name {
				b"META" => deserializer.decode_meta_chunk(&chunk.data)?,
				b"SSTR" => {}
				b"INST" => deserializer.decode_inst_chunk(&chunk.data)?,
				b"PROP" => deserializer.decode_name_prop_chunk(&chunk.data)?,
				b"PRNT" => deserializer.decode_prnt_chunk(&chunk.data)?,
				b"END\0" => break,
				_ if self.strict => {
					return Err(InnerError::StrictUnknownChunk {
						name: String::from_utf8_lossy(&chunk.name).into_owned(),
					}
					.into())
				}
				_ => {}
			}
		}
		Ok(deserializer.finish_structure())
	}

	/// Deserialize structure into an arena while streaming properties to a sink.
	pub fn deserialize_source_with_sink<R: Read>(
		&self,
		reader: R,
		sink: &mut dyn DecodeSink,
	) -> Result<DecodedArena, Error> {
		self.deserialize_source_inner(reader, Some(sink), false)
	}

	/// Decode property columns to a sink without retaining a resulting arena.
	///
	/// Referents match those returned by [`Self::deserialize_structure`] for a
	/// second pass over the same binary file.
	pub fn deserialize_properties_with_sink<R: Read>(&self, reader: R, sink: &mut dyn DecodeSink) -> Result<(), Error> {
		profiling::scope!("rbx_binary::deserialize_properties");
		let mut deserializer = DeserializerState::new(self, reader, Some(sink), true, false)?;
		loop {
			let chunk = deserializer.next_chunk()?;
			match &chunk.name {
				b"META" | b"PRNT" => {}
				b"SSTR" => deserializer.decode_sstr_chunk(&chunk.data)?,
				b"INST" => deserializer.decode_inst_chunk(&chunk.data)?,
				b"PROP" => deserializer.decode_prop_chunk(&chunk.data)?,
				b"END\0" => break,
				_ if self.strict => {
					return Err(InnerError::StrictUnknownChunk {
						name: String::from_utf8_lossy(&chunk.name).into_owned(),
					}
					.into())
				}
				_ => {}
			}
		}
		Ok(())
	}

	fn deserialize_source_inner<R: Read>(
		&self,
		reader: R,
		sink: Option<&mut dyn DecodeSink>,
		compact_properties: bool,
	) -> Result<DecodedArena, Error> {
		profiling::scope!("rbx_binary::deserialize");

		let lean_properties = sink.is_some() || compact_properties;
		let mut deserializer = DeserializerState::new(self, reader, sink, lean_properties, compact_properties)?;

		loop {
			let chunk = deserializer.next_chunk()?;

			match &chunk.name {
				b"META" => deserializer.decode_meta_chunk(&chunk.data)?,
				b"SSTR" => deserializer.decode_sstr_chunk(&chunk.data)?,
				b"INST" => deserializer.decode_inst_chunk(&chunk.data)?,
				b"PROP" => deserializer.decode_prop_chunk(&chunk.data)?,
				b"PRNT" => deserializer.decode_prnt_chunk(&chunk.data)?,
				b"END\0" => {
					deserializer.decode_end_chunk(&chunk.data)?;
					break;
				}
				_ if self.strict => {
					return Err(InnerError::StrictUnknownChunk {
						name: String::from_utf8_lossy(&chunk.name).into_owned(),
					}
					.into())
				}
				_ => match str::from_utf8(&chunk.name) {
					Ok(name) => log::info!("Unknown binary chunk name {name}"),
					Err(_) => log::info!("Unknown binary chunk name {:?}", chunk.name),
				},
			}
		}

		Ok(deserializer.finish_arena())
	}
}

impl Default for Deserializer<'static> {
	fn default() -> Self {
		Self::new_empty()
	}
}
