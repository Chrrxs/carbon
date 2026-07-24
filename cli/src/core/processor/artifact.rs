//! Serializes live changes into the canonical Carbon artifact.

use anyhow::{anyhow, ensure, Context, Result};
use crossbeam_channel::Sender;
use log::{error, trace};
use std::{
	collections::{HashMap, HashSet},
	fs::{self, File},
	io::{BufReader, Read, Write},
	path::{Path, PathBuf},
	sync::{
		atomic::{AtomicU8, Ordering},
		Arc, Mutex,
	},
	thread::Builder,
	time::{Duration, Instant},
};
use uuid::Uuid;

use crate::{
	artifact_store::{self, ArtifactStore},
	core::{changes::Changes, snapshot::Snapshot, tree::Tree},
	lock,
	privileged_bridge::{Bridge, Capabilities},
	project::{self, PreparedCapturePromotion},
};

const MAX_ACTIVE_TRANSACTIONS: usize = 4;
const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TRANSACTION_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(300);
const COMPLETED_TRANSACTION_TTL: Duration = Duration::from_secs(60 * 60);
const TRANSACTION_CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

pub(crate) struct CapturePrecommitAttestation {
	pub bridge: Bridge,
	pub bridge_id: String,
	pub process_id: u32,
	pub studio_session_id: String,
	pub instance_id: String,
	pub engine_generation: u64,
	pub hierarchy_sequence: u64,
	pub change_sequence: u64,
	pub project_path: PathBuf,
	pub project_document: Vec<u8>,
	pub previous_projected: Snapshot,
	pub mapped_refs: HashSet<rbx_dom_weak::types::Ref>,
	pub project_generation: String,
}

impl CapturePrecommitAttestation {
	fn validate(&self) -> Result<()> {
		let current: Capabilities = self.bridge.get("v1/capabilities")?;
		ensure!(
			current.bridge_id == self.bridge_id
				&& current.process_id == self.process_id
				&& current.studio_session_id == self.studio_session_id
				&& current.instance_id == self.instance_id,
			"Studio capture route changed during staging"
		);
		ensure!(
			current.engine_ready && current.engine_generation == self.engine_generation,
			"Studio native observation engine changed during staging"
		);
		ensure!(
			current.hierarchy_sequence == self.hierarchy_sequence && current.change_sequence == self.change_sequence,
			"Studio changed during Capture Manifest staging; retry the capture"
		);
		ensure!(
			project::exact_projected_realization_generation(
				&self.project_path,
				&self.project_document,
				&self.previous_projected,
				&self.mapped_refs,
			)? == self.project_generation,
			"filesystem mapping realization changed during Capture Manifest; retry after project source settles"
		);
		Ok(())
	}
}

fn validate_and_claim_capture(
	phase: &AtomicU8,
	expected_generation: &str,
	current_generation: &str,
	validate_attestation: impl FnOnce() -> Result<()>,
) -> Result<()> {
	ensure!(
		current_generation == expected_generation,
		"served source changed while Capture Manifest was staging; retry the capture"
	);
	validate_attestation()?;
	crate::core::claim_capture_commit(phase)
}

pub struct ArtifactProcessor {
	writer: Sender<WriteJob>,
	transactions: Mutex<TransactionState>,
	spool_dir: PathBuf,
}

struct TransactionState {
	spools: HashMap<(u32, String), Spool>,
	completed: HashMap<(u32, String), CompletedTransaction>,
	last_cleanup: Instant,
}

struct WriteJob {
	payload: WritePayload,
	completion: Sender<std::result::Result<String, String>>,
}

#[allow(clippy::large_enum_variant)]
enum WritePayload {
	Direct(Changes),
	Spool(PathBuf),
	PreparedCapture {
		projected_tree: Tree,
		promotion: PreparedCapturePromotion,
		phase: Arc<AtomicU8>,
		expected_generation: String,
		attestation: CapturePrecommitAttestation,
	},
	PreparedCaptureNoop {
		receipt: artifact_store::ValidatedArtifactReceipt,
		phase: Arc<AtomicU8>,
		expected_generation: String,
		attestation: CapturePrecommitAttestation,
	},
}

struct Spool {
	path: PathBuf,
	file: File,
	pages: Vec<PageReceipt>,
	bytes: u64,
	last_activity: Instant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PageReceipt {
	digest: blake3::Hash,
	done: bool,
}

struct CompletedTransaction {
	pages: Vec<PageReceipt>,
	source_generation: String,
	committed_at: Instant,
}

impl ArtifactProcessor {
	pub fn new(tree: Arc<Mutex<Tree>>, store: ArtifactStore) -> Self {
		Self::new_with_commit_lock(tree, store, Arc::new(Mutex::new(())))
	}

	pub fn new_with_commit_lock(tree: Arc<Mutex<Tree>>, mut store: ArtifactStore, commit_lock: Arc<Mutex<()>>) -> Self {
		let spool_dir = store
			.artifact_path()
			.parent()
			.unwrap_or_else(|| Path::new("."))
			.join(".carbon-transactions");
		if spool_dir.exists() {
			fs::remove_dir_all(&spool_dir).expect("failed to clear stale artifact transactions");
		}
		fs::create_dir_all(&spool_dir).expect("failed to create artifact transaction directory");

		let (writer, receiver) = crossbeam_channel::bounded::<WriteJob>(64);
		Builder::new()
			.name("artifact-processor".into())
			.spawn(move || {
				while let Ok(job) = receiver.recv() {
					let mut tree = lock!(tree);
					// A hybrid project retains only a tiny managed projection. Keep that
					// projection as the transactional recovery point; an error must never
					// fall back to materializing the Studio-owned complement.
					let projected_before = store.is_projected().then(|| tree.clone());
					let apply_result: Result<Option<String>> = {
						// File notifications must never observe a script source written
						// against the transaction's old manifest. The watcher takes this
						// same lock before constructing a causal generation envelope.
						let _commit = commit_lock.lock().unwrap();
						match job.payload {
							WritePayload::Direct(changes) => store.apply(&mut tree, changes).map(|()| None),
							WritePayload::Spool(path) => {
								let result = apply_spool(&path, &mut store, &mut tree);
								if let Err(error) = fs::remove_file(&path) {
									error!("Failed to remove transaction spool {}: {error}", path.display());
								}
								result.map(|()| None)
							}
							WritePayload::PreparedCapture {
								projected_tree,
								promotion,
								phase,
								expected_generation,
								attestation,
							} => (|| -> Result<Option<String>> {
								let current_generation = store.generation()?;
								validate_and_claim_capture(&phase, &expected_generation, &current_generation, || {
									attestation.validate()
								})?;
								let receipt = promotion.artifact_receipt().clone();
								let cleanup = match project::promote_capture_domains(&promotion) {
									Ok(cleanup) => cleanup,
									Err(error) => {
										phase.store(crate::core::CAPTURE_COMMITTED, Ordering::Release);
										return Err(error);
									}
								};
								phase.store(crate::core::CAPTURE_COMMITTED, Ordering::Release);
								*tree = projected_tree;
								store.install_projected_receipt(&receipt);
								drop(cleanup);
								Ok(Some(receipt.generation().to_owned()))
							})(),
							WritePayload::PreparedCaptureNoop {
								receipt,
								phase,
								expected_generation,
								attestation,
							} => (|| -> Result<Option<String>> {
								ensure!(
									receipt.generation() == expected_generation,
									"validated capture receipt does not match the served source generation"
								);
								// Revalidate the artifact and every referenced blob under the
								// commit lock. The canonical artifact generation transitively
								// binds blob hashes, but a blob can be externally replaced
								// without changing the artifact bytes themselves.
								let current_receipt =
									artifact_store::validated_artifact_receipt(store.artifact_path())?;
								let current_generation = current_receipt.generation();
								ensure!(
									current_generation == receipt.generation(),
									"validated capture artifact changed before the exact no-op claim"
								);
								validate_and_claim_capture(&phase, &expected_generation, current_generation, || {
									attestation.validate()
								})?;
								phase.store(crate::core::CAPTURE_COMMITTED, Ordering::Release);
								Ok(Some(current_generation.to_owned()))
							})(),
						}
					};
					let mut recovered = true;
					let result = match apply_result {
						Ok(generation) => {
							trace!("Committed Carbon artifact transaction");
							generation
								.map(Ok)
								.unwrap_or_else(|| store.generation())
								.map_err(|error| format!("{error:#}"))
						}
						Err(error) => {
							let message = format!("{error:#}");
							if crate::core::is_transient_manifest_capture_error(&error) {
								log::info!("Capture Manifest commit attempt was invalidated before commit: {message}");
							} else {
								error!("Failed to commit Carbon artifact transaction: {message}");
							}
							let recovery = if let Some(before) = projected_before {
								*tree = before;
								store.reload_projected(&tree)
							} else {
								artifact_store::load_live(store.artifact_path()).map(|mut loaded| {
									let shared_sources = store.script_sources();
									*shared_sources.write().unwrap() = loaded.script_sources.read().unwrap().clone();
									loaded.store.use_script_sources(shared_sources);
									*tree = loaded.tree;
									store = loaded.store;
								})
							};
							if let Err(reload_error) = recovery {
								recovered = false;
								error!("Failed to restore Carbon artifact after transaction error: {reload_error:#}");
							}
							Err(message)
						}
					};
					job.completion.send(result).ok();
					if !recovered {
						error!("Stopping artifact processor because its source could not be restored");
						break;
					}
				}
			})
			.expect("failed to start artifact processor");
		Self {
			writer,
			transactions: Mutex::new(TransactionState {
				spools: HashMap::new(),
				completed: HashMap::new(),
				last_cleanup: Instant::now(),
			}),
			spool_dir,
		}
	}

	pub fn write_page(
		&self,
		client_id: u32,
		transaction_id: String,
		sequence: u32,
		done: bool,
		changes: Changes,
	) -> Result<Option<String>> {
		ensure!(
			!transaction_id.is_empty() && transaction_id.len() <= 128,
			"invalid transaction id"
		);
		// Transaction spools outlive an individual request and must remain robust
		// when optional protocol fields are added. Named maps avoid compact-struct
		// field shifting when serde skips an absent value.
		let encoded = rmp_serde::to_vec_named(&changes)?;
		ensure!(encoded.len() <= MAX_PAGE_BYTES, "transaction page exceeds 8 MiB");
		let receipt = PageReceipt {
			digest: blake3::hash(&encoded),
			done,
		};
		let frame_len = u32::try_from(encoded.len()).context("transaction page is too large")?;
		let key = (client_id, transaction_id);
		let mut state = self.transactions.lock().unwrap();
		if state.last_cleanup.elapsed() >= TRANSACTION_CLEANUP_INTERVAL {
			let mut stale_paths = Vec::new();
			state.spools.retain(|_, spool| {
				let keep = spool.last_activity.elapsed() < TRANSACTION_TIMEOUT;
				if !keep {
					stale_paths.push(spool.path.clone());
				}
				keep
			});
			state
				.completed
				.retain(|_, completed| completed.committed_at.elapsed() < COMPLETED_TRANSACTION_TTL);
			state.last_cleanup = Instant::now();
			for path in stale_paths {
				let _ = fs::remove_file(path);
			}
		}

		if let Some(completed) = state.completed.get(&key) {
			let expected = completed
				.pages
				.get(sequence as usize)
				.context("transaction replay has an unexpected page")?;
			ensure!(
				expected == &receipt,
				"transaction replay page does not match its committed receipt"
			);
			return Ok(Some(completed.source_generation.clone()));
		}

		if sequence == 0 && done {
			ensure!(!state.spools.contains_key(&key), "transaction page is out of sequence");
			let source_generation = self.submit(WritePayload::Direct(changes))?;
			state.completed.insert(
				key,
				CompletedTransaction {
					pages: vec![receipt],
					source_generation: source_generation.clone(),
					committed_at: Instant::now(),
				},
			);
			return Ok(Some(source_generation));
		}

		if !state.spools.contains_key(&key) {
			ensure!(sequence == 0, "transaction must start at sequence zero");
			ensure!(
				state.spools.len() < MAX_ACTIVE_TRANSACTIONS,
				"too many active transactions"
			);
			let path = self.spool_dir.join(format!("{}.msgpack", Uuid::new_v4()));
			state.spools.insert(
				key.clone(),
				Spool {
					file: File::create(&path)?,
					path,
					pages: Vec::new(),
					bytes: 0,
					last_activity: Instant::now(),
				},
			);
		}
		let spool = state.spools.get_mut(&key).unwrap();
		let sequence = sequence as usize;
		if sequence < spool.pages.len() {
			ensure!(
				spool.pages[sequence] == receipt,
				"transaction replay page does not match its accepted receipt"
			);
			spool.last_activity = Instant::now();
			return Ok(None);
		}
		ensure!(sequence == spool.pages.len(), "transaction page is out of sequence");
		let next_bytes = spool
			.bytes
			.checked_add(u64::from(frame_len) + 4)
			.context("transaction byte count overflow")?;
		ensure!(next_bytes <= MAX_TRANSACTION_BYTES, "transaction exceeds 64 GiB");
		spool.file.write_all(&frame_len.to_le_bytes())?;
		spool.file.write_all(&encoded)?;
		spool.bytes = next_bytes;
		spool.pages.push(receipt);
		spool.last_activity = Instant::now();
		if !done {
			return Ok(None);
		}
		let spool = state.spools.remove(&key).unwrap();
		spool.file.sync_all()?;
		let path = spool.path;
		let pages = spool.pages;
		drop(spool.file);
		let source_generation = self.submit(WritePayload::Spool(path))?;
		state.completed.insert(
			key,
			CompletedTransaction {
				pages,
				source_generation: source_generation.clone(),
				committed_at: Instant::now(),
			},
		);
		Ok(Some(source_generation))
	}

	pub fn abort(&self, client_id: u32, transaction_id: &str) -> Result<()> {
		let key = (client_id, transaction_id.to_owned());
		if let Some(spool) = self.transactions.lock().unwrap().spools.remove(&key) {
			let path = spool.path;
			drop(spool.file);
			fs::remove_file(path)?;
		}
		Ok(())
	}

	pub(crate) fn apply_authoritative(&self, changes: Changes) -> Result<String> {
		self.submit(WritePayload::Direct(changes))
	}

	pub(crate) fn commit_prepared_capture(
		&self,
		projected_tree: Tree,
		promotion: PreparedCapturePromotion,
		phase: Arc<AtomicU8>,
		expected_generation: String,
		attestation: CapturePrecommitAttestation,
	) -> Result<String> {
		self.submit(WritePayload::PreparedCapture {
			projected_tree,
			promotion,
			phase,
			expected_generation,
			attestation,
		})
	}

	pub(crate) fn commit_capture_noop(
		&self,
		receipt: artifact_store::ValidatedArtifactReceipt,
		phase: Arc<AtomicU8>,
		expected_generation: String,
		attestation: CapturePrecommitAttestation,
	) -> Result<String> {
		self.submit(WritePayload::PreparedCaptureNoop {
			receipt,
			phase,
			expected_generation,
			attestation,
		})
	}

	fn submit(&self, payload: WritePayload) -> Result<String> {
		let (completion, result) = crossbeam_channel::bounded(1);
		self.writer.send(WriteJob { payload, completion })?;
		result.recv()?.map_err(|error| anyhow!(error))
	}
}

fn apply_spool(path: &Path, store: &mut ArtifactStore, tree: &mut Tree) -> Result<()> {
	// Additions are flattened in preorder and removals are streamed last. For a
	// large stable-ID native-root replacement, the root addition can therefore
	// precede its removal by many pages. Discover the transaction-wide pairing
	// first and retire old roots before applying any addition page.
	let mut addition_ids = HashSet::new();
	let mut removal_ids = HashSet::new();
	visit_spool(path, |changes| {
		addition_ids.extend(changes.additions.iter().map(|addition| addition.id));
		removal_ids.extend(changes.removals.iter().copied());
		Ok(())
	})?;

	let mut transaction = store.begin_transaction();
	store.prepare_replacements(tree, addition_ids.intersection(&removal_ids).copied(), &mut transaction)?;
	visit_spool(path, |changes| store.apply_page(tree, changes, &mut transaction))?;
	store.commit_transaction(tree, transaction)
}

fn visit_spool(path: &Path, mut visit: impl FnMut(Changes) -> Result<()>) -> Result<()> {
	let mut reader = BufReader::new(File::open(path)?);
	let mut frame = Vec::new();
	loop {
		let mut length = [0_u8; 4];
		if reader.read(&mut length[..1])? == 0 {
			break;
		}
		reader.read_exact(&mut length[1..])?;
		let length = u32::from_le_bytes(length) as usize;
		ensure!(length <= MAX_PAGE_BYTES, "transaction spool contains an oversized page");
		frame.resize(length, 0);
		reader.read_exact(&mut frame)?;
		let changes: Changes = rmp_serde::from_slice(&frame).context("transaction spool contains an invalid page")?;
		visit(changes)?;
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::atomic::AtomicBool;

	#[test]
	fn capture_claim_occurs_only_after_generation_and_attestation_succeed() {
		let phase = AtomicU8::new(crate::core::CAPTURE_COLLECTING);
		let attestation_called = AtomicBool::new(false);
		let error = validate_and_claim_capture(&phase, "expected", "stale", || {
			attestation_called.store(true, Ordering::Release);
			Ok(())
		})
		.unwrap_err();
		assert!(error.to_string().contains("served source changed"));
		assert!(!attestation_called.load(Ordering::Acquire));
		assert_eq!(phase.load(Ordering::Acquire), crate::core::CAPTURE_COLLECTING);

		let error = validate_and_claim_capture(&phase, "expected", "expected", || anyhow::bail!("attestation changed"))
			.unwrap_err();
		assert!(error.to_string().contains("attestation changed"));
		assert_eq!(phase.load(Ordering::Acquire), crate::core::CAPTURE_COLLECTING);

		validate_and_claim_capture(&phase, "expected", "expected", || Ok(())).unwrap();
		assert_eq!(phase.load(Ordering::Acquire), crate::core::CAPTURE_COMMITTING);
	}
}
