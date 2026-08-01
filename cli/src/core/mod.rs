use anyhow::{ensure, Context, Result};
use bytes::Bytes;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
#[cfg(test)]
use rbx_dom_weak::types::Attributes;
use rbx_dom_weak::{
	types::{Ref, Variant},
	Ustr, UstrMap,
};
use rbx_reflection::ClassTag;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
	collections::{BTreeMap, HashMap, HashSet},
	fs::{self, File},
	io::{BufWriter, Write},
	path::Path,
	path::PathBuf,
	sync::{
		atomic::{AtomicBool, AtomicU8, Ordering},
		Arc, Condvar, Mutex, MutexGuard, RwLock,
	},
	thread::{self, Builder},
	time::{Duration, Instant, SystemTime},
};

use self::{
	processor::artifact::{ArtifactProcessor, CapturePrecommitAttestation},
	queue::Queue,
	snapshot::SnapshotPage,
	tree::Tree,
};
use crate::{
	artifact_store,
	capture_provider::{
		wait_until_ready_with_progress, CapturePayloadSpool, CaptureProvider, CaptureRequest, CaptureShellClassRequest,
		RmlCaptureProvider,
	},
	lock,
	privileged_bridge::{Bridge, Capabilities, ManagedHierarchyAttachment, ManagedHierarchyStage, StudioIdentity},
	project,
	source::SourceDetails,
	util,
};

pub mod changes;
pub mod processor;
pub mod queue;
pub mod snapshot;
pub mod tree;

/// Runtime state for one canonical Carbon artifact.
///
/// The hierarchy is kept in a compact arena for reconciliation. Property
/// payloads remain on disk and are streamed through `SourceReader` pages.
pub struct Core {
	name: String,
	worktree: Option<(String, String)>,
	manifest_identity_bootstrap: Option<ManifestIdentityBootstrapContract>,
	live_session_token: Option<String>,
	is_place: bool,
	tree: Arc<Mutex<Tree>>,
	queue: Arc<Queue>,
	writer: Arc<ArtifactProcessor>,
	source_reader: Arc<artifact_store::SourceReader>,
	managed_hierarchy: Arc<RwLock<ManagedHierarchyState>>,
	manifest_path: PathBuf,
	source_commit_lock: Arc<Mutex<()>>,
	_source_watcher: Mutex<RecommendedWatcher>,
	_project_watcher: Mutex<Option<RecommendedWatcher>>,
	live_policy: Option<Arc<RwLock<project::LivePolicy>>>,
	project_snapshot: Option<Arc<RwLock<snapshot::Snapshot>>>,
	project_state_lock: Option<Arc<Mutex<()>>>,
	manifest_capture: Mutex<Option<ManifestCaptureOperation>>,
	last_capture_epoch: Mutex<Option<CaptureEpochReceipt>>,
	ephemeral_paths: Mutex<Vec<PathBuf>>,
	qualification_export: Mutex<Option<QualificationExport>>,
	restart_required: Arc<AtomicBool>,
	managed_reload_transition: Mutex<Option<String>>,
	shutdown_coordinator: ShutdownCoordinator,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CaptureEpochReceipt {
	bridge_id: String,
	process_id: u32,
	studio_session_id: String,
	instance_id: String,
	engine_generation: u64,
	hierarchy_sequence: u64,
	change_sequence: u64,
	source_generation: String,
	managed_contract_id: String,
	project_realization_generation: String,
	artifact_generation: String,
	artifact_fingerprint: String,
	artifact_len: u64,
	artifact_modified: SystemTime,
}

fn capture_epoch_changed_fields(expected: &CaptureEpochReceipt, observed: &CaptureEpochReceipt) -> Vec<&'static str> {
	let mut fields = Vec::new();
	macro_rules! changed {
		($field:ident) => {
			if expected.$field != observed.$field {
				fields.push(stringify!($field));
			}
		};
	}
	changed!(bridge_id);
	changed!(process_id);
	changed!(studio_session_id);
	changed!(instance_id);
	changed!(engine_generation);
	changed!(hierarchy_sequence);
	changed!(change_sequence);
	changed!(source_generation);
	changed!(managed_contract_id);
	changed!(project_realization_generation);
	changed!(artifact_generation);
	changed!(artifact_fingerprint);
	changed!(artifact_len);
	changed!(artifact_modified);
	fields
}

#[derive(Clone, Debug)]
pub(crate) struct QualificationExport {
	pub path: PathBuf,
	token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestCaptureStatus {
	pub request_id: String,
	pub state: String,
	pub source_generation: String,
	pub message: Option<String>,
}

impl ManifestCaptureStatus {
	pub fn terminal_result(&self) -> Result<Option<String>> {
		match self.state.as_str() {
			"complete" => Ok(Some(
				self.message
					.clone()
					.unwrap_or_else(|| "Studio artifact committed".to_owned()),
			)),
			"failed" => anyhow::bail!(
				"Capture Manifest failed: {}",
				self.message
					.clone()
					.unwrap_or_else(|| "Studio did not provide a reason".to_owned())
			),
			"running" => Ok(None),
			state => anyhow::bail!("serve returned unknown Capture Manifest state '{state}'"),
		}
	}
}

#[derive(Clone, Debug)]
enum ShutdownResult {
	Success(String),
	Error(String),
}

enum ShutdownCoordinatorState {
	Unstarted,
	InProgress,
	Done(ShutdownResult),
}

pub(crate) struct ShutdownCoordinator {
	state: Mutex<ShutdownCoordinatorState>,
	condvar: Condvar,
}

impl ShutdownCoordinator {
	pub fn new() -> Self {
		Self {
			state: Mutex::new(ShutdownCoordinatorState::Unstarted),
			condvar: Condvar::new(),
		}
	}

	pub fn execute_or_await<F>(&self, run: F) -> Result<String>
	where
		F: FnOnce() -> Result<String>,
	{
		let mut guard = self.state.lock().unwrap();
		loop {
			match &*guard {
				ShutdownCoordinatorState::Done(result) => {
					return match result {
						ShutdownResult::Success(msg) => Ok(msg.clone()),
						ShutdownResult::Error(err) => Err(anyhow::anyhow!("{err}")),
					};
				}
				ShutdownCoordinatorState::InProgress => {
					guard = self.condvar.wait(guard).unwrap();
				}
				ShutdownCoordinatorState::Unstarted => {
					*guard = ShutdownCoordinatorState::InProgress;
					drop(guard);

					let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
						Ok(Ok(msg)) => ShutdownResult::Success(msg),
						Ok(Err(err)) => ShutdownResult::Error(format!("{err:#}")),
						Err(payload) => {
							let msg = if let Some(s) = payload.downcast_ref::<&str>() {
								(*s).to_string()
							} else if let Some(s) = payload.downcast_ref::<String>() {
								s.clone()
							} else {
								"shutdown capture panicked".to_string()
							};
							ShutdownResult::Error(msg)
						}
					};

					let mut guard = self.state.lock().unwrap();
					*guard = ShutdownCoordinatorState::Done(result.clone());
					self.condvar.notify_all();

					return match result {
						ShutdownResult::Success(msg) => Ok(msg),
						ShutdownResult::Error(err) => Err(anyhow::anyhow!("{err}")),
					};
				}
			}
		}
	}
}

const AUTOMATIC_CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MANIFEST_CAPTURE_MAX_ATTEMPTS: usize = 3;
const MANIFEST_CAPTURE_RETRY_DELAY: Duration = Duration::from_millis(50);
const PROJECT_SYNC_POLL_INTERVAL: Duration = Duration::from_millis(25);
const PROJECT_SYNC_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

fn wait_for_project_synchronization<T, Check, Wait, Expired>(
	mut check: Check,
	mut wait: Wait,
	mut expired: Expired,
) -> Result<T>
where
	Check: FnMut() -> Result<T>,
	Wait: FnMut(Duration),
	Expired: FnMut() -> bool,
{
	loop {
		match check() {
			Ok(value) => return Ok(value),
			Err(error) if error.downcast_ref::<project::ProjectSynchronizationPending>().is_some() && !expired() => {
				wait(PROJECT_SYNC_POLL_INTERVAL);
			}
			Err(error) => return Err(error),
		}
	}
}

pub(crate) fn wait_for_automatic_capture<Begin, Poll, Wait>(
	begin: Begin,
	mut poll: Poll,
	mut wait: Wait,
) -> Result<String>
where
	Begin: FnOnce() -> Result<ManifestCaptureStatus>,
	Poll: FnMut(&str) -> Result<ManifestCaptureStatus>,
	Wait: FnMut(),
{
	let mut status = begin().context("failed to start automatic Capture Manifest before transition")?;
	let request_id = status.request_id.clone();
	loop {
		if let Some(message) = status.terminal_result()? {
			return Ok(message);
		}
		wait();
		status = poll(&request_id).context("failed to monitor automatic Capture Manifest before transition")?;
	}
}

fn run_manifest_capture_attempts<T, Attempt, Retry, Wait>(
	phase: &AtomicU8,
	mut attempt: Attempt,
	mut on_retry: Retry,
	mut wait: Wait,
) -> Result<T>
where
	Attempt: FnMut() -> Result<T>,
	Retry: FnMut(usize, &anyhow::Error) -> Result<()>,
	Wait: FnMut(Duration),
{
	for attempt_number in 1..=MANIFEST_CAPTURE_MAX_ATTEMPTS {
		ensure!(
			phase.load(Ordering::Acquire) == CAPTURE_COLLECTING,
			"Capture Manifest stopped before a native snapshot retry"
		);
		match attempt() {
			Ok(value) => return Ok(value),
			Err(error)
				if attempt_number < MANIFEST_CAPTURE_MAX_ATTEMPTS && is_transient_manifest_capture_error(&error) =>
			{
				on_retry(attempt_number, &error)?;
				wait(MANIFEST_CAPTURE_RETRY_DELAY);
			}
			Err(error) => return Err(error),
		}
	}
	unreachable!("the bounded manifest capture attempt loop always returns")
}

pub(crate) fn is_transient_manifest_capture_error(error: &anyhow::Error) -> bool {
	let message = format!("{error:#}");
	if message.contains("native capture lease cleanup also failed") {
		return false;
	}
	[
		"native snapshot failed: capture page-table plan is no longer active",
		"native snapshot failed: capture page-table epochs changed before staging",
		"DataModel changed between the native hierarchy read and serializer launch",
		"Studio changed during Capture Manifest staging; retry the capture",
	]
	.iter()
	.any(|transient| message.contains(transient))
}

#[cfg(test)]
mod manifest_capture_retry_tests {
	use super::*;

	#[test]
	fn transient_studio_invalidations_retry_the_same_manifest_capture() {
		let phase = AtomicU8::new(CAPTURE_COLLECTING);
		let mut attempts = 0;
		let mut retries = Vec::new();
		let mut waits = 0;

		let result = run_manifest_capture_attempts(
			&phase,
			|| {
				attempts += 1;
				match attempts {
					1 => anyhow::bail!(
						"native snapshot failed: edit DataModel changed between the native hierarchy read and serializer launch"
					),
					2 => anyhow::bail!("Studio changed during Capture Manifest staging; retry the capture"),
					_ => Ok("complete"),
				}
			},
			|attempt, error| {
				retries.push((attempt, error.to_string()));
				Ok(())
			},
			|_| waits += 1,
		)
		.unwrap();

		assert_eq!(result, "complete");
		assert_eq!(attempts, 3);
		assert_eq!(retries.len(), 2);
		assert_eq!(waits, 2);
	}

	#[test]
	fn unrelated_and_unclean_capture_failures_are_not_retried() {
		for message in [
			"capture payload digest does not match its envelope",
			"native capture lease cleanup also failed: native snapshot failed: capture page-table plan is no longer active",
		] {
			let phase = AtomicU8::new(CAPTURE_COLLECTING);
			let mut attempts = 0;
			let error = run_manifest_capture_attempts(
				&phase,
				|| -> Result<()> {
					attempts += 1;
					anyhow::bail!(message)
				},
				|_, _| panic!("non-retryable failure reached the retry callback"),
				|_| panic!("non-retryable failure waited for another attempt"),
			)
			.unwrap_err();
			assert_eq!(error.to_string(), message);
			assert_eq!(attempts, 1);
		}
	}

	#[test]
	fn transient_capture_retries_are_bounded_and_cancellable() {
		let phase = AtomicU8::new(CAPTURE_COLLECTING);
		let mut attempts = 0;
		let error = run_manifest_capture_attempts(
			&phase,
			|| -> Result<()> {
				attempts += 1;
				anyhow::bail!("Studio changed during Capture Manifest staging; retry the capture")
			},
			|_, _| Ok(()),
			|_| {},
		)
		.unwrap_err();
		assert!(error.to_string().contains("Studio changed"));
		assert_eq!(attempts, MANIFEST_CAPTURE_MAX_ATTEMPTS);

		let phase = AtomicU8::new(CAPTURE_COLLECTING);
		let mut attempts = 0;
		let error = run_manifest_capture_attempts(
			&phase,
			|| -> Result<()> {
				attempts += 1;
				anyhow::bail!("native snapshot failed: capture page-table plan is no longer active")
			},
			|_, _| Ok(()),
			|_| phase.store(CAPTURE_CANCELLED, Ordering::Release),
		)
		.unwrap_err();
		assert!(error.to_string().contains("stopped before a native snapshot retry"));
		assert_eq!(attempts, 1);
	}

	#[test]
	fn pending_project_realization_waits_for_synchronization() {
		let mut checks = 0;
		let mut waits = 0;
		let result = wait_for_project_synchronization(
			|| {
				checks += 1;
				if checks < 3 {
					return Err(project::ProjectSynchronizationPending.into());
				}
				Ok("synchronized")
			},
			|_| waits += 1,
			|| false,
		)
		.unwrap();
		assert_eq!(result, "synchronized");
		assert_eq!(checks, 3);
		assert_eq!(waits, 2);
	}

	#[test]
	fn pending_project_realization_wait_is_bounded() {
		let waits = std::cell::Cell::new(0);
		let error = wait_for_project_synchronization::<(), _, _, _>(
			|| Err(project::ProjectSynchronizationPending.into()),
			|_| waits.set(waits.get() + 1),
			|| waits.get() >= 1,
		)
		.unwrap_err();
		assert!(error.downcast_ref::<project::ProjectSynchronizationPending>().is_some());
		assert_eq!(waits.get(), 1);
	}

	#[test]
	fn shutdown_capture_joins_an_active_manual_capture() {
		let operation = |worker_active| {
			Some(ManifestCaptureOperation {
				request_id: "manual-capture".to_owned(),
				client_id: 7,
				source_generation: "served-generation".to_owned(),
				state: "running".to_owned(),
				message: Some("committing".to_owned()),
				phase: Arc::new(AtomicU8::new(CAPTURE_COMMITTING)),
				worker_active,
			})
		};
		let active = operation(true);

		let status = joinable_manifest_capture_status(&active).unwrap();
		assert_eq!(status.request_id, "manual-capture");
		assert_eq!(status.state, "running");

		assert!(joinable_manifest_capture_status(&operation(false)).is_none());
	}

	#[test]
	fn reload_capture_does_not_reuse_a_terminal_shutdown_result() {
		let directory = std::env::temp_dir().join(format!("carbon-reload-capture-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&directory).unwrap();
		let manifest_path = directory.join("place.carbon");
		let tree = tree::Tree::new(
			snapshot::Snapshot::new()
				.with_id(rbx_dom_weak::types::Ref::new())
				.with_class("DataModel")
				.with_name("ReloadCapture"),
		);
		artifact_store::extract_tree(&tree, "ReloadCapture".to_owned(), &manifest_path).unwrap();
		let core = Arc::new(Core::new_artifact(&manifest_path).unwrap());

		let terminal = core.capture_before_shutdown().unwrap();
		assert!(terminal.contains("Studio is disconnected"));
		std::fs::remove_file(&manifest_path).unwrap();
		assert_eq!(core.capture_before_shutdown().unwrap(), terminal);

		let error = core.capture_before_reload().unwrap_err();
		assert!(format!("{error:#}").contains("active served project"), "{error:#}");
		std::fs::remove_dir_all(directory).unwrap();
	}
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ManifestIdentityBootstrapContract {
	pub root_source_id: String,
	pub expected_source_instances: u32,
	pub expected_digest: String,
	pub rebindings: Vec<ManifestIdentityRebindingContract>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ManifestIdentityRebindingContract {
	pub source_id: String,
	pub parent_source_id: String,
	pub class_name: String,
	pub name: String,
	pub kind: &'static str,
	pub related_source_id: Option<String>,
}

fn manifest_identity_bootstrap_contract(
	snapshot: &snapshot::Snapshot,
	excluded: &HashSet<Ref>,
	rebindings: &[project::ManagedIdentityRebinding],
) -> Result<ManifestIdentityBootstrapContract> {
	fn collect(
		snapshot: &snapshot::Snapshot,
		excluded: &HashSet<Ref>,
		rebound: &HashSet<Ref>,
		identities: &mut Vec<Ref>,
	) {
		if excluded.contains(&snapshot.id) && !rebound.contains(&snapshot.id) {
			return;
		}
		identities.push(snapshot.id);
		for child in &snapshot.children {
			collect(child, excluded, rebound, identities);
		}
	}
	let rebound: HashSet<_> = rebindings.iter().map(|rebinding| rebinding.source_id).collect();
	ensure!(
		rebound.len() == rebindings.len(),
		"manifest identity bootstrap repeats a Studio rehydration identity"
	);
	ensure!(
		rebound.iter().all(|identity| excluded.contains(identity)),
		"manifest identity bootstrap rebinds an identity that still carries a marker"
	);
	let mut identities = Vec::new();
	collect(snapshot, excluded, &rebound, &mut identities);
	let included: HashSet<_> = identities.iter().copied().collect();
	ensure!(
		rebindings
			.iter()
			.all(|rebinding| included.contains(&rebinding.parent_source_id)),
		"manifest identity bootstrap rebinds beneath a non-authoritative parent"
	);
	identities.sort_unstable_by_key(ToString::to_string);
	let mut digest = Sha256::new();
	for identity in &identities {
		let uuid = uuid::Uuid::parse_str(&identity.to_string())?;
		digest.update(uuid.as_bytes());
	}
	Ok(ManifestIdentityBootstrapContract {
		root_source_id: snapshot.id.to_string(),
		expected_source_instances: u32::try_from(identities.len())?,
		expected_digest: format!("{:x}", digest.finalize()),
		rebindings: rebindings
			.iter()
			.map(|rebinding| ManifestIdentityRebindingContract {
				source_id: rebinding.source_id.to_string(),
				parent_source_id: rebinding.parent_source_id.to_string(),
				class_name: rebinding.class_name.clone(),
				name: rebinding.name.clone(),
				kind: rebinding.kind.wire_name(),
				related_source_id: rebinding.related_source_id.map(|identity| identity.to_string()),
			})
			.collect(),
	})
}

struct ManifestCaptureOperation {
	request_id: String,
	client_id: u32,
	source_generation: String,
	state: String,
	message: Option<String>,
	phase: Arc<AtomicU8>,
	worker_active: bool,
}

fn manifest_capture_status(operation: &ManifestCaptureOperation) -> ManifestCaptureStatus {
	ManifestCaptureStatus {
		request_id: operation.request_id.clone(),
		state: operation.state.clone(),
		source_generation: operation.source_generation.clone(),
		message: operation.message.clone(),
	}
}

fn joinable_manifest_capture_status(operation: &Option<ManifestCaptureOperation>) -> Option<ManifestCaptureStatus> {
	operation
		.as_ref()
		.filter(|capture| capture.worker_active)
		.map(manifest_capture_status)
}

#[derive(Clone)]
struct ManifestCaptureLaunch {
	client_id: u32,
	bridge_id: String,
	studio_session_id: String,
	instance_id: String,
	force_full: bool,
	managed_reload_transition_id: Option<String>,
}

pub(crate) const CAPTURE_COLLECTING: u8 = 0;
const CAPTURE_CANCELLED: u8 = 1;
const CAPTURE_COMMITTING: u8 = 2;
pub(crate) const CAPTURE_COMMITTED: u8 = 3;

pub(crate) fn claim_capture_commit(phase: &AtomicU8) -> Result<()> {
	phase
		.compare_exchange(
			CAPTURE_COLLECTING,
			CAPTURE_COMMITTING,
			Ordering::AcqRel,
			Ordering::Acquire,
		)
		.map(|_| ())
		.map_err(|state| {
			anyhow::anyhow!(if state == CAPTURE_CANCELLED {
				"Capture Manifest was cancelled before commit"
			} else {
				"Capture Manifest is no longer collectable"
			})
		})
}

fn claim_capture_cancel(phase: &AtomicU8) -> Result<()> {
	phase
		.compare_exchange(
			CAPTURE_COLLECTING,
			CAPTURE_CANCELLED,
			Ordering::AcqRel,
			Ordering::Acquire,
		)
		.map(|_| ())
		.map_err(|state| {
			anyhow::anyhow!(if state == CAPTURE_COMMITTING {
				"Capture Manifest has begun its atomic commit and can no longer be cancelled"
			} else {
				"Capture Manifest request is not cancellable"
			})
		})
}

impl Core {
	pub fn new_artifact(manifest_path: &Path) -> Result<Self> {
		Self::new_artifact_with_contract(manifest_path, None, None, None, None, None)
	}

	pub fn new_artifact_with_worktree(
		manifest_path: &Path,
		worktree: Option<(String, String, String)>,
	) -> Result<Self> {
		Self::new_artifact_with_contract(manifest_path, worktree, None, None, None, None)
	}

	pub fn new_artifact_with_live_session(manifest_path: &Path, session_token: String) -> Result<Self> {
		Self::new_artifact_with_contract(manifest_path, None, Some(session_token), None, None, None)
	}

	pub fn new_project_with_worktree(
		project_path: &Path,
		materialized: &project::MaterializedProject,
		worktree: (String, String, String),
	) -> Result<Self> {
		Self::new_project_with_worktree_and_control(project_path, materialized, worktree, None)
	}

	pub fn new_project_with_worktree_and_control(
		project_path: &Path,
		materialized: &project::MaterializedProject,
		worktree: (String, String, String),
		control_tx: impl Into<Option<crate::server::ServeControlSender>>,
	) -> Result<Self> {
		let manifest_identity_bootstrap = manifest_identity_bootstrap_contract(
			&materialized.snapshot,
			&materialized.identity_exclusions,
			&materialized.identity_rebindings,
		)?;
		Self::new_artifact_with_contract(
			&materialized.manifest_path,
			Some(worktree),
			None,
			Some(project::live_policy(project_path, materialized)),
			Some(manifest_identity_bootstrap),
			control_tx.into(),
		)
	}

	fn new_artifact_with_contract(
		manifest_path: &Path,
		worktree: Option<(String, String, String)>,
		live_session_token: Option<String>,
		live_policy: Option<project::LivePolicy>,
		manifest_identity_bootstrap: Option<ManifestIdentityBootstrapContract>,
		control_tx: Option<crate::server::ServeControlSender>,
	) -> Result<Self> {
		let loaded = match live_policy.as_ref() {
			Some(policy) => {
				artifact_store::load_projected_live(manifest_path, &policy.mapped_refs, &policy.routing_refs)?
			}
			None => artifact_store::load_live(manifest_path)?,
		};
		let is_place = loaded.root_class == "DataModel";
		let initial_managed_contract = match live_policy.as_ref() {
			Some(policy) => encode_project_managed_hierarchy(&loaded.tree, &policy.mapped_refs, &policy.routing_refs)?,
			None => encode_managed_hierarchy(&loaded.tree)?,
		};
		let source_reader = Arc::new(match live_policy.as_ref() {
			Some(_) => {
				artifact_store::SourceReader::new_projected(manifest_path, initial_managed_contract.source_ids.clone())?
			}
			None => artifact_store::SourceReader::new(manifest_path, loaded.live_mask)?,
		});
		let managed_hierarchy = Arc::new(RwLock::new(ManagedHierarchyState {
			authorized_ids: initial_managed_contract.source_ids.clone(),
			current: initial_managed_contract,
		}));
		let tree = Arc::new(Mutex::new(loaded.tree));
		let queue = Arc::new(Queue::new());
		let source_commit_lock = Arc::new(Mutex::new(()));
		let project_mode = live_policy.is_some();
		let project_snapshot = if project_mode {
			// The manifest complement is snapshot-only during serve. Keep only the
			// same managed projection retained by the live Tree; capture identity
			// reconciliation reads its prior state directly from the canonical artifact.
			Some(Arc::new(RwLock::new(project::snapshot_from_tree(
				&tree.lock().unwrap(),
			)?)))
		} else {
			None
		};
		let project_state_lock = project_mode.then(|| Arc::new(Mutex::new(())));
		let restart_required = Arc::new(AtomicBool::new(false));
		let source_watcher = watch_script_sources(
			manifest_path,
			if project_mode { Vec::new() } else { loaded.script_roots },
			loaded.script_sources,
			queue.clone(),
			source_commit_lock.clone(),
			source_reader.clone(),
		)?;
		let writer = Arc::new(ArtifactProcessor::new_with_commit_lock(
			tree.clone(),
			loaded.store,
			source_commit_lock.clone(),
		));
		let live_policy = live_policy.map(|policy| Arc::new(RwLock::new(policy)));
		let project_watcher = live_policy
			.as_ref()
			.map(|policy| {
				watch_project_source(
					policy.clone(),
					project_snapshot.as_ref().unwrap().clone(),
					project_state_lock.as_ref().unwrap().clone(),
					writer.clone(),
					queue.clone(),
					source_reader.clone(),
					managed_hierarchy.clone(),
					restart_required.clone(),
					control_tx,
				)
			})
			.transpose()?;
		Ok(Self {
			name: worktree
				.as_ref()
				.map(|(project, _, _)| project.clone())
				.unwrap_or(loaded.name),
			worktree: worktree.map(|(_, id, token)| (id, token)),
			manifest_identity_bootstrap,
			live_session_token,
			is_place,
			tree,
			queue,
			writer,
			source_reader,
			managed_hierarchy,
			manifest_path: manifest_path.to_owned(),
			source_commit_lock,
			_source_watcher: Mutex::new(source_watcher),
			_project_watcher: Mutex::new(project_watcher),
			live_policy,
			project_snapshot,
			project_state_lock,
			manifest_capture: Mutex::new(None),
			last_capture_epoch: Mutex::new(None),
			ephemeral_paths: Mutex::new(Vec::new()),
			qualification_export: Mutex::new(None),
			restart_required,
			managed_reload_transition: Mutex::new(None),
			shutdown_coordinator: ShutdownCoordinator::new(),
		})
	}

	pub fn name(&self) -> &str {
		&self.name
	}

	pub fn details(&self) -> SourceDetails {
		let tree = self.tree();
		let root_refs = if self.is_place {
			tree.place_root_refs().to_vec()
		} else {
			vec![tree.root_ref()]
		};
		let mapped_root_refs = self
			.live_policy
			.as_ref()
			.map(|policy| policy.read().unwrap().mapped_roots.clone())
			.unwrap_or_default();
		let details = SourceDetails::new(self.name.clone(), root_refs, self.is_place)
			.with_mapped_root_refs(mapped_root_refs)
			.with_source_root_ref(tree.root_ref())
			.with_source_generation(self.source_reader.generation());
		let details = match self.managed_reload_transition() {
			Some(transition_id) => details.with_managed_reload_transition(transition_id),
			None => details,
		};
		match (&self.worktree, &self.live_session_token) {
			(Some((id, token)), None) => details.with_worktree(id.clone(), token.clone()),
			(None, Some(token)) => details.with_session_token(token.clone()),
			(None, None) => details,
			(Some(_), Some(_)) => unreachable!("worktree and live-session contracts are mutually exclusive"),
		}
	}

	pub fn tree(&self) -> MutexGuard<'_, Tree> {
		lock!(self.tree)
	}

	fn encode_current_managed_hierarchy(&self) -> Result<ManagedHierarchyContract> {
		let projection = self.live_policy.as_ref().map(|policy| {
			let policy = policy.read().unwrap();
			(policy.mapped_refs.clone(), policy.routing_refs.clone())
		});
		let tree = self.tree();
		match projection {
			Some((mapped_refs, routing_refs)) => encode_project_managed_hierarchy(&tree, &mapped_refs, &routing_refs),
			None => encode_managed_hierarchy(&tree),
		}
	}

	pub fn queue(&self) -> Arc<Queue> {
		self.queue.clone()
	}

	pub fn register_ephemeral_path(&self, path: PathBuf) {
		self.ephemeral_paths.lock().unwrap().push(path);
	}

	pub(crate) fn enable_qualification_export(&self, path: PathBuf, token: String) -> Result<()> {
		ensure!(!token.is_empty(), "qualification export token is empty");
		ensure!(path.is_file(), "qualification launch place is unavailable");
		ensure!(
			path.extension().is_some_and(|extension| extension == "rbxl"),
			"qualification launch place must be an .rbxl file"
		);
		*self.qualification_export.lock().unwrap() = Some(QualificationExport { path, token });
		Ok(())
	}

	pub(crate) fn authorize_qualification_export(&self, supplied: &str) -> Result<QualificationExport> {
		let export = self
			.qualification_export
			.lock()
			.unwrap()
			.clone()
			.context("qualification place export is disabled")?;
		let expected = export.token.as_bytes();
		let supplied = supplied.as_bytes();
		let mut difference = expected.len() ^ supplied.len();
		for index in 0..expected.len().max(supplied.len()) {
			difference |= usize::from(
				expected.get(index).copied().unwrap_or_default() ^ supplied.get(index).copied().unwrap_or_default(),
			);
		}
		ensure!(difference == 0, "qualification place export is unauthorized");
		Ok(export)
	}

	pub fn cleanup_ephemeral_paths(&self) {
		let paths = std::mem::take(&mut *self.ephemeral_paths.lock().unwrap());
		for path in paths {
			let result = if path.is_dir() {
				std::fs::remove_dir_all(&path)
			} else if path.exists() {
				std::fs::remove_file(&path)
			} else {
				Ok(())
			};
			if let Err(error) = result {
				log::error!("failed to clean disposable serve path {}: {error}", path.display());
			}
		}
	}

	pub fn begin_manifest_capture(self: &Arc<Self>) -> Result<ManifestCaptureStatus> {
		self.begin_manifest_capture_mode_internal(false, false, None)
	}

	pub(crate) fn begin_managed_reload_transition(&self, transition_id: String) -> Result<()> {
		ensure!(
			self.worktree.is_some(),
			"managed synchronization reload requires a served worktree"
		);
		ensure!(
			!transition_id.is_empty(),
			"managed synchronization reload transition identity is empty"
		);
		let mut pending = self.managed_reload_transition.lock().unwrap();
		ensure!(pending.is_none(), "a managed synchronization reload is already pending");
		*pending = Some(transition_id);
		Ok(())
	}

	pub(crate) fn managed_reload_transition(&self) -> Option<String> {
		self.managed_reload_transition.lock().unwrap().clone()
	}

	pub(crate) fn has_pending_managed_reload(&self) -> bool {
		self.managed_reload_transition.lock().unwrap().is_some()
	}

	fn validate_managed_reload_capture(&self, supplied: Option<&str>, force_full: bool) -> Result<Option<String>> {
		let pending = self.managed_reload_transition();
		match (pending, supplied) {
			(None, None) => Ok(None),
			(Some(_), None) => {
				anyhow::bail!("synchronization reload is waiting for Studio to apply the replacement mapping topology")
			}
			(None, Some(_)) => anyhow::bail!("no managed synchronization reload is awaiting acknowledgement"),
			(Some(expected), Some(observed)) => {
				ensure!(
					expected == observed,
					"managed synchronization reload transition identity does not match"
				);
				ensure!(
					force_full,
					"managed synchronization reload acknowledgement requires a full capture"
				);
				Ok(Some(expected))
			}
		}
	}

	fn complete_managed_reload_transition(&self, transition_id: &str) -> Result<()> {
		let mut pending = self.managed_reload_transition.lock().unwrap();
		ensure!(
			pending.as_deref() == Some(transition_id),
			"managed synchronization reload transition changed before acknowledgement"
		);
		*pending = None;
		crate::carbon_info!(
			"Synchronization reload applied automatically after Studio proved replacement topology ({transition_id})"
		);
		Ok(())
	}

	pub fn capture_before_shutdown(self: &Arc<Self>) -> Result<String> {
		self.shutdown_coordinator
			.execute_or_await(|| self.do_automatic_capture())
	}

	pub fn capture_before_reload(self: &Arc<Self>) -> Result<String> {
		self.do_automatic_capture()
	}

	fn do_automatic_capture(self: &Arc<Self>) -> Result<String> {
		let capture_result = wait_for_automatic_capture(
			|| {
				let status = self.begin_manifest_capture_mode_internal(false, true, None)?;
				crate::carbon_info!(
					"Automatic transition is waiting for Capture Manifest {} for served generation {}",
					status.request_id,
					status.source_generation
				);
				Ok(status)
			},
			|request_id| self.manifest_capture_status(request_id),
			|| thread::sleep(AUTOMATIC_CAPTURE_POLL_INTERVAL),
		);

		match capture_result {
			Ok(message) => Ok(message),
			Err(error) => {
				if self.is_studio_disconnected_error(&error) && self.has_valid_manifest_fallback() {
					let message = format!(
						"Studio is disconnected; retained valid manifest {}",
						self.manifest_path.display()
					);
					crate::carbon_info!("{message}");
					Ok(message)
				} else {
					Err(error)
				}
			}
		}
	}

	fn is_studio_disconnected_error(&self, error: &anyhow::Error) -> bool {
		if !self.queue.has_subscribers() {
			return true;
		}
		let message = format!("{error:#}");
		[
			"disconnected",
			"no connected client listener",
			"exact connected Studio route",
			"connected Studio place",
			"connected Studio RML bridge",
			"RML bridge is unavailable",
			"bridge identity changed",
		]
		.iter()
		.any(|keyword| message.contains(keyword))
	}

	fn has_valid_manifest_fallback(&self) -> bool {
		artifact_store::validated_artifact_receipt(&self.manifest_path).is_ok()
	}

	pub(crate) fn begin_manifest_capture_mode(self: &Arc<Self>, force_full: bool) -> Result<ManifestCaptureStatus> {
		self.begin_manifest_capture_mode_internal(force_full, false, None)
	}

	pub(crate) fn begin_manifest_capture_mode_transition(
		self: &Arc<Self>,
		force_full: bool,
		managed_reload_transition_id: Option<String>,
	) -> Result<ManifestCaptureStatus> {
		self.begin_manifest_capture_mode_internal(force_full, false, managed_reload_transition_id)
	}

	fn begin_manifest_capture_mode_internal(
		self: &Arc<Self>,
		force_full: bool,
		join_active: bool,
		managed_reload_transition_id: Option<String>,
	) -> Result<ManifestCaptureStatus> {
		if join_active {
			if let Some(status) = joinable_manifest_capture_status(&self.manifest_capture.lock().unwrap()) {
				return Ok(status);
			}
		}
		ensure!(
			self.live_policy.is_some(),
			"Capture Manifest requires an active served project"
		);
		ensure!(
			!self.restart_required.load(Ordering::Acquire),
			"serve state requires a hard restart before Capture Manifest"
		);
		let managed_reload_transition_id =
			self.validate_managed_reload_capture(managed_reload_transition_id.as_deref(), force_full)?;
		let client_id = self.queue.single_listener_id()?;
		let route = self
			.queue
			.studio_route(client_id)
			.context("Capture Manifest requires an exact connected Studio route")?;
		let bridge_id = route
			.bridge_id
			.clone()
			.filter(|bridge_id| !bridge_id.is_empty())
			.context(
				"Capture Manifest requires the connected Studio RML bridge; wait for Carbon to finish connecting",
			)?;
		let source_generation = self.source_generation();
		let request_id = uuid::Uuid::new_v4().simple().to_string();
		let phase = Arc::new(AtomicU8::new(CAPTURE_COLLECTING));
		let joined_status = {
			let mut operation = self.manifest_capture.lock().unwrap();
			if let Some(status) = joinable_manifest_capture_status(&operation) {
				if !join_active {
					anyhow::bail!("another Capture Manifest operation is already running");
				}
				Some(status)
			} else {
				*operation = Some(ManifestCaptureOperation {
					request_id: request_id.clone(),
					client_id,
					source_generation: source_generation.clone(),
					state: "running".to_owned(),
					message: Some("RML is acquiring one native Studio snapshot lease".to_owned()),
					phase: phase.clone(),
					worker_active: true,
				});
				None
			}
		};
		if let Some(status) = joined_status {
			return Ok(status);
		}
		let core = Arc::clone(self);
		let worker_request_id = request_id.clone();
		if let Err(error) = Builder::new()
			.name(format!("carbon-capture-{request_id}"))
			.spawn(move || {
				let launch = ManifestCaptureLaunch {
					client_id,
					bridge_id,
					studio_session_id: route.studio_session_id,
					instance_id: route.instance_id,
					force_full,
					managed_reload_transition_id,
				};
				if let Err(error) = run_manifest_capture_attempts(
					&phase,
					|| core.run_native_manifest_capture(&worker_request_id, launch.clone(), phase.clone()),
					|attempt, error| {
						let message = format!(
							"Studio changed during native capture attempt {attempt}; retrying attempt {} of {}: {error:#}",
							attempt + 1,
							MANIFEST_CAPTURE_MAX_ATTEMPTS,
						);
						crate::carbon_info!("{message}");
						core.update_manifest_capture_message(&worker_request_id, &message)
					},
					thread::sleep,
				) {
					core.fail_manifest_capture(&worker_request_id, format!("{error:#}"));
				}
				core.settle_manifest_capture_worker(&worker_request_id);
			}) {
			self.fail_manifest_capture(&request_id, format!("failed to start native capture worker: {error}"));
			self.settle_manifest_capture_worker(&request_id);
			return Err(error.into());
		}
		self.manifest_capture_status(&request_id)
	}

	fn run_native_manifest_capture(
		&self,
		request_id: &str,
		launch: ManifestCaptureLaunch,
		phase: Arc<AtomicU8>,
	) -> Result<()> {
		let ManifestCaptureLaunch {
			client_id,
			bridge_id,
			studio_session_id,
			instance_id,
			force_full,
			managed_reload_transition_id,
		} = launch;
		let capture_started = Instant::now();
		let bridge = Bridge::discover(&bridge_id).context("connected Studio RML bridge is unavailable")?;
		let identity: StudioIdentity = bridge.get("v1/identity")?;
		ensure!(
			identity.bridge_id == bridge_id
				&& identity.studio_session_id == studio_session_id
				&& identity.instance_id == instance_id,
			"RML capture route no longer identifies the connected Studio place"
		);
		let capabilities: Capabilities = bridge.get("v1/capabilities")?;
		ensure!(
			capabilities.bridge_id == bridge_id,
			"RML capture bridge identity changed"
		);
		ensure!(capabilities.engine_ready, "RML capture engine is not ready");
		ensure!(
			capabilities.engine_generation != 0,
			"RML capture engine generation is unavailable"
		);
		ensure!(
			capabilities.capture_lease_protocol == crate::capture_provider::CAPTURE_ENVELOPE_VERSION,
			"RML capture lease protocol is incompatible with this Carbon server"
		);
		let route_identity_authority = self
			.queue
			.studio_route(client_id)
			.context("Studio route disappeared before capture")?
			.manifest_identities_authoritative;
		ensure!(
			!route_identity_authority || capabilities.manifest_identities_authoritative,
			"RML lost the authoritative manifest identity ledger after an engine restart; reconnect Studio and reopen a fresh Carbon build"
		);
		self.queue
			.set_manifest_identities_authoritative(client_id, capabilities.manifest_identities_authoritative)?;
		let manifest_identities_authoritative = capabilities.manifest_identities_authoritative;
		let contract = self.managed_hierarchy_contract()?;
		let source_generation = self.source_generation();
		let project_sync_started = Instant::now();
		let project_realization_generation = wait_for_project_synchronization(
			|| {
				let _project_state = self.project_state_lock.as_ref().map(|lock| lock.lock().unwrap());
				self.exact_project_realization_generation()
			},
			thread::sleep,
			|| project_sync_started.elapsed() >= PROJECT_SYNC_WAIT_TIMEOUT,
		)?;
		// `/v1/roots` is a point-in-time set and Studio may lazily create a
		// persistent service between that handshake and native serialization.
		// Send the complete pinned reflection schema; RML still fails closed if a
		// captured shell class is unknown to it.
		let shell_classes = crate::server::privileged::capture_shell_class_names()
			.into_iter()
			.map(|class_name| {
				Ok(CaptureShellClassRequest {
					properties: crate::server::privileged::capture_shell_property_names(&class_name)?,
					class_name,
				})
			})
			.collect::<Result<Vec<_>>>()?;
		let reflection_schema_hash = blake3::hash(&serde_json::to_vec(&shell_classes)?).to_hex().to_string();
		let mapped_root_source_ids = self
			.live_policy
			.as_ref()
			.map(|policy| {
				policy
					.read()
					.unwrap()
					.mapped_roots
					.iter()
					.map(ToString::to_string)
					.collect()
			})
			.unwrap_or_default();
		let request = CaptureRequest {
			capture_id: request_id.to_owned(),
			studio_session_id,
			instance_id,
			engine_generation: capabilities.engine_generation,
			source_generation: source_generation.clone(),
			managed_contract_id: contract.contract_id.clone(),
			reflection_schema_hash,
			manifest_identities_authoritative,
			allow_page_reuse: !force_full,
			mapped_root_source_ids,
			shell_classes,
		};
		// Capture attaches the compact mapping contract only when the user asks
		// for a snapshot. Connection remains verification-free, and the native
		// planner uses mapped roots as opaque exclusion barriers rather than
		// comparing their descendants with filesystem source. Attach before the
		// epoch proof so a trusted launch can attest the same contract without
		// acquiring a native serialization lease.
		refresh_capture_contract_on_bridge(
			&bridge,
			&contract,
			&request.studio_session_id,
			&request.instance_id,
			request.engine_generation,
			capabilities.process_id,
		)
		.context("failed to attach mapping barriers for native capture")?;
		if !force_full
			&& manifest_identities_authoritative
			&& self.try_capture_epoch_noop(
				&bridge,
				&capabilities,
				&request.studio_session_id,
				&request.instance_id,
				&source_generation,
				&contract.contract_id,
				&phase,
			)? {
			let mut operation = self.manifest_capture.lock().unwrap();
			let operation = operation
				.as_mut()
				.filter(|operation| operation.request_id == request_id)
				.context("Capture Manifest request disappeared during epoch no-op validation")?;
			operation.source_generation = source_generation;
			operation.state = "complete".to_owned();
			operation.message =
				Some("Manifest capture exact no-op was proven by the unchanged native mutation epoch".to_owned());
			crate::carbon_info!(
				"Capture Manifest epoch no-op timings for {}: total={:.1}ms",
				request_id,
				capture_started.elapsed().as_secs_f64() * 1_000.0,
			);
			return Ok(());
		}
		let precommit_bridge = bridge.clone();
		let provider = RmlCaptureProvider::new(bridge);
		let lease = provider
			.start(&request)
			.context("RML rejected the native capture lease")?;
		{
			let mut operation = self.manifest_capture.lock().unwrap();
			let operation = operation
				.as_mut()
				.filter(|operation| operation.request_id == request_id)
				.context("Capture Manifest request disappeared before lease acquisition")?;
			operation.message = Some("Studio is serializing bounded native RBXM chunks".to_owned());
		}

		let lease_id = lease.lease_id;
		let parent = self.manifest_path.parent().unwrap_or_else(|| Path::new("."));
		let prefix = format!(".carbon-capture-{request_id}");
		let envelope_path = parent.join(format!("{prefix}.envelope"));
		let payload_path = parent.join(format!("{prefix}.rbxm"));
		let result = (|| -> Result<(
			String,
			Vec<crate::capture_provider::ManifestIdentityRemap>,
			bool,
			u64,
			u64,
			artifact_store::ValidatedArtifactReceipt,
		)> {
			let progressive_payload = provider.supports_progressive_payload();
			let mut payload_spool = CapturePayloadSpool::new(BufWriter::new(File::create(&payload_path)?));
			let mut progressive_bytes = 0_u64;
			let mut reported_chunks = 0_u32;
			let native_wait_started = Instant::now();
			let ready = wait_until_ready_with_progress(
				&provider,
				&lease_id,
				Duration::from_secs(300),
				|| phase.load(Ordering::Acquire) == CAPTURE_CANCELLED,
				|status| {
					if !progressive_payload {
						return Ok(());
					}
					if !matches!(status.state.as_str(), "serializing" | "spooling" | "ready") {
						return Ok(());
					}
					if status.completed_chunks != reported_chunks {
						reported_chunks = status.completed_chunks;
						let total = status
							.total_chunks
							.map(|total| total.to_string())
							.unwrap_or_else(|| "?".to_owned());
						self.update_manifest_capture_message(
							request_id,
							&format!(
								"Studio serialized and streamed {}/{total} bounded chunks",
								status.completed_chunks
							),
						)?;
					}
					if status.committed_model_bytes <= progressive_bytes {
						return Ok(());
					}
					let length = status.committed_model_bytes - progressive_bytes;
					if status.state != "ready" && length < 4 * 1024 * 1024 {
						return Ok(());
					}
					let copied =
						provider.copy_payload_range(&lease_id, progressive_bytes, length, &mut payload_spool)?;
					ensure!(copied == length, "RML returned an incomplete progressive capture range");
					progressive_bytes += copied;
					Ok(())
				},
			)?;
			let native_wait_elapsed = native_wait_started.elapsed();
			ensure!(
				ready.serializer_settled,
				"RML reported a ready capture before its serializer settled"
			);
			self.update_manifest_capture_message(request_id, "Transferring native capture artifacts")?;
			let capture_result =
				(|| -> Result<(
					String,
					Vec<crate::capture_provider::ManifestIdentityRemap>,
					bool,
					u64,
					u64,
					artifact_store::ValidatedArtifactReceipt,
				)> {
				let transfer_started = Instant::now();
				{
					let mut output = BufWriter::new(File::create(&envelope_path)?);
					provider.copy_envelope(&lease_id, &mut output)?;
					output.flush()?;
				}
				if progressive_payload {
					ensure!(
						Some(progressive_bytes) == ready.model_bytes,
						"progressive capture length disagrees with the final RML seal"
					);
				} else {
					provider.copy_payload(&lease_id, &mut payload_spool)?;
				}
				let transfer_elapsed = transfer_started.elapsed();
				self.update_manifest_capture_message(request_id, "Validating the native Studio snapshot")?;
				ensure!(
					phase.load(Ordering::Acquire) == CAPTURE_COLLECTING,
					"Capture Manifest was cancelled before native artifacts were decoded"
				);
				let validation_started = Instant::now();
				let envelope_bytes = fs::read(&envelope_path)?;
				let envelope = crate::capture_provider::CaptureEnvelope::decode(&envelope_bytes)?;
				drop(envelope_bytes);
				envelope.validate_request(&request)?;
				payload_spool.finish(&envelope)?;
				let validation_elapsed = validation_started.elapsed();
				let _project_state = self.project_state_lock.as_ref().map(|lock| lock.lock().unwrap());
				ensure!(
					self.source_generation() == request.source_generation,
					"served mapped source changed during Capture Manifest; retry after Studio is synchronized"
				);
				ensure!(
					!self.restart_required.load(Ordering::Acquire),
					"serve state requires a hard restart before Capture Manifest can commit"
				);
				let validated_capture = crate::capture_store::classify_validated_capture(
					&envelope,
					&self.name,
					&project_realization_generation,
					&self.manifest_path,
				)?;
				let exact_noop_receipt = if force_full {
					None
				} else if let crate::capture_store::ValidatedCaptureClass::ExactNoop(receipt) = &validated_capture {
					Some(receipt)
				} else {
					None
				};
				if let Some(receipt) = exact_noop_receipt {
					ensure!(
						receipt.generation() == request.source_generation,
						"validated exact capture does not match the served source generation"
					);
					let policy = self
						.live_policy
						.as_ref()
						.context("Capture Manifest requires a served project")?
						.read()
						.unwrap()
						.clone();
					ensure!(
						self.exact_project_realization_generation()? == project_realization_generation,
						"filesystem mapping realization changed during Capture Manifest; retry after project source settles"
					);
					ensure!(
						phase.load(Ordering::Acquire) == CAPTURE_COLLECTING,
						"Capture Manifest was cancelled before the exact no-op claim"
					);
					let commit_started = Instant::now();
					let generation = self.writer.commit_capture_noop(
						receipt.clone(),
						phase.clone(),
						request.source_generation.clone(),
						CapturePrecommitAttestation {
							bridge: precommit_bridge.clone(),
							bridge_id: bridge_id.clone(),
							process_id: capabilities.process_id,
							studio_session_id: request.studio_session_id.clone(),
							instance_id: request.instance_id.clone(),
							engine_generation: request.engine_generation,
							hierarchy_sequence: envelope.hierarchy_sequence_after,
							change_sequence: envelope.change_sequence_after,
							project_path: policy.project_path.clone(),
							project_document: policy.project_document.clone(),
							previous_projected: self
								.project_snapshot
								.as_ref()
								.context("hybrid project snapshot is unavailable")?
								.read()
								.unwrap()
								.clone(),
							mapped_refs: policy.mapped_refs.clone(),
							project_generation: project_realization_generation.clone(),
						},
					)?;
					crate::carbon_info!(
							"Capture Manifest exact no-op timings for {}: native-wait={:.1}ms, artifact-transfer={:.1}ms, artifact-validation={:.1}ms, commit={:.1}ms, total={:.1}ms",
							request_id,
							native_wait_elapsed.as_secs_f64() * 1_000.0,
							transfer_elapsed.as_secs_f64() * 1_000.0,
							validation_elapsed.as_secs_f64() * 1_000.0,
							commit_started.elapsed().as_secs_f64() * 1_000.0,
							capture_started.elapsed().as_secs_f64() * 1_000.0,
						);
					return Ok((
						generation,
						Vec::new(),
						true,
						envelope.hierarchy_sequence_after,
						envelope.change_sequence_after,
						receipt.clone(),
					));
				}
				let baseline = match validated_capture {
					crate::capture_store::ValidatedCaptureClass::Rebuild(receipt)
					| crate::capture_store::ValidatedCaptureClass::ExactNoop(receipt) => receipt,
				};
				let cancelled = || phase.load(Ordering::Acquire) == CAPTURE_CANCELLED;
				self.update_manifest_capture_message(request_id, "Compiling the captured Studio state")?;
				let compile_started = Instant::now();
				let compiled = {
					let canonical = self
						.project_snapshot
						.as_ref()
						.context("hybrid project snapshot is unavailable")?
						.read()
						.unwrap();
					crate::capture_store::compile_validated(
						&payload_path,
						&envelope,
						&canonical,
						&self.name,
						&self.manifest_path,
						&baseline,
						&project_realization_generation,
						&cancelled,
					)?
				};
				let compile_elapsed = compile_started.elapsed();
				let referenced_mapped_refs = compiled.referenced_mapped_refs.clone();
				self.update_manifest_capture_message(request_id, "Staging the captured manifest")?;
				let composite_stage_started = Instant::now();
				let (projected_tree, staged_composite, identity_remap) =
					compiled.stage_composite(self.name.clone(), &self.manifest_path, &cancelled)?;
				let composite_stage_elapsed = composite_stage_started.elapsed();
				let policy = self
					.live_policy
					.as_ref()
					.context("Capture Manifest requires a served project")?
					.read()
					.unwrap()
					.clone();
				if staged_composite.is_noop()? {
					ensure!(
						self.exact_project_realization_generation()? == project_realization_generation,
						"filesystem mapping realization changed during Capture Manifest; retry after project source settles"
					);
					ensure!(
						phase.load(Ordering::Acquire) == CAPTURE_COLLECTING,
						"Capture Manifest was cancelled before the authored no-op claim"
					);
					let commit_started = Instant::now();
					let generation = self.writer.commit_capture_noop(
						baseline.clone(),
						phase.clone(),
						request.source_generation.clone(),
						CapturePrecommitAttestation {
							bridge: precommit_bridge.clone(),
							bridge_id: bridge_id.clone(),
							process_id: capabilities.process_id,
							studio_session_id: request.studio_session_id.clone(),
							instance_id: request.instance_id.clone(),
							engine_generation: request.engine_generation,
							hierarchy_sequence: envelope.hierarchy_sequence_after,
							change_sequence: envelope.change_sequence_after,
							project_path: policy.project_path.clone(),
							project_document: policy.project_document.clone(),
							previous_projected: self
								.project_snapshot
								.as_ref()
								.context("hybrid project snapshot is unavailable")?
								.read()
								.unwrap()
								.clone(),
							mapped_refs: policy.mapped_refs.clone(),
							project_generation: project_realization_generation.clone(),
						},
					)?;
					let capture_kind = if force_full {
						"full rebuild authored no-op"
					} else {
						"authored no-op"
					};
					crate::carbon_info!(
						"Capture Manifest {capture_kind} timings for {}: native-wait={:.1}ms, artifact-transfer={:.1}ms, artifact-validation={:.1}ms, compile={:.1}ms, composite-stage={:.1}ms, commit={:.1}ms, total={:.1}ms",
						request_id,
						native_wait_elapsed.as_secs_f64() * 1_000.0,
						transfer_elapsed.as_secs_f64() * 1_000.0,
						validation_elapsed.as_secs_f64() * 1_000.0,
						compile_elapsed.as_secs_f64() * 1_000.0,
						composite_stage_elapsed.as_secs_f64() * 1_000.0,
						commit_started.elapsed().as_secs_f64() * 1_000.0,
						capture_started.elapsed().as_secs_f64() * 1_000.0,
					);
					return Ok((
						generation,
						identity_remap,
						true,
						envelope.hierarchy_sequence_after,
						envelope.change_sequence_after,
						baseline,
					));
				}
				let studio_stage_started = Instant::now();
				let staged_studio = project::stage_captured_studio_domain(&policy, &staged_composite, &cancelled)?;
				let staged_identities = project::stage_mapped_identities(&policy, &referenced_mapped_refs, &cancelled)?;
				let studio_stage_elapsed = studio_stage_started.elapsed();
				let precommit_started = Instant::now();
				let capture_receipt = staged_composite.receipt().clone();
				let promotion = project::prepare_capture_promotion_with_identities(
					staged_composite,
					staged_studio,
					staged_identities,
				)?;
				ensure!(
					self.exact_project_realization_generation()? == project_realization_generation,
					"filesystem mapping realization changed during Capture Manifest; retry after project source settles"
				);
				ensure!(
					phase.load(Ordering::Acquire) == CAPTURE_COLLECTING,
					"Capture Manifest was cancelled before staged promotion"
				);
				let precommit_elapsed = precommit_started.elapsed();
				self.update_manifest_capture_message(request_id, "Committing the captured manifest atomically")?;
				let commit_started = Instant::now();
				let generation = self.writer.commit_prepared_capture(
					projected_tree,
					promotion,
					phase.clone(),
					request.source_generation.clone(),
					CapturePrecommitAttestation {
						bridge: precommit_bridge.clone(),
						bridge_id: bridge_id.clone(),
						process_id: capabilities.process_id,
						studio_session_id: request.studio_session_id.clone(),
						instance_id: request.instance_id.clone(),
						engine_generation: request.engine_generation,
						hierarchy_sequence: envelope.hierarchy_sequence_after,
						change_sequence: envelope.change_sequence_after,
						project_path: policy.project_path.clone(),
						project_document: policy.project_document.clone(),
						previous_projected: self
							.project_snapshot
							.as_ref()
							.context("hybrid project snapshot is unavailable")?
							.read()
							.unwrap()
							.clone(),
						mapped_refs: policy.mapped_refs.clone(),
						project_generation: project_realization_generation.clone(),
					},
				)?;
				let commit_elapsed = commit_started.elapsed();
				let refresh_started = Instant::now();
				let contract = self.encode_current_managed_hierarchy()?;
				let projected_source_ids = contract.source_ids.clone();
				install_managed_hierarchy_contract(&self.managed_hierarchy, contract);
				self.source_reader
					.install_projected_state(projected_source_ids, generation.clone())?;
				let refresh_elapsed = refresh_started.elapsed();
				crate::carbon_info!(
					"Capture Manifest phase timings for {}: pre-lease={:.1}ms, native-wait={:.1}ms, artifact-transfer={:.1}ms, artifact-validation={:.1}ms, compile={:.1}ms, composite-stage={:.1}ms, Studio-stage={:.1}ms, precommit-attestation={:.1}ms, commit={:.1}ms, local-refresh={:.1}ms, total={:.1}ms",
					request_id,
					native_wait_started.duration_since(capture_started).as_secs_f64() * 1_000.0,
					native_wait_elapsed.as_secs_f64() * 1_000.0,
					transfer_elapsed.as_secs_f64() * 1_000.0,
					validation_elapsed.as_secs_f64() * 1_000.0,
					compile_elapsed.as_secs_f64() * 1_000.0,
					composite_stage_elapsed.as_secs_f64() * 1_000.0,
					studio_stage_elapsed.as_secs_f64() * 1_000.0,
					precommit_elapsed.as_secs_f64() * 1_000.0,
					commit_elapsed.as_secs_f64() * 1_000.0,
					refresh_elapsed.as_secs_f64() * 1_000.0,
					capture_started.elapsed().as_secs_f64() * 1_000.0,
				);
				Ok((
					generation,
					identity_remap,
					false,
					envelope.hierarchy_sequence_after,
					envelope.change_sequence_after,
					capture_receipt,
				))
			})();
			capture_result
		})();
		let _ = fs::remove_file(&envelope_path);
		let _ = fs::remove_file(&payload_path);

		match result {
			Ok((generation, identity_remap, exact_noop, hierarchy_sequence, change_sequence, artifact)) => {
				let identity_finalize = if request.manifest_identities_authoritative {
					Ok(())
				} else {
					provider
						.finalize_manifest_identities(&request.capture_id, &identity_remap)
						.and_then(|()| self.queue.mark_manifest_identities_authoritative(client_id))
						.context("failed to finalize adopted manifest identities in RML")
				};
				let identity_error = identity_finalize.err().map(|error| format!("{error:#}"));
				let refresh_error = if exact_noop {
					None
				} else {
					self.refresh_capture_managed_hierarchy(&provider, &request, capabilities.process_id)
						.context("failed to refresh RML after the manifest commit")
						.err()
						.map(|error| format!("{error:#}"))
				};
				let acknowledgement_error = if identity_error.is_none() && refresh_error.is_none() {
					provider
						.acknowledge(&lease_id)
						.context("failed to acknowledge the committed capture page table")
						.err()
						.map(|error| format!("{error:#}"))
				} else {
					None
				};
				let release = provider
					.release(&lease_id)
					.context("failed to release completed RML capture lease")
					.and_then(|result| {
						ensure!(result.released, "RML retained the completed capture lease");
						Ok(())
					});
				let finalization_error = [
					identity_error,
					refresh_error,
					acknowledgement_error,
					release.err().map(|error| format!("{error:#}")),
				]
				.into_iter()
				.flatten()
				.reduce(|left, right| format!("{left}; {right}"));
				if finalization_error.is_none() {
					let capture_fingerprint = artifact.capture_fingerprint().unwrap_or_else(|| artifact.generation());
					self.remember_capture_epoch_if_current(
						&precommit_bridge,
						&capabilities,
						&request.studio_session_id,
						&request.instance_id,
						hierarchy_sequence,
						change_sequence,
						&generation,
						&artifact,
						capture_fingerprint,
					);
				}
				let transition_completed = finalization_error.is_none() && managed_reload_transition_id.is_some();
				{
					let mut operation = self.manifest_capture.lock().unwrap();
					let operation = operation
						.as_mut()
						.filter(|operation| operation.request_id == request_id)
						.context("Capture Manifest request disappeared after commit")?;
					operation.source_generation = generation;
					if let Some(error) = finalization_error {
						operation.state = "failed".to_owned();
						self.restart_required.store(true, Ordering::Release);
						let message = format!(
							"Manifest capture committed atomically, but post-commit RML finalization failed; \
						 hard restart serve before any further capture: {error}"
						);
						operation.message = Some(message.clone());
						let _ = self.queue.push(
							crate::server::Message::RestartRequired(crate::server::RestartRequired { message }),
							Some(client_id),
						);
					} else {
						operation.state = "complete".to_owned();
						operation.message = Some(if exact_noop {
							if force_full {
								"Manifest capture full rebuild verified an exact authored no-op and retained the canonical artifact"
								.to_owned()
							} else {
								"Manifest capture exact no-op was fully validated and retained the exact RML hierarchy contract"
								.to_owned()
							}
						} else {
							"Manifest capture full rebuild committed atomically and refreshed the exact RML hierarchy contract"
							.to_owned()
						});
					}
				}
				if transition_completed {
					self.complete_managed_reload_transition(managed_reload_transition_id.as_deref().unwrap())?;
				}
				Ok(())
			}
			Err(error) => {
				let committed = phase.load(Ordering::Acquire) == CAPTURE_COMMITTED;
				let release_error = if phase.load(Ordering::Acquire) == CAPTURE_CANCELLED {
					provider.cancel(&lease_id).err()
				} else {
					provider.release(&lease_id).err()
				};
				if !committed {
					if let Some(release_error) = release_error {
						return Err(error)
							.context(format!("native capture lease cleanup also failed: {release_error:#}"));
					}
					return Err(error);
				}
				self.restart_required.store(true, Ordering::Release);
				let mut message = format!(
					"Manifest capture changed the served composite, but post-commit persistence failed; hard restart serve before further synchronization: {error:#}"
				);
				if let Some(release_error) = release_error {
					message.push_str(&format!("; capture lease release also failed: {release_error:#}"));
				}
				{
					let mut operation = self.manifest_capture.lock().unwrap();
					let operation = operation
						.as_mut()
						.filter(|operation| operation.request_id == request_id)
						.context("Capture Manifest request disappeared after composite commit")?;
					operation.source_generation = self.source_generation();
					operation.state = "failed".to_owned();
					operation.message = Some(message.clone());
				}
				let _ = self.queue.push(
					crate::server::Message::RestartRequired(crate::server::RestartRequired { message }),
					Some(client_id),
				);
				Ok(())
			}
		}
	}

	#[allow(clippy::too_many_arguments)]
	fn try_capture_epoch_noop(
		&self,
		bridge: &Bridge,
		capabilities: &Capabilities,
		studio_session_id: &str,
		instance_id: &str,
		source_generation: &str,
		managed_contract_id: &str,
		phase: &AtomicU8,
	) -> Result<bool> {
		let Some(expected) = self.last_capture_epoch.lock().unwrap().clone() else {
			crate::carbon_info!("Capture Manifest epoch no-op unavailable: no prior committed capture epoch");
			return Ok(false);
		};
		let observed = CaptureEpochReceipt {
			bridge_id: capabilities.bridge_id.clone(),
			process_id: capabilities.process_id,
			studio_session_id: studio_session_id.to_owned(),
			instance_id: instance_id.to_owned(),
			engine_generation: capabilities.engine_generation,
			hierarchy_sequence: capabilities.hierarchy_sequence,
			change_sequence: capabilities.change_sequence,
			source_generation: source_generation.to_owned(),
			managed_contract_id: managed_contract_id.to_owned(),
			project_realization_generation: expected.project_realization_generation.clone(),
			artifact_generation: expected.artifact_generation.clone(),
			artifact_fingerprint: expected.artifact_fingerprint.clone(),
			artifact_len: expected.artifact_len,
			artifact_modified: expected.artifact_modified,
		};
		let changed_fields = capture_epoch_changed_fields(&expected, &observed);
		if !changed_fields.is_empty() {
			crate::carbon_info!(
				"Capture Manifest epoch no-op unavailable: changed {}",
				changed_fields.join(", ")
			);
			return Ok(false);
		}

		let _project_state = self.project_state_lock.as_ref().map(|lock| lock.lock().unwrap());
		let project_realization_generation = self.exact_project_realization_generation()?;
		let source_changed = self.source_generation() != source_generation;
		let contract_changed = self.managed_hierarchy_contract()?.contract_id != managed_contract_id;
		let project_changed = project_realization_generation != expected.project_realization_generation;
		if source_changed || contract_changed || project_changed {
			let mut reasons = Vec::new();
			if source_changed {
				reasons.push("served source");
			}
			if contract_changed {
				reasons.push("managed hierarchy contract");
			}
			if project_changed {
				reasons.push("project realization");
			}
			crate::carbon_info!(
				"Capture Manifest epoch no-op unavailable: changed {}",
				reasons.join(", ")
			);
			return Ok(false);
		}
		let artifact_metadata = fs::metadata(&self.manifest_path)?;
		let artifact_modified = artifact_metadata.modified()?;
		let generation_changed = expected.artifact_generation != source_generation;
		let fingerprint_missing = expected.artifact_fingerprint.is_empty();
		let length_changed = artifact_metadata.len() != expected.artifact_len;
		let modified_changed = artifact_modified != expected.artifact_modified;
		if generation_changed || fingerprint_missing || length_changed || modified_changed {
			let mut reasons = Vec::new();
			if generation_changed {
				reasons.push("artifact generation");
			}
			if fingerprint_missing {
				reasons.push("missing artifact fingerprint");
			}
			if length_changed {
				reasons.push("artifact length");
			}
			if modified_changed {
				reasons.push("artifact modification time");
			}
			crate::carbon_info!(
				"Capture Manifest epoch no-op unavailable: changed {}",
				reasons.join(", ")
			);
			return Ok(false);
		}
		ensure!(
			phase.load(Ordering::Acquire) == CAPTURE_COLLECTING,
			"Capture Manifest was cancelled before epoch no-op validation"
		);
		let current: Capabilities = bridge.get("v1/capabilities")?;
		let exact = current.bridge_id == observed.bridge_id
			&& current.process_id == observed.process_id
			&& current.studio_session_id == observed.studio_session_id
			&& current.instance_id == observed.instance_id
			&& current.engine_ready
			&& current.engine_generation == observed.engine_generation
			&& current.hierarchy_sequence == observed.hierarchy_sequence
			&& current.change_sequence == observed.change_sequence
			&& current.managed_contract_id == observed.managed_contract_id
			&& current.manifest_identities_authoritative;
		if !exact {
			crate::carbon_info!(
				"Capture Manifest epoch no-op unavailable: final native capability attestation changed"
			);
		}
		Ok(exact)
	}

	/// Retain the generation-matched managed launch as the first capture epoch.
	/// The caller invokes this only after native manifest identities bootstrap.
	pub(crate) fn remember_trusted_managed_launch_epoch_if_current(
		&self,
		bridge: &impl ManagedHierarchyRefreshBridge,
		capabilities: &Capabilities,
	) {
		let source_generation = self.source_generation();
		let result = (|| -> Result<()> {
			let contract = self.managed_hierarchy_contract()?;
			refresh_capture_contract_on_bridge(
				bridge,
				&contract,
				&capabilities.studio_session_id,
				&capabilities.instance_id,
				capabilities.engine_generation,
				capabilities.process_id,
			)?;
			ensure!(
				self.managed_hierarchy_contract()?.contract_id == contract.contract_id,
				"managed hierarchy changed during trusted launch attachment"
			);
			let attached_capabilities = bridge.capabilities()?;
			let artifact = artifact_store::validated_artifact_receipt(&self.manifest_path)?;
			let launch_fingerprint = artifact.generation().to_owned();
			self.remember_capture_epoch_if_current(
				bridge,
				&attached_capabilities,
				&attached_capabilities.studio_session_id,
				&attached_capabilities.instance_id,
				attached_capabilities.hierarchy_sequence,
				attached_capabilities.change_sequence,
				&source_generation,
				&artifact,
				&launch_fingerprint,
			);
			Ok(())
		})();
		if let Err(error) = result {
			crate::carbon_info!("Managed launch capture epoch was not retained: {error:#}");
		}
	}

	#[allow(clippy::too_many_arguments)]
	fn remember_capture_epoch_if_current(
		&self,
		bridge: &impl ManagedHierarchyRefreshBridge,
		capabilities: &Capabilities,
		studio_session_id: &str,
		instance_id: &str,
		hierarchy_sequence: u64,
		change_sequence: u64,
		source_generation: &str,
		artifact: &artifact_store::ValidatedArtifactReceipt,
		artifact_fingerprint: &str,
	) {
		let receipt = (|| -> Result<CaptureEpochReceipt> {
			let current = bridge.capabilities()?;
			ensure!(
				current.bridge_id == capabilities.bridge_id
					&& current.process_id == capabilities.process_id
					&& current.studio_session_id == studio_session_id
					&& current.instance_id == instance_id
					&& current.engine_ready
					&& current.engine_generation == capabilities.engine_generation
					&& current.hierarchy_sequence == hierarchy_sequence
					&& current.change_sequence == change_sequence
					&& current.manifest_identities_authoritative,
				"Studio changed before the capture epoch could be retained"
			);
			let _project_state = self.project_state_lock.as_ref().map(|lock| lock.lock().unwrap());
			ensure!(
				self.source_generation() == source_generation,
				"served source changed before the capture epoch could be retained"
			);
			let project_realization_generation = self.exact_project_realization_generation()?;
			let managed_contract_id = self.managed_hierarchy_contract()?.contract_id;
			ensure!(
				artifact.generation() == source_generation
					&& artifact.name() == self.name
					&& !artifact_fingerprint.is_empty(),
				"served artifact cannot seed an epoch no-op"
			);
			let artifact_metadata = fs::metadata(&self.manifest_path)?;
			Ok(CaptureEpochReceipt {
				bridge_id: current.bridge_id,
				process_id: current.process_id,
				studio_session_id: current.studio_session_id,
				instance_id: current.instance_id,
				engine_generation: current.engine_generation,
				hierarchy_sequence,
				change_sequence,
				source_generation: source_generation.to_owned(),
				managed_contract_id,
				project_realization_generation,
				artifact_generation: artifact.generation().to_owned(),
				artifact_fingerprint: artifact_fingerprint.to_owned(),
				artifact_len: artifact_metadata.len(),
				artifact_modified: artifact_metadata.modified()?,
			})
		})();
		match receipt {
			Ok(receipt) => *self.last_capture_epoch.lock().unwrap() = Some(receipt),
			Err(error) => crate::carbon_info!("Capture epoch no-op cache was not retained: {error:#}"),
		}
	}

	fn exact_project_realization_generation(&self) -> Result<String> {
		let policy = self
			.live_policy
			.as_ref()
			.context("served project policy is unavailable")?
			.read()
			.unwrap()
			.clone();
		let previous = self
			.project_snapshot
			.as_ref()
			.context("hybrid project snapshot is unavailable")?
			.read()
			.unwrap()
			.clone();
		project::exact_projected_realization_generation(
			&policy.project_path,
			&policy.project_document,
			&previous,
			&policy.mapped_refs,
		)
	}

	fn update_manifest_capture_message(&self, request_id: &str, message: &str) -> Result<()> {
		let mut operation = self.manifest_capture.lock().unwrap();
		let operation = operation
			.as_mut()
			.filter(|operation| operation.request_id == request_id)
			.context("Capture Manifest request disappeared while reporting progress")?;
		ensure!(
			operation.state == "running",
			"Capture Manifest request stopped while reporting progress"
		);
		operation.message = Some(message.to_owned());
		Ok(())
	}

	fn refresh_capture_managed_hierarchy(
		&self,
		provider: &RmlCaptureProvider,
		request: &CaptureRequest,
		expected_process_id: u32,
	) -> Result<ManagedHierarchyAttachment> {
		let contract = self.managed_hierarchy_contract()?;
		let attachment = refresh_capture_contract_on_bridge(
			provider.bridge(),
			&contract,
			&request.studio_session_id,
			&request.instance_id,
			request.engine_generation,
			expected_process_id,
		)?;
		ensure!(
			self.managed_hierarchy_contract()?.contract_id == contract.contract_id,
			"managed hierarchy changed while RML attached the post-capture contract"
		);
		Ok(attachment)
	}

	pub fn manifest_capture_status(&self, request_id: &str) -> Result<ManifestCaptureStatus> {
		let mut operation = self.manifest_capture.lock().unwrap();
		let operation = operation
			.as_mut()
			.filter(|operation| operation.request_id == request_id)
			.context("Capture Manifest request was not found")?;
		if operation.state == "running"
			&& !self.queue.is_subscribed(operation.client_id)
			&& claim_capture_cancel(&operation.phase).is_ok()
		{
			operation.state = "failed".to_owned();
			operation.message = Some("Studio disconnected before Capture Manifest completed".to_owned());
		}
		Ok(manifest_capture_status(operation))
	}

	pub fn fail_manifest_capture(&self, request_id: &str, message: String) {
		let mut operation = self.manifest_capture.lock().unwrap();
		let Some(operation) = operation
			.as_mut()
			.filter(|operation| operation.request_id == request_id)
		else {
			return;
		};
		if operation.phase.load(Ordering::Acquire) == CAPTURE_COMMITTED {
			return;
		}
		if operation.state == "running" {
			operation.phase.store(CAPTURE_CANCELLED, Ordering::Release);
			operation.state = "failed".to_owned();
			operation.message = Some(message);
		}
	}

	fn settle_manifest_capture_worker(&self, request_id: &str) {
		if let Some(operation) = self
			.manifest_capture
			.lock()
			.unwrap()
			.as_mut()
			.filter(|operation| operation.request_id == request_id)
		{
			operation.worker_active = false;
		}
	}

	pub fn cancel_manifest_capture(&self, request_id: &str) -> Result<ManifestCaptureStatus> {
		let status = {
			let mut capture = self.manifest_capture.lock().unwrap();
			let operation = capture
				.as_mut()
				.filter(|operation| operation.request_id == request_id)
				.context("Capture Manifest request was not found")?;
			ensure!(operation.state == "running", "Capture Manifest request is not running");
			claim_capture_cancel(&operation.phase)?;
			operation.state = "failed".to_owned();
			operation.message = Some("Manifest capture was cancelled; the previous manifest remains active".to_owned());
			ManifestCaptureStatus {
				request_id: operation.request_id.clone(),
				state: operation.state.clone(),
				source_generation: operation.source_generation.clone(),
				message: operation.message.clone(),
			}
		};
		Ok(status)
	}

	pub fn source_generation(&self) -> String {
		self.source_reader.generation()
	}

	pub(crate) fn requires_hard_restart(&self) -> bool {
		self.restart_required.load(Ordering::Acquire)
	}

	pub(crate) fn managed_hierarchy_contract(&self) -> Result<ManagedHierarchyContract> {
		Ok(self.managed_hierarchy.read().unwrap().current.clone())
	}

	/// Managed identity resolution is lazy, so a running Studio client may ask
	/// for an identity from the hierarchy it verified before a filesystem
	/// transaction replaced that source node. Retain every identity authorized
	/// during this Core lifetime; newly added nodes are installed by SyncChanges,
	/// while removed nodes still need their pre-transaction identity to be found
	/// and deleted in Studio.
	pub(crate) fn is_managed_identity_authorized(&self, id: Ref) -> bool {
		self.managed_hierarchy.read().unwrap().authorized_ids.contains(&id)
	}

	pub(crate) fn has_managed_worktree(&self) -> bool {
		self.worktree.is_some()
	}

	pub(crate) fn manifest_identity_bootstrap(&self) -> Result<ManifestIdentityBootstrapContract> {
		self.manifest_identity_bootstrap
			.clone()
			.context("managed worktree manifest identity contract is unavailable")
	}

	/// Materialize the complete canonical source graph for a native apply. The
	/// live reconciliation tree intentionally omits property payloads, so it must
	/// never be serialized back into Studio as authoritative state.
	pub fn materialized_source_tree(&self, expected_generation: &str) -> Result<Tree> {
		let _commit = self.source_commit_lock.lock().unwrap();
		let before = artifact_store::canonical_source_generation(&self.manifest_path)?;
		ensure!(
			before == expected_generation,
			"native apply source generation changed before materialization"
		);
		let loaded = artifact_store::load_tree(&self.manifest_path)?;
		let after = artifact_store::canonical_source_generation(&self.manifest_path)?;
		ensure!(
			after == expected_generation,
			"native apply source generation changed during materialization"
		);
		Ok(loaded.tree)
	}

	pub fn source_page(
		&self,
		cursor: Option<artifact_store::SourceCursor>,
		max_instances: usize,
		max_bytes: usize,
		metadata_only: bool,
	) -> Result<artifact_store::SourcePage> {
		if metadata_only {
			self.source_reader.metadata_page(cursor, max_instances, max_bytes)
		} else {
			self.source_reader.page(cursor, max_instances, max_bytes)
		}
	}

	/// Return a bounded hierarchy-only page. Properties are always streamed
	/// separately from disk through `source_page` and are never cloned here.
	pub fn snapshot_page(
		&self,
		instance: Ref,
		cursor: Vec<Ref>,
		max_instances: usize,
		max_bytes: usize,
	) -> Result<Option<SnapshotPage>> {
		self.tree().snapshot_page(instance, cursor, max_instances, max_bytes)
	}
}

impl Drop for Core {
	fn drop(&mut self) {
		self.cleanup_ephemeral_paths();
	}
}

const MANAGED_HIERARCHY_MAGIC: &[u8; 9] = b"CARBONID4";

fn append_ref_bytes(output: &mut Vec<u8>, id: Ref) -> Result<()> {
	if id.is_none() {
		output.extend_from_slice(&[0; 16]);
		return Ok(());
	}
	let value = id.to_string();
	ensure!(value.len() == 32, "managed hierarchy source id is not a 128-bit ref");
	for offset in (0..32).step_by(2) {
		output.push(u8::from_str_radix(&value[offset..offset + 2], 16).context("invalid source ref")?);
	}
	Ok(())
}

fn is_runtime_normalized_accessory_head_weld(tree: &Tree, id: Ref) -> bool {
	let Some(weld) = tree.get_instance(id) else {
		return false;
	};
	if weld.class.as_str() != "Weld" || weld.name.as_str() != "HeadWeld" {
		return false;
	}
	let Some(head) = tree.get_instance(weld.parent()) else {
		return false;
	};
	if head.class.as_str() != "Part" || head.name.as_str() != "Head" {
		return false;
	}
	let Some(Variant::Ref(handle_id)) = weld.properties.get(&Ustr::from("Part1")) else {
		return false;
	};
	let Some(handle) = tree.get_instance(*handle_id) else {
		return false;
	};
	if handle.class.as_str() != "Part" || handle.name.as_str() != "Handle" {
		return false;
	}
	let Some(accessory) = tree.get_instance(handle.parent()) else {
		return false;
	};
	accessory.class.as_str() == "Accessory"
		&& handle.children().iter().any(|child_id| {
			tree.get_instance(*child_id)
				.is_some_and(|child| child.class.as_str() == "Weld" && child.name.as_str() == "AccessoryWeld")
		})
}

fn is_redundant_accessory_weld(tree: &Tree, id: Ref) -> bool {
	let Some(weld) = tree.get_instance(id) else {
		return false;
	};
	if weld.class.as_str() != "Weld" || weld.name.as_str() != "AccessoryWeld" {
		return false;
	}
	let Some(handle) = tree.get_instance(weld.parent()) else {
		return false;
	};
	if handle.class.as_str() != "Part" || handle.name.as_str() != "Handle" {
		return false;
	}
	let Some(accessory) = tree.get_instance(handle.parent()) else {
		return false;
	};
	accessory.class.as_str() == "Accessory"
		&& handle.children().iter().any(|child_id| {
			tree.get_instance(*child_id).is_some_and(|child| {
				child.class.as_str() == "RigidConstraint" && child.name.as_str() == "AccessoryRigidConstraint"
			})
		})
}

#[derive(Clone)]
pub(crate) struct ManagedHierarchyContract {
	pub contract_id: String,
	pub payload: Bytes,
	pub source_instances: u32,
	pub excluded_source_ids: Vec<Ref>,
	pub source_ids: HashSet<Ref>,
}

pub(crate) trait ManagedHierarchyRefreshBridge {
	fn bridge_id(&self) -> &str;
	fn identity(&self) -> Result<StudioIdentity>;
	fn capabilities(&self) -> Result<Capabilities>;
	fn stage(&self, contract: &ManagedHierarchyContract) -> Result<ManagedHierarchyStage>;
	fn attach(&self, contract_id: &str) -> Result<ManagedHierarchyAttachment>;
}

impl ManagedHierarchyRefreshBridge for Bridge {
	fn bridge_id(&self) -> &str {
		Bridge::bridge_id(self)
	}

	fn identity(&self) -> Result<StudioIdentity> {
		self.get("v1/identity")
	}

	fn capabilities(&self) -> Result<Capabilities> {
		self.get("v1/capabilities")
	}

	fn stage(&self, contract: &ManagedHierarchyContract) -> Result<ManagedHierarchyStage> {
		self.post_bytes(
			&format!("v1/managed/stage/{}", contract.contract_id),
			contract.payload.clone(),
		)
	}

	fn attach(&self, contract_id: &str) -> Result<ManagedHierarchyAttachment> {
		self.post(
			"v1/managed/attach-staged",
			&serde_json::json!({ "contractId": contract_id }),
		)
	}
}

fn refresh_capture_contract_on_bridge(
	bridge: &impl ManagedHierarchyRefreshBridge,
	contract: &ManagedHierarchyContract,
	studio_session_id: &str,
	instance_id: &str,
	engine_generation: u64,
	expected_process_id: u32,
) -> Result<ManagedHierarchyAttachment> {
	let identity = bridge.identity()?;
	ensure!(
		identity.bridge_id == bridge.bridge_id()
			&& identity.process_id == expected_process_id
			&& identity.studio_session_id == studio_session_id
			&& identity.instance_id == instance_id,
		"RML capture route changed before managed hierarchy refresh"
	);
	let capabilities = bridge.capabilities()?;
	ensure!(
		capabilities.bridge_id == bridge.bridge_id()
			&& capabilities.process_id == expected_process_id
			&& capabilities.engine_ready
			&& capabilities.engine_generation == engine_generation,
		"RML capture runtime changed before managed hierarchy refresh"
	);
	if capabilities.managed_contract_id == contract.contract_id {
		ensure!(
			capabilities.managed_contract_source_instances == contract.source_instances,
			"RML attached managed hierarchy receipt has the wrong source instance count"
		);
		return Ok(ManagedHierarchyAttachment {
			attached: true,
			source_instances: capabilities.managed_contract_source_instances,
			hierarchy_sequence: capabilities.hierarchy_sequence,
			change_sequence: capabilities.change_sequence,
			excluded_source_ids: Vec::new(),
			source_root_debug_ids: Vec::new(),
		});
	}
	let staged = bridge.stage(contract)?;
	ensure!(
		staged.contract_id == contract.contract_id && staged.source_instances == contract.source_instances,
		"RML staged the wrong post-capture managed hierarchy contract"
	);
	let attachment = bridge.attach(&contract.contract_id)?;
	ensure!(
		attachment.attached,
		"RML rejected the post-capture managed hierarchy attachment"
	);
	ensure!(
		attachment.source_instances == contract.source_instances,
		"RML attached the wrong post-capture managed source instance count"
	);
	Ok(attachment)
}

struct ManagedHierarchyState {
	current: ManagedHierarchyContract,
	authorized_ids: HashSet<Ref>,
}

fn install_managed_hierarchy_contract(state: &RwLock<ManagedHierarchyState>, next: ManagedHierarchyContract) {
	// Add authorization before publishing the new contract. A filesystem
	// SyncChanges envelope can immediately require either the old identity for a
	// removal or the new identity for a retained/moved node.
	let mut state = state.write().unwrap();
	state.authorized_ids.extend(next.source_ids.iter().copied());
	state.current = next;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ManagedHierarchyShape {
	class: Ustr,
	name: String,
	children: Vec<(u32, u32)>,
}

fn encode_managed_hierarchy(tree: &Tree) -> Result<ManagedHierarchyContract> {
	encode_managed_hierarchy_selection(tree, None)
}

/// Encode only the hierarchy that Carbon actively manages for a project serve.
///
/// The manifest complement is already present in the disposable launch build
/// and is snapshot-owned. Sending it back through the managed contract would
/// turn connection into an O(place) verification pass. The managed contract
/// therefore contains engine route anchors and filesystem-owned subtrees only.
fn encode_project_managed_hierarchy(
	tree: &Tree,
	mapped_refs: &HashSet<Ref>,
	routing_refs: &HashSet<Ref>,
) -> Result<ManagedHierarchyContract> {
	let root = tree.root_ref();
	let root_node = tree
		.get_instance(root)
		.context("managed hierarchy DataModel is missing")?;
	ensure!(
		root_node.class.as_str() == "DataModel",
		"project managed hierarchy root must be DataModel"
	);

	let mut selected = HashSet::new();
	selected.insert(root);

	// Every direct singleton service is a legal engine join. This is bounded by
	// the engine service surface and deliberately does not traverse its manifest
	// children.
	for child in root_node.children() {
		let node = tree
			.get_instance(*child)
			.context("managed hierarchy DataModel child is missing")?;
		if util::get_reflection_database()
			.classes
			.get(node.class.as_str())
			.is_some_and(|descriptor| descriptor.tags.contains(&ClassTag::Service))
		{
			selected.insert(*child);
		}
	}

	// Expand each mapped root defensively. LivePolicy currently carries the
	// complete mapped closure, but deriving roots and walking them here prevents
	// a partial caller from producing a contract that authorizes only half of a
	// filesystem-owned subtree.
	for id in mapped_refs {
		let node = tree
			.get_instance(*id)
			.with_context(|| format!("mapped managed hierarchy instance {id} is missing"))?;
		if node.parent().is_none() || mapped_refs.contains(&node.parent()) {
			continue;
		}
		selected.extend(tree.subtree_refs(*id)?);
	}
	// A project may explicitly own the DataModel envelope. Treat it as a mapped
	// root without recursively selecting the snapshot-owned complement.
	selected.extend(mapped_refs.iter().copied());
	selected.extend(routing_refs.iter().copied());

	// Shape encoding requires a connected tree. Include only the ancestors of
	// selected mapping/route identities; arbitrary manifest branches remain out.
	let required = selected.iter().copied().collect::<Vec<_>>();
	for id in required {
		let mut current = id;
		loop {
			let node = tree
				.get_instance(current)
				.with_context(|| format!("managed hierarchy instance {current} is missing"))?;
			if current == root {
				break;
			}
			let parent = node.parent();
			ensure!(parent.is_some(), "managed hierarchy instance {current} is detached");
			selected.insert(parent);
			current = parent;
		}
	}

	encode_managed_hierarchy_selection(tree, Some(&selected))
}

fn encode_managed_hierarchy_selection(
	tree: &Tree,
	selected: Option<&HashSet<Ref>>,
) -> Result<ManagedHierarchyContract> {
	let mut refs = Vec::new();
	let mut excluded_roots = HashSet::new();
	let mut excluded_source_ids = Vec::new();
	let mut stack = vec![tree.root_ref()];
	while let Some(id) = stack.pop() {
		if selected.is_some_and(|selected| !selected.contains(&id)) {
			continue;
		}
		let node = tree.get_instance(id).context("managed hierarchy node is missing")?;
		if excluded_roots.contains(&id)
			|| (node.class.as_str() == "Status"
				&& tree
					.get_instance(node.parent())
					.is_some_and(|parent| parent.class.as_str() == "Humanoid"))
			|| (node.class.as_str() == "ConfigureServerService"
				&& tree
					.get_instance(node.parent())
					.is_some_and(|parent| parent.class.as_str() == "DataModel"))
			|| is_runtime_normalized_accessory_head_weld(tree, id)
			|| is_redundant_accessory_weld(tree, id)
		{
			let mut excluded_stack = vec![id];
			while let Some(excluded_id) = excluded_stack.pop() {
				let excluded_node = tree
					.get_instance(excluded_id)
					.context("managed excluded hierarchy node is missing")?;
				excluded_source_ids.push(excluded_id);
				excluded_stack.extend(
					excluded_node
						.children()
						.iter()
						.rev()
						.copied()
						.filter(|child| selected.is_none_or(|selected| selected.contains(child))),
				);
			}
			continue;
		}
		refs.push(id);
		if node.class.as_str() == "Workspace" {
			if let Some(Variant::Ref(edit_camera)) = node.properties.get(&Ustr::from("CurrentCamera")) {
				if edit_camera.is_some() {
					excluded_roots.insert(*edit_camera);
				}
			}
		}
		stack.extend(
			node.children()
				.iter()
				.rev()
				.copied()
				.filter(|child| selected.is_none_or(|selected| selected.contains(child))),
		);
	}
	let source_ids = refs.iter().copied().collect();
	let count = u32::try_from(refs.len()).context("managed hierarchy exceeds the protocol limit")?;
	let index_by_ref: HashMap<_, _> = refs
		.iter()
		.copied()
		.enumerate()
		.map(|(index, id)| (id, index))
		.collect();
	let mut shape_ids = vec![u32::MAX; refs.len()];
	let mut shapes = HashMap::<ManagedHierarchyShape, u32>::new();
	for index in (0..refs.len()).rev() {
		let node = tree
			.get_instance(refs[index])
			.context("managed hierarchy node disappeared while shaping")?;
		let mut child_counts = BTreeMap::<u32, u32>::new();
		for child_id in node.children() {
			let Some(&child_index) = index_by_ref.get(child_id) else {
				continue;
			};
			let child_shape = shape_ids[child_index];
			ensure!(child_shape != u32::MAX, "managed hierarchy child shape is unavailable");
			let entry = child_counts.entry(child_shape).or_default();
			*entry = entry
				.checked_add(1)
				.context("managed hierarchy child count exceeds the protocol limit")?;
		}
		let shape = ManagedHierarchyShape {
			class: node.class,
			name: node.name.clone(),
			children: child_counts.into_iter().collect(),
		};
		let next_shape =
			u32::try_from(shapes.len()).context("managed hierarchy shape count exceeds the protocol limit")?;
		shape_ids[index] = *shapes.entry(shape).or_insert(next_shape);
	}
	let mut child_shape_modes = vec![0_u8; refs.len()];
	for (index, id) in refs.iter().copied().enumerate() {
		let node = tree
			.get_instance(id)
			.context("managed hierarchy node disappeared while grouping")?;
		let mut first_shape_by_identity = HashMap::<(&str, &str), u32>::new();
		for child_id in node.children() {
			let Some(&child_index) = index_by_ref.get(child_id) else {
				continue;
			};
			let child = tree
				.get_instance(*child_id)
				.context("managed hierarchy child disappeared while grouping")?;
			let identity = (child.class.as_str(), child.name.as_str());
			if first_shape_by_identity
				.insert(identity, shape_ids[child_index])
				.is_some_and(|first_shape| first_shape != shape_ids[child_index])
			{
				child_shape_modes[index] = 1;
				break;
			}
		}
	}
	let mut output = Vec::with_capacity(12 + refs.len() * 56);
	output.extend_from_slice(MANAGED_HIERARCHY_MAGIC);
	output.extend_from_slice(&count.to_le_bytes());
	for (index, id) in refs.into_iter().enumerate() {
		let node = tree.get_instance(id).context("managed hierarchy node disappeared")?;
		append_ref_bytes(&mut output, id)?;
		let parent_index = if index == 0 {
			u32::MAX
		} else {
			u32::try_from(
				*index_by_ref
					.get(&node.parent())
					.context("managed hierarchy parent is excluded or missing")?,
			)
			.context("managed hierarchy parent index exceeds the protocol limit")?
		};
		output.extend_from_slice(&parent_index.to_le_bytes());
		output.extend_from_slice(&shape_ids[index].to_le_bytes());
		output.push(child_shape_modes[index]);
		let class = node.class.as_bytes();
		let name = node.name.as_bytes();
		let class_len = u16::try_from(class.len()).context("managed hierarchy class name is too long")?;
		let name_len = u32::try_from(name.len()).context("managed hierarchy instance name is too long")?;
		output.extend_from_slice(&class_len.to_le_bytes());
		output.extend_from_slice(&name_len.to_le_bytes());
		output.extend_from_slice(class);
		output.extend_from_slice(name);
	}
	Ok(ManagedHierarchyContract {
		contract_id: uuid::Uuid::new_v4().simple().to_string(),
		payload: output.into(),
		source_instances: count,
		excluded_source_ids,
		source_ids,
	})
}

#[cfg(test)]
fn strip_studio_route_properties(properties: &mut UstrMap<Variant>, marker: &str) -> Result<bool> {
	let attribute_properties: Vec<_> = properties
		.keys()
		.filter(|property| property.as_str().starts_with("Attributes"))
		.copied()
		.collect();
	let mut stripped = false;
	for property in attribute_properties {
		let Some(value) = properties.get_mut(&property) else {
			continue;
		};
		if !strip_matching_mcp_place_id(value, marker)? {
			continue;
		}
		stripped = true;
		if matches!(value, Variant::Attributes(attributes) if attributes.is_empty()) {
			properties.remove(&property);
		}
	}
	Ok(stripped)
}

#[cfg(test)]
fn strip_matching_mcp_place_id(value: &mut Variant, marker: &str) -> Result<bool> {
	let mut attributes = match value {
		Variant::Attributes(attributes) => attributes.clone(),
		Variant::BinaryString(raw) => {
			let raw: &[u8] = raw.as_ref();
			Attributes::from_reader(raw).context("Studio sent an invalid ServerStorage Attributes payload")?
		}
		_ => return Ok(false),
	};
	let matches_route = match attributes.get("__MCPPlaceId") {
		Some(Variant::String(value)) => value == marker || uuid::Uuid::parse_str(value).is_ok(),
		Some(Variant::BinaryString(value)) => {
			let value: &[u8] = value.as_ref();
			value == marker.as_bytes()
				|| std::str::from_utf8(value).is_ok_and(|value| uuid::Uuid::parse_str(value).is_ok())
		}
		_ => false,
	};
	let mut stripped = false;
	if matches_route {
		attributes.remove("__MCPPlaceId");
		stripped = true;
	}
	for attribute in [
		"__StudioWorktree_CarbonEndpoint",
		"__StudioWorktree_CarbonProject",
		"__StudioWorktree_CarbonGeneration",
		"__StudioWorktree_Identity",
		"__StudioWorktree_Session",
	] {
		stripped |= attributes.remove(attribute).is_some();
	}
	*value = Variant::Attributes(attributes);
	Ok(stripped)
}

struct ProjectSourceCandidate {
	snapshot: snapshot::Snapshot,
	routes: Vec<Vec<String>>,
	watch_roots: Vec<PathBuf>,
	generation: String,
}

fn read_project_candidate(
	project_path: &Path,
	frozen_project_document: &[u8],
	previous_projected: &snapshot::Snapshot,
	mapped_refs: &HashSet<Ref>,
) -> std::result::Result<ProjectSourceCandidate, String> {
	let (snapshot, routes, watch_roots) = project::reevaluate_projected_frozen_tracked(
		project_path,
		frozen_project_document,
		previous_projected,
		mapped_refs,
	)
	.map_err(|error| format!("{error:#}"))?;
	let encoded = rmp_serde::to_vec_named(&(snapshot.clone(), routes.clone(), watch_roots.clone()))
		.map_err(|error| format!("failed to fingerprint project source: {error}"))?;
	Ok(ProjectSourceCandidate {
		snapshot,
		routes,
		watch_roots,
		generation: blake3::hash(&encoded).to_hex().to_string(),
	})
}

fn project_watch_roots(project_root: &Path, mapped_watch_roots: &[PathBuf]) -> Vec<PathBuf> {
	let mut roots = vec![project_root.to_owned()];
	let mut candidates = mapped_watch_roots.to_vec();
	candidates.sort_by(|left, right| {
		left.components()
			.count()
			.cmp(&right.components().count())
			.then_with(|| left.cmp(right))
	});
	candidates.dedup();
	for candidate in candidates {
		if !roots.iter().any(|root| candidate.starts_with(root)) {
			roots.push(candidate);
		}
	}
	roots
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ReloadSignalKey {
	Manifest(blake3::Hash),
	WatchRoots(blake3::Hash),
}

#[derive(Debug, Default)]
struct ReloadSignalState {
	last_signaled: Option<ReloadSignalKey>,
}

#[derive(Debug, PartialEq, Eq)]
enum ManifestReadResult {
	Unchanged,
	ValidChanged { hash: blake3::Hash },
	InvalidJson(String),
}

impl ReloadSignalState {
	fn new() -> Self {
		Self::default()
	}

	fn check_manifest(&self, project_path: &Path, frozen_document: &[u8]) -> ManifestReadResult {
		let mut bytes = match std::fs::read(project_path) {
			Ok(b) => b,
			Err(err) => return ManifestReadResult::InvalidJson(err.to_string()),
		};
		if bytes == frozen_document {
			return ManifestReadResult::Unchanged;
		}
		if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
			std::thread::sleep(Duration::from_millis(50));
			if let Ok(retry_bytes) = std::fs::read(project_path) {
				bytes = retry_bytes;
			}
			if bytes == frozen_document {
				return ManifestReadResult::Unchanged;
			}
			if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
				return ManifestReadResult::InvalidJson("invalid or truncated JSON".to_string());
			}
		}
		let hash = blake3::hash(&bytes);
		ManifestReadResult::ValidChanged { hash }
	}

	fn should_signal_manifest(&mut self, hash: blake3::Hash) -> bool {
		let key = ReloadSignalKey::Manifest(hash);
		if self.last_signaled.as_ref() == Some(&key) {
			false
		} else {
			self.last_signaled = Some(key);
			true
		}
	}

	fn should_signal_watch_roots(&mut self, watch_roots: &[PathBuf]) -> bool {
		let mut hasher = blake3::Hasher::new();
		for root in watch_roots {
			hasher.update(root.to_string_lossy().as_bytes());
			hasher.update(b"\0");
		}
		let key = ReloadSignalKey::WatchRoots(hasher.finalize());
		if self.last_signaled.as_ref() == Some(&key) {
			false
		} else {
			self.last_signaled = Some(key);
			true
		}
	}

	fn signal_manifest_result(
		&mut self,
		result: ManifestReadResult,
		project_path: &Path,
		control_tx: Option<&crate::server::ServeControlSender>,
	) {
		match result {
			ManifestReadResult::InvalidJson(error) => log::warn!(
				"Project document {} changed on disk but is invalid or malformed JSON; retaining previous generation: {}",
				project_path.display(),
				error
			),
			ManifestReadResult::ValidChanged { hash } => {
				if self.should_signal_manifest(hash) {
					log::info!(
						"Project document {} changed stably on disk; signaling recoverable reload",
						project_path.display()
					);
					if let Some(control_tx) = control_tx {
						let _ = control_tx.send(crate::server::ServeControl::Reload);
					}
				} else {
					log::debug!(
						"Coalesced duplicate manifest reload signal for {}",
						project_path.display()
					);
					if let Some(control_tx) = control_tx {
						control_tx.retry_failed_reload();
					}
				}
			}
			ManifestReadResult::Unchanged => {}
		}
	}
}

#[allow(clippy::too_many_arguments)]
fn watch_project_source(
	policy: Arc<RwLock<project::LivePolicy>>,
	project_snapshot: Arc<RwLock<snapshot::Snapshot>>,
	project_state_lock: Arc<Mutex<()>>,
	writer: Arc<ArtifactProcessor>,
	queue: Arc<Queue>,
	source_reader: Arc<artifact_store::SourceReader>,
	managed_hierarchy: Arc<RwLock<ManagedHierarchyState>>,
	restart_required: Arc<AtomicBool>,
	control_tx: Option<crate::server::ServeControlSender>,
) -> Result<RecommendedWatcher> {
	let (project_path, mapped_watch_roots) = {
		let policy = policy.read().unwrap();
		(policy.project_path.clone(), policy.mapped_watch_roots.clone())
	};
	let project_root = project_path.parent().unwrap_or_else(|| Path::new(".")).to_owned();
	let watch_roots = project_watch_roots(&project_root, &mapped_watch_roots);
	let generated_data = project::data_dir(&project_path)?;
	let frozen_project_document = project::frozen_project_document(&project_path)?;
	let data_backup_prefix = format!(
		".{}.backup-",
		generated_data
			.file_name()
			.and_then(|name| name.to_str())
			.context("Carbon data directory name is not UTF-8")?
	);
	let (event_sender, event_receiver) = crossbeam_channel::unbounded();
	let callback_roots = watch_roots.clone();
	let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
		let Ok(event) = event else {
			return;
		};
		if !matches!(
			event.kind,
			EventKind::Any | EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Other
		) {
			return;
		}
		let relevant = event.paths.iter().any(|path| {
			callback_roots.iter().any(|root| path.starts_with(root))
				&& !path.starts_with(&generated_data)
				&& !path.components().any(|component| {
					component.as_os_str().to_str().is_some_and(|name| {
						name.starts_with(".carbon-")
							|| name.starts_with(&data_backup_prefix)
							|| name.ends_with(".transaction.json")
					})
				})
		});
		if relevant {
			let _ = event_sender.send(());
		}
	})?;
	Builder::new().name("carbon-project-watcher".into()).spawn(move || {
		let mut reload_state = ReloadSignalState::new();
		let mut source_sync_pending = false;
		loop {
			let observed_event = match event_receiver.recv_timeout(Duration::from_millis(500)) {
				Ok(()) => true,
				Err(crossbeam_channel::RecvTimeoutError::Timeout) => false,
				Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
			};
			source_sync_pending |= observed_event;
			let _state = project_state_lock.lock().unwrap();
			let manifest_result = reload_state.check_manifest(&project_path, &frozen_project_document);
			if !source_sync_pending {
				reload_state.signal_manifest_result(manifest_result, &project_path, control_tx.as_ref());
				continue;
			}
			let previous_snapshot = project_snapshot.read().unwrap().clone();
			let previous_mapped_refs = policy.read().unwrap().mapped_refs.clone();
			let candidate = coalesce_stable_read(
				&event_receiver,
				Duration::from_millis(150),
				Duration::from_millis(75),
				Duration::from_secs(2),
				|| {
					read_project_candidate(
						&project_path,
						&frozen_project_document,
						&previous_snapshot,
						&previous_mapped_refs,
					)
				},
				|candidate| candidate.generation.as_str(),
			);
			let candidate = match candidate {
				Ok(Some(candidate)) => candidate,
				Ok(None) => return,
				Err(error) => {
					let message = format!(
						"Project source changed but is invalid; retaining the last valid Studio realization: {error}"
					);
					log::error!("{message}");
					continue;
				}
			};
			if candidate.watch_roots != policy.read().unwrap().mapped_watch_roots {
				if reload_state.should_signal_watch_roots(&candidate.watch_roots) {
					log::info!("Mapped watch roots changed; signaling recoverable reload");
					if let Some(control_tx) = &control_tx {
						let _ = control_tx.send(crate::server::ServeControl::Reload);
					}
				} else {
					log::debug!("Coalesced duplicate watch roots reload signal");
					if let Some(control_tx) = &control_tx {
						control_tx.retry_failed_reload();
					}
				}
				continue;
			}
			let changes = match project::diff_snapshots(&previous_snapshot, &candidate.snapshot) {
				Ok(changes) => changes,
				Err(error) => {
					let message = format!("Could not apply project source change transactionally: {error:#}");
					log::error!("{message}");
					continue;
				}
			};
			if changes.is_empty() {
				source_sync_pending = false;
				reload_state.signal_manifest_result(manifest_result, &project_path, control_tx.as_ref());
				continue;
			}
			log::debug!(
				"Applying live project source transaction: {} additions, {} updates, {} removals",
				changes.additions.len(),
				changes.updates.len(),
				changes.removals.len()
			);
			let candidate_tree = Tree::new(candidate.snapshot.clone());
			let (mapped_refs, mapped_roots) = match project::refs_for_routes(&candidate_tree, &candidate.routes) {
				Ok(result) => result,
				Err(error) => {
					let message = format!("Project source has an invalid ownership route: {error:#}");
					log::error!("{message}");
					continue;
				}
			};
			let routing_refs = match project::routing_refs(&candidate_tree, &mapped_roots) {
				Ok(routing) => routing,
				Err(error) => {
					log::error!("Project source has an invalid engine route: {error:#}");
					continue;
				}
			};
			let contract = match encode_project_managed_hierarchy(&candidate_tree, &mapped_refs, &routing_refs) {
				Ok(contract) => contract,
				Err(error) => {
					log::error!("Could not preflight the managed hierarchy after source change: {error:#}");
					continue;
				}
			};
			let projected_source_ids = contract.source_ids.clone();
			let source_generation = if changes.is_empty() {
				source_reader.generation()
			} else {
				match writer.apply_authoritative(changes.clone()) {
					Ok(generation) => generation,
					Err(error) => {
						let message = format!("Could not commit project source change: {error:#}");
						log::error!("{message}");
						continue;
					}
				}
			};
			*project_snapshot.write().unwrap() = candidate.snapshot.clone();
			{
				let mut policy = policy.write().unwrap();
				let retired = project::retired_source_refs(&policy.mapped_refs, &mapped_refs, &changes.removals);
				policy.retired_mapped_refs.extend(retired);
				policy.retired_mapped_refs.retain(|id| !mapped_refs.contains(id));
				policy.mapped_refs = mapped_refs;
				policy.mapped_roots = mapped_roots;
				policy.mapped_watch_roots = candidate.watch_roots;
				policy.routing_refs = routing_refs;
			}
			install_managed_hierarchy_contract(&managed_hierarchy, contract);
			if let Err(error) = source_reader.set_projected_source_ids(projected_source_ids, source_generation.clone()) {
				restart_required.store(true, Ordering::Release);
				let message = format!(
					"Project source committed, but the projected source reader could not be refreshed; hard restart required: {error:#}"
				);
				log::error!("{message}");
				let _ = queue.push(
					crate::server::Message::RestartRequired(crate::server::RestartRequired { message }),
					None,
				);
				return;
			}
			if !changes.is_empty() {
				if let Err(error) = queue.push(
					crate::server::Message::SyncChanges(crate::server::SyncChanges {
						changes,
						source_generation,
					}),
					None,
				) {
					log::error!("Could not deliver project source change to Studio: {error:#}");
					continue;
				}
			}
			source_sync_pending = false;
			reload_state.signal_manifest_result(manifest_result, &project_path, control_tx.as_ref());
		}
	})?;
	for root in watch_roots {
		watcher
			.watch(&root, RecursiveMode::Recursive)
			.with_context(|| format!("failed to watch mapped source root {}", root.display()))?;
	}
	Ok(watcher)
}

fn watch_script_sources(
	manifest_path: &Path,
	script_roots: Vec<PathBuf>,
	sources: Arc<RwLock<HashMap<PathBuf, Ref>>>,
	queue: Arc<Queue>,
	commit_lock: Arc<Mutex<()>>,
	source_reader: Arc<artifact_store::SourceReader>,
) -> Result<RecommendedWatcher> {
	let manifest_path = manifest_path.to_owned();
	let initial_snapshot = artifact_store::canonical_source_snapshot_for_paths(
		&manifest_path,
		sources.read().unwrap().keys().cloned().collect::<Vec<_>>(),
	)?;
	let (event_sender, event_receiver) = crossbeam_channel::unbounded();
	let callback_sources = sources.clone();
	let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
		let Ok(event) = event else {
			return;
		};
		if !matches!(
			event.kind,
			EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
		) {
			return;
		}
		if !event
			.paths
			.iter()
			.any(|path| callback_sources.read().unwrap().contains_key(path))
		{
			return;
		}
		let _ = event_sender.send(());
	})?;
	Builder::new().name("script-source-watcher".into()).spawn(move || {
		let mut previous = initial_snapshot;
		while event_receiver.recv().is_ok() {
			let candidate = coalesce_stable_read(
				&event_receiver,
				Duration::from_millis(150),
				Duration::from_millis(75),
				Duration::from_secs(2),
				|| {
					let _commit = commit_lock.lock().unwrap();
					script_source_candidate(&manifest_path, &previous, &sources)
				},
				|candidate| candidate.snapshot.generation.as_str(),
			);
			let candidate = match candidate {
				Ok(Some(candidate)) => candidate,
				Ok(None) => return,
				Err(error) => {
					let _ = queue.push(crate::server::Disconnect { message: error }, None);
					continue;
				}
			};
			if candidate.snapshot.generation == previous.generation {
				continue;
			}
			if candidate.changes.is_empty() {
				// Canonical generation also covers non-script source data. A delayed
				// duplicate filesystem notification after a Studio property commit must
				// advance this baseline without inventing a script topology failure.
				source_reader.set_generation(candidate.snapshot.generation.clone());
				previous = candidate.snapshot;
				continue;
			}
			if queue
				.push(
					crate::server::Message::SyncChanges(crate::server::SyncChanges {
						changes: candidate.changes,
						source_generation: candidate.snapshot.generation.clone(),
					}),
					None,
				)
				.is_ok()
			{
				source_reader.set_generation(candidate.snapshot.generation.clone());
				previous = candidate.snapshot;
			}
		}
	})?;
	for script_root in script_roots {
		watcher.watch(&script_root, RecursiveMode::Recursive)?;
	}
	Ok(watcher)
}

struct ScriptSourceCandidate {
	snapshot: artifact_store::CanonicalSourceSnapshot,
	changes: changes::Changes,
}

fn script_source_candidate(
	manifest_path: &Path,
	previous: &artifact_store::CanonicalSourceSnapshot,
	sources: &RwLock<HashMap<PathBuf, Ref>>,
) -> std::result::Result<ScriptSourceCandidate, String> {
	let current = artifact_store::canonical_source_snapshot_for_paths(
		manifest_path,
		sources.read().unwrap().keys().cloned().collect::<Vec<_>>(),
	)
	.map_err(|error| format!("Source changed but its canonical snapshot is invalid: {error:#}"))?;
	script_source_candidate_from_snapshot(current, previous, sources)
}

fn script_source_candidate_from_snapshot(
	current: artifact_store::CanonicalSourceSnapshot,
	previous: &artifact_store::CanonicalSourceSnapshot,
	sources: &RwLock<HashMap<PathBuf, Ref>>,
) -> std::result::Result<ScriptSourceCandidate, String> {
	let mut changes = changes::Changes::new();
	if current.generation == previous.generation {
		return Ok(ScriptSourceCandidate {
			snapshot: current,
			changes,
		});
	}
	if current.scripts.len() != previous.scripts.len()
		|| previous.scripts.keys().any(|path| !current.scripts.contains_key(path))
	{
		return Err("Source topology changed outside Carbon; recapture the place".to_owned());
	}
	let source_ids = sources.read().unwrap();
	for (path, bytes) in &current.scripts {
		if previous.scripts.get(path) == Some(bytes) {
			continue;
		}
		let id = source_ids.get(path).copied().ok_or_else(|| {
			format!(
				"Source topology changed outside Carbon at {}; recapture the place",
				path.display()
			)
		})?;
		let source = String::from_utf8(bytes.clone())
			.map_err(|_| format!("Script source is not valid UTF-8: {}", path.display()))?;
		let mut update = snapshot::UpdatedSnapshot::new(id);
		update.properties = Some(UstrMap::from_iter([(Ustr::from("Source"), Variant::String(source))]));
		changes.update(update);
	}
	Ok(ScriptSourceCandidate {
		snapshot: current,
		changes,
	})
}

fn coalesce_stable_read<T, Read, Key>(
	events: &Receiver<()>,
	quiet: Duration,
	stability: Duration,
	invalid_timeout: Duration,
	mut read: Read,
	key: Key,
) -> std::result::Result<Option<T>, String>
where
	Read: FnMut() -> std::result::Result<T, String>,
	Key: Fn(&T) -> &str,
{
	'quiet: loop {
		loop {
			match events.recv_timeout(quiet) {
				Ok(()) => continue,
				Err(RecvTimeoutError::Timeout) => break,
				Err(RecvTimeoutError::Disconnected) => return Ok(None),
			}
		}

		let invalid_since = Instant::now();
		loop {
			let first = read();
			match events.recv_timeout(stability) {
				Ok(()) => continue 'quiet,
				Err(RecvTimeoutError::Disconnected) => return Ok(None),
				Err(RecvTimeoutError::Timeout) => {}
			}
			let second = read();
			match (first, second) {
				(Ok(first), Ok(second)) if key(&first) == key(&second) => return Ok(Some(second)),
				(Err(first), Err(second)) if first == second && invalid_since.elapsed() >= invalid_timeout => {
					return Err(second)
				}
				_ => {}
			}
		}
	}
}

#[cfg(test)]
mod source_watcher_tests {
	use super::*;
	use rbx_dom_weak::types::Variant;
	use std::thread;

	#[test]
	fn capture_epoch_diagnostics_name_the_invalidated_evidence() {
		let expected = CaptureEpochReceipt {
			bridge_id: "bridge".to_owned(),
			process_id: 42,
			studio_session_id: "studio".to_owned(),
			instance_id: "instance".to_owned(),
			engine_generation: 7,
			hierarchy_sequence: 11,
			change_sequence: 13,
			source_generation: "source".to_owned(),
			managed_contract_id: "contract".to_owned(),
			project_realization_generation: "project".to_owned(),
			artifact_generation: "artifact".to_owned(),
			artifact_fingerprint: "fingerprint".to_owned(),
			artifact_len: 100,
			artifact_modified: SystemTime::UNIX_EPOCH,
		};
		let mut observed = expected.clone();
		observed.change_sequence += 1;
		observed.artifact_len += 1;
		assert_eq!(
			capture_epoch_changed_fields(&expected, &observed),
			["change_sequence", "artifact_len"]
		);
	}

	#[test]
	fn project_watches_external_pesde_targets_without_duplicate_nested_roots() {
		let project_root = Path::new("/workspace/game");
		let roots = project_watch_roots(
			project_root,
			&[
				PathBuf::from("/dependencies/package/src"),
				PathBuf::from("/workspace/game/roblox_packages/.pesde"),
				PathBuf::from("/dependencies/package"),
				PathBuf::from("/dependencies/package"),
			],
		);
		assert_eq!(
			roots,
			vec![project_root.to_owned(), PathBuf::from("/dependencies/package")]
		);
	}

	#[test]
	fn manifest_identity_bootstrap_excludes_the_complete_filesystem_domain() {
		let root = Ref::new();
		let mapped = Ref::new();
		let mapped_child = Ref::new();
		let studio = Ref::new();
		let snapshot = snapshot::Snapshot::new()
			.with_id(root)
			.with_name("DataModel")
			.with_class("DataModel")
			.with_children(vec![
				snapshot::Snapshot::new()
					.with_id(mapped)
					.with_name("Mapped")
					.with_class("Folder")
					.with_children(vec![snapshot::Snapshot::new()
						.with_id(mapped_child)
						.with_name("Script")
						.with_class("ModuleScript")]),
				snapshot::Snapshot::new()
					.with_id(studio)
					.with_name("Studio")
					.with_class("Folder"),
			]);
		let contract =
			manifest_identity_bootstrap_contract(&snapshot, &HashSet::from([mapped, mapped_child]), &[]).unwrap();
		assert_eq!(contract.root_source_id, root.to_string());
		assert_eq!(contract.expected_source_instances, 2);
		assert_eq!(contract.expected_digest.len(), 64);
	}

	#[test]
	fn manifest_identity_bootstrap_rebinds_studio_rehydrated_identity() {
		let root = Ref::new();
		let workspace = Ref::new();
		let accessory = Ref::new();
		let handle = Ref::new();
		let weld = Ref::new();
		let weld_snapshot = snapshot::Snapshot::new()
			.with_id(weld)
			.with_name("AccessoryWeld")
			.with_class("Weld");
		let handle_snapshot = snapshot::Snapshot::new()
			.with_id(handle)
			.with_name("Handle")
			.with_class("Part")
			.with_children(vec![weld_snapshot]);
		let accessory_snapshot = snapshot::Snapshot::new()
			.with_id(accessory)
			.with_name("Hat")
			.with_class("Accessory")
			.with_children(vec![handle_snapshot]);
		let workspace_snapshot = snapshot::Snapshot::new()
			.with_id(workspace)
			.with_name("Workspace")
			.with_class("Workspace")
			.with_children(vec![accessory_snapshot]);
		let snapshot = snapshot::Snapshot::new()
			.with_id(root)
			.with_name("DataModel")
			.with_class("DataModel")
			.with_children(vec![workspace_snapshot]);

		let rebindings = project::managed_build_identity_rebindings(&snapshot, &HashSet::new());
		let contract = manifest_identity_bootstrap_contract(&snapshot, &HashSet::from([weld]), &rebindings).unwrap();
		assert_eq!(contract.expected_source_instances, 5);
		assert_eq!(
			serde_json::to_value(&contract).unwrap()["rebindings"],
			serde_json::json!([{
				"sourceId": weld.to_string(),
				"parentSourceId": handle.to_string(),
				"className": "Weld",
				"name": "AccessoryWeld",
				"kind": "accessoryWeld",
				"relatedSourceId": null
			}])
		);
	}

	struct FakeRefreshBridge {
		calls: Mutex<Vec<String>>,
		staged: Mutex<Option<(String, u32)>>,
		attached: Mutex<Option<(String, u32)>>,
		manifest_identities_authoritative: bool,
	}

	impl FakeRefreshBridge {
		fn new() -> Self {
			Self {
				calls: Mutex::new(Vec::new()),
				staged: Mutex::new(None),
				attached: Mutex::new(None),
				manifest_identities_authoritative: false,
			}
		}

		fn authoritative() -> Self {
			Self {
				manifest_identities_authoritative: true,
				..Self::new()
			}
		}
	}

	impl ManagedHierarchyRefreshBridge for FakeRefreshBridge {
		fn bridge_id(&self) -> &str {
			"bridge"
		}

		fn identity(&self) -> Result<StudioIdentity> {
			Ok(StudioIdentity {
				studio_session_id: "studio".to_owned(),
				instance_id: "instance".to_owned(),
				bridge_id: "bridge".to_owned(),
				process_id: 42,
			})
		}

		fn capabilities(&self) -> Result<Capabilities> {
			let attached = self.attached.lock().unwrap().clone();
			Ok(Capabilities {
				protocol_version: 2,
				bridge_id: "bridge".to_owned(),
				process_id: 42,
				engine_ready: true,
				engine_generation: 7,
				studio_session_id: "studio".to_owned(),
				instance_id: "instance".to_owned(),
				hierarchy_sequence: 0,
				change_sequence: 0,
				binary_types: Vec::new(),
				scalar_types: Vec::new(),
				blittable_types: Vec::new(),
				raw_types: Vec::new(),
				native_observation: true,
				engine_creation: true,
				per_root_availability: true,
				serialized_references: true,
				managed_hierarchy_attachment: true,
				managed_contract_id: attached
					.as_ref()
					.map(|(contract_id, _)| contract_id.clone())
					.unwrap_or_default(),
				managed_contract_source_instances: attached.map(|(_, instances)| instances).unwrap_or_default(),
				manifest_identity_ledger: true,
				manifest_identities_authoritative: self.manifest_identities_authoritative,
				capture_lease_protocol: crate::capture_provider::CAPTURE_ENVELOPE_VERSION,
				local_place_save_diagnostic: true,
			})
		}

		fn stage(&self, contract: &ManagedHierarchyContract) -> Result<ManagedHierarchyStage> {
			self.calls
				.lock()
				.unwrap()
				.push(format!("stage:{}", contract.contract_id));
			*self.staged.lock().unwrap() = Some((contract.contract_id.clone(), contract.source_instances));
			Ok(ManagedHierarchyStage {
				contract_id: contract.contract_id.clone(),
				source_instances: contract.source_instances,
			})
		}

		fn attach(&self, contract_id: &str) -> Result<ManagedHierarchyAttachment> {
			self.calls.lock().unwrap().push(format!("attach:{contract_id}"));
			let (staged_id, source_instances) = self.staged.lock().unwrap().clone().unwrap();
			ensure!(staged_id == contract_id, "test attached an unstaged contract");
			*self.attached.lock().unwrap() = Some((contract_id.to_owned(), source_instances));
			Ok(ManagedHierarchyAttachment {
				attached: true,
				source_instances,
				hierarchy_sequence: 1,
				change_sequence: 1,
				excluded_source_ids: Vec::new(),
				source_root_debug_ids: Vec::new(),
			})
		}
	}

	#[test]
	fn trusted_managed_launch_seeds_first_capture_epoch() {
		let directory =
			std::env::temp_dir().join(format!("carbon-trusted-launch-capture-epoch-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&directory).unwrap();
		let project_path = directory.join("game.carbon.json");
		project::initialize(&project_path, "Game".to_owned()).unwrap();

		let materialized = project::materialize(&project_path).unwrap();
		let core = Core::new_project_with_worktree(
			&project_path,
			&materialized,
			("Game".to_owned(), "worktree".to_owned(), "session".to_owned()),
		)
		.unwrap();
		let bridge = FakeRefreshBridge::authoritative();
		let capabilities = bridge.capabilities().unwrap();
		let source_generation = core.source_generation();
		core.remember_trusted_managed_launch_epoch_if_current(&bridge, &capabilities);

		let epoch = core
			.last_capture_epoch
			.lock()
			.unwrap()
			.clone()
			.expect("a trusted unchanged launch must be ready for its first epoch no-op");
		assert_eq!(epoch.artifact_fingerprint, source_generation);
		assert_eq!(
			*bridge.calls.lock().unwrap(),
			[
				format!("stage:{}", epoch.managed_contract_id),
				format!("attach:{}", epoch.managed_contract_id),
			],
			"the epoch must be recorded after the launch attaches its managed contract"
		);
		assert_eq!(epoch.source_generation, source_generation);
		assert_eq!(
			epoch.managed_contract_id,
			core.managed_hierarchy_contract().unwrap().contract_id
		);

		drop(core);
		std::fs::remove_dir_all(materialized.directory).unwrap();
		std::fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn two_successive_captures_stage_and_attach_each_committed_contract() {
		let bridge = FakeRefreshBridge::new();
		let request = CaptureRequest {
			capture_id: "00000000000000000000000000000000".to_owned(),
			studio_session_id: "studio".to_owned(),
			instance_id: "instance".to_owned(),
			engine_generation: 7,
			source_generation: "generation".to_owned(),
			managed_contract_id: "prior".to_owned(),
			reflection_schema_hash: "schema".to_owned(),
			manifest_identities_authoritative: false,
			allow_page_reuse: true,
			mapped_root_source_ids: Vec::new(),
			shell_classes: Vec::new(),
		};
		for (contract_id, source_instances) in [("first", 16), ("second", 17)] {
			let contract = ManagedHierarchyContract {
				contract_id: contract_id.to_owned(),
				payload: Bytes::from_static(b"contract"),
				source_instances,
				excluded_source_ids: Vec::new(),
				source_ids: HashSet::new(),
			};
			let attachment = refresh_capture_contract_on_bridge(
				&bridge,
				&contract,
				&request.studio_session_id,
				&request.instance_id,
				request.engine_generation,
				42,
			)
			.unwrap();
			assert!(attachment.attached);
			assert_eq!(attachment.source_instances, source_instances);
		}
		assert_eq!(
			*bridge.calls.lock().unwrap(),
			["stage:first", "attach:first", "stage:second", "attach:second"]
		);
	}

	#[test]
	fn repeated_capture_reuses_the_attached_managed_contract_receipt() {
		let bridge = FakeRefreshBridge::new();
		let request = CaptureRequest {
			capture_id: "00000000000000000000000000000000".to_owned(),
			studio_session_id: "studio".to_owned(),
			instance_id: "instance".to_owned(),
			engine_generation: 7,
			source_generation: "generation".to_owned(),
			managed_contract_id: "contract".to_owned(),
			reflection_schema_hash: "schema".to_owned(),
			manifest_identities_authoritative: true,
			allow_page_reuse: true,
			mapped_root_source_ids: Vec::new(),
			shell_classes: Vec::new(),
		};
		let contract = ManagedHierarchyContract {
			contract_id: "contract".to_owned(),
			payload: Bytes::from_static(b"contract"),
			source_instances: 1_000_000,
			excluded_source_ids: Vec::new(),
			source_ids: HashSet::new(),
		};

		refresh_capture_contract_on_bridge(
			&bridge,
			&contract,
			&request.studio_session_id,
			&request.instance_id,
			request.engine_generation,
			42,
		)
		.unwrap();
		refresh_capture_contract_on_bridge(
			&bridge,
			&contract,
			&request.studio_session_id,
			&request.instance_id,
			request.engine_generation,
			42,
		)
		.unwrap();
		assert_eq!(
			*bridge.calls.lock().unwrap(),
			["stage:contract", "attach:contract"],
			"an already-attached million-node contract must not be uploaded or parsed again"
		);
	}

	#[test]
	fn accepted_capture_cancellation_closes_the_commit_gate() {
		let phase = AtomicU8::new(CAPTURE_COLLECTING);
		claim_capture_cancel(&phase).unwrap();
		assert_eq!(phase.load(Ordering::Acquire), CAPTURE_CANCELLED);
		assert!(claim_capture_commit(&phase)
			.unwrap_err()
			.to_string()
			.contains("cancelled before commit"));
	}

	#[test]
	fn capture_cancellation_is_rejected_after_atomic_commit_begins() {
		let phase = AtomicU8::new(CAPTURE_COLLECTING);
		claim_capture_commit(&phase).unwrap();
		assert_eq!(phase.load(Ordering::Acquire), CAPTURE_COMMITTING);
		assert!(claim_capture_cancel(&phase)
			.unwrap_err()
			.to_string()
			.contains("can no longer be cancelled"));
	}

	#[test]
	fn truncate_and_partial_notifications_coalesce_to_the_final_script_source() {
		let (sender, receiver) = crossbeam_channel::unbounded();
		let _keepalive = sender.clone();
		let state = Arc::new(Mutex::new(Ok("partial".to_owned())));
		let writer_state = state.clone();
		thread::spawn(move || {
			thread::sleep(Duration::from_millis(5));
			*writer_state.lock().unwrap() = Ok("".to_owned());
			sender.send(()).unwrap();
			thread::sleep(Duration::from_millis(5));
			*writer_state.lock().unwrap() = Ok("return 'final'".to_owned());
			sender.send(()).unwrap();
			thread::sleep(Duration::from_millis(40));
		});

		let result = coalesce_stable_read(
			&receiver,
			Duration::from_millis(15),
			Duration::from_millis(5),
			Duration::from_millis(100),
			|| state.lock().unwrap().clone(),
			String::as_str,
		)
		.unwrap()
		.unwrap();
		assert_eq!(result, "return 'final'");
	}

	#[test]
	fn transient_invalid_utf8_state_waits_for_the_final_valid_source() {
		let (sender, receiver) = crossbeam_channel::unbounded();
		let _keepalive = sender.clone();
		let state = Arc::new(Mutex::new(Err("Script source is not valid UTF-8".to_owned())));
		let writer_state = state.clone();
		thread::spawn(move || {
			thread::sleep(Duration::from_millis(10));
			*writer_state.lock().unwrap() = Ok("return 42".to_owned());
			sender.send(()).unwrap();
			thread::sleep(Duration::from_millis(40));
		});

		let result = coalesce_stable_read(
			&receiver,
			Duration::from_millis(15),
			Duration::from_millis(5),
			Duration::from_millis(100),
			|| state.lock().unwrap().clone(),
			String::as_str,
		)
		.unwrap()
		.unwrap();
		assert_eq!(result, "return 42");
	}

	#[test]
	fn stale_script_signal_after_non_script_generation_change_is_a_baseline_only_noop() {
		let path = PathBuf::from("/tmp/unchanged-script.luau");
		let bytes = b"return 'unchanged'".to_vec();
		let previous = artifact_store::CanonicalSourceSnapshot {
			generation: "generation-before-property-syncback".to_owned(),
			scripts: HashMap::from([(path.clone(), bytes.clone())]),
		};
		let current = artifact_store::CanonicalSourceSnapshot {
			generation: "generation-after-property-syncback".to_owned(),
			scripts: HashMap::from([(path.clone(), bytes)]),
		};
		let sources = RwLock::new(HashMap::from([(path, Ref::new())]));

		let candidate = script_source_candidate_from_snapshot(current, &previous, &sources).unwrap();
		assert!(
			candidate.changes.is_empty(),
			"stale script notification must emit no SyncChanges"
		);
		assert_eq!(candidate.snapshot.generation, "generation-after-property-syncback");
	}

	#[test]
	fn exact_studio_route_attribute_is_removed_from_raw_server_storage_payload() {
		let marker = "4c0ad7cb-c6e7-4e62-8c35-b9537f0c1f30";
		let attributes = Attributes::new()
			.with(
				"__MCPPlaceId",
				rbx_dom_weak::types::BinaryString::from(marker.as_bytes()),
			)
			.with("Authored", rbx_dom_weak::types::BinaryString::from(b"keep".as_slice()));
		let mut raw = Vec::new();
		attributes.to_writer(&mut raw).unwrap();
		let mut value = Variant::BinaryString(raw.into());

		assert!(strip_matching_mcp_place_id(&mut value, marker).unwrap());
		let Variant::Attributes(filtered) = value else {
			panic!("raw Attributes payload was not normalized")
		};
		assert!(filtered.get("__MCPPlaceId").is_none());
		assert_eq!(
			filtered.get("Authored"),
			Some(&Variant::BinaryString(rbx_dom_weak::types::BinaryString::from(
				b"keep".as_slice()
			)))
		);
	}

	#[test]
	fn mismatched_mcp_attribute_remains_authored() {
		let mut value = Variant::Attributes(Attributes::new().with(
			"__MCPPlaceId",
			rbx_dom_weak::types::BinaryString::from(b"authored-collision".as_slice()),
		));

		assert!(!strip_matching_mcp_place_id(&mut value, "active-route").unwrap());
		let Variant::Attributes(attributes) = value else {
			panic!("structured Attributes payload changed type")
		};
		assert_eq!(
			attributes.get("__MCPPlaceId"),
			Some(&Variant::BinaryString(rbx_dom_weak::types::BinaryString::from(
				b"authored-collision".as_slice()
			)))
		);
	}

	#[test]
	fn every_serialized_attributes_alias_is_sanitized_before_source_write() {
		let marker = "active-route";
		let mut raw = Vec::new();
		Attributes::new()
			.with(
				"__MCPPlaceId",
				rbx_dom_weak::types::BinaryString::from(marker.as_bytes()),
			)
			.to_writer(&mut raw)
			.unwrap();
		let mut properties =
			UstrMap::from_iter([(Ustr::from("AttributesReplicate"), Variant::BinaryString(raw.into()))]);

		assert!(strip_studio_route_properties(&mut properties, marker).unwrap());
		assert!(properties.is_empty(), "marker-only aliases must become a no-op write");
	}

	#[test]
	fn disposable_worktree_attributes_are_sanitized_without_an_mcp_marker() {
		let mut properties = UstrMap::from_iter([(
			Ustr::from("Attributes"),
			Variant::Attributes(
				Attributes::new()
					.with(
						"__StudioWorktree_CarbonGeneration",
						Variant::BinaryString(b"generation".as_slice().into()),
					)
					.with("Authored", Variant::String("keep".to_owned())),
			),
		)]);

		assert!(strip_studio_route_properties(&mut properties, "active-route").unwrap());
		let Some(Variant::Attributes(attributes)) = properties.get(&Ustr::from("Attributes")) else {
			panic!("authored attributes disappeared")
		};
		assert_eq!(attributes.get("Authored"), Some(&Variant::String("keep".to_owned())));
		assert!(attributes.get("__StudioWorktree_CarbonGeneration").is_none());
	}

	#[test]
	fn native_materialization_pages_properties_from_the_binary_artifact() {
		let directory = std::env::temp_dir().join(format!("carbon-native-materialize-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&directory).unwrap();
		let manifest = directory.join("place.carbon");
		let data_model = Ref::new();
		let hidden = Ref::new();
		let script = Ref::new();
		let snapshot = snapshot::Snapshot::new()
			.with_id(data_model)
			.with_class("DataModel")
			.with_name("Fixture")
			.with_children(vec![snapshot::Snapshot::new()
				.with_id(hidden)
				.with_class("ServerStorage")
				.with_name("ServerStorage")
				.with_children(vec![snapshot::Snapshot::new()
					.with_id(script)
					.with_class("Script")
					.with_name("AuthoredScript")
					.with_properties(UstrMap::from_iter([
						(Ustr::from("Source"), Variant::String("return 'before'".to_owned())),
						(Ustr::from("Disabled"), Variant::Bool(true)),
					]))])]);
		artifact_store::extract_snapshot(snapshot, "Fixture".to_owned(), &manifest).unwrap();

		let core = Core::new_artifact(&manifest).unwrap();
		let script = {
			let live = core.tree();
			let hidden = live.root().children()[0];
			let script = live.get_instance(hidden).unwrap().children()[0];
			assert!(
				live.get_instance(script).unwrap().properties.is_empty(),
				"the live reconciliation tree should remain hierarchy-only"
			);
			script
		};
		let initial_generation = core.source_generation();
		let initial = core.materialized_source_tree(&initial_generation).unwrap();
		assert_eq!(
			initial.get_instance(script).unwrap().properties[&Ustr::from("Source")],
			Variant::String("return 'before'".to_owned())
		);
		assert_eq!(
			initial.get_instance(script).unwrap().properties[&Ustr::from("Disabled")],
			Variant::Bool(true)
		);

		drop(core);
		std::fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn managed_identity_history_retains_removed_ids_across_live_source_replacement() {
		let root = Ref::new();
		let removed = Ref::new();
		let added = Ref::new();
		let initial = encode_managed_hierarchy(&Tree::new(
			snapshot::Snapshot::new()
				.with_id(root)
				.with_class("DataModel")
				.with_name("Fixture")
				.with_children(vec![snapshot::Snapshot::new()
					.with_id(removed)
					.with_class("Script")
					.with_name("Server")]),
		))
		.unwrap();
		let replacement = encode_managed_hierarchy(&Tree::new(
			snapshot::Snapshot::new()
				.with_id(root)
				.with_class("DataModel")
				.with_name("Fixture")
				.with_children(vec![snapshot::Snapshot::new()
					.with_id(added)
					.with_class("Script")
					.with_name("Server")]),
		))
		.unwrap();
		let state = RwLock::new(ManagedHierarchyState {
			current: initial.clone(),
			authorized_ids: initial.source_ids,
		});

		install_managed_hierarchy_contract(&state, replacement.clone());

		let state = state.read().unwrap();
		assert!(
			state.authorized_ids.contains(&removed),
			"the verified old identity is needed for Studio removal"
		);
		assert!(
			state.authorized_ids.contains(&added),
			"the replacement identity must become authorized"
		);
		assert_eq!(state.current.source_ids, replacement.source_ids);
	}

	#[test]
	fn managed_hierarchy_blob_is_compact_preorder_without_serialized_identity_properties() {
		let root = Ref::new();
		let first = Ref::new();
		let child = Ref::new();
		let tree = Tree::new(
			snapshot::Snapshot::new()
				.with_id(root)
				.with_class("DataModel")
				.with_name("Fixture")
				.with_children(vec![snapshot::Snapshot::new()
					.with_id(first)
					.with_class("Folder")
					.with_name("Gameplay")
					.with_children(vec![snapshot::Snapshot::new()
						.with_id(child)
						.with_class("Script")
						.with_name("Main")])]),
		);
		let contract = encode_managed_hierarchy(&tree).unwrap();
		let encoded = contract.payload;
		let header = MANAGED_HIERARCHY_MAGIC.len();
		assert_eq!(&encoded[..header], b"CARBONID4");
		assert_eq!(u32::from_le_bytes(encoded[header..header + 4].try_into().unwrap()), 3);
		assert!(encoded.len() < 256, "identity hierarchy should remain compact");
		assert!(!encoded.windows(8).any(|window| window == b"UniqueId"));
	}

	#[test]
	fn project_managed_hierarchy_is_constant_size_without_mappings() {
		let root = Ref::new();
		let workspace = Ref::new();
		let manifest_root = Ref::new();
		let manifest_children = (0..40_000)
			.map(|index| {
				let name = format!("Manifest{index:05}");
				snapshot::Snapshot::new()
					.with_id(Ref::new())
					.with_class("Folder")
					.with_name(&name)
			})
			.collect();
		let tree = Tree::new(
			snapshot::Snapshot::new()
				.with_id(root)
				.with_class("DataModel")
				.with_name("Fixture")
				.with_children(vec![snapshot::Snapshot::new()
					.with_id(workspace)
					.with_class("Workspace")
					.with_name("Workspace")
					.with_children(vec![snapshot::Snapshot::new()
						.with_id(manifest_root)
						.with_class("Folder")
						.with_name("Manifest")
						.with_children(manifest_children)])]),
		);

		let contract = encode_project_managed_hierarchy(&tree, &HashSet::new(), &HashSet::new()).unwrap();
		assert_eq!(contract.source_instances, 2);
		assert_eq!(contract.source_ids, HashSet::from([root, workspace]));
		assert!(!contract.source_ids.contains(&manifest_root));
	}

	#[test]
	fn project_managed_hierarchy_keeps_route_and_complete_mapped_subtree_only() {
		let root = Ref::new();
		let replicated_storage = Ref::new();
		let mapped_root = Ref::new();
		let mapped_script = Ref::new();
		let manifest_sibling = Ref::new();
		let tree = Tree::new(
			snapshot::Snapshot::new()
				.with_id(root)
				.with_class("DataModel")
				.with_name("Fixture")
				.with_children(vec![snapshot::Snapshot::new()
					.with_id(replicated_storage)
					.with_class("ReplicatedStorage")
					.with_name("ReplicatedStorage")
					.with_children(vec![
						snapshot::Snapshot::new()
							.with_id(mapped_root)
							.with_class("Folder")
							.with_name("Shared")
							.with_children(vec![snapshot::Snapshot::new()
								.with_id(mapped_script)
								.with_class("ModuleScript")
								.with_name("Main")]),
						snapshot::Snapshot::new()
							.with_id(manifest_sibling)
							.with_class("Folder")
							.with_name("Captured"),
					])]),
		);

		let contract = encode_project_managed_hierarchy(
			&tree,
			&HashSet::from([mapped_root]),
			&HashSet::from([replicated_storage]),
		)
		.unwrap();
		assert_eq!(contract.source_instances, 4);
		assert_eq!(
			contract.source_ids,
			HashSet::from([root, replicated_storage, mapped_root, mapped_script])
		);
		assert!(!contract.source_ids.contains(&manifest_sibling));
	}

	#[test]
	fn managed_hierarchy_omits_only_the_workspace_edit_camera() {
		let root = Ref::new();
		let workspace = Ref::new();
		let edit_camera = Ref::new();
		let authored_camera = Ref::new();
		let mut workspace_properties = UstrMap::default();
		workspace_properties.insert(Ustr::from("CurrentCamera"), Variant::Ref(edit_camera));
		let tree = Tree::new(
			snapshot::Snapshot::new()
				.with_id(root)
				.with_class("DataModel")
				.with_children(vec![snapshot::Snapshot::new()
					.with_id(workspace)
					.with_class("Workspace")
					.with_name("Workspace")
					.with_properties(workspace_properties)
					.with_children(vec![
						snapshot::Snapshot::new()
							.with_id(edit_camera)
							.with_class("Camera")
							.with_name("Camera"),
						snapshot::Snapshot::new()
							.with_id(authored_camera)
							.with_class("Camera")
							.with_name("Camera"),
					])]),
		);

		let contract = encode_managed_hierarchy(&tree).unwrap();
		assert_eq!(contract.excluded_source_ids, [edit_camera]);
		let encoded = contract.payload;
		let header = MANAGED_HIERARCHY_MAGIC.len();
		assert_eq!(u32::from_le_bytes(encoded[header..header + 4].try_into().unwrap()), 3);
		let mut edit_camera_bytes = Vec::new();
		append_ref_bytes(&mut edit_camera_bytes, edit_camera).unwrap();
		let mut authored_camera_bytes = Vec::new();
		append_ref_bytes(&mut authored_camera_bytes, authored_camera).unwrap();
		assert!(!encoded.windows(16).any(|window| window == edit_camera_bytes));
		assert!(encoded.windows(16).any(|window| window == authored_camera_bytes));
	}

	#[test]
	fn managed_hierarchy_omits_only_status_children_of_humanoids() {
		let root = Ref::new();
		let character = Ref::new();
		let humanoid = Ref::new();
		let runtime_status = Ref::new();
		let authored_container = Ref::new();
		let authored_status = Ref::new();
		let tree = Tree::new(
			snapshot::Snapshot::new()
				.with_id(root)
				.with_class("DataModel")
				.with_children(vec![
					snapshot::Snapshot::new()
						.with_id(character)
						.with_class("Model")
						.with_name("Character")
						.with_children(vec![snapshot::Snapshot::new()
							.with_id(humanoid)
							.with_class("Humanoid")
							.with_name("Humanoid")
							.with_children(vec![snapshot::Snapshot::new()
								.with_id(runtime_status)
								.with_class("Status")
								.with_name("Status")])]),
					snapshot::Snapshot::new()
						.with_id(authored_container)
						.with_class("Folder")
						.with_name("Authored")
						.with_children(vec![snapshot::Snapshot::new()
							.with_id(authored_status)
							.with_class("Status")
							.with_name("Status")]),
				]),
		);

		let contract = encode_managed_hierarchy(&tree).unwrap();
		assert_eq!(contract.excluded_source_ids, [runtime_status]);
		let encoded = contract.payload;
		let header = MANAGED_HIERARCHY_MAGIC.len();
		assert_eq!(u32::from_le_bytes(encoded[header..header + 4].try_into().unwrap()), 5);
		let mut runtime_status_bytes = Vec::new();
		append_ref_bytes(&mut runtime_status_bytes, runtime_status).unwrap();
		let mut authored_status_bytes = Vec::new();
		append_ref_bytes(&mut authored_status_bytes, authored_status).unwrap();
		assert!(!encoded.windows(16).any(|window| window == runtime_status_bytes));
		assert!(encoded.windows(16).any(|window| window == authored_status_bytes));
	}

	#[test]
	fn managed_hierarchy_omits_only_the_direct_internal_configure_server_service() {
		let root = Ref::new();
		let internal_service = Ref::new();
		let folder = Ref::new();
		let nested = Ref::new();
		let tree = Tree::new(
			snapshot::Snapshot::new()
				.with_id(root)
				.with_class("DataModel")
				.with_children(vec![
					snapshot::Snapshot::new()
						.with_id(internal_service)
						.with_class("ConfigureServerService")
						.with_name("ConfigureServerService"),
					snapshot::Snapshot::new()
						.with_id(folder)
						.with_class("Folder")
						.with_name("Authored")
						.with_children(vec![snapshot::Snapshot::new()
							.with_id(nested)
							.with_class("ConfigureServerService")
							.with_name("ConfigureServerService")]),
				]),
		);

		let contract = encode_managed_hierarchy(&tree).unwrap();
		assert_eq!(contract.excluded_source_ids, [internal_service]);
		let encoded = contract.payload;
		let header = MANAGED_HIERARCHY_MAGIC.len();
		assert_eq!(u32::from_le_bytes(encoded[header..header + 4].try_into().unwrap()), 3);
		let mut internal_bytes = Vec::new();
		append_ref_bytes(&mut internal_bytes, internal_service).unwrap();
		let mut nested_bytes = Vec::new();
		append_ref_bytes(&mut nested_bytes, nested).unwrap();
		assert!(!encoded.windows(16).any(|window| window == internal_bytes));
		assert!(encoded.windows(16).any(|window| window == nested_bytes));
	}

	#[test]
	fn managed_hierarchy_omits_only_redundant_accessory_attachment_welds() {
		let root = Ref::new();
		let character = Ref::new();
		let head = Ref::new();
		let redundant_weld = Ref::new();
		let accessory = Ref::new();
		let handle = Ref::new();
		let accessory_weld = Ref::new();
		let accessory_constraint = Ref::new();
		let authored_head = Ref::new();
		let authored_weld = Ref::new();
		let mut redundant_properties = UstrMap::default();
		redundant_properties.insert(Ustr::from("Part1"), Variant::Ref(handle));
		let tree = Tree::new(
			snapshot::Snapshot::new()
				.with_id(root)
				.with_class("DataModel")
				.with_children(vec![snapshot::Snapshot::new()
					.with_id(character)
					.with_class("Model")
					.with_name("Character")
					.with_children(vec![
						snapshot::Snapshot::new()
							.with_id(head)
							.with_class("Part")
							.with_name("Head")
							.with_children(vec![snapshot::Snapshot::new()
								.with_id(redundant_weld)
								.with_class("Weld")
								.with_name("HeadWeld")
								.with_properties(redundant_properties)]),
						snapshot::Snapshot::new()
							.with_id(accessory)
							.with_class("Accessory")
							.with_name("Hat")
							.with_children(vec![snapshot::Snapshot::new()
								.with_id(handle)
								.with_class("Part")
								.with_name("Handle")
								.with_children(vec![
									snapshot::Snapshot::new()
										.with_id(accessory_weld)
										.with_class("Weld")
										.with_name("AccessoryWeld"),
									snapshot::Snapshot::new()
										.with_id(accessory_constraint)
										.with_class("RigidConstraint")
										.with_name("AccessoryRigidConstraint"),
								])]),
						snapshot::Snapshot::new()
							.with_id(authored_head)
							.with_class("Part")
							.with_name("Head")
							.with_children(vec![snapshot::Snapshot::new()
								.with_id(authored_weld)
								.with_class("Weld")
								.with_name("HeadWeld")]),
					])]),
		);

		let contract = encode_managed_hierarchy(&tree).unwrap();
		assert_eq!(
			contract.excluded_source_ids.iter().copied().collect::<HashSet<_>>(),
			HashSet::from([redundant_weld, accessory_weld])
		);
		let encoded = contract.payload;
		let header = MANAGED_HIERARCHY_MAGIC.len();
		assert_eq!(u32::from_le_bytes(encoded[header..header + 4].try_into().unwrap()), 8);
		let mut redundant_bytes = Vec::new();
		append_ref_bytes(&mut redundant_bytes, redundant_weld).unwrap();
		let mut authored_bytes = Vec::new();
		append_ref_bytes(&mut authored_bytes, authored_weld).unwrap();
		let mut accessory_weld_bytes = Vec::new();
		append_ref_bytes(&mut accessory_weld_bytes, accessory_weld).unwrap();
		let mut accessory_constraint_bytes = Vec::new();
		append_ref_bytes(&mut accessory_constraint_bytes, accessory_constraint).unwrap();
		assert!(!encoded.windows(16).any(|window| window == redundant_bytes));
		assert!(!encoded.windows(16).any(|window| window == accessory_weld_bytes));
		assert!(encoded.windows(16).any(|window| window == accessory_constraint_bytes));
		assert!(encoded.windows(16).any(|window| window == authored_bytes));
	}
	#[test]
	fn reload_signal_state_signals_valid_manifest_changes_and_coalesces_duplicates() {
		let mut state = ReloadSignalState::new();
		let doc1 = b"{\"name\":\"Game1\"}";
		let doc2 = b"{\"name\":\"Game2\"}";
		let hash1 = blake3::hash(doc1);
		let hash2 = blake3::hash(doc2);

		assert!(state.should_signal_manifest(hash1));
		assert!(!state.should_signal_manifest(hash1));

		assert!(state.should_signal_manifest(hash2));
		assert!(!state.should_signal_manifest(hash2));
	}

	#[test]
	fn reload_signal_state_ignores_malformed_json_without_signaling() {
		let temp = tempfile::tempdir().unwrap();
		let manifest_path = temp.path().join("game.carbon.json");
		let frozen = b"{\"name\":\"Game\"}";

		let state = ReloadSignalState::new();
		std::fs::write(&manifest_path, b"{\"name\":\"Game\"}").unwrap();
		assert_eq!(
			state.check_manifest(&manifest_path, frozen),
			ManifestReadResult::Unchanged
		);

		std::fs::write(&manifest_path, b"{\"name\":").unwrap();
		assert!(matches!(
			state.check_manifest(&manifest_path, frozen),
			ManifestReadResult::InvalidJson(_)
		));

		std::fs::write(&manifest_path, b"{\"name\":\"Updated\"}").unwrap();
		let res = state.check_manifest(&manifest_path, frozen);
		assert!(matches!(res, ManifestReadResult::ValidChanged { .. }));
	}

	#[test]
	fn reload_signal_state_signals_watch_roots_and_coalesces_duplicates() {
		let mut state = ReloadSignalState::new();
		let roots1 = vec![PathBuf::from("/a"), PathBuf::from("/b")];
		let roots2 = vec![PathBuf::from("/a"), PathBuf::from("/c")];

		assert!(state.should_signal_watch_roots(&roots1));
		assert!(!state.should_signal_watch_roots(&roots1));

		assert!(state.should_signal_watch_roots(&roots2));
		assert!(!state.should_signal_watch_roots(&roots2));
	}
}
