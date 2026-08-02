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
use std::{
	collections::{BTreeMap, HashMap, HashSet},
	path::Path,
	path::PathBuf,
	sync::{
		atomic::{AtomicBool, AtomicU8, Ordering},
		Arc, Condvar, Mutex, MutexGuard, RwLock, Weak,
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
use crate::{artifact_store, lock, project, source::SourceDetails, util};

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
	last_successful_capture_generation: Mutex<Option<String>>,
	studio_change_state: Mutex<StudioChangeState>,
	studio_change_condvar: Condvar,
	ephemeral_paths: Mutex<Vec<PathBuf>>,
	served_place_path: Mutex<Option<PathBuf>>,
	restart_required: Arc<AtomicBool>,
	managed_reload_transition: Mutex<Option<String>>,
	automatic_capture_enabled: AtomicBool,
	shutdown_capture_requested: AtomicBool,
	shutdown_recovery_studio_generation: Mutex<Option<String>>,
	shutdown_coordinator: ShutdownCoordinator,
}

#[derive(Default)]
struct StudioChangeState {
	last_captured_generation: Option<String>,
	pending_probe: Option<PendingStudioChangeProbe>,
}

struct PendingStudioChangeProbe {
	request_id: String,
	acknowledged_generation: Option<String>,
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

	fn reset_error(&self) {
		let mut guard = self.state.lock().unwrap();
		if matches!(*guard, ShutdownCoordinatorState::Done(ShutdownResult::Error(_))) {
			*guard = ShutdownCoordinatorState::Unstarted;
			self.condvar.notify_all();
		}
	}
}

const AUTOMATIC_CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const STUDIO_CHANGE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
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

pub(crate) fn is_transient_manifest_capture_error(error: &anyhow::Error) -> bool {
	let message = format!("{error:#}");
	["Studio changed during Capture Manifest staging; retry the capture"]
		.iter()
		.any(|transient| message.contains(transient))
}

#[cfg(test)]
mod manifest_capture_retry_tests {
	use super::*;

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
	fn shutdown_capture_joins_only_an_active_capture() {
		let operation = |worker_active| {
			Some(ManifestCaptureOperation {
				request_id: "capture".to_owned(),
				client_id: 7,
				source_generation: "served-generation".to_owned(),
				state: "running".to_owned(),
				message: Some("committing".to_owned()),
				phase: Arc::new(AtomicU8::new(CAPTURE_COMMITTING)),
				worker_active,
			})
		};
		let active = operation(true);

		let status = joinable_automatic_manifest_capture_status(&active).unwrap();
		assert_eq!(status.request_id, "capture");
		assert_eq!(status.state, "running");

		assert!(joinable_automatic_manifest_capture_status(&operation(false)).is_none());
	}

	fn successful_capture_with_idle_monitor() -> (Arc<Core>, PathBuf, String) {
		let directory = std::env::temp_dir().join(format!("carbon-shutdown-idle-capture-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&directory).unwrap();
		let project_path = directory.join("game.carbon.json");
		let recovery_path = directory.join("served.rbxl");
		project::initialize(&project_path, "ShutdownIdleCapture".to_owned()).unwrap();
		let materialized = project::materialize(&project_path).unwrap();
		let contract = artifact_store::WorktreeContract {
			endpoint: String::new(),
			project: "ShutdownIdleCapture".to_owned(),
			worktree_id: "shutdown-worktree".to_owned(),
			session_token: "shutdown-session".to_owned(),
			identity_exclusions: materialized.identity_exclusions.clone(),
		};
		let first_source = crate::recovery::RecoverySource::served_place(recovery_path.clone()).unwrap();
		let first_started_at = SystemTime::now();
		artifact_store::compile_worktree(&materialized.manifest_path, &recovery_path, &contract).unwrap();
		let core = Arc::new(
			Core::new_project_with_worktree(
				&project_path,
				&materialized,
				(
					"ShutdownIdleCapture".to_owned(),
					"shutdown-worktree".to_owned(),
					"shutdown-session".to_owned(),
				),
			)
			.unwrap(),
		);
		core.queue()
			.subscribe(
				7,
				"ShutdownIdleCapture",
				Some(queue::StudioRoute {
					studio_session_id: "studio-session".to_owned(),
					instance_id: "anon:shutdown".to_owned(),
				}),
			)
			.unwrap();

		let completed = core
			.begin_manifest_capture_internal(
				ManifestCaptureSource {
					sources: vec![first_source],
					started_at: first_started_at,
				},
				false,
				None,
			)
			.unwrap();
		wait_for_automatic_capture(
			|| Ok(completed),
			|request_id| core.manifest_capture_status(request_id),
			|| thread::sleep(Duration::from_millis(10)),
		)
		.unwrap();
		*core.studio_change_state.lock().unwrap() = StudioChangeState {
			last_captured_generation: Some("studio-clean-1".to_owned()),
			pending_probe: None,
		};

		let idle_source = crate::recovery::RecoverySource::served_place(recovery_path).unwrap();
		let idle = core
			.begin_manifest_capture_internal(
				ManifestCaptureSource {
					sources: vec![idle_source],
					started_at: SystemTime::now(),
				},
				false,
				None,
			)
			.unwrap();
		assert_eq!(idle.state, "running");
		assert_eq!(
			core.manifest_capture
				.lock()
				.unwrap()
				.as_ref()
				.unwrap()
				.phase
				.load(Ordering::Acquire),
			CAPTURE_COLLECTING
		);
		(core, directory, idle.request_id)
	}

	fn fail_capture_after(core: Arc<Core>, request_id: String) -> thread::JoinHandle<()> {
		thread::spawn(move || {
			thread::sleep(Duration::from_millis(150));
			core.fail_manifest_capture(
				&request_id,
				"timed out after 360 seconds waiting for Roblox Studio".to_owned(),
			);
		})
	}

	fn acknowledge_studio_probe(core: Arc<Core>, generation: &'static str) -> thread::JoinHandle<()> {
		thread::spawn(move || loop {
			let Some(message) = core.queue().get_timeout(7).unwrap() else {
				continue;
			};
			if let crate::server::Message::StudioChangeProbe(probe) = message {
				core.acknowledge_studio_change_generation(7, &probe.request_id, generation.to_owned())
					.unwrap();
				return;
			}
		})
	}

	#[test]
	fn shutdown_reuses_the_last_successful_capture_when_the_monitor_is_idle() {
		let (core, directory, idle_request) = successful_capture_with_idle_monitor();
		let acknowledgement = acknowledge_studio_probe(Arc::clone(&core), "studio-clean-1");
		let timeout_core = Arc::clone(&core);
		let timeout = fail_capture_after(timeout_core, idle_request);
		let started = Instant::now();
		let message = core.capture_before_shutdown().unwrap();
		assert!(started.elapsed() < Duration::from_secs(1));
		assert!(message.contains("retained valid manifest"), "{message}");
		acknowledgement.join().unwrap();
		timeout.join().unwrap();
		core.stop_automatic_capture_monitor();
		drop(core);
		std::fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn shutdown_does_not_reuse_a_capture_that_predates_a_live_studio_edit() {
		let (core, directory, idle_request) = successful_capture_with_idle_monitor();
		let acknowledgement = acknowledge_studio_probe(Arc::clone(&core), "studio-edit-2");
		let timeout = fail_capture_after(Arc::clone(&core), idle_request);

		let error = core.capture_before_shutdown().unwrap_err();
		assert!(
			format!("{error:#}").contains("timed out after 360 seconds waiting for Roblox Studio"),
			"{error:#}"
		);
		acknowledgement.join().unwrap();
		timeout.join().unwrap();
		core.stop_automatic_capture_monitor();
		drop(core);
		std::fs::remove_dir_all(directory).unwrap();
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

	#[test]
	fn preexisting_recovery_requires_the_current_studio_generation() {
		assert!(recovery_matches_studio_generation(
			crate::recovery::RecoveryFreshness::Preexisting,
			Some("studio-generation-1"),
			Some("studio-generation-1"),
		));
		assert!(!recovery_matches_studio_generation(
			crate::recovery::RecoveryFreshness::Preexisting,
			Some("studio-generation-1"),
			Some("studio-generation-2"),
		));
		assert!(!recovery_matches_studio_generation(
			crate::recovery::RecoveryFreshness::Preexisting,
			Some("studio-generation-1"),
			None,
		));
		assert!(recovery_matches_studio_generation(
			crate::recovery::RecoveryFreshness::New,
			None,
			None,
		));
	}

	#[test]
	fn automatic_capture_canonical_includes_studio_owned_descendants() {
		use rbx_dom_weak::types::{CFrame, Matrix3, Vector3};

		fn find<'a>(snapshot: &'a snapshot::Snapshot, name: &str) -> Option<&'a snapshot::Snapshot> {
			if snapshot.name == name {
				return Some(snapshot);
			}
			snapshot.children.iter().find_map(|child| find(child, name))
		}

		let directory = std::env::temp_dir().join(format!("carbon-capture-canonical-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&directory).unwrap();
		let manifest_path = directory.join("state.carbon");
		let root = Ref::new();
		let workspace = Ref::new();
		let mapped_parent = Ref::new();
		let mapped = Ref::new();
		let transform = CFrame::new(Vector3::new(1.0, 2.0, 3.0), Matrix3::identity());
		let canonical = snapshot::Snapshot::new()
			.with_id(root)
			.with_name("CaptureCanonical")
			.with_class("DataModel")
			.with_children(vec![
				snapshot::Snapshot::new()
					.with_id(workspace)
					.with_name("Workspace")
					.with_class("Workspace")
					.with_children(vec![snapshot::Snapshot::new()
						.with_id(Ref::new())
						.with_name("Live constraint")
						.with_class("AnimationConstraint")
						.with_properties(UstrMap::from_iter([(
							Ustr::from("Transform"),
							Variant::CFrame(transform),
						)]))]),
				snapshot::Snapshot::new()
					.with_id(mapped_parent)
					.with_name("ServerStorage")
					.with_class("ServerStorage")
					.with_children(vec![snapshot::Snapshot::new()
						.with_id(mapped)
						.with_name("Mapped")
						.with_class("Folder")]),
			]);
		artifact_store::extract_snapshot(canonical, "CaptureCanonical".to_owned(), &manifest_path).unwrap();

		let projected = artifact_store::load_projected_live(
			&manifest_path,
			&HashSet::from([mapped]),
			&HashSet::from([root, mapped_parent]),
		)
		.unwrap()
		.tree
		.into_snapshot()
		.unwrap();
		assert!(find(&projected, "Live constraint").is_none());

		let loaded = load_manifest_capture_canonical(&manifest_path).unwrap();
		assert_eq!(
			find(&loaded, "Live constraint")
				.unwrap()
				.properties
				.get(&Ustr::from("Transform")),
			Some(&Variant::CFrame(transform))
		);
		std::fs::remove_dir_all(directory).unwrap();
	}
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

fn joinable_automatic_manifest_capture_status(
	operation: &Option<ManifestCaptureOperation>,
) -> Option<ManifestCaptureStatus> {
	operation
		.as_ref()
		.filter(|capture| capture.worker_active && capture.state == "running")
		.map(manifest_capture_status)
}

struct ManifestCaptureSource {
	sources: Vec<crate::recovery::RecoverySource>,
	started_at: SystemTime,
}

struct ManifestCaptureLaunch {
	client_id: u32,
	source: ManifestCaptureSource,
	managed_reload_transition_id: Option<String>,
}

fn load_manifest_capture_canonical(manifest_path: &Path) -> Result<snapshot::Snapshot> {
	artifact_store::load_tree(manifest_path)?.tree.into_snapshot()
}

fn recovery_matches_studio_generation(
	freshness: crate::recovery::RecoveryFreshness,
	recovery_generation: Option<&str>,
	studio_generation: Option<&str>,
) -> bool {
	match freshness {
		crate::recovery::RecoveryFreshness::New => true,
		crate::recovery::RecoveryFreshness::Preexisting => {
			studio_generation.is_some() && recovery_generation == studio_generation
		}
	}
}

pub(crate) const CAPTURE_COLLECTING: u8 = 0;
pub(crate) const CAPTURE_RECOVERED: u8 = 1;
const CAPTURE_CANCELLED: u8 = 2;
const CAPTURE_COMMITTING: u8 = 3;
pub(crate) const CAPTURE_COMMITTED: u8 = 4;

fn claim_capture_recovery(phase: &AtomicU8) -> Result<()> {
	phase
		.compare_exchange(
			CAPTURE_COLLECTING,
			CAPTURE_RECOVERED,
			Ordering::AcqRel,
			Ordering::Acquire,
		)
		.map(|_| ())
		.map_err(|state| {
			anyhow::anyhow!(if state == CAPTURE_CANCELLED {
				"Capture Manifest was cancelled before the accepted recovery could be staged"
			} else {
				"Capture Manifest already accepted recovery evidence"
			})
		})
}

pub(crate) fn claim_capture_commit(phase: &AtomicU8) -> Result<()> {
	phase
		.compare_exchange(
			CAPTURE_RECOVERED,
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
	loop {
		let state = phase.load(Ordering::Acquire);
		if !matches!(state, CAPTURE_COLLECTING | CAPTURE_RECOVERED) {
			return Err(anyhow::anyhow!(if state == CAPTURE_COMMITTING {
				"Capture Manifest has begun its atomic commit and can no longer be cancelled"
			} else {
				"Capture Manifest request is not cancellable"
			}));
		}
		match phase.compare_exchange(state, CAPTURE_CANCELLED, Ordering::AcqRel, Ordering::Acquire) {
			Ok(_) => return Ok(()),
			Err(_) => continue,
		}
	}
}

fn claim_idle_capture_cancel(phase: &AtomicU8) -> Result<()> {
	phase
		.compare_exchange(
			CAPTURE_COLLECTING,
			CAPTURE_CANCELLED,
			Ordering::AcqRel,
			Ordering::Acquire,
		)
		.map(|_| ())
		.map_err(|_| anyhow::anyhow!("Capture Manifest received recovery evidence before idle shutdown cancellation"))
}

impl Core {
	pub fn new_artifact(manifest_path: &Path) -> Result<Self> {
		Self::new_artifact_with_contract(manifest_path, None, None, None, None)
	}

	pub fn new_artifact_with_worktree(
		manifest_path: &Path,
		worktree: Option<(String, String, String)>,
	) -> Result<Self> {
		Self::new_artifact_with_contract(manifest_path, worktree, None, None, None)
	}

	pub fn new_artifact_with_live_session(manifest_path: &Path, session_token: String) -> Result<Self> {
		Self::new_artifact_with_contract(manifest_path, None, Some(session_token), None, None)
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
		Self::new_artifact_with_contract(
			&materialized.manifest_path,
			Some(worktree),
			None,
			Some(project::live_policy(project_path, materialized)),
			control_tx.into(),
		)
	}

	fn new_artifact_with_contract(
		manifest_path: &Path,
		worktree: Option<(String, String, String)>,
		live_session_token: Option<String>,
		live_policy: Option<project::LivePolicy>,
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
			last_successful_capture_generation: Mutex::new(None),
			studio_change_state: Mutex::new(StudioChangeState::default()),
			studio_change_condvar: Condvar::new(),
			ephemeral_paths: Mutex::new(Vec::new()),
			served_place_path: Mutex::new(None),
			restart_required,
			managed_reload_transition: Mutex::new(None),
			automatic_capture_enabled: AtomicBool::new(false),
			shutdown_capture_requested: AtomicBool::new(false),
			shutdown_recovery_studio_generation: Mutex::new(None),
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

	pub fn register_served_place(&self, path: PathBuf) {
		*self.served_place_path.lock().unwrap() = Some(path);
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
		self.begin_manifest_capture_mode_internal(true, None)
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

	fn validate_managed_reload_capture(&self, supplied: Option<&str>) -> Result<Option<String>> {
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
		self.shutdown_capture_requested.store(true, Ordering::Release);
		let monitor_was_enabled = self.automatic_capture_enabled.swap(false, Ordering::AcqRel);
		let result = self.shutdown_coordinator.execute_or_await(|| {
			let studio_generation = if self.queue.has_subscribers() {
				self.probe_studio_change_generation(self.queue.single_listener_id()?)?
			} else {
				None
			};
			if let Some(message) =
				self.retain_last_successful_capture_for_idle_shutdown(studio_generation.as_deref())?
			{
				return Ok(message);
			}
			*self.shutdown_recovery_studio_generation.lock().unwrap() = studio_generation;
			self.do_automatic_capture()
		});
		*self.shutdown_recovery_studio_generation.lock().unwrap() = None;
		if result.is_err() {
			// A failed stop must leave the live Studio session recoverable and permit
			// a later stop to retry once Studio produces recovery evidence.
			self.shutdown_coordinator.reset_error();
			self.shutdown_capture_requested.store(false, Ordering::Release);
			if monitor_was_enabled {
				self.automatic_capture_enabled.store(true, Ordering::Release);
			}
		}
		result
	}

	pub fn capture_before_reload(self: &Arc<Self>) -> Result<String> {
		self.do_automatic_capture()
	}

	fn do_automatic_capture(self: &Arc<Self>) -> Result<String> {
		let capture_result = wait_for_automatic_capture(
			|| {
				let status = self.begin_manifest_capture_mode_internal(true, None)?;
				crate::carbon_info!(
					"Automatic capture request {} is waiting for Studio auto-recovery or a manual save to the temporary served place for served generation {}",
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
		]
		.iter()
		.any(|keyword| message.contains(keyword))
	}

	fn has_valid_manifest_fallback(&self) -> bool {
		artifact_store::validated_artifact_receipt(&self.manifest_path).is_ok()
	}

	fn probe_studio_change_generation(&self, client_id: u32) -> Result<Option<String>> {
		let request_id = uuid::Uuid::new_v4().simple().to_string();
		{
			let mut state = self.studio_change_state.lock().unwrap();
			ensure!(
				state.pending_probe.is_none(),
				"another Studio change probe is already pending"
			);
			state.pending_probe = Some(PendingStudioChangeProbe {
				request_id: request_id.clone(),
				acknowledged_generation: None,
			});
		}
		if let Err(error) = self.queue.push(
			crate::server::Message::StudioChangeProbe(crate::server::StudioChangeProbe {
				request_id: request_id.clone(),
			}),
			Some(client_id),
		) {
			self.studio_change_state.lock().unwrap().pending_probe = None;
			return Err(error);
		}

		let deadline = Instant::now() + STUDIO_CHANGE_PROBE_TIMEOUT;
		let mut state = self.studio_change_state.lock().unwrap();
		loop {
			let pending = state
				.pending_probe
				.as_ref()
				.filter(|pending| pending.request_id == request_id)
				.context("Studio change probe disappeared before acknowledgement")?;
			if let Some(generation) = pending.acknowledged_generation.clone() {
				state.pending_probe = None;
				return Ok(Some(generation));
			}
			let now = Instant::now();
			if now >= deadline {
				state.pending_probe = None;
				return Ok(None);
			}
			let (next, _) = self.studio_change_condvar.wait_timeout(state, deadline - now).unwrap();
			state = next;
		}
	}

	pub(crate) fn acknowledge_studio_change_generation(
		&self,
		client_id: u32,
		request_id: &str,
		generation: String,
	) -> Result<()> {
		ensure!(
			self.queue.is_subscribed(client_id),
			"Studio change acknowledgement is not subscribed"
		);
		ensure!(
			!generation.is_empty(),
			"Studio change acknowledgement generation is empty"
		);
		let mut state = self.studio_change_state.lock().unwrap();
		let pending = state
			.pending_probe
			.as_mut()
			.filter(|pending| pending.request_id == request_id)
			.context("Studio change acknowledgement does not match the pending probe")?;
		ensure!(
			pending.acknowledged_generation.is_none(),
			"Studio change probe was already acknowledged"
		);
		pending.acknowledged_generation = Some(generation);
		self.studio_change_condvar.notify_all();
		Ok(())
	}

	fn retain_last_successful_capture_for_idle_shutdown(
		&self,
		studio_generation: Option<&str>,
	) -> Result<Option<String>> {
		let Some(successful_generation) = self.last_successful_capture_generation.lock().unwrap().clone() else {
			return Ok(None);
		};
		if self.source_generation() != successful_generation || !self.has_valid_manifest_fallback() {
			return Ok(None);
		}
		{
			let capture = self.manifest_capture.lock().unwrap();
			match capture.as_ref() {
				Some(operation)
					if operation.source_generation == successful_generation
						&& (operation.state == "complete"
							|| (operation.state == "running" && operation.worker_active)) => {}
				None => {}
				Some(_) => return Ok(None),
			}
		}
		let Some(studio_generation) = studio_generation else {
			return Ok(None);
		};
		if self
			.studio_change_state
			.lock()
			.unwrap()
			.last_captured_generation
			.as_deref()
			!= Some(studio_generation)
		{
			return Ok(None);
		}

		let idle_request = {
			let mut capture = self.manifest_capture.lock().unwrap();
			match capture.as_mut() {
				Some(operation)
					if operation.worker_active
						&& operation.state == "running"
						&& operation.source_generation == successful_generation =>
				{
					if claim_idle_capture_cancel(&operation.phase).is_err() {
						return Ok(None);
					}
					operation.state = "failed".to_owned();
					operation.message =
						Some("Idle automatic recovery wait stopped after the last successful capture".to_owned());
					Some(operation.request_id.clone())
				}
				Some(operation)
					if operation.state == "complete" && operation.source_generation == successful_generation =>
				{
					None
				}
				None => None,
				Some(_) => return Ok(None),
			}
		};

		if let Some(request_id) = idle_request {
			loop {
				let worker_active = self
					.manifest_capture
					.lock()
					.unwrap()
					.as_ref()
					.is_some_and(|operation| operation.request_id == request_id && operation.worker_active);
				if !worker_active {
					break;
				}
				thread::sleep(Duration::from_millis(25));
			}
		}

		let message = format!(
			"No newer Studio recovery followed the last successful capture; retained valid manifest {}",
			self.manifest_path.display()
		);
		crate::carbon_info!("{message}");
		Ok(Some(message))
	}

	pub fn start_automatic_capture_monitor(self: &Arc<Self>) -> Result<bool> {
		ensure!(
			self.live_policy.is_some(),
			"automatic Studio auto-recovery capture requires an active served project"
		);
		let client_id = self.queue.single_listener_id()?;
		self.queue
			.studio_route(client_id)
			.context("automatic Studio auto-recovery capture requires an exact connected Studio route")?;
		if self
			.automatic_capture_enabled
			.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
			.is_err()
		{
			return Ok(false);
		}
		self.shutdown_capture_requested.store(false, Ordering::Release);
		let weak = Arc::downgrade(self);
		if let Err(error) = Builder::new()
			.name("carbon-auto-recovery-monitor".to_owned())
			.spawn(move || Self::run_automatic_capture_monitor(weak))
		{
			self.automatic_capture_enabled.store(false, Ordering::Release);
			return Err(error).context("failed to start Studio auto-recovery monitor");
		}
		crate::carbon_info!("Automatic Studio auto-recovery capture is active");
		Ok(true)
	}

	fn run_automatic_capture_monitor(weak: Weak<Self>) {
		loop {
			let Some(core) = weak.upgrade() else {
				break;
			};
			if !core.automatic_capture_enabled.load(Ordering::Acquire)
				|| core.shutdown_capture_requested.load(Ordering::Acquire)
				|| !core.queue.has_subscribers()
			{
				break;
			}
			match core.do_automatic_capture() {
				Ok(message) => crate::carbon_info!("Automatic Studio auto-recovery capture completed: {message}"),
				Err(error) if core.is_studio_disconnected_error(&error) => break,
				Err(error) => crate::carbon_error!("Automatic Studio auto-recovery capture failed: {error:#}"),
			}
			drop(core);
			thread::sleep(AUTOMATIC_CAPTURE_POLL_INTERVAL);
		}
		if let Some(core) = weak.upgrade() {
			core.automatic_capture_enabled.store(false, Ordering::Release);
		}
	}

	pub(crate) fn stop_automatic_capture_monitor(&self) {
		self.automatic_capture_enabled.store(false, Ordering::Release);
		loop {
			let worker_active = {
				let mut capture = self.manifest_capture.lock().unwrap();
				let Some(operation) = capture.as_mut().filter(|operation| operation.worker_active) else {
					return;
				};
				if operation.state == "running" && claim_capture_cancel(&operation.phase).is_ok() {
					operation.state = "failed".to_owned();
					operation.message = Some("Automatic auto-recovery monitoring stopped".to_owned());
				}
				operation.worker_active
			};
			if !worker_active {
				return;
			}
			thread::sleep(Duration::from_millis(25));
		}
	}

	pub(crate) fn begin_manifest_capture_mode_transition(
		self: &Arc<Self>,
		managed_reload_transition_id: Option<String>,
	) -> Result<ManifestCaptureStatus> {
		let join_active = managed_reload_transition_id.is_none();
		self.begin_manifest_capture_mode_internal(join_active, managed_reload_transition_id)
	}

	fn begin_manifest_capture_mode_internal(
		self: &Arc<Self>,
		join_active: bool,
		managed_reload_transition_id: Option<String>,
	) -> Result<ManifestCaptureStatus> {
		if join_active {
			let operation = self.manifest_capture.lock().unwrap();
			if let Some(status) = joinable_automatic_manifest_capture_status(&operation) {
				return Ok(status);
			}
		}
		let autosaves = crate::recovery::autosaves_dir()?;
		let mut sources = vec![crate::recovery::RecoverySource::studio_auto_recovery(autosaves)?];
		if let Some(path) = self.served_place_path.lock().unwrap().clone() {
			sources.push(crate::recovery::RecoverySource::served_place(path)?);
		}
		let started_at = SystemTime::now();
		self.begin_manifest_capture_internal(
			ManifestCaptureSource { sources, started_at },
			join_active,
			managed_reload_transition_id,
		)
	}

	fn begin_manifest_capture_internal(
		self: &Arc<Self>,
		source: ManifestCaptureSource,
		join_active: bool,
		managed_reload_transition_id: Option<String>,
	) -> Result<ManifestCaptureStatus> {
		ensure!(
			self.live_policy.is_some(),
			"Capture Manifest requires an active served project"
		);
		ensure!(
			!self.restart_required.load(Ordering::Acquire),
			"serve state requires a hard restart before Capture Manifest"
		);
		let managed_reload_transition_id =
			self.validate_managed_reload_capture(managed_reload_transition_id.as_deref())?;
		let client_id = self.queue.single_listener_id()?;
		self.queue
			.studio_route(client_id)
			.context("Capture Manifest requires an exact connected Studio route")?;
		let source_generation = self.source_generation();
		let request_id = uuid::Uuid::new_v4().simple().to_string();
		let phase = Arc::new(AtomicU8::new(CAPTURE_COLLECTING));
		{
			let mut operation = self.manifest_capture.lock().unwrap();
			if let Some(active) = operation
				.as_mut()
				.filter(|active| active.worker_active && active.state == "running")
			{
				if join_active {
					return Ok(manifest_capture_status(active));
				}
				anyhow::bail!("another Capture Manifest operation is already running");
			}
			let message = if let Some(path) = self.served_place_path.lock().unwrap().as_ref() {
				format!(
					"Waiting up to six minutes for Studio auto-recovery or a manual save to {}",
					path.display()
				)
			} else {
				"Waiting up to six minutes for Studio to write a new auto-recovery place".to_owned()
			};
			*operation = Some(ManifestCaptureOperation {
				request_id: request_id.clone(),
				client_id,
				source_generation: source_generation.clone(),
				state: "running".to_owned(),
				message: Some(message),
				phase: phase.clone(),
				worker_active: true,
			});
		}
		let core = Arc::clone(self);
		let worker_request_id = request_id.clone();
		if let Err(error) = Builder::new()
			.name(format!("carbon-capture-{request_id}"))
			.spawn(move || {
				let launch = ManifestCaptureLaunch {
					client_id,
					source,
					managed_reload_transition_id,
				};
				if let Err(error) = core.run_manifest_capture(&worker_request_id, launch, phase) {
					core.fail_manifest_capture(&worker_request_id, format!("{error:#}"));
				}
				core.settle_manifest_capture_worker(&worker_request_id);
			}) {
			self.fail_manifest_capture(&request_id, format!("failed to start Capture Manifest worker: {error}"));
			self.settle_manifest_capture_worker(&request_id);
			return Err(error.into());
		}
		self.manifest_capture_status(&request_id)
	}

	fn run_manifest_capture(
		&self,
		request_id: &str,
		launch: ManifestCaptureLaunch,
		phase: Arc<AtomicU8>,
	) -> Result<()> {
		let ManifestCaptureLaunch {
			client_id,
			source,
			managed_reload_transition_id,
		} = launch;
		let source_generation = self.source_generation();
		let (worktree_id, session_token) = self
			.worktree
			.as_ref()
			.context("Capture Manifest requires a managed Studio worktree")?;
		let policy = self
			.live_policy
			.as_ref()
			.context("Capture Manifest requires a served project")?
			.read()
			.unwrap()
			.clone();
		let previous_projected = self
			.project_snapshot
			.as_ref()
			.context("hybrid project snapshot is unavailable")?
			.read()
			.unwrap()
			.clone();
		// The live project snapshot intentionally contains only mapped source and
		// routing ancestors. Recovery reconciliation also needs the Studio-owned
		// complement (for example authored constraint transforms and explicit
		// default-valued properties), so read the complete canonical artifact.
		let canonical = load_manifest_capture_canonical(&self.manifest_path)?;
		let expected = artifact_store::WorktreeContract {
			endpoint: String::new(),
			project: self.name.clone(),
			worktree_id: worktree_id.clone(),
			session_token: session_token.clone(),
			identity_exclusions: HashSet::new(),
		};
		let ManifestCaptureSource { sources, started_at } = source;
		let (capture_kind, capture_path, recovered) = crate::recovery::wait_for_recovery(
			&sources,
			started_at,
			crate::recovery::CAPTURE_TIMEOUT,
			|| phase.load(Ordering::Acquire) == CAPTURE_CANCELLED || !self.queue.is_subscribed(client_id),
			|path, freshness| {
				let studio_generation = self.shutdown_recovery_studio_generation.lock().unwrap().clone();
				if freshness == crate::recovery::RecoveryFreshness::Preexisting && studio_generation.is_none() {
					return Ok(crate::recovery::RecoveryAcceptance::Retry);
				}
				match project::decode_captured_place(path, &expected, &canonical, &policy.mapped_roots) {
					Ok(recovered)
						if recovery_matches_studio_generation(
							freshness,
							recovered.studio_change_generation.as_deref(),
							studio_generation.as_deref(),
						) =>
					{
						claim_capture_recovery(&phase)?;
						Ok(crate::recovery::RecoveryAcceptance::Accept(recovered))
					}
					Ok(_) => Ok(crate::recovery::RecoveryAcceptance::Reject),
					Err(error) if format!("{error:#}").contains("different Carbon Studio session") => {
						Ok(crate::recovery::RecoveryAcceptance::Reject)
					}
					Err(error) => Err(error),
				}
			},
		)?;
		let recovered_tree = recovered.tree;
		let captured_studio_change_generation = recovered.studio_change_generation;
		let capture_kind = capture_kind.label();
		self.update_manifest_capture_message(
			request_id,
			&format!("{capture_kind} arrived; validating {}", capture_path.display()),
		)?;

		self.update_manifest_capture_message(request_id, "Waiting for project source to settle")?;
		let project_sync_started = Instant::now();
		let project_realization_generation = wait_for_project_synchronization(
			|| {
				let _project_state = self.project_state_lock.as_ref().map(|lock| lock.lock().unwrap());
				self.exact_project_realization_generation()
			},
			thread::sleep,
			|| project_sync_started.elapsed() >= PROJECT_SYNC_WAIT_TIMEOUT,
		)?;
		let cancelled = || phase.load(Ordering::Acquire) == CAPTURE_CANCELLED;
		ensure!(
			self.source_generation() == source_generation,
			"served source changed during Capture Manifest"
		);
		let metadata = artifact_store::validated_artifact_receipt(&self.manifest_path)?
			.metadata()
			.clone();
		self.update_manifest_capture_message(request_id, "Staging the recovered Studio place")?;
		let staged_composite = artifact_store::stage_compiled_capture(
			&recovered_tree,
			recovered_tree.root_ref(),
			self.name.clone(),
			metadata,
			&policy.mapped_refs,
			&self.manifest_path,
			&cancelled,
		)?;
		let projected_tree = artifact_store::load_projected_live(
			staged_composite.artifact(),
			&policy.mapped_refs,
			&policy.routing_refs,
		)?
		.tree;
		let staged_studio = project::stage_captured_studio_domain(&policy, &staged_composite, &cancelled)?;
		let promotion = project::prepare_capture_promotion(staged_composite, staged_studio)?;
		ensure!(
			self.exact_project_realization_generation()? == project_realization_generation,
			"filesystem mapping realization changed during Capture Manifest; retry after project source settles"
		);
		self.update_manifest_capture_message(request_id, "Committing the recovered manifest atomically")?;
		let generation = self.writer.commit_prepared_capture(
			projected_tree,
			promotion,
			phase,
			source_generation,
			CapturePrecommitAttestation {
				project_path: policy.project_path.clone(),
				project_document: policy.project_document.clone(),
				previous_projected,
				mapped_refs: policy.mapped_refs.clone(),
				project_generation: project_realization_generation,
			},
		)?;
		let contract = self.encode_current_managed_hierarchy()?;
		let projected_source_ids = contract.source_ids.clone();
		install_managed_hierarchy_contract(&self.managed_hierarchy, contract);
		self.source_reader
			.install_projected_state(projected_source_ids, generation.clone())?;
		*self.last_successful_capture_generation.lock().unwrap() = Some(generation.clone());
		self.studio_change_state.lock().unwrap().last_captured_generation = captured_studio_change_generation;
		{
			let mut operation = self.manifest_capture.lock().unwrap();
			let operation = operation
				.as_mut()
				.filter(|operation| operation.request_id == request_id)
				.context("Capture Manifest request disappeared after recovery commit")?;
			operation.source_generation = generation;
			operation.state = "complete".to_owned();
			operation.message = Some(format!(
				"{capture_kind} {} was captured and committed atomically",
				capture_path.display()
			));
		}
		if let Some(transition_id) = managed_reload_transition_id {
			self.complete_managed_reload_transition(&transition_id)?;
		}
		if let Err(error) = self
			.queue
			.push(crate::server::Message::SyncDetails(self.details()), Some(client_id))
		{
			log::warn!("captured manifest committed, but Studio did not receive refreshed source details: {error:#}");
		}
		Ok(())
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
#[allow(dead_code)]
pub(crate) struct ManagedHierarchyContract {
	pub contract_id: String,
	pub payload: Bytes,
	pub source_instances: u32,
	pub excluded_source_ids: Vec<Ref>,
	pub source_ids: HashSet<Ref>,
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
	fn idle_shutdown_cancellation_cannot_discard_accepted_recovery() {
		let phase = AtomicU8::new(CAPTURE_COLLECTING);
		claim_capture_recovery(&phase).unwrap();
		assert_eq!(phase.load(Ordering::Acquire), CAPTURE_RECOVERED);
		assert!(claim_idle_capture_cancel(&phase)
			.unwrap_err()
			.to_string()
			.contains("received recovery evidence"));
		assert_eq!(phase.load(Ordering::Acquire), CAPTURE_RECOVERED);
	}

	#[test]
	fn capture_cancellation_is_rejected_after_atomic_commit_begins() {
		let phase = AtomicU8::new(CAPTURE_COLLECTING);
		claim_capture_recovery(&phase).unwrap();
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
