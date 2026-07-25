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
	sync::{mpsc::Receiver, Arc, OnceLock},
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
}

pub(crate) type StopHandle = Arc<OnceLock<ServerHandle>>;

impl Server {
	pub fn new(core: Arc<Core>, host: &str, port: u16) -> Self {
		Self {
			core,
			host: host.to_owned(),
			port,
		}
	}

	#[actix_web::main]
	pub async fn start(&self) -> Result<()> {
		let listener = TcpListener::bind((self.host.clone(), self.port))?;
		self.run(listener).await
	}

	#[actix_web::main]
	pub async fn start_with_listener(&self, listener: TcpListener) -> Result<()> {
		self.run(listener).await
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
		let server = self.http_server(listener)?;
		let handle = server.handle();
		actix_web::rt::spawn(async move {
			loop {
				match shutdown_signal.try_recv() {
					Ok(()) => break,
					Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
					Err(std::sync::mpsc::TryRecvError::Empty) => {
						actix_web::rt::time::sleep(Duration::from_millis(25)).await;
					}
				}
			}
			let _ = actix_web::rt::task::spawn_blocking(shutdown).await;
			// Keep the endpoint and owning Studio process alive until the blocking
			// shutdown operation (such as automatic capture) has settled.
			handle.stop(false).await;
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

		let server = HttpServer::new(move || {
			let mut msgpack_config = MsgPackConfig::default();
			msgpack_config.limit(MAX_PAYLOAD_SIZE);

			App::new()
				.app_data(Data::new(core.clone()))
				.app_data(Data::new(app_stop_handle.clone()))
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
