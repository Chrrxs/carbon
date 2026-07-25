use anyhow::{Context, Result};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use reqwest::blocking::Client;
use serde_json::{json, Value};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::io::{BufRead, BufReader, Write};
use std::{
	fmt, fs,
	path::{Path, PathBuf},
	process::{Child, Command, Stdio},
	sync::{Arc, Mutex},
	thread,
	time::{Duration, Instant},
};

use crate::{rml, studio_plugin};

#[cfg(target_os = "windows")]
use winsafe::{co::SW, EnumWindows};

const DEFAULT_MCP_URL: &str = "http://127.0.0.1:58741";
const MCP_URL_ENV: &str = "CARBON_STUDIO_MCP_URL";
const LIFECYCLE_ENV: &str = "CARBON_STUDIO_LIFECYCLE";
const MCP_PROTOCOL_VERSION: u64 = 3;
const MCP_PROBE_TIMEOUT: Duration = Duration::from_millis(750);
const MCP_LAUNCH_TIMEOUT: Duration = Duration::from_secs(120);
const MCP_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
enum ManagedStudioLifecycle {
	Direct {
		process_id: u32,
		data_model_name: String,
		studio_executable: String,
		started_at_file_time: u64,
	},
	Mcp {
		endpoint: String,
		auth_token: Option<String>,
		launch_id: String,
		process_id: u32,
		studio_executable: String,
		started_at_file_time: Option<u64>,
	},
}

#[derive(Clone)]
pub struct ManagedStudio {
	lifecycle: ManagedStudioLifecycle,
	startup_guard: Arc<Mutex<Option<studio_plugin::Installation>>>,
	bridge_id: Option<String>,
}

impl ManagedStudio {
	pub fn process_id(&self) -> u32 {
		match &self.lifecycle {
			ManagedStudioLifecycle::Direct { process_id, .. } | ManagedStudioLifecycle::Mcp { process_id, .. } => {
				*process_id
			}
		}
	}

	pub fn owner(&self) -> &'static str {
		match &self.lifecycle {
			ManagedStudioLifecycle::Direct { .. } => "Carbon",
			ManagedStudioLifecycle::Mcp { .. } => "robloxstudio-mcp",
		}
	}

	/// Release the shared plugin version once this Studio process has proved it
	/// loaded the plugin by subscribing to its unique serve endpoint.
	pub fn finish_startup(&self) {
		self.startup_guard.lock().unwrap().take();
	}

	pub fn wait_for_instance_id(&self) -> Result<Option<String>> {
		if let ManagedStudioLifecycle::Direct { data_model_name, .. } = &self.lifecycle {
			return resolve_direct_mcp_instance_id(
				data_model_name,
				Instant::now() + MCP_LIFECYCLE_TIMEOUT,
				query_mcp_health,
				|| thread::sleep(Duration::from_millis(100)),
			);
		}
		let ManagedStudioLifecycle::Mcp {
			endpoint,
			auth_token,
			launch_id,
			..
		} = &self.lifecycle
		else {
			return Ok(None);
		};
		resolve_mcp_instance_id(
			launch_id,
			Instant::now() + MCP_LIFECYCLE_TIMEOUT,
			|| {
				mcp_tool(
					endpoint,
					auth_token.as_deref(),
					&json!({"action": "status", "launch_id": launch_id}),
					MCP_LIFECYCLE_TIMEOUT,
				)
				.map_err(anyhow::Error::new)
			},
			|| thread::sleep(Duration::from_millis(100)),
		)
		.map(Some)
		.with_context(|| format!("robloxstudio-mcp could not resolve Studio launch {launch_id}"))
	}

	pub fn stop(&self) -> Result<()> {
		let stopped = match &self.lifecycle {
			ManagedStudioLifecycle::Direct {
				process_id,
				studio_executable,
				started_at_file_time,
				..
			} => terminate_process(*process_id, studio_executable, *started_at_file_time),
			ManagedStudioLifecycle::Mcp {
				endpoint,
				auth_token,
				launch_id,
				process_id,
				studio_executable,
				started_at_file_time,
			} => stop_mcp_owned_process(
				*process_id,
				started_at_file_time.map(|started_at| (studio_executable.as_str(), started_at)),
				|| {
					let result = mcp_tool(
						endpoint,
						auth_token.as_deref(),
						&json!({"action": "close", "launch_id": launch_id}),
						MCP_LIFECYCLE_TIMEOUT,
					)
					.with_context(|| format!("robloxstudio-mcp could not close Studio launch {launch_id}"))?;
					let close_status = result.get("close_status").and_then(Value::as_str);
					anyhow::ensure!(
						result.get("launch_id").and_then(Value::as_str) == Some(launch_id),
						"robloxstudio-mcp closed a different Studio launch than {launch_id}: {result}"
					);
					anyhow::ensure!(
						matches!(close_status, Some("closed" | "already_closed")),
						"robloxstudio-mcp returned an invalid close status for Studio launch {launch_id}: {result}"
					);
					anyhow::ensure!(
						result.get("process_running").and_then(Value::as_bool) == Some(false),
						"robloxstudio-mcp did not confirm that Studio launch {launch_id} stopped: {result}"
					);
					Ok(())
				},
				terminate_process,
			),
		};
		stopped?;
		if let Some(bridge_id) = &self.bridge_id {
			cleanup_stopped_bridge(self.process_id(), bridge_id)?;
		}
		Ok(())
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LifecyclePreference {
	Auto,
	Mcp,
	Direct,
}

impl LifecyclePreference {
	fn from_env() -> Result<Self> {
		match std::env::var(LIFECYCLE_ENV)
			.unwrap_or_else(|_| "auto".to_owned())
			.trim()
			.to_ascii_lowercase()
			.as_str()
		{
			"auto" => Ok(Self::Auto),
			"mcp" => Ok(Self::Mcp),
			"direct" => Ok(Self::Direct),
			value => anyhow::bail!("{LIFECYCLE_ENV} must be auto, mcp, or direct; received {value:?}"),
		}
	}
}

#[derive(Clone)]
struct McpLifecycle {
	endpoint: String,
	auth_token: Option<String>,
	preference: LifecyclePreference,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpRequestFailureStage {
	PreDispatch,
	DispatchAmbiguous,
	BrokerRejected,
	InvalidResponse,
}

#[derive(Debug)]
struct McpRequestFailure {
	endpoint: String,
	stage: McpRequestFailureStage,
	cause: anyhow::Error,
}

impl McpRequestFailure {
	fn new(endpoint: &str, stage: McpRequestFailureStage, cause: impl Into<anyhow::Error>) -> Self {
		Self {
			endpoint: endpoint.to_owned(),
			stage,
			cause: cause.into(),
		}
	}
}

impl fmt::Display for McpRequestFailure {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self.stage {
			McpRequestFailureStage::PreDispatch => write!(
				formatter,
				"robloxstudio-mcp lifecycle request to {} failed before request dispatch",
				self.endpoint
			),
			McpRequestFailureStage::DispatchAmbiguous => write!(
				formatter,
				"robloxstudio-mcp lifecycle request to {} failed after dispatch or with ambiguous dispatch",
				self.endpoint
			),
			McpRequestFailureStage::BrokerRejected => write!(
				formatter,
				"robloxstudio-mcp at {} rejected the lifecycle request after request dispatch",
				self.endpoint
			),
			McpRequestFailureStage::InvalidResponse => write!(
				formatter,
				"robloxstudio-mcp at {} returned an invalid lifecycle response after request dispatch",
				self.endpoint
			),
		}
	}
}

impl std::error::Error for McpRequestFailure {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(self.cause.as_ref())
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManagedLaunchFailureStage {
	PreDispatch,
	DispatchAmbiguous,
	BrokerRejected,
	InvalidResponse,
}

#[derive(Debug)]
struct ManagedLaunchFailure {
	endpoint: String,
	stage: ManagedLaunchFailureStage,
	cause: anyhow::Error,
}

impl ManagedLaunchFailure {
	fn pre_dispatch(endpoint: &str, cause: anyhow::Error) -> Self {
		Self {
			endpoint: endpoint.to_owned(),
			stage: ManagedLaunchFailureStage::PreDispatch,
			cause,
		}
	}

	fn from_request(error: McpRequestFailure) -> Self {
		let stage = match error.stage {
			McpRequestFailureStage::PreDispatch => ManagedLaunchFailureStage::PreDispatch,
			McpRequestFailureStage::DispatchAmbiguous => ManagedLaunchFailureStage::DispatchAmbiguous,
			McpRequestFailureStage::BrokerRejected => ManagedLaunchFailureStage::BrokerRejected,
			McpRequestFailureStage::InvalidResponse => ManagedLaunchFailureStage::InvalidResponse,
		};
		Self {
			endpoint: error.endpoint.clone(),
			stage,
			cause: anyhow::Error::new(error),
		}
	}

	fn invalid_response(endpoint: &str, cause: anyhow::Error) -> Self {
		Self {
			endpoint: endpoint.to_owned(),
			stage: ManagedLaunchFailureStage::InvalidResponse,
			cause,
		}
	}

	fn safe_for_direct_fallback(&self, preference: LifecyclePreference) -> bool {
		preference == LifecyclePreference::Auto && self.stage == ManagedLaunchFailureStage::PreDispatch
	}
}

impl fmt::Display for ManagedLaunchFailure {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self.stage {
			ManagedLaunchFailureStage::PreDispatch => write!(
				formatter,
				"robloxstudio-mcp was selected at {}, but the managed launch failed before request dispatch; no launch request was sent, so direct launch is safe. For an explicit next attempt, run: {LIFECYCLE_ENV}=direct carbon serve",
				self.endpoint
			),
			ManagedLaunchFailureStage::DispatchAmbiguous => write!(
				formatter,
				"robloxstudio-mcp was selected at {}, but the managed launch request may have been dispatched; automatic direct fallback was withheld to avoid a duplicate or unowned Studio process. Inspect or restart the lifecycle broker, or explicitly choose direct lifecycle on a subsequent attempt: {LIFECYCLE_ENV}=direct carbon serve",
				self.endpoint
			),
			ManagedLaunchFailureStage::BrokerRejected => write!(
				formatter,
				"robloxstudio-mcp at {} rejected the managed launch after request dispatch; automatic direct fallback was withheld because launch ownership remains with the broker. Inspect or restart the lifecycle broker, or explicitly choose direct lifecycle on a subsequent attempt: {LIFECYCLE_ENV}=direct carbon serve",
				self.endpoint
			),
			ManagedLaunchFailureStage::InvalidResponse => write!(
				formatter,
				"robloxstudio-mcp at {} returned an invalid managed-launch response after request dispatch; launch ownership is ambiguous, so automatic direct fallback was withheld. Inspect or restart the lifecycle broker, or explicitly choose direct lifecycle on a subsequent attempt: {LIFECYCLE_ENV}=direct carbon serve",
				self.endpoint
			),
		}
	}
}

impl std::error::Error for ManagedLaunchFailure {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(self.cause.as_ref())
	}
}

pub fn launch(path: Option<PathBuf>) -> Result<Option<u32>> {
	let require_engine_ready = path.is_some();
	let (rml_launch, _plugin_launch) = prepare_launch(None)?;
	let loader_path = rml_launch.loader_path().to_owned();
	let (studio_executable, _) = mcp_launch_paths(&rml_launch)?;
	let Some(mut launched) = launch_prepared(path, rml_launch, false)? else {
		return Ok(None);
	};
	let process_id = launched.process_id();
	let started_at_file_time = match launched.started_at_file_time() {
		Ok(value) => value,
		Err(error) => return Err(abort_direct_launch(&mut launched, error)),
	};
	launched.set_identity(studio_executable.clone(), started_at_file_time);
	let mut bridge_id = None;
	let startup = (|| {
		rml::inject_loader(
			process_id,
			&loader_path,
			&studio_executable,
			started_at_file_time,
			|| launched.resume_for_injection(),
		)
		.with_context(|| format!("failed to load Carbon RML into Roblox Studio process {process_id}"))?;
		let loaded_bridge_id = attest_rml_bridge(process_id, &studio_executable, started_at_file_time, false)?;
		bridge_id = Some(loaded_bridge_id.clone());
		if require_engine_ready {
			let ready_bridge_id = attest_rml_bridge(process_id, &studio_executable, started_at_file_time, true)?;
			anyhow::ensure!(
				ready_bridge_id == loaded_bridge_id,
				"RML bridge identity changed while Roblox Studio initialized"
			);
		}
		launched.finish()?;
		Ok::<(), anyhow::Error>(())
	})()
	.context("Carbon RML did not start with the launched Roblox Studio process");
	if let Err(error) = startup {
		return Err(abort_direct_launch_with_bridge(
			&mut launched,
			error,
			bridge_id.as_deref(),
		));
	}

	Ok(Some(process_id))
}

fn prepare_direct_managed_launch(
	path: PathBuf,
	rml_launch: rml::Launch,
	studio_executable: &str,
) -> Result<(ManagedStudioLifecycle, DirectLaunch)> {
	let data_model_name = path
		.file_name()
		.context("managed Studio place path does not have a file name")?
		.to_string_lossy()
		.into_owned();
	let mut launched =
		launch_prepared(Some(path), rml_launch, true)?.context("Studio launcher did not return a process ID")?;
	let process_id = launched.process_id();
	let started_at_file_time = match launched.started_at_file_time() {
		Ok(value) => value,
		Err(error) => return Err(abort_direct_launch(&mut launched, error)),
	};
	launched.set_identity(studio_executable.to_owned(), started_at_file_time);
	Ok((
		ManagedStudioLifecycle::Direct {
			process_id,
			data_model_name,
			studio_executable: studio_executable.to_owned(),
			started_at_file_time,
		},
		launched,
	))
}

fn prefer_suspended_direct_lifecycle(preference: LifecyclePreference) -> bool {
	cfg!(target_os = "linux") && preference == LifecyclePreference::Auto
}

pub fn launch_managed(path: PathBuf, studio_dir: &Path) -> Result<ManagedStudio> {
	let (rml_launch, plugin_launch) = prepare_launch(Some(studio_dir))?;
	let loader_path = rml_launch.loader_path().to_owned();
	let (studio_executable, _) = mcp_launch_paths(&rml_launch)?;
	let mut direct_launch = None;
	let preference = LifecyclePreference::from_env()?;
	let mcp_lifecycle = if prefer_suspended_direct_lifecycle(preference) {
		log::debug!(
			"Selected suspended direct Studio lifecycle so Carbon stages exact-process RML injection before releasing Roblox Studio; set {LIFECYCLE_ENV}=mcp to explicitly delegate process ownership"
		);
		None
	} else {
		discover_mcp_lifecycle()?
	};
	let lifecycle = if let Some(mcp) = mcp_lifecycle {
		log::debug!(
			"Selected robloxstudio-mcp lifecycle owner at {} for managed Studio launch",
			mcp.endpoint
		);
		match launch_through_mcp(path.clone(), &rml_launch, &mcp) {
			Ok(lifecycle) => lifecycle,
			Err(error) if error.safe_for_direct_fallback(mcp.preference) => {
				let broker_error = format!("{:#}", anyhow::Error::new(error));
				log::warn!(
					"robloxstudio-mcp launch failed before request dispatch at {}; falling back to direct lifecycle because no launch request was sent: {broker_error}",
					mcp.endpoint,
				);
				let (lifecycle, launched) = prepare_direct_managed_launch(path, rml_launch, &studio_executable)
					.with_context(|| {
						format!(
							"safe direct fallback failed after robloxstudio-mcp pre-dispatch failure: {broker_error}"
						)
					})?;
				direct_launch = Some(launched);
				lifecycle
			}
			Err(error) => return Err(anyhow::Error::new(error)),
		}
	} else {
		let (lifecycle, launched) = prepare_direct_managed_launch(path, rml_launch, &studio_executable)?;
		direct_launch = Some(launched);
		lifecycle
	};
	let mut managed = ManagedStudio {
		lifecycle,
		startup_guard: Arc::new(Mutex::new(Some(plugin_launch))),
		bridge_id: None,
	};
	let process_id = managed.process_id();
	let started_at_file_time = match &managed.lifecycle {
		ManagedStudioLifecycle::Direct {
			started_at_file_time, ..
		} => *started_at_file_time,
		ManagedStudioLifecycle::Mcp {
			started_at_file_time, ..
		} => started_at_file_time.unwrap_or(0),
	};
	let startup = (|| {
		rml::inject_loader(
			process_id,
			&loader_path,
			&studio_executable,
			started_at_file_time,
			|| match &managed.lifecycle {
				ManagedStudioLifecycle::Direct { .. } => direct_launch
					.as_mut()
					.context("retained direct Roblox Studio launch is unavailable")?
					.resume_for_injection(),
				ManagedStudioLifecycle::Mcp {
					endpoint,
					auth_token,
					launch_id,
					process_id,
					started_at_file_time,
					..
				} => {
					let status = mcp_tool(
						endpoint,
						auth_token.as_deref(),
						&json!({
							"action": "authorize",
							"launch_id": launch_id
						}),
						MCP_LIFECYCLE_TIMEOUT,
					)?;
					anyhow::ensure!(
						managed_injection_status_has_identity(
							&status,
							launch_id,
							*process_id,
							*started_at_file_time
						),
						"robloxstudio-mcp no longer authorizes Studio process {process_id} for launch {launch_id}: {status}"
					);
					Ok(())
				}
			},
		)
		.with_context(|| format!("failed to load Carbon RML into managed Roblox Studio process {process_id}"))?;
		let bridge_id = attest_rml_bridge(process_id, &studio_executable, started_at_file_time, false)?;
		managed.bridge_id = Some(bridge_id.clone());
		let ready_bridge_id = attest_rml_bridge(process_id, &studio_executable, started_at_file_time, true)?;
		anyhow::ensure!(
			ready_bridge_id == bridge_id,
			"RML bridge identity changed while managed Roblox Studio initialized"
		);
		if let Some(launched) = direct_launch.as_mut() {
			launched.finish()?;
		}
		if let ManagedStudioLifecycle::Mcp {
			endpoint,
			auth_token,
			launch_id,
			process_id,
			started_at_file_time,
			..
		} = &managed.lifecycle
		{
			let status = mcp_tool(
				endpoint,
				auth_token.as_deref(),
				&json!({
					"action": "complete",
					"launch_id": launch_id
				}),
				MCP_LIFECYCLE_TIMEOUT,
			)?;
			anyhow::ensure!(
				managed_completion_status_has_identity(
					&status,
					launch_id,
					*process_id,
					*started_at_file_time
				),
				"robloxstudio-mcp did not release Studio process {process_id} ownership for launch {launch_id}: {status}"
			);
		}
		Ok::<(), anyhow::Error>(())
	})()
	.context("Carbon RML did not start with the managed Roblox Studio process");
	if let Err(error) = startup {
		return Err(match direct_launch.as_mut() {
			Some(launched) => abort_direct_launch_with_bridge(launched, error, managed.bridge_id.as_deref()),
			None => stop_failed_managed_launch(&managed, error),
		});
	}
	Ok(managed)
}

fn prepare_launch(studio_dir: Option<&Path>) -> Result<(rml::Launch, studio_plugin::Installation)> {
	let installation = studio_plugin::ensure_current()?;
	match installation.status {
		studio_plugin::InstallStatus::Current => {}
		studio_plugin::InstallStatus::Installed => {
			crate::carbon_info!("Installed Carbon Studio plugin at {}", installation.path.display());
		}
		studio_plugin::InstallStatus::Updated => {
			crate::carbon_info!("Updated Carbon Studio plugin at {}", installation.path.display());
		}
	}
	Ok((rml::prepare_launch(studio_dir, None)?, installation))
}

fn discover_mcp_lifecycle() -> Result<Option<McpLifecycle>> {
	let preference = LifecyclePreference::from_env()?;
	let base_url = std::env::var(MCP_URL_ENV).unwrap_or_else(|_| DEFAULT_MCP_URL.to_owned());
	discover_mcp_lifecycle_at(preference, &base_url)
}

fn discover_mcp_lifecycle_at(preference: LifecyclePreference, base_url: &str) -> Result<Option<McpLifecycle>> {
	if preference == LifecyclePreference::Direct {
		return Ok(None);
	}

	let health_url = mcp_health_url(base_url)
		.with_context(|| format!("invalid robloxstudio-mcp URL from {MCP_URL_ENV}: {base_url:?}"))?;
	let client = Client::builder()
		.connect_timeout(MCP_PROBE_TIMEOUT)
		.timeout(MCP_PROBE_TIMEOUT)
		.build()?;
	let response = match client.get(&health_url).header("Accept", "application/json").send() {
		Ok(response) => response,
		Err(error) if preference == LifecyclePreference::Auto => {
			log::debug!("robloxstudio-mcp lifecycle probe was unavailable: {error}");
			return Ok(None);
		}
		Err(error) => {
			return Err(error).with_context(|| format!("{LIFECYCLE_ENV}=mcp but {health_url} was unavailable"));
		}
	};
	if !response.status().is_success() {
		if preference == LifecyclePreference::Auto {
			log::debug!("robloxstudio-mcp lifecycle probe returned HTTP {}", response.status());
			return Ok(None);
		}
		anyhow::bail!(
			"{LIFECYCLE_ENV}=mcp but robloxstudio-mcp health returned HTTP {}",
			response.status()
		);
	}
	let health: Value = match response.json() {
		Ok(health) => health,
		Err(error) if preference == LifecyclePreference::Auto => {
			log::debug!("robloxstudio-mcp lifecycle probe returned invalid JSON: {error}");
			return Ok(None);
		}
		Err(error) => return Err(error).context("robloxstudio-mcp health returned invalid JSON"),
	};
	let Some(endpoint) = mcp_lifecycle_endpoint(base_url, &health) else {
		if preference == LifecyclePreference::Auto {
			log::debug!("local MCP does not advertise the compatible Studio lifecycle capability");
			return Ok(None);
		}
		anyhow::bail!(
			"{LIFECYCLE_ENV}=mcp but {base_url} does not advertise robloxstudio-mcp Studio lifecycle protocol {MCP_PROTOCOL_VERSION}"
		);
	};

	Ok(Some(McpLifecycle {
		endpoint,
		auth_token: load_mcp_auth_token(),
		preference,
	}))
}

fn mcp_health_url(base_url: &str) -> Result<String> {
	let mut url = reqwest::Url::parse(base_url.trim())?;
	anyhow::ensure!(url.scheme() == "http", "Studio lifecycle MCP URL must use http");
	anyhow::ensure!(
		matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1")),
		"Studio lifecycle MCP URL must use a loopback host"
	);
	anyhow::ensure!(
		url.username().is_empty() && url.password().is_none(),
		"Studio lifecycle MCP URL must not contain credentials"
	);
	url.set_query(None);
	url.set_fragment(None);
	url.set_path("/health");
	Ok(url.into())
}

fn connected_mcp_instance_id(health: &Value, data_model_name: &str) -> Result<Option<String>> {
	if health.get("status").and_then(Value::as_str) != Some("ok")
		|| health.get("service").and_then(Value::as_str) != Some("robloxstudio-mcp")
		|| health.get("serverName").and_then(Value::as_str) != Some("robloxstudio-mcp")
	{
		return Ok(None);
	}
	let mut matches = health
		.get("instances")
		.and_then(Value::as_array)
		.into_iter()
		.flatten()
		.filter(|instance| {
			instance.get("dataModelName").and_then(Value::as_str) == Some(data_model_name)
				&& instance.get("role").and_then(Value::as_str) == Some("edit")
		})
		.filter_map(|instance| instance.get("instanceId").and_then(Value::as_str))
		.filter(|instance_id| !instance_id.is_empty());
	let instance_id = matches.next().map(str::to_owned);
	anyhow::ensure!(
		matches.next().is_none(),
		"robloxstudio-mcp reported multiple edit instances named {data_model_name}"
	);
	Ok(instance_id)
}

fn query_mcp_health() -> Result<Option<Value>> {
	let base_url = std::env::var(MCP_URL_ENV).unwrap_or_else(|_| DEFAULT_MCP_URL.to_owned());
	let health_url = match mcp_health_url(&base_url) {
		Ok(health_url) => health_url,
		Err(error) => {
			log::debug!("direct Studio MCP identity lookup ignored an invalid {MCP_URL_ENV}: {error:#}");
			return Ok(None);
		}
	};
	let client = Client::builder()
		.connect_timeout(MCP_PROBE_TIMEOUT)
		.timeout(MCP_PROBE_TIMEOUT)
		.build()?;
	let response = match client.get(&health_url).header("Accept", "application/json").send() {
		Ok(response) if response.status().is_success() => response,
		Ok(response) => {
			log::debug!("direct Studio MCP identity lookup returned HTTP {}", response.status());
			return Ok(None);
		}
		Err(error) => {
			log::debug!("direct Studio MCP identity lookup was unavailable: {error}");
			return Ok(None);
		}
	};
	match response.json() {
		Ok(health) => Ok(Some(health)),
		Err(error) => {
			log::debug!("direct Studio MCP identity lookup returned invalid JSON: {error}");
			Ok(None)
		}
	}
}

fn resolve_direct_mcp_instance_id<F, W>(
	data_model_name: &str,
	deadline: Instant,
	mut health: F,
	mut wait: W,
) -> Result<Option<String>>
where
	F: FnMut() -> Result<Option<Value>>,
	W: FnMut(),
{
	loop {
		let Some(health) = health()? else {
			return Ok(None);
		};
		if let Some(instance_id) = connected_mcp_instance_id(&health, data_model_name)? {
			return Ok(Some(instance_id));
		}
		if Instant::now() >= deadline {
			return Ok(None);
		}
		wait();
	}
}

fn mcp_lifecycle_endpoint(base_url: &str, health: &Value) -> Option<String> {
	if health.get("status").and_then(Value::as_str) != Some("ok")
		|| health.get("service").and_then(Value::as_str) != Some("robloxstudio-mcp")
		|| health.get("serverName").and_then(Value::as_str) != Some("robloxstudio-mcp")
	{
		return None;
	}
	let capability = health.pointer("/capabilities/studioLifecycle")?;
	if capability.get("protocolVersion").and_then(Value::as_u64) != Some(MCP_PROTOCOL_VERSION) {
		return None;
	}
	let path = capability.get("endpoint").and_then(Value::as_str)?;
	if path != "/mcp/manage_instance" {
		return None;
	}
	let mut url = reqwest::Url::parse(base_url.trim()).ok()?;
	if url.scheme() != "http"
		|| !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
		|| !url.username().is_empty()
		|| url.password().is_some()
	{
		return None;
	}
	url.set_query(None);
	url.set_fragment(None);
	url.set_path(path);
	Some(url.into())
}
fn managed_launch_identity(result: &Value) -> Result<(String, u32, Option<u64>)> {
	let launch_id = result
		.get("launch_id")
		.and_then(Value::as_str)
		.filter(|value| !value.is_empty())
		.context("robloxstudio-mcp launch response did not include launch_id")?
		.to_owned();
	let process_id = result
		.get("pid")
		.and_then(Value::as_u64)
		.filter(|value| *value > 0)
		.and_then(|value| u32::try_from(value).ok())
		.context("robloxstudio-mcp launch response did not include a valid native Studio PID")?;
	let started_at_file_time = result
		.get("process_started_at_file_time")
		.and_then(Value::as_str)
		.and_then(|value| value.parse::<u64>().ok())
		.filter(|value| *value > 0);
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	anyhow::ensure!(
		started_at_file_time.is_some(),
		"robloxstudio-mcp launch response did not include the native Studio process creation time"
	);
	anyhow::ensure!(
		result.get("managed").and_then(Value::as_bool) == Some(true)
			&& result.get("source").and_then(Value::as_str) == Some("local_file"),
		"robloxstudio-mcp did not return a managed local-file Studio launch"
	);
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	anyhow::ensure!(
		result.get("state").and_then(Value::as_str) == Some("launching")
			&& result.get("process_running").and_then(Value::as_bool) == Some(true)
			&& result.get("process_authorized").and_then(Value::as_bool) == Some(false),
		"robloxstudio-mcp did not return a suspended launch awaiting Carbon authorization"
	);
	#[cfg(not(any(target_os = "linux", target_os = "windows")))]
	anyhow::ensure!(
		matches!(
			result.get("state").and_then(Value::as_str),
			Some("launching" | "connected")
		) && result.get("process_running").and_then(Value::as_bool) == Some(true),
		"robloxstudio-mcp did not return a running managed Studio launch"
	);
	// Protocol v2 requires the exact process to remain suspended until Carbon
	// has prepared its injector and explicitly authorizes this launch ID.
	Ok((launch_id, process_id, started_at_file_time))
}

fn managed_injection_status_has_identity(
	result: &Value,
	launch_id: &str,
	process_id: u32,
	started_at_file_time: Option<u64>,
) -> bool {
	let state = result.get("state").and_then(Value::as_str);
	result.get("launch_id").and_then(Value::as_str) == Some(launch_id)
		&& result.get("pid").and_then(Value::as_u64) == Some(u64::from(process_id))
		&& result.get("managed").and_then(Value::as_bool) == Some(true)
		&& result.get("source").and_then(Value::as_str) == Some("local_file")
		&& matches!(state, Some("launching" | "connected"))
		&& result.get("process_running").and_then(Value::as_bool) == Some(true)
		&& result.get("process_authorized").and_then(Value::as_bool) == Some(true)
		&& started_at_file_time.is_none_or(|expected| {
			result
				.get("process_started_at_file_time")
				.and_then(Value::as_str)
				.and_then(|value| value.parse::<u64>().ok())
				== Some(expected)
		})
}

fn managed_completion_status_has_identity(
	result: &Value,
	launch_id: &str,
	process_id: u32,
	started_at_file_time: Option<u64>,
) -> bool {
	managed_injection_status_has_identity(result, launch_id, process_id, started_at_file_time)
		&& result.get("process_ownership_released").and_then(Value::as_bool) == Some(true)
}

fn attest_rml_bridge(
	process_id: u32,
	studio_executable: &str,
	started_at_file_time: u64,
	require_engine_ready: bool,
) -> Result<String> {
	verify_process_identity(process_id, studio_executable, started_at_file_time)
		.context("launched Roblox Studio identity changed before RML bridge attestation")?;
	let attestation = if require_engine_ready {
		crate::privileged_bridge::Bridge::wait_for_process(process_id, Duration::from_secs(30))
	} else {
		crate::privileged_bridge::Bridge::wait_for_loaded_process(process_id, Duration::from_secs(30))
	};
	let bridge = attestation.with_context(|| {
		format!("RML bridge for Roblox Studio process {process_id} did not attest this Carbon build")
	})?;
	verify_process_identity(process_id, studio_executable, started_at_file_time)
		.context("launched Roblox Studio identity changed during RML bridge attestation")?;
	Ok(bridge.bridge_id().to_owned())
}

fn stop_mcp_owned_process<C, T>(
	process_id: u32,
	process_identity: Option<(&str, u64)>,
	close: C,
	terminate: T,
) -> Result<()>
where
	C: FnOnce() -> Result<()>,
	T: FnOnce(u32, &str, u64) -> Result<()>,
{
	let close_error = match close() {
		Ok(()) => return Ok(()),
		Err(error) => error,
	};
	let Some((studio_executable, started_at_file_time)) = process_identity else {
		return Err(close_error.context(
			"identity-safe local cleanup is unavailable because Carbon did not capture the Studio process creation time",
		));
	};
	match terminate(process_id, studio_executable, started_at_file_time) {
		Ok(()) => {
			log::warn!(
				"robloxstudio-mcp could not close Studio process {process_id}; Carbon terminated the exact captured process identity instead: {close_error:#}"
			);
			Ok(())
		}
		Err(terminate_error) => Err(terminate_error.context(format!(
			"robloxstudio-mcp cleanup also failed before identity-safe local termination: {close_error:#}"
		))),
	}
}

fn stop_failed_managed_launch(managed: &ManagedStudio, error: anyhow::Error) -> anyhow::Error {
	match managed.stop() {
		Ok(()) => error,
		Err(stop_error) => error.context(format!(
			"additionally failed to stop managed Roblox Studio process {}: {stop_error:#}",
			managed.process_id()
		)),
	}
}

fn mcp_broker_place_path(path: &Path) -> &Path {
	// The broker resolves this path in its own host namespace and converts it
	// to a Windows Studio argument only when spawning the native process.
	path
}

fn launch_through_mcp(
	path: PathBuf,
	rml_launch: &rml::Launch,
	mcp: &McpLifecycle,
) -> std::result::Result<ManagedStudioLifecycle, ManagedLaunchFailure> {
	let (studio_executable, loader_path) =
		mcp_launch_paths(rml_launch).map_err(|error| ManagedLaunchFailure::pre_dispatch(&mcp.endpoint, error))?;
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	let dotnet_root = Some(
		windows_dotnet_root(&loader_path).map_err(|error| ManagedLaunchFailure::pre_dispatch(&mcp.endpoint, error))?,
	);
	#[cfg(not(any(target_os = "linux", target_os = "windows")))]
	let dotnet_root = std::env::var("DOTNET_ROOT")
		.ok()
		.filter(|value| !value.trim().is_empty());
	let payload = mcp_launch_request(
		mcp_broker_place_path(&path),
		&studio_executable,
		&loader_path,
		rml_launch.build_version(),
		dotnet_root.as_deref(),
	);
	let result = mcp_tool(&mcp.endpoint, mcp.auth_token.as_deref(), &payload, MCP_LAUNCH_TIMEOUT)
		.map_err(ManagedLaunchFailure::from_request)?;
	let (launch_id, process_id, started_at_file_time) = managed_launch_identity(&result)
		.map_err(|error| ManagedLaunchFailure::invalid_response(&mcp.endpoint, error))?;
	Ok(ManagedStudioLifecycle::Mcp {
		endpoint: mcp.endpoint.clone(),
		auth_token: mcp.auth_token.clone(),
		launch_id,
		process_id,
		studio_executable,
		started_at_file_time,
	})
}

fn mcp_launch_request(
	path: &std::path::Path,
	studio: &str,
	loader: &str,
	build_version: &str,
	dotnet_root: Option<&str>,
) -> Value {
	let mut payload = json!({
		"action": "launch",
		"source": "local_file",
		"local_place_file": path,
		"studio_executable": studio,
		"process_environment": {
			"set": {
				rml::LOADER_ENV: loader,
				rml::EXPECTED_BUILD_ENV: build_version
			},
			"remove": [rml::LOADED_BUILD_ENV]
		},
		"wait_for_connection": false,
		"require_process_identity": cfg!(any(target_os = "linux", target_os = "windows"))
	});
	if let Some(dotnet_root) = dotnet_root {
		payload["process_environment"]["set"]["DOTNET_ROOT"] = Value::String(dotnet_root.to_owned());
	}
	payload
}

fn windows_nonverbatim_path(path: &str) -> String {
	if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
		format!(r"\\{path}")
	} else {
		path.strip_prefix(r"\\?\").unwrap_or(path).to_owned()
	}
}

fn mcp_launch_paths(rml_launch: &rml::Launch) -> Result<(String, String)> {
	#[cfg(target_os = "linux")]
	{
		Ok((
			windows_nonverbatim_path(&windows_path(
				rml_launch.studio_executable(),
				"Roblox Studio executable",
			)?),
			windows_nonverbatim_path(&windows_path(rml_launch.loader_path(), "RML loader")?),
		))
	}

	#[cfg(not(target_os = "linux"))]
	{
		Ok((
			windows_nonverbatim_path(&rml_launch.studio_executable().to_string_lossy()),
			windows_nonverbatim_path(&rml_launch.loader_path().to_string_lossy()),
		))
	}
}

fn load_mcp_auth_token() -> Option<String> {
	if std::env::var("ROBLOX_STUDIO_NO_AUTH")
		.is_ok_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true"))
	{
		return None;
	}
	for name in ["ROBLOX_STUDIO_AUTH_TOKEN", "ROBLOX_STUDIO_MCP_AUTH_TOKEN"] {
		if let Ok(token) = std::env::var(name) {
			let token = token.trim().to_owned();
			if !token.is_empty() {
				return Some(token);
			}
		}
	}
	let explicit = std::env::var_os("ROBLOX_STUDIO_MCP_AUTH_TOKEN_FILE").map(PathBuf::from);
	let home = std::env::var_os("HOME")
		.or_else(|| std::env::var_os("USERPROFILE"))
		.map(PathBuf::from)
		.map(|path| path.join(".robloxstudio-mcp").join("auth-token"));
	explicit
		.or(home)
		.and_then(|path| fs::read_to_string(path).ok())
		.map(|token| token.trim().to_owned())
		.filter(|token| !token.is_empty())
}

fn sanitized_broker_fields(value: &Value) -> Option<String> {
	fn text(value: &Value) -> Option<String> {
		match value {
			Value::String(value) => {
				let sanitized = value
					.chars()
					.map(|character| if character.is_control() { ' ' } else { character })
					.collect::<String>()
					.split_whitespace()
					.collect::<Vec<_>>()
					.join(" ");
				(!sanitized.is_empty()).then(|| sanitized.chars().take(512).collect())
			}
			Value::Number(value) => Some(value.to_string()),
			_ => None,
		}
	}

	let mut fields = Vec::new();
	for name in ["error", "code", "message", "remediation"] {
		let Some(value) = value.get(name) else {
			continue;
		};
		if let Some(value) = text(value) {
			fields.push(format!("{name}: {value}"));
			continue;
		}
		if let Value::Object(error) = value {
			for nested in ["code", "message", "remediation"] {
				if let Some(value) = error.get(nested).and_then(text) {
					fields.push(format!("{name}.{nested}: {value}"));
				}
			}
		}
	}
	(!fields.is_empty()).then(|| fields.join("; "))
}

fn sanitized_broker_body(bytes: &[u8]) -> Option<String> {
	serde_json::from_slice::<Value>(bytes)
		.ok()
		.as_ref()
		.and_then(sanitized_broker_fields)
}

fn mcp_tool(
	endpoint: &str,
	auth_token: Option<&str>,
	payload: &Value,
	timeout: Duration,
) -> std::result::Result<Value, McpRequestFailure> {
	let client = Client::builder()
		.connect_timeout(timeout)
		.timeout(timeout)
		.build()
		.map_err(|error| McpRequestFailure::new(endpoint, McpRequestFailureStage::PreDispatch, error))?;
	let mut request = client.post(endpoint).header("Accept", "application/json").json(payload);
	if let Some(token) = auth_token {
		request = request.header("X-MCP-Auth", token);
	}
	let request = request
		.build()
		.map_err(|error| McpRequestFailure::new(endpoint, McpRequestFailureStage::PreDispatch, error))?;
	let response = client.execute(request).map_err(|error| {
		let stage = if error.is_connect() {
			McpRequestFailureStage::PreDispatch
		} else {
			McpRequestFailureStage::DispatchAmbiguous
		};
		McpRequestFailure::new(endpoint, stage, error)
	})?;
	let status = response.status();
	let bytes = response
		.bytes()
		.map_err(|error| McpRequestFailure::new(endpoint, McpRequestFailureStage::InvalidResponse, error))?;
	if !status.is_success() {
		let detail = sanitized_broker_body(&bytes)
			.map(|detail| format!("HTTP {status}; {detail}"))
			.unwrap_or_else(|| format!("HTTP {status}; broker returned no safe structured diagnostic"));
		return Err(McpRequestFailure::new(
			endpoint,
			McpRequestFailureStage::BrokerRejected,
			anyhow::anyhow!(detail),
		));
	}
	let envelope: Value = serde_json::from_slice(&bytes).map_err(|error| {
		McpRequestFailure::new(
			endpoint,
			McpRequestFailureStage::InvalidResponse,
			anyhow::Error::new(error).context("robloxstudio-mcp lifecycle response was invalid JSON"),
		)
	})?;
	let result = envelope.get("content").and_then(Value::as_array).and_then(|content| {
		content
			.iter()
			.filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
			.filter_map(|item| item.get("text").and_then(Value::as_str))
			.find_map(|text| serde_json::from_str::<Value>(text).ok())
	});
	if envelope.get("isError").and_then(Value::as_bool) == Some(true) {
		let detail = result.as_ref().and_then(sanitized_broker_fields).unwrap_or_else(|| {
			"broker reported a lifecycle tool error without a safe structured diagnostic".to_owned()
		});
		return Err(McpRequestFailure::new(
			endpoint,
			McpRequestFailureStage::BrokerRejected,
			anyhow::anyhow!(detail),
		));
	}
	let result = result.ok_or_else(|| {
		McpRequestFailure::new(
			endpoint,
			McpRequestFailureStage::InvalidResponse,
			anyhow::anyhow!("robloxstudio-mcp lifecycle response contained no JSON result"),
		)
	})?;
	if result.get("error").is_some_and(|value| !value.is_null())
		|| result.get("success").and_then(Value::as_bool) == Some(false)
	{
		let detail = sanitized_broker_fields(&result).unwrap_or_else(|| {
			"broker rejected the lifecycle tool request without a safe structured diagnostic".to_owned()
		});
		return Err(McpRequestFailure::new(
			endpoint,
			McpRequestFailureStage::BrokerRejected,
			anyhow::anyhow!(detail),
		));
	}
	Ok(result)
}

fn mcp_instance_id(result: &Value, launch_id: &str) -> Result<String> {
	anyhow::ensure!(
		result.get("launch_id").and_then(Value::as_str) == Some(launch_id),
		"robloxstudio-mcp reported a different Studio launch than {launch_id}: {result}"
	);
	anyhow::ensure!(
		result.get("managed").and_then(Value::as_bool) == Some(true),
		"robloxstudio-mcp no longer owns Studio launch {launch_id}: {result}"
	);
	anyhow::ensure!(
		result.get("state").and_then(Value::as_str) == Some("connected"),
		"robloxstudio-mcp Studio launch {launch_id} is not connected: {result}"
	);
	result
		.get("instance_id")
		.and_then(Value::as_str)
		.filter(|value| !value.is_empty())
		.map(str::to_owned)
		.context("robloxstudio-mcp connected Studio status did not include instance_id")
}

fn resolve_mcp_instance_id<F, W>(launch_id: &str, deadline: Instant, mut status: F, mut wait: W) -> Result<String>
where
	F: FnMut() -> Result<Value>,
	W: FnMut(),
{
	loop {
		let status_error = match status() {
			Ok(result) => {
				if result
					.get("instance_id")
					.and_then(Value::as_str)
					.is_some_and(|value| !value.is_empty())
				{
					return mcp_instance_id(&result, launch_id);
				}
				let state = result.get("state").and_then(Value::as_str).unwrap_or("unknown");
				anyhow::ensure!(
					!matches!(state, "failed" | "exited"),
					"robloxstudio-mcp Studio launch {launch_id} entered state {state} before reporting an instance ID: {result}"
				);
				None
			}
			Err(error) => Some(error),
		};

		if Instant::now() >= deadline {
			let message = format!(
				"robloxstudio-mcp Studio launch {launch_id} did not report an instance ID within {} seconds",
				MCP_LIFECYCLE_TIMEOUT.as_secs()
			);
			return match status_error {
				Some(error) => Err(error).context(message),
				None => anyhow::bail!(message),
			};
		}
		wait();
	}
}

struct DirectLaunch {
	process_id: u32,
	child: Option<Child>,
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	studio_executable: Option<String>,
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	started_at_file_time: Option<u64>,
}

impl DirectLaunch {
	fn process_id(&self) -> u32 {
		self.process_id
	}

	fn started_at_file_time(&self) -> Result<u64> {
		#[cfg(any(target_os = "linux", target_os = "windows"))]
		{
			self.started_at_file_time
				.context("suspended Roblox Studio launcher did not return its process creation time")
		}
		#[cfg(not(any(target_os = "linux", target_os = "windows")))]
		{
			Ok(0)
		}
	}

	#[cfg(any(target_os = "linux", target_os = "windows"))]
	fn set_identity(&mut self, studio_executable: String, started_at_file_time: u64) {
		self.studio_executable = Some(studio_executable);
		self.started_at_file_time = Some(started_at_file_time);
	}

	#[cfg(not(any(target_os = "linux", target_os = "windows")))]
	fn set_identity(&mut self, _studio_executable: String, _started_at_file_time: u64) {}

	fn resume_for_injection(&mut self) -> Result<()> {
		#[cfg(any(target_os = "linux", target_os = "windows"))]
		{
			let child = self
				.child
				.as_mut()
				.context("Roblox Studio launcher process is unavailable")?;
			let stdin = child
				.stdin
				.as_mut()
				.context("Roblox Studio launcher stdin is unavailable")?;
			writeln!(stdin, "CARBON_STUDIO_LAUNCH_RESUME")
				.context("failed to authorize Roblox Studio process resume")?;
			let stdout = child
				.stdout
				.take()
				.context("Roblox Studio launcher stdout is unavailable")?;
			let mut stdout = BufReader::new(stdout);
			let mut response = String::new();
			let read_result = stdout.read_line(&mut response);
			child.stdout = Some(stdout.into_inner());
			read_result.context("failed to read Roblox Studio resume confirmation")?;
			anyhow::ensure!(
				response.trim() == "CARBON_STUDIO_LAUNCH_RESUMED",
				"Roblox Studio launcher did not confirm process resume"
			);
		}
		Ok(())
	}

	fn finish(&mut self) -> Result<()> {
		#[cfg(any(target_os = "linux", target_os = "windows"))]
		{
			self.complete_launcher("CARBON_STUDIO_LAUNCH_COMPLETE")
		}
		#[cfg(not(any(target_os = "linux", target_os = "windows")))]
		{
			self.child.take();
			Ok(())
		}
	}

	fn abort(&mut self) -> Result<()> {
		#[cfg(any(target_os = "linux", target_os = "windows"))]
		self.complete_launcher("CARBON_STUDIO_LAUNCH_ABORT")?;
		#[cfg(not(any(target_os = "linux", target_os = "windows")))]
		{
			if let Some(mut child) = self.child.take() {
				if child.try_wait()?.is_none() {
					if let Err(kill_error) = child.kill() {
						match child.try_wait() {
							Ok(Some(_)) => {}
							Ok(None) => {
								return Err(kill_error).context("failed to stop the launched Roblox Studio process");
							}
							Err(recheck_error) => {
								return Err(kill_error).context(format!(
									"failed to stop the launched Roblox Studio process and could not recheck it: {recheck_error}"
								));
							}
						}
					} else {
						child
							.wait()
							.context("failed to wait for the launched Roblox Studio process")?;
					}
				}
			}
		}
		Ok(())
	}

	#[cfg(any(target_os = "linux", target_os = "windows"))]
	fn complete_launcher(&mut self, command: &str) -> Result<()> {
		let Some(mut child) = self.child.take() else {
			let error = anyhow::anyhow!("Roblox Studio launcher process is unavailable");
			return Err(self.cleanup_ambiguous_launch(error));
		};
		let write_error = match child.stdin.as_mut() {
			Some(stdin) => writeln!(stdin, "{command}").err().map(|error| {
				anyhow::Error::new(error).context("failed to send completion to the Roblox Studio launcher")
			}),
			None => Some(anyhow::anyhow!("Roblox Studio launcher stdin is unavailable")),
		};
		child.stdin.take();
		let output = child.wait_with_output();
		if write_error.is_none() && output.as_ref().is_ok_and(|output| output.status.success()) {
			return Ok(());
		}
		let mut error = write_error.unwrap_or_else(|| {
			anyhow::anyhow!(
				"Roblox Studio launcher cleanup failed: {}",
				output
					.as_ref()
					.ok()
					.map(|output| String::from_utf8_lossy(&output.stderr).trim().to_owned())
					.unwrap_or_else(|| "launcher wait failed".to_owned())
			)
		});
		if let Err(wait_error) = output {
			error = error.context(format!("failed to wait for the Roblox Studio launcher: {wait_error}"));
		}
		Err(self.cleanup_ambiguous_launch(error))
	}

	#[cfg(any(target_os = "linux", target_os = "windows"))]
	fn terminate_known_process(&self) -> Result<()> {
		let (Some(studio_executable), Some(started_at_file_time)) =
			(&self.studio_executable, self.started_at_file_time)
		else {
			anyhow::bail!("Roblox Studio process identity is unavailable");
		};
		terminate_process(self.process_id, studio_executable, started_at_file_time)
	}

	#[cfg(any(target_os = "linux", target_os = "windows"))]
	fn cleanup_ambiguous_launch(&self, error: anyhow::Error) -> anyhow::Error {
		match self.terminate_known_process() {
			Ok(()) => error,
			Err(stop_error) => error.context(format!(
				"additionally failed identity-safe fallback cleanup for Roblox Studio process {}: {stop_error:#}",
				self.process_id
			)),
		}
	}
}

fn abort_direct_launch(launched: &mut DirectLaunch, error: anyhow::Error) -> anyhow::Error {
	abort_direct_launch_with_bridge(launched, error, None)
}

fn abort_direct_launch_with_bridge(
	launched: &mut DirectLaunch,
	error: anyhow::Error,
	bridge_id: Option<&str>,
) -> anyhow::Error {
	if let Err(stop_error) = launched.abort() {
		#[cfg(any(target_os = "linux", target_os = "windows"))]
		if let Err(fallback_error) = launched.terminate_known_process() {
			return error.context(format!(
				"additionally failed to stop the retained Roblox Studio launch: {stop_error:#}; identity-safe fallback also failed: {fallback_error:#}"
			));
		}
		#[cfg(not(any(target_os = "linux", target_os = "windows")))]
		return error.context(format!(
			"additionally failed to stop the retained Roblox Studio launch: {stop_error:#}"
		));
	}
	let Some(bridge_id) = bridge_id else {
		return error;
	};
	match cleanup_stopped_bridge(launched.process_id(), bridge_id) {
		Ok(()) => error,
		Err(cleanup_error) => error.context(format!(
			"additionally failed to remove the stopped Roblox Studio bridge record: {cleanup_error:#}"
		)),
	}
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn reap_failed_launcher(mut child: Child, error: anyhow::Error) -> anyhow::Error {
	child.stdin.take();
	match child.wait_with_output() {
		Ok(output) => {
			let stderr = String::from_utf8_lossy(&output.stderr);
			let stderr = stderr.trim();
			if stderr.is_empty() {
				error
			} else {
				error.context(format!("Roblox Studio launcher failed: {stderr}"))
			}
		}
		Err(wait_error) => error.context(format!("failed to reap the Roblox Studio launcher: {wait_error}")),
	}
}

fn launch_prepared(
	path: Option<PathBuf>,
	rml_launch: rml::Launch,
	managed_place: bool,
) -> Result<Option<DirectLaunch>> {
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	{
		#[cfg(target_os = "linux")]
		anyhow::ensure!(
			std::env::var_os("WSL_DISTRO_NAME").is_some(),
			"Roblox Studio launch is supported on Linux only through WSL"
		);
		let windows_place = path
			.map(|path| windows_path(&path, "Studio place"))
			.transpose()?
			.unwrap_or_default();
		let windows_studio = windows_path(rml_launch.studio_executable(), "Roblox Studio executable")?;
		let windows_loader = windows_path(rml_launch.loader_path(), "RML loader")?;
		let windows_dotnet_root = windows_dotnet_root(&windows_loader)?;
		let script = powershell_launch_script(
			&windows_studio,
			&windows_loader,
			rml_launch.build_version(),
			&windows_dotnet_root,
			&windows_place,
			managed_place,
		);
		// Windows PowerShell 5 concatenates everything after `-Command` into the
		// script instead of exposing trailing values through `$args`. Only the
		// fixed base64 alphabet is interpolated here so arbitrary paths and build
		// versions can never become executable PowerShell source.
		let mut child = Command::new("powershell.exe")
			.args(["-NoProfile", "-Command", script.as_str()])
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.context("failed to start the Roblox Studio launcher")?;
		let Some(stdout) = child.stdout.take() else {
			let error = anyhow::anyhow!("Roblox Studio launcher stdout is unavailable");
			return Err(reap_failed_launcher(child, error));
		};
		let mut stdout = BufReader::new(stdout);
		let mut process_id = String::new();
		let mut started_at_file_time = String::new();
		let read_result = stdout
			.read_line(&mut process_id)
			.and_then(|_| stdout.read_line(&mut started_at_file_time));
		child.stdout = Some(stdout.into_inner());
		if let Err(error) = read_result {
			let error = anyhow::Error::new(error).context("failed to read the launched Roblox Studio process identity");
			return Err(reap_failed_launcher(child, error));
		}
		let process_id = match process_id.trim().parse::<u32>() {
			Ok(process_id) => process_id,
			Err(error) => {
				let error =
					anyhow::Error::new(error).context("PowerShell returned an invalid Roblox Studio process ID");
				return Err(reap_failed_launcher(child, error));
			}
		};
		let started_at_file_time = match started_at_file_time.trim().parse::<u64>() {
			Ok(started_at_file_time) if started_at_file_time > 0 => started_at_file_time,
			Ok(_) => {
				return Err(reap_failed_launcher(
					child,
					anyhow::anyhow!("PowerShell returned an invalid Roblox Studio process creation time"),
				));
			}
			Err(error) => {
				let error = anyhow::Error::new(error)
					.context("PowerShell returned an invalid Roblox Studio process creation time");
				return Err(reap_failed_launcher(child, error));
			}
		};
		Ok(Some(DirectLaunch {
			process_id,
			child: Some(child),
			studio_executable: None,
			started_at_file_time: Some(started_at_file_time),
		}))
	}

	#[cfg(not(any(target_os = "linux", target_os = "windows")))]
	{
		let mut command = Command::new(rml_launch.studio_executable());
		let _ = managed_place;
		command
			.arg(path.unwrap_or_default())
			.env(rml::LOADER_ENV, rml_launch.loader_path())
			.env(rml::EXPECTED_BUILD_ENV, rml_launch.build_version())
			.env_remove(rml::LOADED_BUILD_ENV)
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null());
		#[cfg(target_os = "windows")]
		command.env(
			"DOTNET_ROOT",
			windows_dotnet_root(&windows_nonverbatim_path(
				rml_launch
					.loader_path()
					.to_str()
					.context("RML loader path is not valid UTF-8")?,
			))?,
		);
		let child = command.spawn()?;
		Ok(Some(DirectLaunch {
			process_id: child.id(),
			child: Some(child),
		}))
	}
}

#[cfg(target_os = "linux")]
fn windows_path(path: &std::path::Path, description: &str) -> Result<String> {
	let output = Command::new("wslpath").arg("-w").arg(path).output()?;
	anyhow::ensure!(
		output.status.success(),
		"failed to translate the {description} path for Windows"
	);
	Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(target_os = "windows")]
fn windows_path(path: &std::path::Path, description: &str) -> Result<String> {
	let path = path
		.to_str()
		.with_context(|| format!("{description} path is not valid UTF-8"))?;
	Ok(windows_nonverbatim_path(path))
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn windows_dotnet_root_script(loader: &str) -> String {
	let encoded_loader = BASE64_STANDARD.encode(loader.as_bytes());
	r#"
$loader = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__CARBON_LOADER_BASE64__'))
$runtimeConfigPath = [System.IO.Path]::Combine(
    [System.IO.Path]::GetDirectoryName($loader),
    'runtime\RML.runtimeconfig.json'
)
if (-not (Test-Path -LiteralPath $runtimeConfigPath -PathType Leaf)) {
    Write-Error "Carbon RML runtime configuration is missing: $runtimeConfigPath"
    exit 4
}
$runtimeConfig = Get-Content -LiteralPath $runtimeConfigPath -Raw | ConvertFrom-Json
$frameworks = @()
if ($null -ne $runtimeConfig.runtimeOptions.framework) {
    $frameworks += $runtimeConfig.runtimeOptions.framework
}
if ($null -ne $runtimeConfig.runtimeOptions.frameworks) {
    $frameworks += @($runtimeConfig.runtimeOptions.frameworks)
}
$framework = $frameworks | Where-Object { $_.name -eq 'Microsoft.NETCore.App' } | Select-Object -First 1
if ($null -eq $framework) {
    Write-Error "Carbon RML runtime configuration does not name Microsoft.NETCore.App"
    exit 4
}
$requiredVersion = [Version]$framework.version
$candidates = @()
if (-not [string]::IsNullOrWhiteSpace($env:DOTNET_ROOT)) {
    $candidates += $env:DOTNET_ROOT
}
if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
    $candidates += Join-Path $env:USERPROFILE '.dotnet'
}
if (-not [string]::IsNullOrWhiteSpace($env:ProgramFiles)) {
    $candidates += Join-Path $env:ProgramFiles 'dotnet'
}
$dotnet = Get-Command dotnet.exe -ErrorAction SilentlyContinue
if ($null -ne $dotnet) {
    $candidates += Split-Path -Parent $dotnet.Source
}
foreach ($root in $candidates | Select-Object -Unique) {
    $hostfxr = Get-ChildItem -Path (Join-Path $root 'host\fxr\*\hostfxr.dll') -File -ErrorAction SilentlyContinue |
        Select-Object -First 1
    $runtimePath = Join-Path $root 'shared\Microsoft.NETCore.App'
    $compatibleRuntime = Get-ChildItem -LiteralPath $runtimePath -Directory -ErrorAction SilentlyContinue |
        Where-Object {
            try {
                $candidateVersion = [Version]$_.Name
                $candidateVersion.Major -eq $requiredVersion.Major -and $candidateVersion -ge $requiredVersion
            } catch {
                $false
            }
        } |
        Select-Object -First 1
    if ($null -ne $hostfxr -and $null -ne $compatibleRuntime) {
        Write-Output $root
        exit 0
    }
}
Write-Error "A Windows Microsoft.NETCore.App runtime compatible with $requiredVersion is required by Carbon RML"
exit 3
"#
	.replace("__CARBON_LOADER_BASE64__", &encoded_loader)
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn windows_dotnet_root(loader: &str) -> Result<String> {
	let script = windows_dotnet_root_script(loader);
	let output = Command::new("powershell.exe")
		.args(["-NoProfile", "-NonInteractive", "-Command", &script])
		.output()
		.context("failed to locate the Windows .NET runtime required by Carbon RML")?;
	anyhow::ensure!(
		output.status.success(),
		"failed to locate the Windows .NET runtime required by Carbon RML: {}",
		String::from_utf8_lossy(&output.stderr).trim()
	);
	let root = String::from_utf8(output.stdout)?.trim().to_owned();
	anyhow::ensure!(!root.is_empty(), "Windows .NET returned an empty installation path");
	Ok(root)
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn powershell_launch_script(
	studio: &str,
	loader: &str,
	build_version: &str,
	dotnet_root: &str,
	place: &str,
	managed_place: bool,
) -> String {
	let encoded_studio = BASE64_STANDARD.encode(studio.as_bytes());
	let encoded_loader = BASE64_STANDARD.encode(loader.as_bytes());
	let encoded_build_version = BASE64_STANDARD.encode(build_version.as_bytes());
	let encoded_dotnet_root = BASE64_STANDARD.encode(dotnet_root.as_bytes());
	let encoded_place = BASE64_STANDARD.encode(place.as_bytes());
	let managed_place = if managed_place { "$true" } else { "$false" };
	r#"
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

public sealed class CarbonSuspendedStudio : IDisposable
{
    private const uint CREATE_SUSPENDED = 0x00000004;
    private const uint CREATE_BREAKAWAY_FROM_JOB = 0x01000000;
    private const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000;
    private const int JOB_OBJECT_EXTENDED_LIMIT_INFORMATION = 9;
    private const uint WAIT_OBJECT_0 = 0x00000000;
    private const uint WAIT_TIMEOUT = 0x00000102;
    private const uint WAIT_FAILED = 0xFFFFFFFF;
    private const uint RESUME_FAILED = 0xFFFFFFFF;

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct StartupInfo
    {
        public int cb;
        public string lpReserved;
        public string lpDesktop;
        public string lpTitle;
        public uint dwX;
        public uint dwY;
        public uint dwXSize;
        public uint dwYSize;
        public uint dwXCountChars;
        public uint dwYCountChars;
        public uint dwFillAttribute;
        public uint dwFlags;
        public short wShowWindow;
        public short cbReserved2;
        public IntPtr lpReserved2;
        public IntPtr hStdInput;
        public IntPtr hStdOutput;
        public IntPtr hStdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct ProcessInformation
    {
        public IntPtr hProcess;
        public IntPtr hThread;
        public uint dwProcessId;
        public uint dwThreadId;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct BasicLimitInformation
    {
        public long PerProcessUserTimeLimit;
        public long PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize;
        public UIntPtr MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass;
        public uint SchedulingClass;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct IoCounters
    {
        public ulong ReadOperationCount;
        public ulong WriteOperationCount;
        public ulong OtherOperationCount;
        public ulong ReadTransferCount;
        public ulong WriteTransferCount;
        public ulong OtherTransferCount;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct ExtendedLimitInformation
    {
        public BasicLimitInformation BasicLimitInformation;
        public IoCounters IoInfo;
        public UIntPtr ProcessMemoryLimit;
        public UIntPtr JobMemoryLimit;
        public UIntPtr PeakProcessMemoryUsed;
        public UIntPtr PeakJobMemoryUsed;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct FileTime
    {
        public uint Low;
        public uint High;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool CreateProcessW(
        string applicationName,
        StringBuilder commandLine,
        IntPtr processAttributes,
        IntPtr threadAttributes,
        bool inheritHandles,
        uint creationFlags,
        IntPtr environment,
        string currentDirectory,
        ref StartupInfo startupInfo,
        out ProcessInformation processInformation);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetProcessTimes(
        IntPtr process,
        out FileTime creation,
        out FileTime exit,
        out FileTime kernel,
        out FileTime user);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr CreateJobObject(IntPtr jobAttributes, string name);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetInformationJobObject(
        IntPtr job,
        int informationClass,
        IntPtr information,
        uint informationLength);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint ResumeThread(IntPtr thread);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool TerminateProcess(IntPtr process, uint exitCode);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);

    [DllImport("kernel32.dll")]
    private static extern bool CloseHandle(IntPtr handle);

    private IntPtr process;
    private IntPtr thread;
    private IntPtr job;

    public uint ProcessId { get; private set; }
    public ulong StartedAtFileTime { get; private set; }

    private CarbonSuspendedStudio(
        ProcessInformation processInformation,
        IntPtr jobHandle,
        ulong startedAtFileTime)
    {
        process = processInformation.hProcess;
        thread = processInformation.hThread;
        job = jobHandle;
        ProcessId = processInformation.dwProcessId;
        StartedAtFileTime = startedAtFileTime;
    }

    private static void TerminateAndWait(IntPtr process)
    {
        uint state = WaitForSingleObject(process, 0);
        if (state == WAIT_FAILED)
            throw new Win32Exception(Marshal.GetLastWin32Error(), "WaitForSingleObject failed");
        if (state == WAIT_OBJECT_0)
            return;
        if (!TerminateProcess(process, 1))
        {
            state = WaitForSingleObject(process, 0);
            if (state == WAIT_OBJECT_0)
                return;
            throw new Win32Exception(Marshal.GetLastWin32Error(), "TerminateProcess failed");
        }
        state = WaitForSingleObject(process, 30000);
        if (state == WAIT_TIMEOUT)
            throw new TimeoutException("Roblox Studio did not terminate");
        if (state != WAIT_OBJECT_0)
            throw new Win32Exception(Marshal.GetLastWin32Error(), "WaitForSingleObject failed");
    }

    public static CarbonSuspendedStudio Start(string executable, string place, bool managedPlace)
    {
        IntPtr job = CreateJobObject(IntPtr.Zero, null);
        if (job == IntPtr.Zero)
            throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateJobObject failed");

        ProcessInformation processInformation = new ProcessInformation();
        try
        {
            SetKillOnClose(job, true);
            string command = Quote(executable);
            if (!String.IsNullOrEmpty(place))
            {
                if (managedPlace)
                    command += " --task EditFile --localPlaceFile " + Quote(place);
                else
                    command += " " + Quote(place);
            }

            StartupInfo startupInfo = new StartupInfo();
            startupInfo.cb = Marshal.SizeOf(typeof(StartupInfo));
            bool created = CreateProcessW(
                executable,
                new StringBuilder(command),
                IntPtr.Zero,
                IntPtr.Zero,
                false,
                CREATE_SUSPENDED | CREATE_BREAKAWAY_FROM_JOB,
                IntPtr.Zero,
                System.IO.Path.GetDirectoryName(executable),
                ref startupInfo,
                out processInformation);
            if (!created && Marshal.GetLastWin32Error() == 5)
            {
                created = CreateProcessW(
                    executable,
                    new StringBuilder(command),
                    IntPtr.Zero,
                    IntPtr.Zero,
                    false,
                    CREATE_SUSPENDED,
                    IntPtr.Zero,
                    System.IO.Path.GetDirectoryName(executable),
                    ref startupInfo,
                    out processInformation);
            }
            if (!created)
                throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateProcessW failed");
            if (!AssignProcessToJobObject(job, processInformation.hProcess))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "AssignProcessToJobObject failed");
            FileTime creation;
            FileTime exit;
            FileTime kernel;
            FileTime user;
            if (!GetProcessTimes(processInformation.hProcess, out creation, out exit, out kernel, out user))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "GetProcessTimes failed");
            ulong startedAtFileTime = ((ulong)creation.High << 32) | creation.Low;
            return new CarbonSuspendedStudio(processInformation, job, startedAtFileTime);
        }
        catch
        {
            try
            {
                if (processInformation.hProcess != IntPtr.Zero)
                    TerminateAndWait(processInformation.hProcess);
            }
            finally
            {
                Close(processInformation.hThread);
                Close(processInformation.hProcess);
                Close(job);
            }
            throw;
        }
    }

    public void Resume()
    {
        if (thread == IntPtr.Zero)
            throw new InvalidOperationException("Roblox Studio launch is no longer suspended");
        if (ResumeThread(thread) == RESUME_FAILED)
            throw new Win32Exception(Marshal.GetLastWin32Error(), "ResumeThread failed");
        Close(thread);
        thread = IntPtr.Zero;
    }

    public void Release()
    {
        if (process == IntPtr.Zero)
            throw new InvalidOperationException("Roblox Studio launch is no longer retained");
        try
        {
            SetKillOnClose(job, false);
        }
        catch
        {
            TerminateAndWait(process);
            throw;
        }
        finally
        {
            ReleaseHandles();
        }
    }

    public void Abort()
    {
        try
        {
            if (process == IntPtr.Zero)
                return;
            TerminateAndWait(process);
        }
        finally
        {
            ReleaseHandles();
        }
    }

    public void Dispose()
    {
        try { Abort(); } catch { }
        GC.SuppressFinalize(this);
    }

    ~CarbonSuspendedStudio()
    {
        ReleaseHandles();
    }

    private static string Quote(string value)
    {
        return "\"" + value.Replace("\"", "\\\"") + "\"";
    }

    private static void SetKillOnClose(IntPtr job, bool enabled)
    {
        ExtendedLimitInformation information = new ExtendedLimitInformation();
        if (enabled)
            information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        int length = Marshal.SizeOf(typeof(ExtendedLimitInformation));
        IntPtr pointer = Marshal.AllocHGlobal(length);
        try
        {
            Marshal.StructureToPtr(information, pointer, false);
            if (!SetInformationJobObject(job, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION, pointer, (uint)length))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "SetInformationJobObject failed");
        }
        finally
        {
            Marshal.FreeHGlobal(pointer);
        }
    }

    private void ReleaseHandles()
    {
        Close(thread);
        Close(process);
        Close(job);
        thread = IntPtr.Zero;
        process = IntPtr.Zero;
        job = IntPtr.Zero;
    }

    private static void Close(IntPtr handle)
    {
        if (handle != IntPtr.Zero)
            CloseHandle(handle);
    }
}
'@
$studio = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__CARBON_STUDIO_BASE64__'))
$loader = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__CARBON_LOADER_BASE64__'))
$buildVersion = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__CARBON_BUILD_VERSION_BASE64__'))
$dotnetRoot = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__CARBON_DOTNET_ROOT_BASE64__'))
$place = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__CARBON_PLACE_BASE64__'))
if (-not [IO.Path]::IsPathRooted($loader)) { throw "Carbon supplied a non-absolute RML loader path" }
if (-not [IO.Path]::IsPathRooted($dotnetRoot)) { throw "Carbon supplied a non-absolute .NET runtime path" }
$env:CARBON_RML_LOADER = $loader
$env:CARBON_RML_BUILD_VERSION = $buildVersion
$env:DOTNET_ROOT = $dotnetRoot
Remove-Item Env:CARBON_RML_LOADED_BUILD_VERSION -ErrorAction SilentlyContinue
$launch = [CarbonSuspendedStudio]::Start($studio, $place, __CARBON_MANAGED_PLACE__)
$completed = $false
try {
    [Console]::Out.WriteLine($launch.ProcessId)
    [Console]::Out.WriteLine($launch.StartedAtFileTime)
    [Console]::Out.Flush()
    $command = [Console]::In.ReadLine()
    if ($command -eq 'CARBON_STUDIO_LAUNCH_RESUME') {
        $launch.Resume()
        [Console]::Out.WriteLine('CARBON_STUDIO_LAUNCH_RESUMED')
        [Console]::Out.Flush()
        $command = [Console]::In.ReadLine()
    }
    if ($command -eq 'CARBON_STUDIO_LAUNCH_COMPLETE') {
        $launch.Release()
        $completed = $true
        exit 0
    }
    if ($command -eq 'CARBON_STUDIO_LAUNCH_ABORT') {
        exit 0
    }
    throw "Carbon did not authorize completion of the Roblox Studio launch"
} finally {
    if (-not $completed) {
        $launch.Abort()
    }
}
"#
	.replace("__CARBON_STUDIO_BASE64__", &encoded_studio)
	.replace("__CARBON_LOADER_BASE64__", &encoded_loader)
	.replace("__CARBON_BUILD_VERSION_BASE64__", &encoded_build_version)
	.replace("__CARBON_DOTNET_ROOT_BASE64__", &encoded_dotnet_root)
	.replace("__CARBON_PLACE_BASE64__", &encoded_place)
	.replace("__CARBON_MANAGED_PLACE__", managed_place)
}

pub fn wait_for_exit(process_id: u32) -> Result<()> {
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	{
		let script = format!(
			"$process = Get-Process -Id {process_id} -ErrorAction SilentlyContinue; if ($null -ne $process) {{ $process.WaitForExit() }}"
		);
		let status = Command::new("powershell.exe")
			.args(["-NoProfile", "-NonInteractive", "-Command", script.as_str()])
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status()
			.context("failed to start the Roblox Studio process monitor")?;
		anyhow::ensure!(status.success(), "Roblox Studio process monitor failed");
		Ok(())
	}

	#[cfg(not(any(target_os = "linux", target_os = "windows")))]
	{
		use std::{thread, time::Duration};
		loop {
			let status = Command::new("kill")
				.arg("-0")
				.arg(process_id.to_string())
				.stdin(Stdio::null())
				.stdout(Stdio::null())
				.stderr(Stdio::null())
				.status()
				.context("failed to inspect the Roblox Studio process")?;
			if !status.success() {
				return Ok(());
			}
			thread::sleep(Duration::from_millis(250));
		}
	}
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn powershell_verify_process_script(process_id: u32, studio_executable: &str, started_at_file_time: u64) -> String {
	let encoded_executable = BASE64_STANDARD.encode(studio_executable.as_bytes());
	r#"
$ErrorActionPreference = 'Stop'
$processId = [uint32]__CARBON_PROCESS_ID__
$startedAtFileTime = [uint64]__CARBON_STARTED_AT_FILE_TIME__
$expectedExecutable = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__CARBON_STUDIO_BASE64__'))
function Normalize-CarbonPath([string]$Path) {
    if ($Path.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) {
        $Path = '\\' + $Path.Substring(8)
    } elseif ($Path.StartsWith('\\?\', [StringComparison]::Ordinal)) {
        $Path = $Path.Substring(4)
    }
    return [IO.Path]::GetFullPath($Path).TrimEnd([IO.Path]::DirectorySeparatorChar)
}
$studioProcess = Get-Process -Id $processId -ErrorAction Stop
$actualStartedAt = [uint64]$studioProcess.StartTime.ToUniversalTime().ToFileTimeUtc()
if ($actualStartedAt -ne $startedAtFileTime) {
    Write-Error 'Roblox Studio process creation identity changed'
    exit 2
}
$actualExecutable = $studioProcess.Path
if ([string]::IsNullOrWhiteSpace($actualExecutable) -or
    -not [string]::Equals(
        (Normalize-CarbonPath $actualExecutable),
        (Normalize-CarbonPath $expectedExecutable),
        [StringComparison]::OrdinalIgnoreCase)) {
    Write-Error 'Roblox Studio executable identity changed'
    exit 3
}
"#
	.replace("__CARBON_PROCESS_ID__", &process_id.to_string())
	.replace("__CARBON_STARTED_AT_FILE_TIME__", &started_at_file_time.to_string())
	.replace("__CARBON_STUDIO_BASE64__", &encoded_executable)
}

fn verify_process_identity(process_id: u32, studio_executable: &str, started_at_file_time: u64) -> Result<()> {
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	{
		let script = powershell_verify_process_script(process_id, studio_executable, started_at_file_time);
		let output = Command::new("powershell.exe")
			.args(["-NoProfile", "-NonInteractive", "-Command", script.as_str()])
			.output()
			.context("failed to inspect the launched Roblox Studio process identity")?;
		anyhow::ensure!(
			output.status.success(),
			"launched Roblox Studio process identity no longer matches: {}",
			String::from_utf8_lossy(&output.stderr).trim()
		);
	}

	#[cfg(not(any(target_os = "linux", target_os = "windows")))]
	{
		let _ = (process_id, studio_executable, started_at_file_time);
	}
	Ok(())
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn powershell_terminate_script(process_id: u32, studio_executable: &str, started_at_file_time: u64) -> String {
	let encoded_executable = BASE64_STANDARD.encode(studio_executable.as_bytes());
	r#"
$processId = [uint32]__CARBON_PROCESS_ID__
$startedAtFileTime = [uint64]__CARBON_STARTED_AT_FILE_TIME__
$expectedExecutable = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__CARBON_STUDIO_BASE64__'))
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;

public static class CarbonStudioTerminator
{
    private const uint ProcessTerminate = 0x0001;
    private const uint ProcessQueryInformation = 0x0400;
    private const uint Synchronize = 0x00100000;
    private const uint WaitObject0 = 0x00000000;
    private const uint WaitTimeout = 0x00000102;
    private const int ErrorInvalidParameter = 87;

    [StructLayout(LayoutKind.Sequential)]
    private struct FileTime
    {
        public uint Low;
        public uint High;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr OpenProcess(uint access, bool inheritHandle, uint processId);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool QueryFullProcessImageName(
        IntPtr process,
        uint flags,
        StringBuilder executable,
        ref uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetProcessTimes(
        IntPtr process,
        out FileTime creation,
        out FileTime exit,
        out FileTime kernel,
        out FileTime user);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool TerminateProcess(IntPtr process, uint exitCode);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool CloseHandle(IntPtr handle);

    private static string NormalizePath(string path)
    {
        if (path.StartsWith(@"\\?\UNC\", StringComparison.OrdinalIgnoreCase))
        {
            path = @"\\" + path.Substring(8);
        }
        else if (path.StartsWith(@"\\?\", StringComparison.Ordinal))
        {
            path = path.Substring(4);
        }
        return Path.GetFullPath(path).TrimEnd(Path.DirectorySeparatorChar);
    }

    private static void Check(bool condition, string operation)
    {
        if (!condition)
        {
            throw new Win32Exception(Marshal.GetLastWin32Error(), operation);
        }
    }

    public static void Stop(uint processId, string expectedExecutable, ulong expectedStartedAt)
    {
        IntPtr process = OpenProcess(
            ProcessTerminate | ProcessQueryInformation | Synchronize,
            false,
            processId);
        if (process == IntPtr.Zero)
        {
            int error = Marshal.GetLastWin32Error();
            if (error == ErrorInvalidParameter)
            {
                return;
            }
            throw new Win32Exception(error, "OpenProcess");
        }

        try
        {
            if (WaitForSingleObject(process, 0) == WaitObject0)
            {
                return;
            }

            FileTime creation;
            FileTime exit;
            FileTime kernel;
            FileTime user;
            Check(GetProcessTimes(process, out creation, out exit, out kernel, out user), "GetProcessTimes");
            ulong startedAt = ((ulong)creation.High << 32) | creation.Low;
            if (startedAt != expectedStartedAt)
            {
                return;
            }

            var executable = new StringBuilder(32768);
            uint executableLength = (uint)executable.Capacity;
            Check(
                QueryFullProcessImageName(process, 0, executable, ref executableLength),
                "QueryFullProcessImageName");
            if (!string.Equals(
                NormalizePath(executable.ToString()),
                NormalizePath(expectedExecutable),
                StringComparison.OrdinalIgnoreCase))
            {
                throw new InvalidOperationException(
                    "Studio executable identity changed before shutdown");
            }

            if (!TerminateProcess(process, 1))
            {
                if (WaitForSingleObject(process, 0) == WaitObject0)
                {
                    return;
                }
                Check(false, "TerminateProcess");
            }
            uint wait = WaitForSingleObject(process, 30000);
            if (wait == WaitTimeout)
            {
                throw new TimeoutException("managed Roblox Studio process did not stop");
            }
            Check(wait == WaitObject0, "WaitForSingleObject");
        }
        finally
        {
            CloseHandle(process);
        }
    }
}
'@
[CarbonStudioTerminator]::Stop($processId, $expectedExecutable, $startedAtFileTime)
"#
	.replace("__CARBON_PROCESS_ID__", &process_id.to_string())
	.replace("__CARBON_STARTED_AT_FILE_TIME__", &started_at_file_time.to_string())
	.replace("__CARBON_STUDIO_BASE64__", &encoded_executable)
}

fn cleanup_stopped_bridge(process_id: u32, bridge_id: &str) -> Result<()> {
	let removed = crate::privileged_bridge::cleanup_bridge_discovery(process_id, bridge_id)
		.with_context(|| format!("failed to remove stale RML bridge records for Roblox Studio process {process_id}"))?;
	if removed > 0 {
		log::debug!("Removed {removed} stale RML bridge record(s) for Roblox Studio process {process_id}");
	}
	Ok(())
}

/// Stop exactly the Studio process returned by [`launch`]. Managed launchers
/// own their child process; using the native PID avoids title- or array-order
/// heuristics when several worktrees have Studio open simultaneously.
fn terminate_process(process_id: u32, studio_executable: &str, started_at_file_time: u64) -> Result<()> {
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	{
		let script = powershell_terminate_script(process_id, studio_executable, started_at_file_time);
		let output = Command::new("powershell.exe")
			.args(["-NoProfile", "-NonInteractive", "-Command", script.as_str()])
			.output()
			.context("failed to stop the managed Roblox Studio process")?;
		anyhow::ensure!(
			output.status.success(),
			"managed Roblox Studio process did not stop cleanly: {}",
			String::from_utf8_lossy(&output.stderr).trim()
		);
	}

	#[cfg(not(any(target_os = "linux", target_os = "windows")))]
	{
		let status = Command::new("kill")
			.arg(process_id.to_string())
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status()
			.context("failed to stop the managed Roblox Studio process")?;
		anyhow::ensure!(status.success(), "managed Roblox Studio process did not stop cleanly");
	}
	Ok(())
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn powershell_focus_script(process_id: u32) -> String {
	r#"
$process = Get-Process -Id __CARBON_STUDIO_PID__ -ErrorAction SilentlyContinue
if ($null -eq $process) { throw "Managed Roblox Studio process __CARBON_STUDIO_PID__ is not running" }
if ($process.ProcessName -ne 'RobloxStudioBeta') { throw "Managed process __CARBON_STUDIO_PID__ is not Roblox Studio" }
$window = $process.MainWindowHandle
if ($window -eq [IntPtr]::Zero) { throw "Roblox Studio process __CARBON_STUDIO_PID__ has no main window" }
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class CarbonStudioWindow {
    [DllImport("user32.dll")]
    public static extern bool ShowWindowAsync(IntPtr window, int command);
    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr window);
}
'@
[CarbonStudioWindow]::ShowWindowAsync($window, 9) | Out-Null
$focused = [CarbonStudioWindow]::SetForegroundWindow($window)
if (-not $focused) {
    $shell = New-Object -ComObject WScript.Shell
    $focused = $shell.AppActivate([int]$process.Id)
}
if (-not $focused) { throw "Windows denied foreground activation for Roblox Studio process __CARBON_STUDIO_PID__" }
"#
	.replace("__CARBON_STUDIO_PID__", &process_id.to_string())
}

/// Focus exactly the managed Roblox Studio process identified at launch.
/// Process-name validation prevents a recycled native PID from targeting an
/// unrelated application after Studio exits.
pub fn focus_process(process_id: u32) -> Result<()> {
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	{
		#[cfg(target_os = "linux")]
		anyhow::ensure!(
			std::env::var_os("WSL_DISTRO_NAME").is_some(),
			"Roblox Studio focus is supported on Linux only through WSL"
		);
		let script = powershell_focus_script(process_id);
		let output = Command::new("powershell.exe")
			.args(["-NoProfile", "-NonInteractive", "-Command", script.as_str()])
			.stdin(Stdio::null())
			.output()
			.context("failed to start the Roblox Studio window activator")?;
		anyhow::ensure!(
			output.status.success(),
			"Roblox Studio window activation failed: {}",
			String::from_utf8_lossy(&output.stderr).trim()
		);
		Ok(())
	}

	#[cfg(target_os = "macos")]
	{
		let script = format!(
			r#"tell application "System Events"
	set matches to every process whose unix id is {process_id} and name is "RobloxStudio"
	if (count of matches) is not 1 then error "Managed Roblox Studio process {process_id} is not running"
	tell item 1 of matches
		set frontmost to true
		perform action "AXRaise" of window 1
	end tell
end tell"#
		);
		let output = Command::new("osascript").args(["-e", script.as_str()]).output()?;
		anyhow::ensure!(
			output.status.success(),
			"Roblox Studio window activation failed: {}",
			String::from_utf8_lossy(&output.stderr).trim()
		);
		Ok(())
	}

	#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
	{
		let _ = process_id;
		anyhow::bail!("Roblox Studio window activation is not supported on this platform")
	}
}

#[allow(unused_variables)]
pub fn is_running(title: Option<String>) -> Result<bool> {
	#[cfg(target_os = "macos")]
	{
		let output = Command::new("osascript")
				.args([
					"-e",
					"tell app \"System Events\" to get the title of every window of (processes whose background only is false)",
				])
				.output()?;

		let windows = String::from_utf8(output.stdout)?;

		if let Some(title) = title {
			Ok(windows.contains(&format!("{title} - Roblox Studio")))
		} else {
			Ok(windows.contains("Roblox Studio"))
		}
	}

	#[cfg(target_os = "windows")]
	{
		let is_studio_running = EnumWindows(|hwnd| -> bool {
			if !hwnd.IsWindowVisible() {
				return true;
			}

			if let Ok(text) = hwnd.GetWindowText() {
				if let Some(title) = &title {
					if text == format!("{} - Roblox Studio", title) {
						return false;
					}
				} else if text.contains("Roblox Studio") {
					return false;
				}
			}

			true
		})
		.is_err();

		Ok(is_studio_running)
	}

	#[cfg(target_os = "linux")]
	{
		anyhow::bail!("This feature is not yet supported on Linux!");
	}
}

#[allow(unused_variables)]
pub fn focus(title: Option<String>) -> Result<()> {
	#[cfg(target_os = "macos")]
	{
		if let Some(title) = title {
			Command::new("osascript")
				.args([
					"-e",
					r#"tell application "System Events"
						repeat with theProcess in processes whose name is "RobloxStudio"
								tell theProcess
									set windowList to windows whose name contains "Carbon - Roblox Studio"
									
									if (count of windowList) > 0 then
										set frontmost to true
										perform action "AXRaise" of window 1
									end if
								end tell
						end repeat
					end tell"#,
				])
				.output()?;
		} else {
			Command::new("osascript")
				.args([
					"-e",
					r#"tell application "System Events"
						tell process "RobloxStudio"
							set frontmost to true
							perform action "AXRaise" of window 1
						end tell
					end tell"#,
				])
				.output()?;
		}

		Ok(())
	}

	#[cfg(target_os = "windows")]
	{
		let result = EnumWindows(|hwnd| -> bool {
			if !hwnd.IsWindowVisible() {
				return true;
			}

			if let Ok(text) = hwnd.GetWindowText() {
				if let Some(title) = &title {
					if text == format!("{} - Roblox Studio", title) {
						hwnd.SetForegroundWindow();
						hwnd.ShowWindow(SW::RESTORE);

						return false;
					}
				} else if text.contains("Roblox Studio") {
					hwnd.SetForegroundWindow();
					hwnd.ShowWindow(SW::RESTORE);

					return false;
				}
			}

			true
		});

		match result {
			Ok(()) => (),
			Err(err) => {
				if err.raw() != 0 {
					anyhow::bail!("Failed to focus Roblox Studio: {}", err)
				}
			}
		}

		Ok(())
	}

	#[cfg(target_os = "linux")]
	{
		anyhow::bail!("This feature is not yet supported on Linux!");
	}
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
	use super::*;
	use serde_json::json;
	use std::{
		io::{Read, Write},
		net::TcpListener,
	};

	fn fake_mcp_response(
		response: impl FnOnce(std::net::TcpStream) + Send + 'static,
	) -> (String, thread::JoinHandle<()>) {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let address = listener.local_addr().unwrap();
		let server = thread::spawn(move || {
			let (mut stream, _) = listener.accept().unwrap();
			let mut request = [0_u8; 4096];
			let _ = stream.read(&mut request).unwrap();
			response(stream);
		});
		(format!("http://{address}/mcp/manage_instance"), server)
	}

	fn write_http_response(mut stream: std::net::TcpStream, status: &str, body: &[u8]) {
		write!(
			stream,
			"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
			body.len()
		)
		.unwrap();
		stream.write_all(body).unwrap();
	}

	fn compatible_health_body() -> &'static [u8] {
		br#"{"status":"ok","service":"robloxstudio-mcp","serverName":"robloxstudio-mcp","capabilities":{"studioLifecycle":{"protocolVersion":3,"endpoint":"/mcp/manage_instance"}}}"#
	}

	#[test]
	fn automatic_wsl_lifecycle_is_suspended_direct_but_explicit_modes_are_honored() {
		assert!(prefer_suspended_direct_lifecycle(LifecyclePreference::Auto));
		assert!(!prefer_suspended_direct_lifecycle(LifecyclePreference::Mcp));
		assert!(!prefer_suspended_direct_lifecycle(LifecyclePreference::Direct));
	}

	#[test]
	fn native_windows_launch_paths_drop_verbatim_prefixes() {
		assert_eq!(
			windows_nonverbatim_path(r"\\?\C:\Carbon\rml\roblox_modloader.dll"),
			r"C:\Carbon\rml\roblox_modloader.dll"
		);
		assert_eq!(
			windows_nonverbatim_path(r"\\?\UNC\server\share\roblox_modloader.dll"),
			r"\\server\share\roblox_modloader.dll"
		);
	}

	#[test]
	fn mcp_close_failure_uses_only_the_captured_process_identity() {
		let observed = std::cell::RefCell::new(None);
		stop_mcp_owned_process(
			47_312,
			Some((r"C:\Roblox\RobloxStudioBeta.exe", 133_700_123_456)),
			|| Err(anyhow::anyhow!("broker unavailable")),
			|process_id, executable, started_at_file_time| {
				*observed.borrow_mut() = Some((process_id, executable.to_owned(), started_at_file_time));
				Ok(())
			},
		)
		.unwrap();

		assert_eq!(
			*observed.borrow(),
			Some((47_312, r"C:\Roblox\RobloxStudioBeta.exe".to_owned(), 133_700_123_456))
		);

		let error = stop_mcp_owned_process(
			47_312,
			Some((r"C:\Roblox\RobloxStudioBeta.exe", 133_700_123_456)),
			|| Err(anyhow::anyhow!("broker unavailable")),
			|_, _, _| Err(anyhow::anyhow!("identity-safe termination failed")),
		)
		.unwrap_err();
		let rendered = format!("{error:#}");
		assert!(rendered.contains("broker unavailable"));
		assert!(rendered.contains("identity-safe termination failed"));
	}
	#[test]
	fn managed_launch_failure_fake_broker_disconnects_after_compatible_discovery() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let base_url = format!("http://{}", listener.local_addr().unwrap());
		let server = thread::spawn(move || {
			let (mut health, _) = listener.accept().unwrap();
			let mut request = [0_u8; 4096];
			let _ = health.read(&mut request).unwrap();
			write_http_response(health, "200 OK", compatible_health_body());

			let (mut launch, _) = listener.accept().unwrap();
			let _ = launch.read(&mut request).unwrap();
			drop(launch);
		});

		let lifecycle = discover_mcp_lifecycle_at(LifecyclePreference::Auto, &base_url)
			.unwrap()
			.unwrap();
		let error = mcp_tool(
			&lifecycle.endpoint,
			None,
			&json!({"action": "launch"}),
			Duration::from_secs(1),
		)
		.unwrap_err();
		server.join().unwrap();
		let failure = ManagedLaunchFailure::from_request(error);
		assert_eq!(failure.stage, ManagedLaunchFailureStage::DispatchAmbiguous);
		assert!(!failure.safe_for_direct_fallback(lifecycle.preference));

		let rendered = format!("{:#}", anyhow::Error::new(failure));
		assert!(rendered.contains("robloxstudio-mcp was selected"));
		assert!(rendered.contains("may have been dispatched"));
		assert!(rendered.contains("automatic direct fallback was withheld"));
	}

	#[test]
	fn managed_launch_failure_reports_ambiguous_dispatch_and_recovery() {
		let (endpoint, server) = fake_mcp_response(drop);
		let error = mcp_tool(&endpoint, None, &json!({"action": "launch"}), Duration::from_secs(1)).unwrap_err();
		server.join().unwrap();
		let error = ManagedLaunchFailure::from_request(error);
		let rendered = format!("{:#}", anyhow::Error::new(error));

		assert!(rendered.contains("robloxstudio-mcp"));
		assert!(rendered.contains(&endpoint));
		assert!(rendered.contains("may have been dispatched"));
		assert!(rendered.contains("automatic direct fallback was withheld"));
		assert!(rendered.contains("CARBON_STUDIO_LIFECYCLE=direct carbon serve"));
	}

	#[test]
	fn managed_launch_failure_sanitizes_structured_broker_rejection() {
		let body = br#"{"error":"launch_rejected","message":"restart the lifecycle broker","secret":"do-not-print"}"#;
		let (endpoint, server) = fake_mcp_response(move |stream| {
			write_http_response(stream, "409 Conflict", body);
		});
		let error = mcp_tool(&endpoint, None, &json!({"action": "launch"}), Duration::from_secs(1)).unwrap_err();
		server.join().unwrap();
		let rendered = format!("{:#}", anyhow::Error::new(error));

		assert!(rendered.contains("409 Conflict"));
		assert!(rendered.contains("launch_rejected"));
		assert!(rendered.contains("restart the lifecycle broker"));
		assert!(!rendered.contains("do-not-print"));
	}

	#[test]
	fn managed_launch_failure_surfaces_structured_tool_rejection_without_unrelated_fields() {
		let result = json!({
			"error": "launch_rejected",
			"message": "restart the lifecycle broker",
			"private_path": "/private/place.rbxl"
		});
		let body = serde_json::to_vec(&json!({
			"content": [{"type": "text", "text": result.to_string()}],
			"isError": true
		}))
		.unwrap();
		let (endpoint, server) = fake_mcp_response(move |stream| {
			write_http_response(stream, "200 OK", &body);
		});
		let error = mcp_tool(&endpoint, None, &json!({"action": "launch"}), Duration::from_secs(1)).unwrap_err();
		server.join().unwrap();
		let rendered = format!("{:#}", anyhow::Error::new(error));

		assert!(rendered.contains("launch_rejected"));
		assert!(rendered.contains("restart the lifecycle broker"));
		assert!(!rendered.contains("/private/place.rbxl"));
	}

	#[test]
	fn managed_launch_failure_classifies_malformed_response_after_dispatch() {
		let (endpoint, server) = fake_mcp_response(|stream| {
			write_http_response(stream, "200 OK", b"not-json");
		});
		let error = mcp_tool(&endpoint, None, &json!({"action": "launch"}), Duration::from_secs(1)).unwrap_err();
		server.join().unwrap();
		let rendered = format!("{:#}", anyhow::Error::new(error));

		assert!(rendered.contains(&endpoint));
		assert!(rendered.contains("response was invalid JSON"));
		assert!(rendered.contains("after request dispatch"));
	}

	#[test]
	fn managed_launch_failure_classifies_response_timeout_as_ambiguous() {
		let (endpoint, server) = fake_mcp_response(|_stream| {
			thread::sleep(Duration::from_millis(150));
		});
		let error = mcp_tool(&endpoint, None, &json!({"action": "launch"}), Duration::from_millis(30)).unwrap_err();
		server.join().unwrap();

		assert_eq!(error.stage, McpRequestFailureStage::DispatchAmbiguous);
		assert!(format!("{:#}", anyhow::Error::new(error)).contains("timed out"));
	}

	#[test]
	fn managed_launch_failure_only_allows_safe_auto_fallback_before_dispatch() {
		let failure = |stage| ManagedLaunchFailure {
			endpoint: DEFAULT_MCP_URL.to_owned(),
			stage,
			cause: anyhow::anyhow!("synthetic failure"),
		};

		assert!(failure(ManagedLaunchFailureStage::PreDispatch).safe_for_direct_fallback(LifecyclePreference::Auto));
		assert!(!failure(ManagedLaunchFailureStage::PreDispatch).safe_for_direct_fallback(LifecyclePreference::Mcp));
		assert!(
			!failure(ManagedLaunchFailureStage::DispatchAmbiguous).safe_for_direct_fallback(LifecyclePreference::Auto)
		);
		assert!(!failure(ManagedLaunchFailureStage::BrokerRejected).safe_for_direct_fallback(LifecyclePreference::Auto));
		assert!(
			!failure(ManagedLaunchFailureStage::InvalidResponse).safe_for_direct_fallback(LifecyclePreference::Auto)
		);
	}

	#[test]
	fn managed_launch_failure_connection_refusal_is_provably_pre_dispatch() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let endpoint = format!("http://{}/mcp/manage_instance", listener.local_addr().unwrap());
		drop(listener);
		let error = mcp_tool(
			&endpoint,
			None,
			&json!({"action": "launch"}),
			Duration::from_millis(100),
		)
		.unwrap_err();

		assert_eq!(error.stage, McpRequestFailureStage::PreDispatch);
		let failure = ManagedLaunchFailure::from_request(error);
		assert!(failure.safe_for_direct_fallback(LifecyclePreference::Auto));
	}

	#[test]
	fn managed_launch_failure_discovery_unavailable_retains_auto_direct_selection() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let base_url = format!("http://{}", listener.local_addr().unwrap());
		drop(listener);

		assert!(discover_mcp_lifecycle_at(LifecyclePreference::Auto, &base_url)
			.unwrap()
			.is_none());
		let error = discover_mcp_lifecycle_at(LifecyclePreference::Mcp, &base_url)
			.err()
			.unwrap();
		let rendered = format!("{error:#}");
		assert!(rendered.contains("CARBON_STUDIO_LIFECYCLE=mcp"));
		assert!(rendered.contains("was unavailable"));
	}

	#[test]
	fn managed_launch_failure_invalid_discovery_response_is_actionable_in_mcp_mode() {
		let (base_url, server) = fake_mcp_response(|stream| {
			write_http_response(stream, "200 OK", b"not-json");
		});
		let error = discover_mcp_lifecycle_at(LifecyclePreference::Mcp, &base_url)
			.err()
			.unwrap();
		server.join().unwrap();

		assert!(format!("{error:#}").contains("robloxstudio-mcp health returned invalid JSON"));
	}

	#[test]
	fn managed_launch_failure_invalid_lifecycle_url_is_actionable() {
		let error = discover_mcp_lifecycle_at(LifecyclePreference::Mcp, "https://example.com")
			.err()
			.unwrap();
		let rendered = format!("{error:#}");

		assert!(rendered.contains("invalid robloxstudio-mcp URL"));
		assert!(rendered.contains("must use http"));
	}

	#[test]
	fn compatible_mcp_owns_managed_studio_lifecycle() {
		let health = json!({
			"status": "ok",
			"service": "robloxstudio-mcp",
			"serverName": "robloxstudio-mcp",
			"capabilities": {
				"studioLifecycle": {
					"protocolVersion": 3,
					"endpoint": "/mcp/manage_instance"
				}
			}
		});

		assert_eq!(
			mcp_lifecycle_endpoint("http://127.0.0.1:58741", &health).as_deref(),
			Some("http://127.0.0.1:58741/mcp/manage_instance")
		);
	}

	#[test]
	fn lifecycle_discovery_rejects_other_servers_and_protocols() {
		let mut health = json!({
			"status": "ok",
			"service": "robloxstudio-mcp",
			"serverName": "robloxstudio-mcp-inspector",
			"capabilities": {
				"studioLifecycle": {
					"protocolVersion": 3,
					"endpoint": "/mcp/manage_instance"
				}
			}
		});
		assert!(mcp_lifecycle_endpoint(DEFAULT_MCP_URL, &health).is_none());

		health["serverName"] = json!("robloxstudio-mcp");
		health["capabilities"]["studioLifecycle"]["protocolVersion"] = json!(1);
		assert!(mcp_lifecycle_endpoint(DEFAULT_MCP_URL, &health).is_none());

		health["capabilities"]["studioLifecycle"]["protocolVersion"] = json!(3);
		assert!(mcp_lifecycle_endpoint("http://192.0.2.1:58741", &health).is_none());
	}

	#[test]
	fn managed_launch_transfers_exact_rml_environment() {
		let payload = mcp_launch_request(
			std::path::Path::new("/tmp/carbon-managed.rbxl"),
			r"C:\Roblox\RobloxStudioBeta.exe",
			r"C:\Carbon\RobloxModLoader\roblox_modloader.dll",
			"0.0.0+build.test",
			Some(r"C:\Users\builder\.dotnet"),
		);

		assert_eq!(payload["action"], "launch");
		assert_eq!(payload["source"], "local_file");
		assert_eq!(payload["local_place_file"], "/tmp/carbon-managed.rbxl");
		assert_eq!(
			payload["process_environment"]["set"][rml::LOADER_ENV],
			r"C:\Carbon\RobloxModLoader\roblox_modloader.dll"
		);
		assert_eq!(
			payload["process_environment"]["set"][rml::EXPECTED_BUILD_ENV],
			"0.0.0+build.test"
		);
		assert_eq!(
			payload["process_environment"]["set"]["DOTNET_ROOT"],
			r"C:\Users\builder\.dotnet"
		);
		assert_eq!(payload["process_environment"]["remove"], json!([rml::LOADED_BUILD_ENV]));
		assert_eq!(payload["wait_for_connection"], false);
		assert_eq!(payload["require_process_identity"], true);
	}

	#[test]
	fn managed_launch_keeps_place_path_in_broker_namespace() {
		let place = PathBuf::from("/tmp/CarbonQualification-managed.rbxl");
		assert_eq!(mcp_broker_place_path(&place), place.as_path());
	}

	#[test]
	fn mcp_launch_requires_a_suspended_authorization_boundary() {
		let response = json!({
			"launch_id": "launch-starting",
			"managed": true,
			"source": "local_file",
			"state": "launching",
			"pid": 47312,
			"process_started_at_file_time": "133700123456",
			"process_running": true,
			"process_authorized": false
		});

		assert_eq!(
			managed_launch_identity(&response).unwrap(),
			("launch-starting".to_owned(), 47312, Some(133_700_123_456))
		);
		let mut already_authorized = response.clone();
		already_authorized["process_authorized"] = json!(true);
		assert!(managed_launch_identity(&already_authorized).is_err());
		let mut missing_creation_time = response;
		missing_creation_time
			.as_object_mut()
			.unwrap()
			.remove("process_started_at_file_time");
		assert!(managed_launch_identity(&missing_creation_time).is_err());
	}

	#[test]
	fn mcp_injection_requires_the_exact_running_launch_identity() {
		let mut status = json!({
			"launch_id": "launch-owned",
			"managed": true,
			"source": "local_file",
			"state": "launching",
			"pid": 47312,
			"process_started_at_file_time": "133700123456",
			"process_running": true,
			"process_authorized": true
		});
		assert!(managed_injection_status_has_identity(
			&status,
			"launch-owned",
			47_312,
			Some(133_700_123_456)
		));

		status["process_running"] = Value::Null;
		assert!(!managed_injection_status_has_identity(
			&status,
			"launch-owned",
			47_312,
			Some(133_700_123_456)
		));
		status["process_running"] = json!(true);
		status["process_authorized"] = json!(false);
		assert!(!managed_injection_status_has_identity(
			&status,
			"launch-owned",
			47_312,
			Some(133_700_123_456)
		));
		status["process_authorized"] = json!(true);
		status["state"] = json!("failed");
		assert!(!managed_injection_status_has_identity(
			&status,
			"launch-owned",
			47_312,
			Some(133_700_123_456)
		));
	}

	#[test]
	fn mcp_completion_requires_exact_identity_and_explicit_ownership_release() {
		let mut status = json!({
			"launch_id": "launch-owned",
			"managed": true,
			"source": "local_file",
			"state": "launching",
			"pid": 47312,
			"process_started_at_file_time": "133700123456",
			"process_running": true,
			"process_authorized": true,
			"process_ownership_released": true
		});
		assert!(managed_completion_status_has_identity(
			&status,
			"launch-owned",
			47_312,
			Some(133_700_123_456)
		));

		status["process_ownership_released"] = json!(false);
		assert!(!managed_completion_status_has_identity(
			&status,
			"launch-owned",
			47_312,
			Some(133_700_123_456)
		));
		status["process_ownership_released"] = json!(true);
		status["pid"] = json!(47313);
		assert!(!managed_completion_status_has_identity(
			&status,
			"launch-owned",
			47_312,
			Some(133_700_123_456)
		));
	}

	#[test]
	fn mcp_managed_studio_reports_manage_instance_identity() {
		let status = json!({
			"launch_id": "launch-strain",
			"instance_id": "anon:strain",
			"managed": true,
			"state": "connected",
			"pid": 47312,
			"process_running": true,
			"roles": ["edit"]
		});

		assert_eq!(mcp_instance_id(&status, "launch-strain").unwrap(), "anon:strain");

		let attempts = std::cell::Cell::new(0);
		let resolved = resolve_mcp_instance_id(
			"launch-strain",
			Instant::now() + Duration::from_secs(1),
			|| {
				attempts.set(attempts.get() + 1);
				if attempts.get() == 1 {
					anyhow::bail!("operation timed out");
				}
				Ok(status.clone())
			},
			|| {},
		)
		.unwrap();

		assert_eq!(attempts.get(), 2);
		assert_eq!(resolved, "anon:strain");
	}

	#[test]
	fn merge_stress_regression_direct_studio_resolves_exact_mcp_instance_by_data_model() {
		let attempts = std::cell::Cell::new(0);
		let resolved = resolve_direct_mcp_instance_id(
			"carbon-serve-target.rbxl",
			Instant::now() + Duration::from_secs(1),
			|| {
				attempts.set(attempts.get() + 1);
				let instances = if attempts.get() == 1 {
					json!([{
						"instanceId": "anon:other",
						"role": "edit",
						"dataModelName": "carbon-serve-other.rbxl"
					}])
				} else {
					json!([
						{
							"instanceId": "anon:target-mcp",
							"role": "edit",
							"dataModelName": "carbon-serve-target.rbxl"
						},
						{
							"instanceId": "anon:target-runtime",
							"role": "server",
							"dataModelName": "carbon-serve-target.rbxl"
						}
					])
				};
				Ok(Some(json!({
					"status": "ok",
					"service": "robloxstudio-mcp",
					"serverName": "robloxstudio-mcp",
					"instances": instances
				})))
			},
			|| {},
		)
		.unwrap();

		assert_eq!(attempts.get(), 2);
		assert_eq!(resolved.as_deref(), Some("anon:target-mcp"));
	}

	#[test]
	fn powershell_launch_suspends_one_studio_until_its_rml_package_is_injected() {
		let studio = r#"C:\Roblox\Versions\worktree'; throw 'studio\RobloxStudioBeta.exe"#;
		let loader = r#"C:\Carbon\rml\worktree'; throw 'loader\RobloxModLoader\roblox_modloader.dll"#;
		let build = "0.0.0+worktree'; throw 'build";
		let dotnet_root = r#"C:\Users\builder\worktree'; throw 'dotnet\.dotnet"#;
		let place = r#"C:\places\worktree'; throw 'place.rbxl"#;
		let script = powershell_launch_script(studio, loader, build, dotnet_root, place, true);
		let model_script = powershell_launch_script(studio, loader, build, dotnet_root, place, false);

		for untrusted in [studio, loader, build, dotnet_root, place] {
			assert!(!script.contains(untrusted));
		}
		assert!(script.contains("$env:CARBON_RML_LOADER = $loader"));
		assert!(script.contains("$env:CARBON_RML_BUILD_VERSION = $buildVersion"));
		assert!(script.contains("$env:DOTNET_ROOT = $dotnetRoot"));
		assert!(script.contains("Remove-Item Env:CARBON_RML_LOADED_BUILD_VERSION"));
		assert!(script.contains("CreateProcessW"));
		assert!(script.contains("CREATE_SUSPENDED"));
		assert!(script.contains("JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE"));
		assert!(script
			.contains("GetProcessTimes(processInformation.hProcess, out creation, out exit, out kernel, out user)"));
		assert!(script.contains("[Console]::Out.WriteLine($launch.StartedAtFileTime)"));
		assert!(script.contains("$launch = [CarbonSuspendedStudio]::Start($studio, $place, $true)"));
		assert!(model_script.contains("$launch = [CarbonSuspendedStudio]::Start($studio, $place, $false)"));
		assert!(model_script.contains("command += \" \" + Quote(place);"));
		assert!(script.contains("$launch.Resume()"));
		assert!(script.contains("CARBON_STUDIO_LAUNCH_RESUMED"));
		assert!(script.contains("$launch.Release()"));
		assert!(!script.contains("Start-Process"));
		assert!(!script.contains("Get-ChildItem"));
		assert!(script.contains("[Console]::In.ReadLine()"));
		assert!(script.contains("CARBON_STUDIO_LAUNCH_COMPLETE"));
		assert!(script.contains("CARBON_STUDIO_LAUNCH_ABORT"));
		assert!(script.contains("if (!TerminateProcess(process, 1))"));
		assert!(script.contains("if (state == WAIT_TIMEOUT)"));
		assert!(script.contains("TerminateAndWait(processInformation.hProcess)"));
	}

	#[cfg(any(target_os = "linux", target_os = "windows"))]
	#[test]
	fn powershell_shutdown_uses_the_identity_checked_process_handle() {
		let studio = r#"\\?\C:\Roblox\Versions\unsafe'; throw 'Studio\RobloxStudioBeta.exe"#;
		let script = powershell_terminate_script(47_312, studio, 133_700_123_456);

		assert!(!script.contains(studio));
		assert!(script.contains("$processId = [uint32]47312"));
		assert!(script.contains("$startedAtFileTime = [uint64]133700123456"));
		assert!(script.contains("if (startedAt != expectedStartedAt)\n            {\n                return;"));
		assert!(script.contains(r#"path.StartsWith(@"\\?\UNC\""#));
		for operation in [
			"OpenProcess",
			"GetProcessTimes",
			"QueryFullProcessImageName",
			"TerminateProcess(process, 1)",
			"WaitForSingleObject(process, 30000)",
			"CloseHandle(process)",
		] {
			assert!(script.contains(operation), "missing {operation}");
		}
		assert!(!script.contains("Stop-Process"));
	}

	#[cfg(any(target_os = "linux", target_os = "windows"))]
	#[test]
	fn powershell_attestation_revalidates_the_exact_process_identity() {
		let studio = r#"\\?\C:\Roblox\Versions\unsafe'; throw 'Studio\RobloxStudioBeta.exe"#;
		let script = powershell_verify_process_script(47_312, studio, 133_700_123_456);

		assert!(!script.contains(studio));
		assert!(script.contains(&BASE64_STANDARD.encode(studio.as_bytes())));
		assert!(script.contains("$processId = [uint32]47312"));
		assert!(script.contains("$startedAtFileTime = [uint64]133700123456"));
		assert!(script.contains("Get-Process -Id $processId -ErrorAction Stop"));
		assert!(script.contains("StartTime.ToUniversalTime().ToFileTimeUtc()"));
		assert!(script.contains("$studioProcess.Path"));
		assert!(script.contains("Normalize-CarbonPath"));
		assert!(!script.contains("Stop-Process"));
	}

	#[cfg(any(target_os = "linux", target_os = "windows"))]
	#[test]
	fn powershell_dotnet_discovery_matches_the_rml_runtime_contract() {
		let loader = r#"\\?\C:\Carbon\rml\worktree'; throw 'loader\RobloxModLoader\roblox_modloader.dll"#;
		let script = windows_dotnet_root_script(loader);

		assert!(!script.contains(loader));
		assert!(script.contains("runtime\\RML.runtimeconfig.json"));
		assert!(script.contains("[System.IO.Path]::GetDirectoryName($loader)"));
		assert!(!script.contains("Split-Path -Parent $loader"));
		assert!(script.contains("Microsoft.NETCore.App"));
		assert!(script.contains("$candidateVersion.Major -eq $requiredVersion.Major"));
		assert!(script.contains("$candidateVersion -ge $requiredVersion"));
		assert!(script.contains("host\\fxr\\*\\hostfxr.dll"));
	}

	#[test]
	fn powershell_focus_is_pinned_to_the_managed_studio_pid() {
		let script = powershell_focus_script(47_312);

		assert!(script.contains("Get-Process -Id 47312"));
		assert!(script.contains("$process.ProcessName -ne 'RobloxStudioBeta'"));
		assert!(script.contains("$process.MainWindowHandle"));
		assert!(script.contains("SetForegroundWindow($window)"));
		assert!(script.contains("AppActivate([int]$process.Id)"));
		assert!(!script.contains("MainWindowTitle"));
		assert!(!script.contains("EnumWindows"));
	}
}
