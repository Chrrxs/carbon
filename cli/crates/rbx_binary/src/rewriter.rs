use std::{
	collections::{BTreeMap, HashMap},
	convert::TryInto,
	io::{self, Cursor, Read, Write},
};

use rbx_dom_weak::types::Attributes;

use crate::{
	chunk::{Chunk, ChunkBuilder},
	core::{RbxReadExt, RbxWriteExt},
	serializer::CompressionType,
};

const FILE_HEADER_LEN: usize = 32;
const CHUNK_HEADER_LEN: usize = 16;
const ZSTD_MAGIC_NUMBER: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];

fn invalid_data(message: impl Into<String>) -> io::Error {
	io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn decoded_chunk(header: &[u8; CHUNK_HEADER_LEN], payload: &[u8]) -> io::Result<Chunk> {
	let mut encoded = Vec::with_capacity(header.len() + payload.len());
	encoded.extend_from_slice(header);
	encoded.extend_from_slice(payload);
	Chunk::decode(Cursor::new(encoded))
}

fn chunk_compression(header: &[u8; CHUNK_HEADER_LEN], payload: &[u8]) -> CompressionType {
	let compressed_len = u32::from_le_bytes(header[4..8].try_into().unwrap());
	if compressed_len == 0 {
		CompressionType::None
	} else if payload.starts_with(ZSTD_MAGIC_NUMBER) {
		CompressionType::Zstd
	} else {
		CompressionType::Lz4
	}
}

/// Replacement for one Script, LocalScript, or ModuleScript Source value at
/// its zero-based position inside a serialized Roblox class group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptSourcePatch {
	/// Roblox script class containing the Source property.
	pub class_name: String,
	/// Zero-based position within the class group's property columns.
	pub index: u32,
	/// Exact UTF-8 Luau source bytes to serialize.
	pub source: Vec<u8>,
}

/// Rewrite selected script Source values while copying every unrelated binary
/// chunk byte-for-byte.
///
/// Returns the number of requested source entries that were rewritten.
pub fn rewrite_script_sources<R: Read, W: Write>(
	mut input: R,
	mut output: W,
	patches: &[ScriptSourcePatch],
) -> io::Result<usize> {
	let mut requested = BTreeMap::<String, BTreeMap<u32, &[u8]>>::new();
	for patch in patches {
		if requested
			.entry(patch.class_name.clone())
			.or_default()
			.insert(patch.index, &patch.source)
			.is_some()
		{
			return Err(invalid_data("script Source patch repeats a class position"));
		}
	}

	let mut file_header = [0_u8; FILE_HEADER_LEN];
	input.read_exact(&mut file_header)?;
	if &file_header[..8] != b"<roblox!" {
		return Err(invalid_data("input is not a Roblox binary model"));
	}
	output.write_all(&file_header)?;

	let mut types = HashMap::<u32, (String, u32)>::new();
	let mut rewritten = 0_usize;
	loop {
		let mut header = [0_u8; CHUNK_HEADER_LEN];
		input.read_exact(&mut header)?;
		let name: [u8; 4] = header[..4].try_into().unwrap();
		let compressed_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
		let decoded_len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
		let payload_len = if compressed_len == 0 {
			decoded_len
		} else {
			compressed_len
		};
		let mut payload = vec![0_u8; payload_len];
		input.read_exact(&mut payload)?;

		if name == *b"INST" {
			let chunk = decoded_chunk(&header, &payload)?;
			let mut data = chunk.data.as_slice();
			let type_id = data.read_le_u32()?;
			let class_name = data.read_string()?;
			data.read_u8()?;
			let count = data.read_le_u32()?;
			types.insert(type_id, (class_name, count));
		}

		let mut replaced_chunk = false;
		if name == *b"PROP" {
			let chunk = decoded_chunk(&header, &payload)?;
			let mut data = chunk.data.as_slice();
			let type_id = data.read_le_u32()?;
			let property_name = data.read_string()?;
			if property_name == "Source" {
				if let Some((class_name, count)) = types.get(&type_id) {
					if let Some(class_patches) = requested.get(class_name) {
						let property_type = data.read_u8()?;
						let mut builder = ChunkBuilder::new(b"PROP", chunk_compression(&header, &payload));
						builder.write_le_u32(type_id)?;
						builder.write_string(&property_name)?;
						builder.write_u8(property_type)?;
						for index in 0..*count {
							let existing = data.read_binary_string()?;
							if let Some(source) = class_patches.get(&index) {
								builder.write_binary_string(source)?;
								rewritten += 1;
							} else {
								builder.write_binary_string(&existing)?;
							}
						}
						if !data.is_empty() {
							return Err(invalid_data("Script Source column has trailing bytes"));
						}
						builder.dump(&mut output)?;
						replaced_chunk = true;
					}
				}
			}
		}

		if !replaced_chunk {
			output.write_all(&header)?;
			output.write_all(&payload)?;
		}
		if name == *b"END\0" {
			break;
		}
	}
	Ok(rewritten)
}

/// Rewrite the serialized Attributes column for the single Workspace in a
/// Roblox place while copying every unrelated binary chunk byte-for-byte.
///
/// Returns `true` when a Workspace Attributes column was found and rewritten.
/// Models without Workspace return `false` and are otherwise copied unchanged.
pub fn rewrite_workspace_attributes<R, W, F>(mut input: R, mut output: W, rewrite: F) -> io::Result<bool>
where
	R: Read,
	W: Write,
	F: FnOnce(&mut Attributes),
{
	let mut file_header = [0_u8; FILE_HEADER_LEN];
	input.read_exact(&mut file_header)?;
	if &file_header[..8] != b"<roblox!" {
		return Err(invalid_data("input is not a Roblox binary model"));
	}
	output.write_all(&file_header)?;

	let mut workspace_type = None;
	let mut rewrite = Some(rewrite);
	let mut rewritten = false;
	loop {
		let mut header = [0_u8; CHUNK_HEADER_LEN];
		input.read_exact(&mut header)?;
		let name: [u8; 4] = header[..4].try_into().unwrap();
		let compressed_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
		let decoded_len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
		let payload_len = if compressed_len == 0 {
			decoded_len
		} else {
			compressed_len
		};
		let mut payload = vec![0_u8; payload_len];
		input.read_exact(&mut payload)?;

		if name == *b"INST" {
			let chunk = decoded_chunk(&header, &payload)?;
			let mut data = chunk.data.as_slice();
			let type_id = data.read_le_u32()?;
			let class_name = data.read_string()?;
			if class_name == "Workspace" && workspace_type.replace(type_id).is_some() {
				return Err(invalid_data("place declares more than one Workspace type group"));
			}
		}

		let mut replaced_chunk = false;
		if name == *b"PROP" && rewrite.is_some() {
			let chunk = decoded_chunk(&header, &payload)?;
			let mut data = chunk.data.as_slice();
			let type_id = data.read_le_u32()?;
			let property_name = data.read_string()?;
			if Some(type_id) == workspace_type && matches!(property_name.as_str(), "Attributes" | "AttributesSerialize")
			{
				let property_type = data.read_u8()?;
				let encoded = data.read_binary_string()?;
				if !data.is_empty() {
					return Err(invalid_data("Workspace Attributes column contains multiple values"));
				}
				let mut attributes =
					Attributes::from_reader(encoded.as_slice()).map_err(|error| invalid_data(error.to_string()))?;
				rewrite.take().unwrap()(&mut attributes);
				let mut encoded = Vec::new();
				attributes
					.to_writer(&mut encoded)
					.map_err(|error| invalid_data(error.to_string()))?;

				let mut builder = ChunkBuilder::new(b"PROP", chunk_compression(&header, &payload));
				builder.write_le_u32(type_id)?;
				builder.write_string(&property_name)?;
				builder.write_u8(property_type)?;
				builder.write_binary_string(&encoded)?;
				builder.dump(&mut output)?;
				rewritten = true;
				replaced_chunk = true;
			}
		}

		if !replaced_chunk {
			output.write_all(&header)?;
			output.write_all(&payload)?;
		}
		if name == *b"END\0" {
			break;
		}
	}
	Ok(rewritten)
}
