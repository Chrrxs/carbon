use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::collections::VecDeque;
use std::collections::{BTreeMap, HashSet};

pub(crate) const PREFIX_BYTES: usize = 14;
const COUNTER_BYTES: usize = 2;
const IDENTIFIER_BYTES: usize = PREFIX_BYTES + COUNTER_BYTES;
const COUNTERS_PER_PREFIX: u32 = u16::MAX as u32 + 1;

/// Allocates stable 128-bit Carbon identities from collision-resistant blocks.
///
/// A prefix is never resumed: allocator construction and block exhaustion both
/// draw a fresh 112-bit prefix. The counter is exact within that prefix, so a
/// native lifetime receives one identity without paying 128 random bits in the
/// canonical artifact for every instance.
pub(crate) struct ManifestIdentityAllocator {
	prefix_source: Box<dyn FnMut() -> [u8; PREFIX_BYTES]>,
	issued_prefixes: HashSet<[u8; PREFIX_BYTES]>,
	current_prefix: Option<[u8; PREFIX_BYTES]>,
	next_counter: u32,
}

impl ManifestIdentityAllocator {
	pub(crate) fn new() -> Self {
		Self::with_prefix_source(|| {
			let mut prefix = [0; PREFIX_BYTES];
			getrandom::fill(&mut prefix).expect("operating-system identity randomness is unavailable");
			prefix
		})
	}

	fn with_prefix_source(source: impl FnMut() -> [u8; PREFIX_BYTES] + 'static) -> Self {
		Self {
			prefix_source: Box::new(source),
			issued_prefixes: HashSet::new(),
			current_prefix: None,
			next_counter: COUNTERS_PER_PREFIX,
		}
	}

	#[cfg(test)]
	fn from_prefixes(prefixes: impl IntoIterator<Item = [u8; PREFIX_BYTES]>) -> Self {
		let mut prefixes = prefixes.into_iter().collect::<VecDeque<_>>();
		Self::with_prefix_source(move || prefixes.pop_front().expect("test identity prefixes are exhausted"))
	}

	fn rotate(&mut self) {
		loop {
			let prefix = (self.prefix_source)();
			if prefix != [0; PREFIX_BYTES] && self.issued_prefixes.insert(prefix) {
				self.current_prefix = Some(prefix);
				self.next_counter = 0;
				return;
			}
		}
	}

	pub(crate) fn next(&mut self) -> u128 {
		if self.next_counter == COUNTERS_PER_PREFIX {
			self.rotate();
		}
		let prefix = self.current_prefix.expect("identity allocator has no active prefix");
		let counter = self.next_counter as u16;
		self.next_counter += 1;
		let mut bytes = [0; IDENTIFIER_BYTES];
		bytes[..PREFIX_BYTES].copy_from_slice(&prefix);
		bytes[PREFIX_BYTES..].copy_from_slice(&counter.to_be_bytes());
		u128::from_be_bytes(bytes)
	}
}

/// Canonical columnar representation of the stable identity assigned to each
/// dense RBXL referent ordinal.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct IdentityColumn {
	#[serde(with = "serde_bytes")]
	prefixes: Vec<u8>,
	index_bits: u8,
	#[serde(with = "serde_bytes")]
	block_indexes: Vec<u8>,
	#[serde(with = "serde_bytes")]
	counters: Vec<u8>,
}

impl IdentityColumn {
	pub(crate) fn encode(ids: &[[u8; IDENTIFIER_BYTES]]) -> Result<Self> {
		let mut ordered = ids
			.iter()
			.map(|id| id[..PREFIX_BYTES].try_into().expect("identity prefix has fixed width"))
			.collect::<Vec<[u8; PREFIX_BYTES]>>();
		ordered.sort_unstable();
		ordered.dedup();
		let indexes = ordered
			.iter()
			.copied()
			.enumerate()
			.map(|(index, prefix)| (prefix, index))
			.collect::<BTreeMap<_, _>>();
		let index_bits = required_index_bits(ordered.len());
		let mut block_indexes = vec![0; packed_len(ids.len(), index_bits)?];
		let mut counters = Vec::with_capacity(ids.len() * COUNTER_BYTES);
		for (ordinal, id) in ids.iter().enumerate() {
			ensure!(*id != [0; IDENTIFIER_BYTES], "Carbon identity cannot be zero");
			let prefix: [u8; PREFIX_BYTES] = id[..PREFIX_BYTES].try_into().unwrap();
			pack_index(&mut block_indexes, ordinal, index_bits, indexes[&prefix]);
			counters.extend_from_slice(&id[PREFIX_BYTES..]);
		}
		let mut prefixes = Vec::with_capacity(ordered.len() * PREFIX_BYTES);
		for prefix in ordered {
			prefixes.extend_from_slice(&prefix);
		}
		let column = Self {
			prefixes,
			index_bits,
			block_indexes,
			counters,
		};
		let _ = column.decode(ids.len())?;
		Ok(column)
	}

	pub(crate) fn decode(&self, count: usize) -> Result<Vec<[u8; IDENTIFIER_BYTES]>> {
		ensure!(
			self.prefixes.len().is_multiple_of(PREFIX_BYTES),
			"identity prefix column length is invalid"
		);
		let prefix_count = self.prefixes.len() / PREFIX_BYTES;
		ensure!(
			prefix_count != 0 || count == 0,
			"nonempty identity column has no allocation prefixes"
		);
		ensure!(
			self.index_bits == required_index_bits(prefix_count),
			"identity block index width is not canonical"
		);
		ensure!(
			self.block_indexes.len() == packed_len(count, self.index_bits)?,
			"identity block index column length is invalid"
		);
		ensure!(
			self.counters.len()
				== count
					.checked_mul(COUNTER_BYTES)
					.context("identity counter length overflows")?,
			"identity counter column length is invalid"
		);
		if self.index_bits != 0 && count != 0 {
			let used_bits = count * self.index_bits as usize;
			let trailing = used_bits % 8;
			if trailing != 0 {
				let mask = !((1_u8 << trailing) - 1);
				ensure!(
					self.block_indexes.last().copied().unwrap_or_default() & mask == 0,
					"identity block index padding is nonzero"
				);
			}
		}
		let prefixes = self
			.prefixes
			.chunks_exact(PREFIX_BYTES)
			.map(|prefix| <[u8; PREFIX_BYTES]>::try_from(prefix).unwrap())
			.collect::<Vec<_>>();
		ensure!(
			prefixes.windows(2).all(|pair| pair[0] < pair[1]),
			"identity allocation prefixes are not canonical"
		);
		let mut referenced = vec![false; prefix_count];
		let mut unique = HashSet::with_capacity(count);
		let mut ids = Vec::with_capacity(count);
		for ordinal in 0..count {
			let index = unpack_index(&self.block_indexes, ordinal, self.index_bits);
			let prefix = prefixes
				.get(index)
				.context("identity block index is outside its prefix table")?;
			referenced[index] = true;
			let mut id = [0; IDENTIFIER_BYTES];
			id[..PREFIX_BYTES].copy_from_slice(prefix);
			id[PREFIX_BYTES..].copy_from_slice(&self.counters[ordinal * COUNTER_BYTES..][..COUNTER_BYTES]);
			ensure!(id != [0; IDENTIFIER_BYTES], "Carbon identity cannot be zero");
			ensure!(unique.insert(id), "Carbon identity is duplicated");
			ids.push(id);
		}
		ensure!(
			referenced.into_iter().all(|value| value),
			"identity allocation prefix is unused"
		);
		Ok(ids)
	}

	#[cfg(test)]
	fn encoded_len(&self) -> usize {
		self.prefixes.len() + self.block_indexes.len() + self.counters.len()
	}
}

fn required_index_bits(prefixes: usize) -> u8 {
	if prefixes <= 1 {
		0
	} else {
		(usize::BITS - (prefixes - 1).leading_zeros()) as u8
	}
}

fn packed_len(count: usize, bits: u8) -> Result<usize> {
	count
		.checked_mul(bits as usize)
		.and_then(|bits| bits.checked_add(7))
		.map(|bits| bits / 8)
		.context("identity block index length overflows")
}

fn pack_index(bytes: &mut [u8], ordinal: usize, bits: u8, index: usize) {
	for bit in 0..bits as usize {
		let offset = ordinal * bits as usize + bit;
		bytes[offset / 8] |= (((index >> bit) & 1) as u8) << (offset % 8);
	}
}

fn unpack_index(bytes: &[u8], ordinal: usize, bits: u8) -> usize {
	let mut index = 0;
	for bit in 0..bits as usize {
		let offset = ordinal * bits as usize + bit;
		index |= (((bytes[offset / 8] >> (offset % 8)) & 1) as usize) << bit;
	}
	index
}

#[cfg(test)]
mod tests {
	use super::{IdentityColumn, ManifestIdentityAllocator, PREFIX_BYTES};

	#[test]
	fn structured_identities_round_trip_compactly() {
		const COUNT: usize = 100_000;
		let prefixes = [[0x11; PREFIX_BYTES], [0x22; PREFIX_BYTES]];
		let mut allocator = ManifestIdentityAllocator::from_prefixes(prefixes);
		let identities = (0..COUNT).map(|_| allocator.next()).collect::<Vec<_>>();

		assert_eq!(&identities[0].to_be_bytes()[..PREFIX_BYTES], &prefixes[0]);
		assert_eq!(
			u16::from_be_bytes(identities[0].to_be_bytes()[PREFIX_BYTES..].try_into().unwrap()),
			0
		);
		assert_eq!(&identities[65_535].to_be_bytes()[..PREFIX_BYTES], &prefixes[0]);
		assert_eq!(
			u16::from_be_bytes(identities[65_535].to_be_bytes()[PREFIX_BYTES..].try_into().unwrap()),
			u16::MAX
		);
		assert_eq!(&identities[65_536].to_be_bytes()[..PREFIX_BYTES], &prefixes[1]);
		assert_eq!(
			u16::from_be_bytes(identities[65_536].to_be_bytes()[PREFIX_BYTES..].try_into().unwrap()),
			0
		);

		let identity_bytes = identities
			.iter()
			.map(|identity| identity.to_be_bytes())
			.collect::<Vec<_>>();
		let column = IdentityColumn::encode(&identity_bytes).unwrap();
		assert!(column.encoded_len() < PREFIX_BYTES * prefixes.len() + COUNT * 3);
		assert_eq!(
			column.decode(COUNT).unwrap(),
			identities
				.iter()
				.map(|identity| identity.to_be_bytes())
				.collect::<Vec<_>>()
		);
	}

	#[test]
	fn identity_column_rejects_noncanonical_and_duplicate_values() {
		let mut first = [0x11; 16];
		first[14..].copy_from_slice(&0_u16.to_be_bytes());
		let mut second = [0x22; 16];
		second[14..].copy_from_slice(&1_u16.to_be_bytes());
		let ids = [first, second];

		let mut padded = IdentityColumn::encode(&ids).unwrap();
		padded.block_indexes[0] |= 0x80;
		assert_eq!(
			padded.decode(ids.len()).unwrap_err().to_string(),
			"identity block index padding is nonzero"
		);

		let mut unsorted = IdentityColumn::encode(&ids).unwrap();
		let (left, right) = unsorted.prefixes.split_at_mut(PREFIX_BYTES);
		left.swap_with_slice(right);
		assert_eq!(
			unsorted.decode(ids.len()).unwrap_err().to_string(),
			"identity allocation prefixes are not canonical"
		);

		let mut unused = IdentityColumn::encode(&ids).unwrap();
		unused.block_indexes[0] = 0;
		assert_eq!(
			unused.decode(ids.len()).unwrap_err().to_string(),
			"identity allocation prefix is unused"
		);

		assert_eq!(
			IdentityColumn::encode(&[first, first]).unwrap_err().to_string(),
			"Carbon identity is duplicated"
		);
	}
}
