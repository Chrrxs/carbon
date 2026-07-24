mod error;
mod state;

use std::{
	collections::{BTreeMap, HashMap, HashSet},
	io::Write,
};

use rbx_dom_weak::{
	types::{Ref, SharedString, Variant},
	Ustr, UstrMap, WeakDom,
};
use rbx_reflection::ReflectionDatabase;

use self::state::SerializerState;

pub use self::error::Error;

/// Logical payload counts and columns produced by one serialization.
#[derive(Clone, Debug, Default)]
pub struct SerializationReport {
	/// Canonical properties that a strict decoder will materialize. Structural
	/// UTF-8 names are excluded; exact raw-name properties are included.
	pub properties: u64,
	/// Canonical `(class, property)` columns present in the payload.
	pub property_columns: Vec<(Ustr, Ustr)>,
}

/// Location of one selected instance inside its serialized Roblox class group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SerializedInstancePosition {
	/// Roblox class name of the serialized instance group.
	pub class_name: String,
	/// Zero-based position within that class group's INST and PROP columns.
	pub index: u32,
}

/// Incremental binary writer for bounded type groups.
///
/// Callers may emit multiple groups with the same Roblox class as long as each
/// group has a distinct type ID. This keeps property chunks bounded while
/// preserving one global referent namespace.
pub struct StreamingSerializer<'db, W> {
	serializer: Serializer<'db>,
	writer: W,
}

/// A borrowed instance record consumed by the binary serializer.
///
/// This view lets callers serialize compact arenas and streaming compiler
/// records without first materializing a [`WeakDom`].
#[derive(Clone, Copy, Debug)]
pub struct InstanceView<'a> {
	/// Transient identity used to connect parents and reference properties.
	pub referent: Ref,
	/// Parent identity, or [`Ref::none`] for a serialized root.
	pub parent: Ref,
	/// Roblox class name.
	pub class: Ustr,
	/// Roblox instance name.
	pub name: &'a str,
	/// Original Name bytes when the serialized value is not valid UTF-8.
	pub raw_name: Option<&'a [u8]>,
	/// Explicit serialized properties.
	pub properties: &'a UstrMap<Variant>,
	/// Child identities in semantic sibling order.
	pub children: &'a [Ref],
}

/// Read-only instance source accepted by the binary serializer.
///
/// Implementations must keep every returned name, property map, and child
/// slice alive for the duration of serialization. Child order is preserved.
pub trait InstanceSource: Sync {
	/// Borrow one instance by its transient identity.
	fn get_by_ref<'a>(&'a self, referent: Ref) -> Option<InstanceView<'a>>;
}

impl InstanceSource for WeakDom {
	fn get_by_ref<'a>(&'a self, referent: Ref) -> Option<InstanceView<'a>> {
		let instance = WeakDom::get_by_ref(self, referent)?;
		Some(InstanceView {
			referent: instance.referent(),
			parent: instance.parent(),
			class: instance.class,
			name: &instance.name,
			raw_name: instance
				.properties
				.get(&Ustr::from("__CarbonRawName"))
				.and_then(|value| match value {
					Variant::BinaryString(value) => Some(value.as_ref()),
					_ => None,
				}),
			properties: &instance.properties,
			children: instance.children(),
		})
	}
}

/// A configurable serializer for Roblox binary models and places.
///
/// ## Example
/// ```no_run
/// use std::fs::File;
/// use std::io::BufWriter;
///
/// use rbx_binary::Serializer;
/// use rbx_dom_weak::{InstanceBuilder, WeakDom};
///
/// let dom = WeakDom::new(InstanceBuilder::new("Folder"));
///
/// let output = BufWriter::new(File::create("PlainFolder.rbxm")?);
/// let serializer = Serializer::new();
/// serializer.serialize(output, &dom, &[dom.root_ref()])?;
///
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// ## Configuration
///
/// A custom [`ReflectionDatabase`][ReflectionDatabase] can be specified via
/// [`reflection_database`][reflection_database].
///
/// By default, the Serializer uses LZ4 compression, mimicking Roblox. This can
/// be changed via [`compression_type`][compression_type].
///
/// [ReflectionDatabase]: rbx_reflection::ReflectionDatabase
/// [reflection_database]: Serializer#method.reflection_database
/// [compression_type]: Serializer#method.compression_type
//
// future settings:
// * recursive: bool = true
#[non_exhaustive]
pub struct Serializer<'db> {
	database: &'db ReflectionDatabase<'db>,
	compression: CompressionType,
	metadata: BTreeMap<String, String>,
	property_workers: usize,
}

impl<'db> Serializer<'db> {
	/// Create a new `Serializer` with the default settings.
	pub fn new() -> Self {
		Serializer {
			database: rbx_reflection_database::get().unwrap(),
			compression: CompressionType::default(),
			metadata: BTreeMap::new(),
			property_workers: 4,
		}
	}

	/// Sets what reflection database for the serializer to use.
	#[inline]
	pub fn reflection_database(self, database: &'db ReflectionDatabase<'db>) -> Self {
		Self { database, ..self }
	}

	/// Sets what type of compression the serializer will use for compression.
	#[inline]
	pub fn compression_type(self, compression: CompressionType) -> Self {
		Self { compression, ..self }
	}

	/// Sets file-level key/value metadata written to the META chunk.
	///
	/// Entries are emitted in key order so equivalent metadata has one
	/// canonical binary representation.
	#[inline]
	pub fn metadata(self, metadata: BTreeMap<String, String>) -> Self {
		Self { metadata, ..self }
	}

	/// Sets the maximum number of property columns encoded concurrently.
	///
	/// Values above four are capped to keep transient encoded chunks bounded.
	/// A value of one forces the deterministic serial path.
	#[inline]
	pub fn property_workers(self, workers: usize) -> Self {
		assert!(workers > 0, "property worker count must be positive");
		Self {
			property_workers: workers.min(4),
			..self
		}
	}

	/// Serialize a Roblox binary model or place into the given stream using
	/// this serializer.
	pub fn serialize<W: Write>(&self, writer: W, dom: &WeakDom, refs: &[Ref]) -> Result<(), Error> {
		self.serialize_source(writer, dom, refs)
	}

	/// Serialize instances from any read-only instance source.
	pub fn serialize_source<W: Write>(
		&self,
		writer: W,
		source: &dyn InstanceSource,
		refs: &[Ref],
	) -> Result<(), Error> {
		self.serialize_source_with_report(writer, source, refs).map(|_| ())
	}

	/// Serialize instances and report the logical property representation that
	/// was actually emitted after reflection defaults and migrations.
	pub fn serialize_source_with_report<W: Write>(
		&self,
		writer: W,
		source: &dyn InstanceSource,
		refs: &[Ref],
	) -> Result<SerializationReport, Error> {
		profiling::scope!("rbx_binary::seserialize");

		let mut serializer = SerializerState::new(self, source, writer);

		serializer.add_instances(refs)?;
		serializer.generate_referents();
		serializer.write_header()?;
		serializer.serialize_metadata()?;
		serializer.serialize_shared_strings()?;
		serializer.serialize_instances()?;
		let report = serializer.serialize_properties()?;
		serializer.serialize_parents()?;
		serializer.serialize_end()?;

		Ok(report)
	}

	/// Serialize instances and report positions for only the selected referents.
	///
	/// This supports bounded binary patch indexes without retaining a position
	/// map for every instance in very large places.
	pub fn serialize_source_with_report_and_index<W: Write>(
		&self,
		writer: W,
		source: &dyn InstanceSource,
		refs: &[Ref],
		indexed_refs: &HashSet<Ref>,
	) -> Result<(SerializationReport, HashMap<Ref, SerializedInstancePosition>), Error> {
		profiling::scope!("rbx_binary::seserialize");

		let mut serializer = SerializerState::new(self, source, writer);
		serializer.add_instances(refs)?;
		let positions = serializer.instance_positions(indexed_refs);
		serializer.generate_referents();
		serializer.write_header()?;
		serializer.serialize_metadata()?;
		serializer.serialize_shared_strings()?;
		serializer.serialize_instances()?;
		let report = serializer.serialize_properties()?;
		serializer.serialize_parents()?;
		serializer.serialize_end()?;

		Ok((report, positions))
	}

	/// Start an incremental binary serialization session.
	pub fn streaming<W: Write>(self, writer: W) -> StreamingSerializer<'db, W> {
		StreamingSerializer {
			serializer: self,
			writer,
		}
	}
}

impl<W: Write> StreamingSerializer<'_, W> {
	/// Write the file header before any type groups.
	pub fn write_header(&mut self, type_count: u32, instance_count: u32) -> Result<(), Error> {
		state::write_stream_header(&mut self.writer, type_count, instance_count).map_err(Into::into)
	}

	/// Write canonical file-level key/value metadata after the header.
	pub fn write_metadata(&mut self, metadata: &BTreeMap<String, String>) -> Result<(), Error> {
		state::write_stream_metadata(&mut self.writer, self.serializer.compression, metadata).map_err(Into::into)
	}

	/// Write the global shared-string table before property groups.
	pub fn write_shared_strings(&mut self, values: &[SharedString]) -> Result<(), Error> {
		state::write_stream_shared_strings(&mut self.writer, self.serializer.compression, values).map_err(Into::into)
	}

	/// Write one bounded INST group.
	pub fn write_type_group_instances(
		&mut self,
		source: &dyn InstanceSource,
		instances: &[Ref],
		type_id: u32,
		referents: &[(Ref, i32)],
	) -> Result<(), Error> {
		let mut state = SerializerState::new(&self.serializer, source, &mut self.writer);
		state.seed_referents(referents);
		state.add_instances(instances)?;
		state.set_single_type_id(type_id);
		state.serialize_instances()?;
		Ok(())
	}

	/// Write one bounded PROP group after all INST groups are declared.
	pub fn write_type_group_properties(
		&mut self,
		source: &dyn InstanceSource,
		instances: &[Ref],
		type_id: u32,
		referents: &[(Ref, i32)],
		shared_strings: &[(SharedString, u32)],
	) -> Result<(), Error> {
		let mut state = SerializerState::new(&self.serializer, source, &mut self.writer);
		state.seed_referents(referents);
		state.seed_shared_strings(shared_strings);
		state.add_instances(instances)?;
		state.set_single_type_id(type_id);
		let _ = state.serialize_properties()?;
		Ok(())
	}

	/// Write global hierarchy relations after all type groups.
	pub fn write_parents(&mut self, relations: &[(i32, i32)]) -> Result<(), Error> {
		state::write_stream_parents(&mut self.writer, self.serializer.compression, relations).map_err(Into::into)
	}

	/// Finish the binary file and return the output writer.
	pub fn finish(mut self) -> Result<W, Error> {
		state::write_stream_end(&mut self.writer).map_err(Error::from)?;
		Ok(self.writer)
	}
}

impl Default for Serializer<'_> {
	fn default() -> Self {
		Self::new()
	}
}

/// Indicates the types of compression that files can be written with.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub enum CompressionType {
	/// LZ4 compression. This is what Roblox uses by default.
	#[default]
	Lz4,
	/// No compression.
	None,
	/// ZSTD compression.
	Zstd,
}
