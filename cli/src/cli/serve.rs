use anyhow::{ensure, Context, Result};
use clap::Parser;
use colored::Colorize;
use parking_lot::Mutex;
use std::{
	fs, io,
	net::TcpListener,
	path::{Path, PathBuf},
	process,
	sync::{
		atomic::{AtomicBool, Ordering},
		mpsc, Arc,
	},
	thread,
	time::{Duration, Instant},
};

use crate::{
	artifact_resolution, artifact_store,
	config::Config,
	core::Core,
	ext::PathExt,
	project,
	server::{self, Server},
	sessions::{self, Session},
	source, studio, util,
};

const SERVE_HOST: &str = "127.0.0.1";
const QUALIFICATION_STUDIO_NAME: &str = "CARBON_QUALIFICATION_STUDIO_NAME";
const MANAGED_STUDIO_ATTESTATION_TIMEOUT: Duration = Duration::from_secs(120);

#[cfg(unix)]
fn desired_open_file_limit(_soft: u64, hard: u64) -> u64 {
	hard
}

#[cfg(unix)]
fn raise_open_file_limit() -> Result<()> {
	let mut limit = libc::rlimit {
		rlim_cur: 0,
		rlim_max: 0,
	};
	// SAFETY: `limit` points to valid writable storage for one `rlimit`.
	let status = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
	if status != 0 {
		return Err(io::Error::last_os_error()).context("failed to read the process open-file limit");
	}
	let desired = desired_open_file_limit(limit.rlim_cur, limit.rlim_max);
	if desired == limit.rlim_cur {
		return Ok(());
	}
	let raised = libc::rlimit {
		rlim_cur: desired,
		rlim_max: limit.rlim_max,
	};
	// SAFETY: `raised` is initialized and preserves the kernel-provided hard
	// limit while changing only the soft limit.
	let status = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) };
	if status != 0 {
		return Err(io::Error::last_os_error()).with_context(|| {
			format!(
				"failed to raise the process open-file soft limit from {} to {}",
				limit.rlim_cur, desired
			)
		});
	}
	Ok(())
}

#[cfg(not(unix))]
fn raise_open_file_limit() -> Result<()> {
	Ok(())
}

fn bind_serve_listener(requested: Option<u16>, preferred: u16, scan: bool) -> Result<(TcpListener, u16)> {
	fn bind(port: u16) -> io::Result<(TcpListener, u16)> {
		let listener = TcpListener::bind((SERVE_HOST, port))?;
		let selected = listener.local_addr()?.port();
		Ok((listener, selected))
	}

	if let Some(port) = requested {
		return bind(port).map_err(|error| anyhow::anyhow!("port {port} is already in use on loopback: {error}"));
	}
	match bind(preferred) {
		Ok(listener) => return Ok(listener),
		Err(error) if error.kind() == io::ErrorKind::AddrInUse && scan => {}
		Err(error) => {
			return Err(anyhow::anyhow!(
				"could not bind preferred loopback port {preferred}: {error}"
			))
		}
	}
	for port in preferred.saturating_add(1)..=u16::MAX {
		match bind(port) {
			Ok(listener) => return Ok(listener),
			Err(error) if error.kind() == io::ErrorKind::AddrInUse => {}
			Err(error) => return Err(error).with_context(|| format!("could not bind loopback port {port}")),
		}
	}
	anyhow::bail!("no loopback port is available at or above {preferred}")
}

fn temporary_build_path() -> Result<PathBuf> {
	if let Some(name) = std::env::var_os(QUALIFICATION_STUDIO_NAME) {
		let name = PathBuf::from(name);
		ensure!(
			name.file_name() == Some(name.as_os_str()) && name.extension().is_some_and(|extension| extension == "rbxl"),
			"{QUALIFICATION_STUDIO_NAME} must be a plain .rbxl file name"
		);
		return Ok(std::env::temp_dir().join(name));
	}
	Ok(std::env::temp_dir().join(format!("carbon-serve-{}.rbxl", uuid::Uuid::new_v4().simple())))
}

fn build_managed_place(
	project_path: &Path,
	build_path: &Path,
	contract: &artifact_store::WorktreeContract,
) -> Result<artifact_store::CompileReport> {
	project::compile(project_path, build_path, Some(contract))
}

fn launch_disposable_managed_place<T>(build_path: &Path, launch: impl FnOnce() -> Result<T>) -> Result<T> {
	match launch() {
		Ok(studio) => Ok(studio),
		Err(error) => {
			let _ = fs::remove_file(build_path);
			Err(error).context("failed to launch the managed Roblox Studio place")
		}
	}
}

#[derive(Clone)]
struct ServeCleanupPaths {
	build: PathBuf,
	composites: Arc<Mutex<Vec<PathBuf>>>,
}

impl ServeCleanupPaths {
	fn new(build: PathBuf, composite: PathBuf) -> Self {
		Self {
			build,
			composites: Arc::new(Mutex::new(if composite.as_os_str().is_empty() {
				Vec::new()
			} else {
				vec![composite]
			})),
		}
	}

	fn register_composite(&self, composite: PathBuf) {
		let mut composites = self.composites.lock();
		if !composites.contains(&composite) {
			composites.push(composite);
		}
	}

	fn clean(&self) {
		let _ = fs::remove_file(&self.build);
		for composite in self.composites.lock().clone() {
			let _ = fs::remove_dir_all(composite);
		}
	}
}

fn clean(paths: &ServeCleanupPaths, session: Option<&Session>, managed_studio: Option<&studio::ManagedStudio>) {
	if let Some(managed_studio) = managed_studio {
		if let Err(error) = managed_studio.stop() {
			crate::carbon_error!(
				"failed to stop {}-managed Studio PID {}: {error:#}",
				managed_studio.owner(),
				managed_studio.process_id()
			);
		}
	}
	if let Some(session) = session {
		let _ = sessions::remove(session);
	}
	paths.clean();
}

fn capture_before_shutdown(core: &Arc<Core>) -> Result<()> {
	if core.has_pending_managed_reload() {
		crate::carbon_info!(
			"End signal received during synchronization reload; retaining the latest committed manifest"
		);
		return Ok(());
	}
	crate::carbon_info!("End signal received; capturing the connected Studio place before shutdown");
	let message = core.capture_before_shutdown()?;
	crate::carbon_info!("Automatic Capture Manifest completed: {}", message.bold());
	Ok(())
}

fn capture_then_close_managed_studio(
	capture: impl FnOnce() -> Result<()>,
	close: impl FnOnce() -> Result<()>,
) -> Result<()> {
	let capture_result = capture();
	let close_result = close();
	match (capture_result, close_result) {
		(Ok(()), Ok(())) => Ok(()),
		(Err(error), Ok(())) => Err(error),
		(Ok(()), Err(error)) => Err(error).context("failed to close managed Studio after final capture"),
		(Err(capture_error), Err(close_error)) => Err(capture_error).context(format!(
			"managed Studio close also failed after final capture failure: {close_error:#}"
		)),
	}
}

enum PluginAttestation {
	Connected(Option<String>),
	Stopped,
}

fn wait_for_plugin_attestation(
	receiver: &mpsc::Receiver<Option<String>>,
	stopping: &AtomicBool,
	timeout: Duration,
) -> Result<PluginAttestation> {
	let deadline = Instant::now() + timeout;
	loop {
		if stopping.load(Ordering::Acquire) {
			return Ok(PluginAttestation::Stopped);
		}
		let remaining = deadline.saturating_duration_since(Instant::now());
		ensure!(
			!remaining.is_zero(),
			"managed Studio did not attest through the Carbon plugin within {} seconds",
			timeout.as_secs_f64()
		);
		match receiver.recv_timeout(remaining.min(Duration::from_millis(100))) {
			Ok(instance_id) => return Ok(PluginAttestation::Connected(instance_id)),
			Err(mpsc::RecvTimeoutError::Timeout) => {}
			Err(mpsc::RecvTimeoutError::Disconnected) if stopping.load(Ordering::Acquire) => {
				return Ok(PluginAttestation::Stopped);
			}
			Err(mpsc::RecvTimeoutError::Disconnected) => {
				anyhow::bail!("managed Studio Carbon plugin attestation channel disconnected")
			}
		}
	}
}

fn await_plugin_attestation(
	receiver: &mpsc::Receiver<Option<String>>,
	stopping: &AtomicBool,
	timeout: Duration,
	close: impl FnOnce() -> Result<()>,
) -> Result<PluginAttestation> {
	match wait_for_plugin_attestation(receiver, stopping, timeout) {
		Ok(attestation) => Ok(attestation),
		Err(error) => match close() {
			Ok(()) => Err(error.context("broker cleanup completed after Carbon plugin attestation failure")),
			Err(cleanup_error) => Err(error.context(format!(
				"broker cleanup also failed after Carbon plugin attestation failure: {cleanup_error:#}"
			))),
		},
	}
}

fn persist_served_studio_domain(project_path: &Path, cleanup_paths: &ServeCleanupPaths) -> Result<()> {
	let materialized = project::materialize(project_path).context("failed to materialize served project topology")?;
	cleanup_paths.register_composite(materialized.directory.clone());
	let policy = project::live_policy(project_path, &materialized);
	project::persist_studio_domain(&policy).context("failed to prune mapping barriers before serve startup")
}

fn prepare_served_core(
	project_path: &Path,
	worktree_id: &str,
	session_token: &str,
	control_sender: &server::ServeControlSender,
	cleanup_paths: &ServeCleanupPaths,
) -> Result<Arc<Core>> {
	let materialized = project::materialize(project_path).context("failed to materialize served project topology")?;
	let project_name = materialized.name.clone();
	let composite_directory = materialized.directory.clone();
	cleanup_paths.register_composite(composite_directory.clone());
	let core = Arc::new(Core::new_project_with_worktree_and_control(
		project_path,
		&materialized,
		(project_name, worktree_id.to_owned(), session_token.to_owned()),
		Some(control_sender.clone()),
	)?);
	core.register_ephemeral_path(composite_directory);
	core.register_served_place(cleanup_paths.build.clone());
	Ok(core)
}

/// Build and launch a managed place, then serve its frozen project topology.
/// SIGINT, SIGTERM, and SIGHUP capture the connected Studio place before exit.
#[derive(Parser)]
pub struct Serve {
	/// Explicit `*.carbon.json` project, or a directory containing exactly one.
	#[arg()]
	source: Option<PathBuf>,

	/// Loopback plugin endpoint port.
	#[arg(short = 'P', long)]
	port: Option<u16>,
}

impl Serve {
	pub fn main(self) -> Result<()> {
		raise_open_file_limit().context("Carbon serve cannot guarantee Capture Manifest capacity")?;
		let project_path = source::resolve(self.source.unwrap_or_default())?;
		let worktree = sessions::detect_worktree(&project_path)?;
		Config::load_workspace(project_path.get_parent());
		let (preferred_port, scan_ports, studio_desktop) = {
			let config = Config::new();
			(config.port, config.scan_ports, config.studio_desktop.clone())
		};
		let inspection = project::inspect(&project_path)?;
		ensure!(inspection.is_place(), "serve requires a complete place project");
		if artifact_resolution::configure_repository(&project_path)? {
			crate::carbon_info!(
				"Configured local Git semantic merges for {}",
				project_path.to_string().bold()
			);
		}

		let (listener, port) = bind_serve_listener(self.port, preferred_port, scan_ports)?;
		let endpoint = server::format_address(SERVE_HOST, port);
		let worktree_id = uuid::Uuid::new_v4().simple().to_string();
		let session_token = uuid::Uuid::new_v4().simple().to_string();
		let contract = artifact_store::WorktreeContract {
			endpoint: endpoint.clone(),
			project: inspection.name.clone(),
			worktree_id: worktree_id.clone(),
			session_token: session_token.clone(),
			identity_exclusions: Default::default(),
		};

		let build_path = temporary_build_path()?;
		let cleanup_paths = ServeCleanupPaths::new(build_path.clone(), PathBuf::new());
		persist_served_studio_domain(&project_path, &cleanup_paths)?;
		crate::carbon_info!("Building managed place from {}", project_path.to_string().bold());
		let report = match build_managed_place(&project_path, &build_path, &contract) {
			Ok(report) => report,
			Err(error) => {
				let _ = fs::remove_file(&build_path);
				return Err(error).context("failed to build disposable managed place");
			}
		};
		crate::carbon_info!(
			"Built managed place with {} instances and {} properties",
			report.instances.to_string().bold(),
			report.properties.to_string().bold()
		);
		crate::carbon_info!("Launching Roblox Studio");
		let studio_dir = &util::get_reflection_snapshot().studio_dir;
		let managed_studio = launch_disposable_managed_place(&build_path, || {
			studio::launch_managed(build_path.clone(), studio_dir, &studio_desktop)
		})?;
		let studio_process_id = managed_studio.process_id();
		crate::carbon_info!(
			"Waiting for Roblox Studio to connect on {} (launch ID: {}, Studio PID {}, lifecycle: {})",
			endpoint.bold(),
			managed_studio.launch_id().bold(),
			studio_process_id,
			managed_studio.owner()
		);

		let (control_sender, control_receiver) = server::serve_control_channel();
		let mut core = match prepare_served_core(
			&project_path,
			&worktree_id,
			&session_token,
			&control_sender,
			&cleanup_paths,
		) {
			Ok(generation) => generation,
			Err(error) => {
				clean(&cleanup_paths, None, Some(&managed_studio));
				return Err(error);
			}
		};
		let (studio_executable, creation_filetime) = match managed_studio.focus_metadata() {
			Some(meta) => (Some(meta.studio_executable), Some(meta.creation_filetime)),
			None => (None, None),
		};
		let session = Session {
			pid: process::id(),
			host: Some(SERVE_HOST.to_owned()),
			port: Some(port),
			studio_pid: Some(studio_process_id),
			worktree,
			studio_executable,
			creation_filetime,
			launch_id: Some(managed_studio.launch_id().to_owned()),
		};
		if let Err(error) = sessions::add(Some(managed_studio.launch_id().to_owned()), session.clone(), true) {
			clean(&cleanup_paths, None, Some(&managed_studio));
			return Err(error).context("failed to register managed Studio launch");
		}
		let end_signal_received = Arc::new(AtomicBool::new(false));
		let handler_end_signal_received = Arc::clone(&end_signal_received);
		let handler_control = control_sender.clone();
		let force_cleanup_paths = cleanup_paths.clone();
		let force_session = session.clone();
		let force_studio = managed_studio.clone();
		ctrlc::set_handler(move || {
			if handler_end_signal_received.swap(true, Ordering::AcqRel) {
				clean(&force_cleanup_paths, Some(&force_session), Some(&force_studio));
				process::exit(130);
			}
			let _ = handler_control.try_send(server::ServeControl::Shutdown);
		})?;

		let ready_endpoint = endpoint.clone();
		let ready_project_path = project_path.clone();
		let ready_studio = managed_studio.clone();
		let ready_session = session.clone();
		let (ready_sender, ready_receiver) = mpsc::sync_channel::<Option<String>>(1);
		let readiness_stopping = Arc::new(AtomicBool::new(false));
		let worker_stopping = Arc::clone(&readiness_stopping);
		let readiness_failure = Arc::new(Mutex::new(None::<String>));
		let worker_failure = Arc::clone(&readiness_failure);
		let final_instance_id = Arc::new(Mutex::new(None::<String>));
		let shutdown_failure = Arc::new(Mutex::new(None::<String>));
		let worker_final_instance_id = Arc::clone(&final_instance_id);
		let readiness_control = control_sender.clone();
		let readiness_worker = thread::spawn(move || {
			let subscribed_instance_id = match await_plugin_attestation(
				&ready_receiver,
				&worker_stopping,
				MANAGED_STUDIO_ATTESTATION_TIMEOUT,
				|| ready_studio.stop(),
			) {
				Ok(PluginAttestation::Connected(instance_id)) => instance_id,
				Ok(PluginAttestation::Stopped) => return,
				Err(error) => {
					let message = format!(
						"Studio launch {} could not establish Carbon plugin attestation: {error:#}",
						ready_studio.launch_id()
					);
					*worker_failure.lock() = Some(message.clone());
					crate::carbon_error!("{message}");
					let _ = readiness_control.try_send(server::ServeControl::Abort);
					return;
				}
			};
			if worker_stopping.load(Ordering::Acquire) {
				return;
			}
			match ready_studio.establish_instance_id() {
				Ok(instance_id) => {
					if worker_stopping.load(Ordering::Acquire) {
						return;
					}
					if let Err(error) = sessions::replace_id(&ready_session, instance_id.clone()) {
						let message = format!("failed to register final Studio instance ID {instance_id}: {error:#}");
						*worker_failure.lock() = Some(message.clone());
						crate::carbon_error!("{message}");
						let _ = readiness_control.try_send(server::ServeControl::Abort);
						return;
					}
					*worker_final_instance_id.lock() = Some(instance_id.clone());
					if subscribed_instance_id
						.as_deref()
						.is_some_and(|subscribed| subscribed != instance_id)
					{
						log::debug!(
							"Carbon plugin initially subscribed with provisional route {}; readiness waited for broker instance {}",
							subscribed_instance_id.as_deref().unwrap_or_default(),
							instance_id
						);
					}
					crate::carbon_info!(
						"Studio connected. Serving {} instances and {} properties on {}, instance ID: {}, launch ID: {}, project: {} (Studio PID {}, lifecycle: {})",
						report.instances,
						report.properties,
						ready_endpoint.bold(),
						instance_id.bold(),
						ready_studio.launch_id().bold(),
						ready_project_path.to_string().bold(),
						studio_process_id,
						ready_studio.owner()
					);
				}
				Err(error) if !worker_stopping.load(Ordering::Acquire) => {
					let message = format!(
						"Studio launch {} could not establish a final robloxstudio-mcp instance ID: {error:#}",
						ready_studio.launch_id()
					);
					*worker_failure.lock() = Some(message.clone());
					crate::carbon_error!("{message}");
					let _ = readiness_control.try_send(server::ServeControl::Abort);
				}
				Err(_) => {}
			}
		});
		core.queue().on_first_subscribe(move |_, route| {
			let _ = ready_sender.try_send(route.map(|route| route.instance_id.clone()));
		});
		let result = loop {
			let refreshed_final_instance_id = Arc::clone(&final_instance_id);
			let refreshed_failure = Arc::clone(&readiness_failure);
			let refreshed_control = control_sender.clone();
			core.queue().on_studio_route_refresh(move |route| {
				if refreshed_final_instance_id
					.lock()
					.as_deref()
					.is_some_and(|instance_id| instance_id != route.instance_id)
				{
					let message = format!(
						"connected Studio changed its MCP route to {} after managed lifecycle completion",
						route.instance_id
					);
					*refreshed_failure.lock() = Some(message.clone());
					crate::carbon_error!("{message}");
					let _ = refreshed_control.try_send(server::ServeControl::Abort);
				}
			});
			let prepared = Arc::new(Mutex::new(None));
			let prepared_result = Arc::clone(&prepared);
			let active_core = Arc::clone(&core);
			let shutdown_studio = managed_studio.clone();
			let active_shutdown_failure = Arc::clone(&shutdown_failure);
			let reload_project = project_path.clone();
			let reload_worktree = worktree_id.clone();
			let reload_session = session_token.clone();
			let reload_control = control_sender.clone();
			let reload_cleanup_paths = cleanup_paths.clone();
			let active_server = Server::new(Arc::clone(&core), SERVE_HOST, port);
			let reload_stop_requested = active_server.external_stop_signal();
			let server_result = active_server.start_with_listener_control(
				listener.try_clone().context("failed to clone the serve listener")?,
				control_receiver.clone(),
				move |control| match control {
					server::ServeControl::Abort => true,
					server::ServeControl::Shutdown => {
						if let Err(error) = capture_then_close_managed_studio(
							|| capture_before_shutdown(&active_core),
							|| shutdown_studio.stop(),
						) {
							let message = format!("managed Studio shutdown failed: {error:#}");
							*active_shutdown_failure.lock() = Some(message.clone());
							crate::carbon_error!("{message}");
						}
						true
					}
					server::ServeControl::Reload => {
						if reload_stop_requested.load(Ordering::Acquire) {
							return true;
						}
						if active_core.has_pending_managed_reload() {
							crate::carbon_info!(
								"Project manifest changed again; waiting for Studio to finish the active synchronization reload"
							);
							let wait_started = std::time::Instant::now();
							while active_core.has_pending_managed_reload() {
								if reload_stop_requested.load(Ordering::Acquire) {
									return true;
								}
								if wait_started.elapsed() >= std::time::Duration::from_secs(300) {
									crate::carbon_error!(
										"Studio did not acknowledge the active synchronization reload; retaining its candidate generation"
									);
									reload_control.fail_reload(false);
									return false;
								}
								thread::sleep(std::time::Duration::from_millis(25));
							}
						}
						crate::carbon_info!("Project manifest changed; capturing Studio before synchronization reload");
						if let Err(error) = active_core.capture_before_reload() {
							crate::carbon_error!("Capture Manifest before synchronization reload failed: {error:#}");
							reload_control.fail_reload(true);
							return false;
						}
						if let Err(error) = persist_served_studio_domain(&reload_project, &reload_cleanup_paths) {
							crate::carbon_error!(
								"Could not persist Studio state before synchronization reload; retaining active generation: {error:#}"
							);
							reload_control.fail_reload(false);
							return false;
						}
						let next_core =
							match prepare_served_core(
								&reload_project,
								&reload_worktree,
								&reload_session,
								&reload_control,
								&reload_cleanup_paths,
							) {
								Ok(generation) => generation,
								Err(error) => {
									crate::carbon_error!("Could not prepare synchronization reload; retaining active generation: {error:#}");
									reload_control.fail_reload(false);
									return false;
								}
							};
						if reload_stop_requested.load(Ordering::Acquire) {
							return true;
						}
						let transition_id = uuid::Uuid::new_v4().simple().to_string();
						if let Err(error) = next_core.begin_managed_reload_transition(transition_id.clone()) {
							crate::carbon_error!("Could not prepare managed synchronization reload: {error:#}");
							reload_control.fail_reload(false);
							return false;
						}
						let reload_listener = match active_core.queue().single_listener_id() {
							Ok(listener) => listener,
							Err(error) => {
								crate::carbon_error!("Could not announce managed synchronization reload: {error:#}");
								reload_control.fail_reload(true);
								return false;
							}
						};
						active_core.stop_automatic_capture_monitor();
						if let Err(error) = active_core.queue().push(
							server::Message::ManagedReload(server::ManagedReload {
								transition_id: transition_id.clone(),
								project_name: next_core.name().to_owned(),
								source_generation: next_core.source_generation(),
								worktree_id: reload_worktree.clone(),
								session_token: reload_session.clone(),
							}),
							Some(reload_listener),
						) {
							crate::carbon_error!("Could not announce managed synchronization reload: {error:#}");
							if let Err(restart_error) = active_core.start_automatic_capture_monitor() {
								crate::carbon_error!(
									"Could not resume automatic Studio auto-recovery capture: {restart_error:#}"
								);
							}
							reload_control.fail_reload(true);
							return false;
						}
						*prepared_result.lock() = Some(next_core);
						reload_control.acknowledge_reload();
						true
					}
				},
			);
			if active_server.external_stop_requested() {
				prepared.lock().take();
				break server_result;
			}
			if let Some(next_core) = prepared.lock().take() {
				core = next_core;
				crate::carbon_info!(
					"Synchronization reload candidate is ready on {}; waiting for Studio to apply and prove the replacement topology",
					endpoint.bold()
				);
				continue;
			}
			break server_result;
		};
		core.stop_automatic_capture_monitor();
		readiness_stopping.store(true, Ordering::Release);
		let _ = readiness_worker.join();
		clean(&cleanup_paths, Some(&session), Some(&managed_studio));
		if let Some(error) = readiness_failure.lock().take() {
			anyhow::bail!(error);
		}
		if let Some(error) = shutdown_failure.lock().take() {
			anyhow::bail!(error);
		}
		if end_signal_received.load(Ordering::Acquire) {
			process::exit(130);
		}
		result.context("serve endpoint failed")
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::cell::Cell;

	#[cfg(unix)]
	#[test]
	fn merge_stress_regression_serve_raises_soft_open_file_limit_to_hard_limit() {
		const CHILD_ENV: &str = "CARBON_TEST_LOW_NOFILE_CHILD";
		if std::env::var_os(CHILD_ENV).is_some() {
			let mut original = libc::rlimit {
				rlim_cur: 0,
				rlim_max: 0,
			};
			// SAFETY: `original` is valid writable storage for one `rlimit`.
			assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut original) }, 0);
			assert!(
				original.rlim_max > 1024,
				"test host hard open-file limit is too low to exercise raising the soft limit"
			);
			let lowered = libc::rlimit {
				rlim_cur: 1024,
				rlim_max: original.rlim_max,
			};
			// SAFETY: the child process preserves its hard limit and lowers only
			// its own soft limit.
			assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lowered) }, 0);

			raise_open_file_limit().unwrap();

			let mut raised = libc::rlimit {
				rlim_cur: 0,
				rlim_max: 0,
			};
			// SAFETY: `raised` is valid writable storage for one `rlimit`.
			assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut raised) }, 0);
			assert_eq!(raised.rlim_cur, original.rlim_max);
			return;
		}

		let status = std::process::Command::new(std::env::current_exe().unwrap())
			.args([
				"--exact",
				"cli::serve::tests::merge_stress_regression_serve_raises_soft_open_file_limit_to_hard_limit",
				"--nocapture",
			])
			.env(CHILD_ENV, "1")
			.status()
			.unwrap();
		assert!(status.success(), "low-open-file-limit child regression failed");
	}

	#[test]
	fn managed_launch_failure_removes_the_disposable_build() {
		let build = std::env::temp_dir().join(format!("carbon-managed-launch-test-{}.rbxl", uuid::Uuid::new_v4()));
		fs::write(&build, b"disposable managed place").unwrap();

		let error =
			launch_disposable_managed_place::<()>(&build, || anyhow::bail!("synthetic broker failure")).unwrap_err();

		assert!(!build.exists());
		assert_eq!(
			format!("{error:#}"),
			"failed to launch the managed Roblox Studio place: synthetic broker failure"
		);
	}

	#[test]
	fn forced_cleanup_removes_the_current_reloaded_composite() {
		let temp = tempfile::tempdir().unwrap();
		let build = temp.path().join("launch.rbxl");
		let initial = temp.path().join(".carbon-composite-initial");
		let reloaded = temp.path().join(".carbon-composite-reloaded");
		fs::write(&build, b"managed launch").unwrap();
		fs::create_dir(&initial).unwrap();
		fs::create_dir(&reloaded).unwrap();
		let paths = ServeCleanupPaths::new(build.clone(), initial.clone());

		paths.register_composite(reloaded.clone());
		paths.clean();

		assert!(!build.exists());
		assert!(!reloaded.exists());
		assert!(!initial.exists());
	}

	#[test]
	fn parallel_serve_uses_automatic_port_selection_by_default() {
		let parsed = Serve::try_parse_from(["serve"]);
		assert!(parsed.is_ok());
		assert_eq!(parsed.unwrap().port, None);
	}

	#[test]
	fn parallel_serve_leases_another_port_when_the_preferred_port_is_busy() {
		let occupied = TcpListener::bind((SERVE_HOST, 0)).unwrap();
		let preferred = occupied.local_addr().unwrap().port();
		assert_ne!(preferred, u16::MAX, "the fixture needs room to scan upward");

		let (leased, port) = bind_serve_listener(None, preferred, true).unwrap();

		assert_ne!(port, preferred);
		assert_eq!(leased.local_addr().unwrap().port(), port);
	}

	#[test]
	fn parallel_serve_keeps_explicit_ports_strict() {
		let occupied = TcpListener::bind((SERVE_HOST, 0)).unwrap();
		let port = occupied.local_addr().unwrap().port();

		let error = bind_serve_listener(Some(port), 8000, true).unwrap_err();

		assert!(error.to_string().contains(&format!("port {port} is already in use")));
	}

	#[test]
	fn disposable_builds_are_unique_places() {
		assert_ne!(temporary_build_path().unwrap(), temporary_build_path().unwrap());
	}

	#[test]
	fn repeated_managed_launch_build_uses_the_project_cache() {
		let root = std::env::temp_dir().join(format!("carbon-serve-cache-test-{}", uuid::Uuid::new_v4().simple()));
		fs::create_dir_all(&root).unwrap();
		let project_path = root.join("game.carbon.json");
		project::initialize(&project_path, "ServeCache".to_owned()).unwrap();
		let first_output = root.join("first.rbxl");
		let second_output = root.join("second.rbxl");
		let first_contract = artifact_store::WorktreeContract {
			endpoint: "http://127.0.0.1:8000".to_owned(),
			project: "ServeCache".to_owned(),
			worktree_id: "serve-cache-first".to_owned(),
			session_token: "serve-cache-first".to_owned(),
			identity_exclusions: Default::default(),
		};
		build_managed_place(&project_path, &first_output, &first_contract).unwrap();

		let second_contract = artifact_store::WorktreeContract {
			worktree_id: "serve-cache-second".to_owned(),
			session_token: "serve-cache-second".to_owned(),
			..first_contract
		};
		let (result, tree_loads) =
			artifact_store::count_tree_loads(|| build_managed_place(&project_path, &second_output, &second_contract));
		result.unwrap();
		assert_eq!(
			tree_loads, 0,
			"a repeated serve build must rewrite only the launch contract from the validated cache"
		);
		assert_ne!(
			fs::read(&first_output).unwrap(),
			fs::read(&second_output).unwrap(),
			"the repeated build reused stale launch credentials"
		);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn qualification_launch_names_must_be_plain_rbxl_files() {
		for invalid in ["nested/place.rbxl", "place.rbxm", "../place.rbxl"] {
			let name = PathBuf::from(invalid);
			assert!(
				name.file_name() != Some(name.as_os_str())
					|| name.extension().is_none_or(|extension| extension != "rbxl")
			);
		}
	}

	#[test]
	fn end_signal_captures_before_shutdown() {
		let begin_calls = Cell::new(0);
		let poll_calls = Cell::new(0);
		let wait_calls = Cell::new(0);
		let message = crate::core::wait_for_automatic_capture(
			|| {
				begin_calls.set(begin_calls.get() + 1);
				Ok(crate::core::ManifestCaptureStatus {
					request_id: "automatic-capture".to_owned(),
					state: "running".to_owned(),
					source_generation: "initial-generation".to_owned(),
					message: None,
				})
			},
			|request_id| {
				assert_eq!(request_id, "automatic-capture");
				let poll = poll_calls.get() + 1;
				poll_calls.set(poll);
				Ok(crate::core::ManifestCaptureStatus {
					request_id: request_id.to_owned(),
					state: if poll == 2 { "complete" } else { "running" }.to_owned(),
					source_generation: "captured-generation".to_owned(),
					message: (poll == 2).then(|| "manifest committed".to_owned()),
				})
			},
			|| wait_calls.set(wait_calls.get() + 1),
		)
		.unwrap();

		assert_eq!(message, "manifest committed");
		assert_eq!(begin_calls.get(), 1);
		assert_eq!(poll_calls.get(), 2);
		assert_eq!(wait_calls.get(), 2);
	}

	#[test]
	fn recovery_aware_shutdown_closes_managed_studio_after_capture_failure() {
		let events = std::cell::RefCell::new(Vec::new());
		let error = capture_then_close_managed_studio(
			|| {
				events.borrow_mut().push("capture");
				anyhow::bail!("synthetic recovery failure")
			},
			|| {
				events.borrow_mut().push("close");
				Ok(())
			},
		)
		.unwrap_err();

		assert_eq!(*events.borrow(), ["capture", "close"]);
		assert!(format!("{error:#}").contains("synthetic recovery failure"));
	}

	#[test]
	fn plugin_attestation_timeout_closes_the_managed_launch() {
		let (_sender, receiver) = mpsc::sync_channel(1);
		let stopping = AtomicBool::new(false);
		let closed = Cell::new(false);

		let error = await_plugin_attestation(&receiver, &stopping, std::time::Duration::from_millis(5), || {
			closed.set(true);
			Ok(())
		})
		.err()
		.expect("missing Carbon plugin attestation must fail startup");

		assert!(closed.get());
		assert!(format!("{error:#}").contains("did not attest"));
	}
}
