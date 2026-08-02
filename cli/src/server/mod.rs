use actix_msgpack::MsgPackConfig;
use actix_web::{
	dev::ServerHandle,
	web::{self, Data},
	App, HttpServer, Responder,
};
use derive_from_one::FromOne;
use serde::{Deserialize, Serialize};
use std::{
	io::Result,
	net::TcpListener,
	sync::{
		atomic::{AtomicBool, AtomicU8, Ordering},
		mpsc::Receiver,
		Arc, OnceLock,
	},
	time::Duration,
};

use crate::{constants::MAX_PAYLOAD_SIZE, core::Core};

mod capture;
mod details;
mod diagnostics;
mod exec;
mod home;
mod read;
mod reflection;
mod snapshot;
mod stop;
mod studio_change;
mod subscribe;
mod unsubscribe;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeControl {
	Abort,
	Shutdown,
	Reload,
}

const MAX_AUTOMATIC_RELOAD_RETRIES: u8 = 3;

#[derive(Clone)]
pub struct ServeControlSender {
	sender: crossbeam_channel::Sender<ServeControl>,
	reload_pending: Arc<AtomicBool>,
	reload_again: Arc<AtomicBool>,
	reload_failed: Arc<AtomicBool>,
	reload_retry_attempts: Arc<AtomicU8>,
}

impl ServeControlSender {
	pub fn try_send(
		&self,
		control: ServeControl,
	) -> std::result::Result<(), crossbeam_channel::TrySendError<ServeControl>> {
		if control == ServeControl::Reload {
			self.reload_failed.store(false, Ordering::Release);
			self.reload_retry_attempts.store(0, Ordering::Release);
		}
		self.try_enqueue(control)
	}

	fn try_enqueue(
		&self,
		control: ServeControl,
	) -> std::result::Result<(), crossbeam_channel::TrySendError<ServeControl>> {
		if control == ServeControl::Reload && self.reload_pending.swap(true, Ordering::AcqRel) {
			self.reload_again.store(true, Ordering::Release);
			return Ok(());
		}
		match self.sender.try_send(control) {
			Ok(()) => Ok(()),
			Err(error) => {
				if control == ServeControl::Reload {
					self.reload_pending.store(false, Ordering::Release);
				}
				Err(error)
			}
		}
	}

	pub fn send(&self, control: ServeControl) -> std::result::Result<(), crossbeam_channel::SendError<ServeControl>> {
		if control == ServeControl::Reload {
			self.reload_failed.store(false, Ordering::Release);
			self.reload_retry_attempts.store(0, Ordering::Release);
		}
		if control == ServeControl::Reload && self.reload_pending.swap(true, Ordering::AcqRel) {
			self.reload_again.store(true, Ordering::Release);
			return Ok(());
		}
		match self.sender.send(control) {
			Ok(()) => Ok(()),
			Err(error) => {
				if control == ServeControl::Reload {
					self.reload_pending.store(false, Ordering::Release);
				}
				Err(error)
			}
		}
	}

	pub fn fail_reload(&self, retry: bool) {
		self.reload_pending.store(false, Ordering::Release);
		self.reload_failed.store(retry, Ordering::Release);
		if self.reload_again.swap(false, Ordering::AcqRel) {
			self.reload_failed.store(false, Ordering::Release);
			let _ = self.try_send(ServeControl::Reload);
			return;
		}
		if !retry {
			return;
		}
		let attempt = self.reload_retry_attempts.fetch_add(1, Ordering::AcqRel) + 1;
		if attempt > MAX_AUTOMATIC_RELOAD_RETRIES {
			self.reload_failed.store(false, Ordering::Release);
			return;
		}
		let retry = self.clone();
		std::thread::spawn(move || {
			std::thread::sleep(Duration::from_millis(100 * (1_u64 << (attempt - 1))));
			retry.retry_failed_reload();
		});
	}

	pub fn retry_failed_reload(&self) {
		if self.reload_failed.swap(false, Ordering::AcqRel) {
			let _ = self.try_enqueue(ServeControl::Reload);
		}
	}

	pub fn acknowledge_reload(&self) {
		self.reload_failed.store(false, Ordering::Release);
		self.reload_pending.store(false, Ordering::Release);
		self.reload_retry_attempts.store(0, Ordering::Release);
		if self.reload_again.swap(false, Ordering::AcqRel) {
			let _ = self.try_send(ServeControl::Reload);
		}
	}
}

pub fn serve_control_channel() -> (ServeControlSender, crossbeam_channel::Receiver<ServeControl>) {
	let (sender, receiver) = crossbeam_channel::unbounded();
	(
		ServeControlSender {
			sender,
			reload_pending: Arc::new(AtomicBool::new(false)),
			reload_again: Arc::new(AtomicBool::new(false)),
			reload_failed: Arc::new(AtomicBool::new(false)),
			reload_retry_attempts: Arc::new(AtomicU8::new(0)),
		},
		receiver,
	)
}

#[derive(Debug, Clone, Serialize, FromOne)]
pub enum Message {
	SyncChanges(SyncChanges),
	SyncDetails(crate::source::SourceDetails),
	RestartRequired(RestartRequired),
	ExecuteCode(ExecuteCode),
	Disconnect(Disconnect),
	ManagedReload(ManagedReload),
	StudioChangeProbe(StudioChangeProbe),
}

#[derive(Debug, Clone, Serialize)]
pub struct RestartRequired {
	pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncChanges {
	pub changes: crate::core::changes::Changes,
	pub source_generation: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecuteCode {
	pub code: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Disconnect {
	pub message: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedReload {
	pub transition_id: String,
	pub project_name: String,
	pub source_generation: String,
	pub worktree_id: String,
	pub session_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StudioChangeProbe {
	pub request_id: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AuthRequest {
	client_id: u32,
}

pub struct Server {
	core: Arc<Core>,
	host: String,
	port: u16,
	external_stop_requested: Arc<AtomicBool>,
}

pub(crate) type StopHandle = Arc<OnceLock<ServerHandle>>;
pub(crate) type StopRequested = Arc<AtomicBool>;

impl Server {
	pub fn new(core: Arc<Core>, host: &str, port: u16) -> Self {
		Self {
			core,
			host: host.to_owned(),
			port,
			external_stop_requested: Arc::new(AtomicBool::new(false)),
		}
	}

	pub fn external_stop_requested(&self) -> bool {
		self.external_stop_requested.load(Ordering::Acquire)
	}
	pub(crate) fn external_stop_signal(&self) -> Arc<AtomicBool> {
		Arc::clone(&self.external_stop_requested)
	}

	#[actix_web::main]
	pub async fn start(&self) -> Result<()> {
		let listener = TcpListener::bind((self.host.clone(), self.port))?;
		self.run(listener).await
	}

	#[actix_web::main]
	pub async fn start_control<F>(
		&self,
		control_receiver: crossbeam_channel::Receiver<ServeControl>,
		control_callback: F,
	) -> Result<()>
	where
		F: FnMut(ServeControl) -> bool + Send + 'static,
	{
		let listener = TcpListener::bind((self.host.clone(), self.port))?;
		self.run_with_listener_control(listener, control_receiver, control_callback)
			.await
	}

	#[actix_web::main]
	pub async fn start_with_listener(&self, listener: TcpListener) -> Result<()> {
		self.run(listener).await
	}

	#[actix_web::main]
	pub async fn start_with_listener_control<F>(
		&self,
		listener: TcpListener,
		control_receiver: crossbeam_channel::Receiver<ServeControl>,
		control_callback: F,
	) -> Result<()>
	where
		F: FnMut(ServeControl) -> bool + Send + 'static,
	{
		self.run_with_listener_control(listener, control_receiver, control_callback)
			.await
	}

	#[actix_web::main]
	pub async fn start_with_listener_until<F>(
		&self,
		listener: TcpListener,
		shutdown_signal: Receiver<()>,
		shutdown: F,
	) -> Result<()>
	where
		F: FnOnce() + Send + 'static,
	{
		let (control_sender, control_receiver) = serve_control_channel();
		let shutdown = Arc::new(std::sync::Mutex::new(Some(shutdown)));

		actix_web::rt::spawn(async move {
			loop {
				match shutdown_signal.try_recv() {
					Ok(()) => {
						let _ = control_sender.send(ServeControl::Shutdown);
						break;
					}
					Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
					Err(std::sync::mpsc::TryRecvError::Empty) => {
						actix_web::rt::time::sleep(Duration::from_millis(25)).await;
					}
				}
			}
		});

		self.run_with_listener_control(listener, control_receiver, move |control| match control {
			ServeControl::Abort | ServeControl::Shutdown | ServeControl::Reload => {
				if let Ok(mut lock) = shutdown.lock() {
					if let Some(f) = lock.take() {
						f();
					}
				}
				true
			}
		})
		.await
	}

	async fn run_with_listener_control<F>(
		&self,
		listener: TcpListener,
		control_receiver: crossbeam_channel::Receiver<ServeControl>,
		control_callback: F,
	) -> Result<()>
	where
		F: FnMut(ServeControl) -> bool + Send + 'static,
	{
		let server = self.http_server(listener)?;
		let handle = server.handle();
		let control_callback = Arc::new(std::sync::Mutex::new(control_callback));
		actix_web::rt::spawn(async move {
			loop {
				match control_receiver.try_recv() {
					Ok(control) => {
						let graceful_stop = control == ServeControl::Shutdown;
						let callback = Arc::clone(&control_callback);
						let should_stop = actix_web::rt::task::spawn_blocking(move || {
							callback.lock().unwrap_or_else(|error| error.into_inner())(control)
						})
						.await
						.unwrap_or(true);
						if should_stop {
							if graceful_stop {
								// Match `/stop`'s response grace so a concurrent request can
								// flush after both shutdown sources converge on one capture.
								actix_web::rt::time::sleep(Duration::from_millis(50)).await;
							}
							handle.stop(false).await;
							break;
						}
					}
					Err(crossbeam_channel::TryRecvError::Disconnected) => break,
					Err(crossbeam_channel::TryRecvError::Empty) => {
						actix_web::rt::time::sleep(Duration::from_millis(25)).await;
					}
				}
			}
		});
		server.await
	}

	async fn run(&self, listener: TcpListener) -> Result<()> {
		self.http_server(listener)?.await
	}

	fn http_server(&self, listener: TcpListener) -> Result<actix_web::dev::Server> {
		let core = self.core.clone();
		let stop_handle: StopHandle = Arc::new(OnceLock::new());
		let app_stop_handle = stop_handle.clone();
		let stop_requested = Arc::clone(&self.external_stop_requested);
		let server = HttpServer::new(move || {
			let mut msgpack_config = MsgPackConfig::default();
			msgpack_config.limit(MAX_PAYLOAD_SIZE);

			App::new()
				.app_data(Data::new(core.clone()))
				.app_data(Data::new(app_stop_handle.clone()))
				.app_data(Data::new(stop_requested.clone()))
				.app_data(msgpack_config)
				.service(details::main)
				.service(reflection::main)
				.service(subscribe::main)
				.service(unsubscribe::main)
				.service(snapshot::page)
				.service(snapshot::source_page)
				.service(capture::initiate)
				.service(capture::automatic)
				.service(capture::get_status)
				.service(capture::cancel)
				.service(diagnostics::warnings)
				.service(read::main)
				.service(exec::main)
				.service(stop::main)
				.service(studio_change::acknowledge)
				.service(home::main)
				.default_service(web::to(Self::default_redirect))
		})
		.backlog(0)
		.disable_signals()
		.listen(listener)?
		.run();
		let _ = stop_handle.set(server.handle());
		Ok(server)
	}

	async fn default_redirect() -> impl Responder {
		web::Redirect::to("/")
	}
}

pub fn is_port_free(host: &str, port: u16) -> bool {
	TcpListener::bind((host, port)).is_ok()
}

pub fn get_free_port(host: &str, port: u16) -> u16 {
	let mut port = port;

	while !is_port_free(host, port) {
		port += 1;

		// This should never happen, but just in case
		if port == 65535 {
			break;
		}
	}

	port
}

pub fn format_address(host: &str, port: u16) -> String {
	format!("http://{host}:{port}")
}
#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

	fn test_core() -> Arc<Core> {
		let temp_dir = std::env::temp_dir().join(format!("carbon-test-server-{}", uuid::Uuid::new_v4()));
		let _ = std::fs::create_dir_all(&temp_dir);
		let manifest = temp_dir.join("carbon.json");
		let snapshot = crate::core::snapshot::Snapshot::new().with_name("Fixture");
		let _ = crate::artifact_store::extract_snapshot(snapshot, "Fixture".to_owned(), &manifest);
		Arc::new(Core::new_artifact(&manifest).unwrap())
	}

	#[test]
	fn managed_reload_serialization_is_camel_case() {
		let reload = ManagedReload {
			transition_id: "tx-123".into(),
			project_name: "RenamedGame".into(),
			source_generation: "gen-234".into(),
			worktree_id: "wt-456".into(),
			session_token: "tok-789".into(),
		};
		let json = serde_json::to_value(&reload).unwrap();
		assert_eq!(json["transitionId"], "tx-123");
		assert_eq!(json["projectName"], "RenamedGame");
		assert_eq!(json["sourceGeneration"], "gen-234");
		assert_eq!(json["worktreeId"], "wt-456");
		assert_eq!(json["sessionToken"], "tok-789");

		let message = Message::ManagedReload(reload);
		let msg_json = serde_json::to_value(&message).unwrap();
		assert_eq!(msg_json["ManagedReload"]["transitionId"], "tx-123");
		assert_eq!(msg_json["ManagedReload"]["projectName"], "RenamedGame");
		assert_eq!(msg_json["ManagedReload"]["sourceGeneration"], "gen-234");
		assert_eq!(msg_json["ManagedReload"]["worktreeId"], "wt-456");
		assert_eq!(msg_json["ManagedReload"]["sessionToken"], "tok-789");
	}

	#[test]
	fn studio_change_probe_serialization_is_camel_case() {
		let message = Message::StudioChangeProbe(StudioChangeProbe {
			request_id: "probe-123".into(),
		});
		let json = serde_json::to_value(&message).unwrap();

		assert_eq!(json["StudioChangeProbe"]["requestId"], "probe-123");
	}

	#[test]
	fn serve_control_latches_reload_behind_shutdown_and_replays_inflight_change() {
		let (sender, receiver) = serve_control_channel();
		sender.send(ServeControl::Shutdown).unwrap();
		sender.try_send(ServeControl::Reload).unwrap();
		sender.try_send(ServeControl::Reload).unwrap();

		assert_eq!(receiver.recv().unwrap(), ServeControl::Shutdown);
		assert_eq!(receiver.recv().unwrap(), ServeControl::Reload);
		assert!(receiver.try_recv().is_err());

		sender.acknowledge_reload();
		assert_eq!(receiver.recv().unwrap(), ServeControl::Reload);
		sender.acknowledge_reload();
		assert!(receiver.try_recv().is_err());
	}

	#[test]
	fn failed_reload_retries_on_the_next_duplicate_event() {
		let (sender, receiver) = serve_control_channel();
		sender.try_send(ServeControl::Reload).unwrap();
		assert_eq!(receiver.recv().unwrap(), ServeControl::Reload);

		sender.fail_reload(true);
		assert!(receiver.try_recv().is_err());
		sender.retry_failed_reload();
		assert_eq!(receiver.recv().unwrap(), ServeControl::Reload);

		sender.acknowledge_reload();
		sender.retry_failed_reload();
		assert!(receiver.try_recv().is_err());
	}

	#[test]
	fn permanent_reload_failure_waits_for_a_new_source_revision() {
		let (sender, receiver) = serve_control_channel();
		sender.try_send(ServeControl::Reload).unwrap();
		assert_eq!(receiver.recv().unwrap(), ServeControl::Reload);

		sender.fail_reload(false);
		std::thread::sleep(Duration::from_millis(150));
		assert!(receiver.try_recv().is_err());

		sender.try_send(ServeControl::Reload).unwrap();
		assert_eq!(receiver.recv().unwrap(), ServeControl::Reload);
	}

	#[test]
	fn transient_reload_retries_are_bounded_until_a_new_source_revision() {
		let (sender, receiver) = serve_control_channel();
		sender.try_send(ServeControl::Reload).unwrap();
		assert_eq!(receiver.recv().unwrap(), ServeControl::Reload);

		for _ in 0..3 {
			sender.fail_reload(true);
			assert_eq!(
				receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
				ServeControl::Reload
			);
		}
		sender.fail_reload(true);
		assert!(receiver.recv_timeout(Duration::from_millis(150)).is_err());

		sender.try_send(ServeControl::Reload).unwrap();
		assert_eq!(receiver.recv().unwrap(), ServeControl::Reload);
	}

	#[test]
	fn server_control_reload_callback_false_keeps_server_live_and_true_stops() {
		let core = test_core();
		let port = get_free_port("127.0.0.1", 20000);
		let server = Server::new(core, "127.0.0.1", port);
		let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
		let (control_sender, control_receiver) = serve_control_channel();
		let call_count = Arc::new(AtomicUsize::new(0));
		let call_count_cb = call_count.clone();
		let callback_sender = control_sender.clone();
		let handle = std::thread::spawn(move || {
			server.start_with_listener_control(listener, control_receiver, move |control| {
				let count = call_count_cb.fetch_add(1, Ordering::SeqCst);
				match control {
					ServeControl::Reload if count == 0 => {
						callback_sender.fail_reload(true);
						false
					}
					ServeControl::Reload => {
						callback_sender.acknowledge_reload();
						true
					}
					ServeControl::Abort | ServeControl::Shutdown => true,
				}
			})
		});
		std::thread::sleep(Duration::from_millis(50));
		control_sender.send(ServeControl::Reload).unwrap();
		assert!(handle.join().unwrap().is_ok());
		assert_eq!(call_count.load(Ordering::SeqCst), 2);
	}

	#[test]
	fn server_control_shutdown_signal_stops_server() {
		let core = test_core();
		let port = get_free_port("127.0.0.1", 21000);
		let server = Server::new(core, "127.0.0.1", port);
		let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
		let (shutdown_sender, shutdown_receiver) = std::sync::mpsc::channel();
		let shutdown_called = Arc::new(AtomicBool::new(false));
		let shutdown_called_cb = shutdown_called.clone();
		let handle = std::thread::spawn(move || {
			server.start_with_listener_until(listener, shutdown_receiver, move || {
				shutdown_called_cb.store(true, Ordering::SeqCst);
			})
		});
		std::thread::sleep(Duration::from_millis(50));
		shutdown_sender.send(()).unwrap();
		assert!(handle.join().unwrap().is_ok());
		assert!(shutdown_called.load(Ordering::SeqCst));
	}
}
