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
	sync::{atomic::AtomicBool, mpsc::Receiver, Arc, OnceLock},
	time::Duration,
};

use crate::{constants::MAX_PAYLOAD_SIZE, core::Core};

mod capture;
mod details;
mod diagnostics;
mod exec;
mod home;
pub(crate) mod privileged;
mod read;
mod reflection;
mod snapshot;
mod stop;
mod subscribe;
mod unsubscribe;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeControl {
	Shutdown,
}

#[derive(Clone)]
pub struct ServeControlSender {
	sender: crossbeam_channel::Sender<ServeControl>,
}

impl ServeControlSender {
	pub fn try_send(
		&self,
		control: ServeControl,
	) -> std::result::Result<(), crossbeam_channel::TrySendError<ServeControl>> {
		self.sender.try_send(control)
	}

	pub fn send(&self, control: ServeControl) -> std::result::Result<(), crossbeam_channel::SendError<ServeControl>> {
		self.sender.send(control)
	}
}

pub fn serve_control_channel() -> (ServeControlSender, crossbeam_channel::Receiver<ServeControl>) {
	let (sender, receiver) = crossbeam_channel::unbounded();
	(ServeControlSender { sender }, receiver)
}

#[derive(Debug, Clone, Serialize, FromOne)]
pub enum Message {
	SyncChanges(SyncChanges),
	RestartRequired(RestartRequired),
	ExecuteCode(ExecuteCode),
	Disconnect(Disconnect),
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

		self.run_with_listener_control(listener, control_receiver, move |ServeControl::Shutdown| {
			if let Ok(mut lock) = shutdown.lock() {
				if let Some(f) = lock.take() {
					f();
				}
			}
			true
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
						let callback = Arc::clone(&control_callback);
						let should_stop = actix_web::rt::task::spawn_blocking(move || {
							callback.lock().unwrap_or_else(|error| error.into_inner())(control)
						})
						.await
						.unwrap_or(true);
						if should_stop {
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
				.service(capture::get_status)
				.service(capture::cancel)
				.service(diagnostics::warnings)
				.service(diagnostics::export_place)
				.service(read::main)
				.service(exec::main)
				.service(stop::main)
				.service(home::main)
				.service(privileged::capabilities)
				.service(privileged::attach_managed_hierarchy)
				.service(privileged::bootstrap_manifest_identities)
				.service(privileged::resolve_managed_identities)
				.service(privileged::poll_managed_identities)
				.service(privileged::read_property)
				.service(privileged::read_properties)
				.service(privileged::read_references)
				.service(privileged::read_default_properties)
				.service(privileged::write_property)
				.service(privileged::write_reference)
				.service(privileged::copy_property)
				.service(privileged::materialize_property)
				.service(privileged::write_materialized_property)
				.service(privileged::create_instance)
				.service(privileged::roots)
				.service(privileged::root_snapshots)
				.service(privileged::apply_hidden_roots)
				.service(privileged::changes)
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
	use std::sync::atomic::{AtomicBool, Ordering};

	fn test_core() -> Arc<Core> {
		let temp_dir = std::env::temp_dir().join(format!("carbon-test-server-{}", uuid::Uuid::new_v4()));
		let _ = std::fs::create_dir_all(&temp_dir);
		let manifest = temp_dir.join("carbon.json");
		let snapshot = crate::core::snapshot::Snapshot::new().with_name("Fixture");
		let _ = crate::artifact_store::extract_snapshot(snapshot, "Fixture".to_owned(), &manifest);
		Arc::new(Core::new_artifact(&manifest).unwrap())
	}

	#[test]
	fn serve_control_forwards_shutdown() {
		let (sender, receiver) = serve_control_channel();

		sender.send(ServeControl::Shutdown).unwrap();
		assert_eq!(receiver.recv().unwrap(), ServeControl::Shutdown);
		assert!(receiver.try_recv().is_err());
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
