//! On-demand full-place saves through Roblox Studio's local test server.
//!
//! `StudioTestService:ExecuteMultiplayerTestAsync` runs Studio's Start Server
//! action. Before that action launches the test server, Studio serializes the
//! complete edit DataModel with its native place saver to `server.rbxl`. The
//! Carbon plugin starts that test on request, Carbon stops the test-server
//! processes it caused, and a private copy of the save is staged for the
//! ordinary recovery decoder. Studio auto-recovery remains the fallback.

use std::{
	fs,
	path::{Path, PathBuf},
	sync::{Arc, Condvar, Mutex},
	thread::{self, Builder},
	time::{Duration, Instant, SystemTime},
};

use anyhow::{bail, ensure, Context, Result};

use crate::recovery::RecoveryFingerprint;

const QUICK_SAVE_TIMEOUT: Duration = Duration::from_secs(60);
const LOCK_TIMEOUT: Duration = Duration::from_secs(90);
/// An earlier quick save may first wait for the cross-process lock, then for
/// Studio, then stop its test server and stage its copy.
const IN_FLIGHT_TIMEOUT: Duration = Duration::from_secs(LOCK_TIMEOUT.as_secs() + QUICK_SAVE_TIMEOUT.as_secs() + 15);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const STABLE_POLLS: usize = 2;
/// The test server starts right after Studio finishes the save. Only processes
/// created after this margin before the save are attributed to the quick save.
const TEST_SERVER_START_MARGIN: Duration = Duration::from_secs(10);
const STAGED_PREFIX: &str = "quick-save-";
const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

/// Stops the test-server and test-client processes Studio started at or after
/// the given time, returning how many processes were stopped.
pub(crate) type TestSessionReaper = Arc<dyn Fn(SystemTime) -> Result<usize> + Send + Sync>;

#[derive(Clone)]
pub(crate) struct StudioQuickSaveHost {
	/// The file Studio writes before it launches a local test server.
	pub(crate) quick_save_file: PathBuf,
	/// Serializes quick saves across Carbon processes on this host.
	pub(crate) lock_file: PathBuf,
	pub(crate) reaper: TestSessionReaper,
}

/// One armed quick save. It holds the cross-process lock until it is dropped,
/// because every Studio on the host writes the same `server.rbxl`.
pub(crate) struct QuickSaveTicket {
	pub(crate) token: String,
	baseline: Option<RecoveryFingerprint>,
	_lock: fs::File,
}

struct InFlight {
	token: String,
	client_id: u32,
	failure: Option<String>,
}

pub(crate) struct QuickSaveCoordinator {
	host: StudioQuickSaveHost,
	staging_dir: PathBuf,
	in_flight: Mutex<Option<InFlight>>,
}

impl QuickSaveCoordinator {
	pub(crate) fn new(host: StudioQuickSaveHost) -> Result<Arc<Self>> {
		let staging_dir = std::env::temp_dir().join(format!("carbon-quick-save-{}", uuid::Uuid::new_v4().simple()));
		fs::create_dir_all(&staging_dir).with_context(|| {
			format!(
				"failed to create the Studio quick-save staging directory {}",
				staging_dir.display()
			)
		})?;
		Ok(Arc::new(Self {
			host,
			staging_dir,
			in_flight: Mutex::new(None),
		}))
	}

	/// Carbon's private directory of staged quick-save copies.
	pub(crate) fn staging_dir(&self) -> &Path {
		&self.staging_dir
	}

	/// Reserve the next quick save for `client_id`. Returns `None` while another
	/// quick save from this session is still in flight.
	pub(crate) fn arm(&self, client_id: u32) -> Result<Option<QuickSaveTicket>> {
		let token = uuid::Uuid::new_v4().simple().to_string();
		{
			let mut in_flight = self.in_flight.lock().unwrap();
			if in_flight.is_some() {
				return Ok(None);
			}
			*in_flight = Some(InFlight {
				token: token.clone(),
				client_id,
				failure: None,
			});
		}
		let armed = acquire_lock(&self.host.lock_file, LOCK_TIMEOUT).and_then(|lock| {
			Ok(QuickSaveTicket {
				token: token.clone(),
				baseline: fingerprint(&self.host.quick_save_file)?,
				_lock: lock,
			})
		});
		if armed.is_err() {
			self.clear(&token);
		}
		armed.map(Some)
	}

	/// Reserve a quick save, first waiting for one already in flight to finish:
	/// a save requested earlier shows Studio as it was then, not now.
	pub(crate) fn arm_after_in_flight(&self, client_id: u32) -> Result<QuickSaveTicket> {
		let deadline = Instant::now() + IN_FLIGHT_TIMEOUT;
		loop {
			if let Some(ticket) = self.arm(client_id)? {
				return Ok(ticket);
			}
			ensure!(
				Instant::now() < deadline,
				"an earlier Studio quick save did not finish within {} seconds",
				IN_FLIGHT_TIMEOUT.as_secs()
			);
			thread::sleep(POLL_INTERVAL);
		}
	}

	/// Record that the plugin could not start the quick-save test.
	pub(crate) fn report_failure(&self, client_id: u32, token: &str, message: String) -> Result<()> {
		let mut in_flight = self.in_flight.lock().unwrap();
		let pending = in_flight
			.as_mut()
			.filter(|pending| pending.token == token)
			.context("Studio quick-save report does not match the pending request")?;
		ensure!(
			pending.client_id == client_id,
			"Studio quick-save report came from a different Studio client"
		);
		pending.failure.get_or_insert(message);
		Ok(())
	}

	/// Finish `ticket` on a worker thread. A failure leaves capture waiting for
	/// Studio auto-recovery, so it is reported as a warning rather than an error.
	pub(crate) fn complete_in_background(self: &Arc<Self>, ticket: QuickSaveTicket) {
		let coordinator = Arc::clone(self);
		let token = ticket.token.clone();
		let spawned = Builder::new()
			.name(format!("carbon-quick-save-{token}"))
			.spawn(move || match coordinator.finish(ticket) {
				Ok(staged) => log::debug!("Staged Studio quick save at {}", staged.display()),
				Err(error) => crate::carbon_warn!(
					"Studio quick save did not complete; waiting for Studio auto-recovery instead: {error:#}"
				),
			});
		if let Err(error) = spawned {
			self.clear(&token);
			crate::carbon_warn!(
				"Could not monitor the Studio quick save; waiting for Studio auto-recovery instead: {error}"
			);
		}
	}

	/// Wait for `ticket`'s save, stop its test server, and stage the copy.
	pub(crate) fn finish(&self, ticket: QuickSaveTicket) -> Result<PathBuf> {
		let result = self.complete(&ticket);
		self.clear(&ticket.token);
		result
	}

	/// Release a ticket whose request never reached Studio.
	pub(crate) fn abandon(&self, ticket: QuickSaveTicket) {
		self.clear(&ticket.token);
	}

	fn complete(&self, ticket: &QuickSaveTicket) -> Result<PathBuf> {
		let deadline = Instant::now() + QUICK_SAVE_TIMEOUT;
		let saved = wait_for_quick_save(
			ticket.baseline,
			|| fingerprint(&self.host.quick_save_file),
			|| self.failure(&ticket.token),
			|| Instant::now() >= deadline,
			thread::sleep,
		)?;
		// Stop the test before staging: the save is complete, and nothing in the
		// test session is needed. Studio's plugin backstop ends it if this fails.
		let since = saved
			.modified
			.checked_sub(TEST_SERVER_START_MARGIN)
			.unwrap_or(SystemTime::UNIX_EPOCH);
		match (self.host.reaper)(since) {
			Ok(stopped) => log::debug!("Stopped {stopped} Studio quick-save test process(es)"),
			Err(error) => crate::carbon_warn!(
				"Could not stop the Studio quick-save test server; the Carbon plugin will end it: {error:#}"
			),
		}
		stage(&self.host.quick_save_file, &self.staging_dir, saved)
	}

	fn failure(&self, token: &str) -> Option<String> {
		self.in_flight
			.lock()
			.unwrap()
			.as_ref()
			.filter(|pending| pending.token == token)
			.and_then(|pending| pending.failure.clone())
	}

	fn clear(&self, token: &str) {
		let mut in_flight = self.in_flight.lock().unwrap();
		if in_flight.as_ref().is_some_and(|pending| pending.token == token) {
			*in_flight = None;
		}
	}
}

impl Drop for QuickSaveCoordinator {
	fn drop(&mut self) {
		if let Err(error) = fs::remove_dir_all(&self.staging_dir) {
			if error.kind() != std::io::ErrorKind::NotFound {
				log::warn!(
					"failed to remove the Studio quick-save staging directory {}: {error}",
					self.staging_dir.display()
				);
			}
		}
	}
}

fn acquire_lock(path: &Path, timeout: Duration) -> Result<fs::File> {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent)
			.with_context(|| format!("failed to create the quick-save lock directory {}", parent.display()))?;
	}
	let file = fs::OpenOptions::new()
		.create(true)
		.truncate(false)
		.read(true)
		.write(true)
		.open(path)
		.with_context(|| format!("failed to open the Studio quick-save lock {}", path.display()))?;
	let deadline = Instant::now() + timeout;
	loop {
		match file.try_lock() {
			Ok(()) => return Ok(file),
			Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
			Err(fs::TryLockError::WouldBlock) => bail!(
				"another Carbon session held the Studio quick-save lock {} for {} seconds",
				path.display(),
				timeout.as_secs()
			),
			Err(fs::TryLockError::Error(error)) => {
				return Err(error).with_context(|| format!("failed to lock {}", path.display()));
			}
		}
	}
}

fn fingerprint(path: &Path) -> Result<Option<RecoveryFingerprint>> {
	match fs::metadata(path) {
		Ok(metadata) if metadata.is_file() => Ok(Some(
			RecoveryFingerprint::from_metadata(&metadata)
				.with_context(|| format!("failed to read the modification time of {}", path.display()))?,
		)),
		Ok(_) => Ok(None),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
		Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
	}
}

/// Wait until Studio replaces the quick-save file and leaves it unchanged for
/// consecutive polls. Only a changed fingerprint counts, which keeps detection
/// independent of clock skew between WSL and Windows.
fn wait_for_quick_save(
	baseline: Option<RecoveryFingerprint>,
	mut current: impl FnMut() -> Result<Option<RecoveryFingerprint>>,
	mut failure: impl FnMut() -> Option<String>,
	mut expired: impl FnMut() -> bool,
	mut sleep: impl FnMut(Duration),
) -> Result<RecoveryFingerprint> {
	let mut observed: Option<(RecoveryFingerprint, usize)> = None;
	loop {
		if let Some(reason) = failure() {
			bail!("Studio could not start the quick-save test: {reason}");
		}
		match current()?.filter(|candidate| candidate.len > 0 && Some(*candidate) != baseline) {
			Some(candidate) => {
				let polls = match observed {
					Some((previous, polls)) if previous == candidate => polls + 1,
					_ => 1,
				};
				if polls >= STABLE_POLLS {
					return Ok(candidate);
				}
				observed = Some((candidate, polls));
			}
			None => observed = None,
		}
		if expired() {
			bail!(
				"Studio did not write a quick save within {} seconds",
				QUICK_SAVE_TIMEOUT.as_secs()
			);
		}
		sleep(POLL_INTERVAL);
	}
}

/// Copy the quick save into the staging directory under a fresh name, so a
/// later Studio test launch cannot replace evidence while Carbon decodes it.
fn stage(quick_save_file: &Path, staging_dir: &Path, expected: RecoveryFingerprint) -> Result<PathBuf> {
	fs::create_dir_all(staging_dir).with_context(|| format!("failed to create {}", staging_dir.display()))?;
	let id = uuid::Uuid::new_v4().simple();
	// Without the .rbxl extension the recovery scan ignores the partial copy.
	let partial = staging_dir.join(format!("{STAGED_PREFIX}{id}.partial"));
	let staged = staging_dir.join(format!("{STAGED_PREFIX}{id}.rbxl"));
	let copied = fs::copy(quick_save_file, &partial)
		.with_context(|| format!("failed to copy the Studio quick save {}", quick_save_file.display()))
		.and_then(|_| {
			ensure!(
				fingerprint(quick_save_file)? == Some(expected),
				"Studio replaced its quick save while Carbon copied it"
			);
			fs::rename(&partial, &staged)
				.with_context(|| format!("failed to stage the Studio quick save at {}", staged.display()))
		});
	if copied.is_err() {
		let _ = fs::remove_file(&partial);
	}
	copied.map(|()| staged)
}

/// Convert to a Windows FILETIME: 100-nanosecond intervals since 1601-01-01.
pub(crate) fn filetime(time: SystemTime) -> u64 {
	let since_unix_epoch = time.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
	FILETIME_UNIX_EPOCH + u64::try_from(since_unix_epoch.as_nanos() / 100).unwrap_or(u64::MAX - FILETIME_UNIX_EPOCH)
}

/// How long an edit plugin's playtest stop request stays valid. A play server
/// that starts polling later than this belongs to a newer playtest.
const PLAYTEST_STOP_LIFETIME: Duration = Duration::from_secs(15);

/// Hands the edit plugin's request to stop a running playtest to the play
/// server, the only DataModel that can call `StudioTestService:EndTest`.
pub(crate) struct PlaytestStopSignal {
	requested_at: Mutex<Option<Instant>>,
	changed: Condvar,
	lifetime: Duration,
}

impl Default for PlaytestStopSignal {
	fn default() -> Self {
		Self::with_lifetime(PLAYTEST_STOP_LIFETIME)
	}
}

impl PlaytestStopSignal {
	pub(crate) fn with_lifetime(lifetime: Duration) -> Self {
		Self {
			requested_at: Mutex::new(None),
			changed: Condvar::new(),
			lifetime,
		}
	}

	pub(crate) fn request(&self) {
		*self.requested_at.lock().unwrap() = Some(Instant::now());
		self.changed.notify_all();
	}

	/// Wait up to `timeout` for a live request and consume it.
	pub(crate) fn wait(&self, timeout: Duration) -> bool {
		let deadline = Instant::now() + timeout;
		let mut requested_at = self.requested_at.lock().unwrap();
		loop {
			if let Some(at) = *requested_at {
				*requested_at = None;
				if at.elapsed() <= self.lifetime {
					return true;
				}
			}
			let now = Instant::now();
			if now >= deadline {
				return false;
			}
			requested_at = self.changed.wait_timeout(requested_at, deadline - now).unwrap().0;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::atomic::{AtomicUsize, Ordering};

	fn print(len: u64, seconds: u64) -> RecoveryFingerprint {
		RecoveryFingerprint {
			len,
			modified: SystemTime::UNIX_EPOCH + Duration::from_secs(seconds),
		}
	}

	#[test]
	fn quick_save_waits_for_a_changed_file_to_settle() {
		let observations = [
			Some(print(10, 1)),
			Some(print(0, 2)),
			Some(print(40, 3)),
			Some(print(80, 3)),
			Some(print(80, 3)),
		];
		let mut polls = observations.into_iter();
		let mut sleeps = 0;
		let saved = wait_for_quick_save(
			Some(print(10, 1)),
			|| Ok(polls.next().unwrap()),
			|| None,
			|| false,
			|_| sleeps += 1,
		)
		.unwrap();
		assert_eq!(saved, print(80, 3));
		assert_eq!(
			sleeps, 4,
			"the baseline, an empty write, and a growing file are not complete saves"
		);
	}

	#[test]
	fn quick_save_stops_on_a_plugin_failure() {
		let error = wait_for_quick_save(
			None,
			|| Ok(None),
			|| Some("a test session is already running".to_owned()),
			|| false,
			|_| {},
		)
		.unwrap_err();
		assert!(
			format!("{error:#}").contains("a test session is already running"),
			"{error:#}"
		);
	}

	#[test]
	fn quick_save_wait_is_bounded() {
		let mut checks = 0;
		let error = wait_for_quick_save(
			None,
			|| Ok(None),
			|| None,
			|| {
				checks += 1;
				checks > 2
			},
			|_| {},
		)
		.unwrap_err();
		assert!(format!("{error:#}").contains("did not write a quick save"), "{error:#}");
	}

	#[test]
	fn filetime_counts_from_1601() {
		assert_eq!(filetime(SystemTime::UNIX_EPOCH), FILETIME_UNIX_EPOCH);
		assert_eq!(
			filetime(SystemTime::UNIX_EPOCH + Duration::from_secs(1)),
			FILETIME_UNIX_EPOCH + 10_000_000
		);
	}

	fn host(directory: &Path, reaper: TestSessionReaper) -> StudioQuickSaveHost {
		StudioQuickSaveHost {
			quick_save_file: directory.join("server.rbxl"),
			lock_file: directory.join("quick-save.lock"),
			reaper,
		}
	}

	#[test]
	fn completed_quick_save_stops_the_test_server_and_stages_a_private_copy() {
		let directory = tempfile::tempdir().unwrap();
		let reaped = Arc::new(Mutex::new(Vec::new()));
		let calls = Arc::clone(&reaped);
		let coordinator = QuickSaveCoordinator::new(host(
			directory.path(),
			Arc::new(move |since| {
				calls.lock().unwrap().push(since);
				Ok(1)
			}),
		))
		.unwrap();
		fs::write(directory.path().join("server.rbxl"), b"previous save").unwrap();

		let ticket = coordinator.arm(7).unwrap().unwrap();
		assert!(
			coordinator.arm(7).unwrap().is_none(),
			"one quick save at a time per session"
		);
		fs::write(directory.path().join("server.rbxl"), b"current edit DataModel").unwrap();
		let staged = coordinator.complete(&ticket).unwrap();

		assert_eq!(fs::read(&staged).unwrap(), b"current edit DataModel");
		assert!(staged.starts_with(coordinator.staging_dir()));
		assert_eq!(staged.extension().unwrap(), "rbxl");
		let modified = fs::metadata(directory.path().join("server.rbxl"))
			.unwrap()
			.modified()
			.unwrap();
		assert_eq!(*reaped.lock().unwrap(), [modified - TEST_SERVER_START_MARGIN]);

		let staging_dir = coordinator.staging_dir().to_owned();
		drop(ticket);
		drop(coordinator);
		assert!(
			!staging_dir.exists(),
			"the session's staged copies are private and temporary"
		);
	}

	#[test]
	fn plugin_failure_reports_only_reach_the_matching_request() {
		let directory = tempfile::tempdir().unwrap();
		let reaped = Arc::new(AtomicUsize::new(0));
		let calls = Arc::clone(&reaped);
		let coordinator = QuickSaveCoordinator::new(host(
			directory.path(),
			Arc::new(move |_| {
				calls.fetch_add(1, Ordering::SeqCst);
				Ok(0)
			}),
		))
		.unwrap();
		let ticket = coordinator.arm(7).unwrap().unwrap();

		assert!(coordinator.report_failure(7, "other", "late".to_owned()).is_err());
		assert!(coordinator
			.report_failure(8, &ticket.token, "spoofed".to_owned())
			.is_err());
		coordinator
			.report_failure(7, &ticket.token, "Studio is running a test session".to_owned())
			.unwrap();
		let error = coordinator.complete(&ticket).unwrap_err();

		assert!(
			format!("{error:#}").contains("Studio is running a test session"),
			"{error:#}"
		);
		assert_eq!(
			reaped.load(Ordering::SeqCst),
			0,
			"no test started, so nothing is stopped"
		);
	}

	#[test]
	fn quick_saves_are_serialized_across_carbon_sessions() {
		let directory = tempfile::tempdir().unwrap();
		let first = QuickSaveCoordinator::new(host(directory.path(), Arc::new(|_| Ok(0)))).unwrap();
		let second = QuickSaveCoordinator::new(host(directory.path(), Arc::new(|_| Ok(0)))).unwrap();
		let held = first.arm(1).unwrap().unwrap();

		let error = acquire_lock(&directory.path().join("quick-save.lock"), Duration::ZERO).unwrap_err();
		assert!(
			format!("{error:#}").contains("held the Studio quick-save lock"),
			"{error:#}"
		);
		drop(held);
		assert!(second.arm(2).unwrap().is_some());
	}

	#[test]
	fn a_fresh_quick_save_waits_for_the_one_already_in_flight() {
		let directory = tempfile::tempdir().unwrap();
		let coordinator = QuickSaveCoordinator::new(host(directory.path(), Arc::new(|_| Ok(0)))).unwrap();
		let earlier = coordinator.arm(7).unwrap().unwrap();
		let earlier_token = earlier.token.clone();
		let finishing = Arc::clone(&coordinator);
		let finisher = thread::spawn(move || {
			thread::sleep(Duration::from_millis(200));
			finishing.abandon(earlier);
		});

		let started = Instant::now();
		let fresh = coordinator.arm_after_in_flight(7).unwrap();
		finisher.join().unwrap();

		assert!(started.elapsed() >= Duration::from_millis(200));
		assert_ne!(
			fresh.token, earlier_token,
			"the fresh request must not reuse the earlier save"
		);
	}

	#[test]
	fn a_stale_playtest_stop_request_never_ends_a_later_playtest() {
		let signal = PlaytestStopSignal::with_lifetime(Duration::from_millis(50));
		signal.request();
		thread::sleep(Duration::from_millis(80));
		assert!(
			!signal.wait(Duration::from_millis(10)),
			"a request older than its lifetime must not end a playtest that starts later"
		);

		signal.request();
		assert!(signal.wait(Duration::from_millis(10)));
		assert!(!signal.wait(Duration::from_millis(10)), "each request is consumed once");
	}
}
