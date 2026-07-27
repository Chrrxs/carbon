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
		helper_path: PathBuf,
	},
	Mcp {
		endpoint: String,
		auth_token: Option<String>,
		launch_id: String,
		process_id: u32,
		studio_executable: String,
		started_at_file_time: Option<u64>,
		helper_path: PathBuf,
	},
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FocusMetadata {
	pub studio_executable: String,
	pub creation_filetime: u64,
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
	pub fn focus_metadata(&self) -> Option<FocusMetadata> {
		#[cfg(any(target_os = "linux", target_os = "windows"))]
		{
			match &self.lifecycle {
				ManagedStudioLifecycle::Direct {
					studio_executable,
					started_at_file_time,
					..
				} => Some(FocusMetadata {
					studio_executable: studio_executable.clone(),
					creation_filetime: *started_at_file_time,
				}),
				ManagedStudioLifecycle::Mcp {
					studio_executable,
					started_at_file_time: Some(started_at_file_time),
					..
				} => Some(FocusMetadata {
					studio_executable: studio_executable.clone(),
					creation_filetime: *started_at_file_time,
				}),
				ManagedStudioLifecycle::Mcp {
					started_at_file_time: None,
					..
				} => None,
			}
		}

		#[cfg(not(any(target_os = "linux", target_os = "windows")))]
		{
			None
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
				helper_path,
				..
			} => terminate_process(helper_path, *process_id, studio_executable, *started_at_file_time),
			ManagedStudioLifecycle::Mcp {
				endpoint,
				auth_token,
				launch_id,
				process_id,
				studio_executable,
				started_at_file_time,
				helper_path,
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
				|pid, exe, ft| terminate_process(helper_path, pid, exe, ft),
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
	let expected_bridge_id = rml_launch.bridge_id().to_owned();
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
		launched.verify_identity()?;
		let bridge = attest_loaded_rml_bridge(&expected_bridge_id, process_id)?;
		launched.verify_identity()?;
		bridge_id = Some(bridge.bridge_id().to_owned());
		if require_engine_ready {
			launched.verify_identity()?;
			attest_ready_rml_bridge(&bridge, process_id)?;
			launched.verify_identity()?;
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
	let helper_path = rml_launch.helper_path().to_path_buf();
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
			helper_path,
		},
		launched,
	))
}

fn should_discover_mcp_lifecycle(preference: LifecyclePreference) -> bool {
	preference != LifecyclePreference::Direct
}

pub fn launch_managed(path: PathBuf, studio_dir: &Path) -> Result<ManagedStudio> {
	let (rml_launch, plugin_launch) = prepare_launch(Some(studio_dir))?;
	let loader_path = rml_launch.loader_path().to_owned();
	let expected_bridge_id = rml_launch.bridge_id().to_owned();
	let (studio_executable, _) = mcp_launch_paths(&rml_launch)?;
	let mut direct_launch = None;
	let preference = LifecyclePreference::from_env()?;
	let mcp_lifecycle = if should_discover_mcp_lifecycle(preference) {
		discover_mcp_lifecycle()?
	} else {
		log::debug!("Selected suspended direct Studio lifecycle because {LIFECYCLE_ENV}=direct");
		None
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
		if let Some(launched) = direct_launch.as_mut() {
			launched.verify_identity()?;
		}
		let bridge = attest_loaded_rml_bridge(&expected_bridge_id, process_id)?;
		if let Some(launched) = direct_launch.as_mut() {
			launched.verify_identity()?;
		}
		managed.bridge_id = Some(bridge.bridge_id().to_owned());
		if let Some(launched) = direct_launch.as_mut() {
			launched.verify_identity()?;
		}
		attest_ready_rml_bridge(&bridge, process_id)?;
		if let Some(launched) = direct_launch.as_mut() {
			launched.verify_identity()?;
		}
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
	// Protocol v3 requires the exact process to remain suspended until Carbon
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

fn attest_loaded_rml_bridge(bridge_id: &str, process_id: u32) -> Result<crate::privileged_bridge::Bridge> {
	crate::privileged_bridge::Bridge::wait_for_loaded_process(bridge_id, process_id, Duration::from_secs(30))
		.with_context(|| format!("RML bridge for Roblox Studio process {process_id} did not attest this Carbon build"))
}

fn attest_ready_rml_bridge(bridge: &crate::privileged_bridge::Bridge, process_id: u32) -> Result<()> {
	bridge
		.wait_until_process_ready(process_id, Duration::from_secs(30))
		.with_context(|| format!("RML bridge for Roblox Studio process {process_id} did not attest this Carbon build"))
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
		rml_launch.bridge_id(),
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
		helper_path: rml_launch.helper_path().to_path_buf(),
	})
}

fn mcp_launch_request(
	path: &std::path::Path,
	studio: &str,
	loader: &str,
	build_version: &str,
	bridge_id: &str,
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
				rml::EXPECTED_BUILD_ENV: build_version,
				rml::BRIDGE_ID_ENV: bridge_id
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
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	helper_path: Option<PathBuf>,
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

	fn verify_identity(&mut self) -> Result<()> {
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
			writeln!(stdin, "CARBON_STUDIO_LAUNCH_VERIFY")
				.context("failed to request Roblox Studio identity verification")?;
			let stdout = child
				.stdout
				.take()
				.context("Roblox Studio launcher stdout is unavailable")?;
			let mut stdout = BufReader::new(stdout);
			let mut response = String::new();
			let read_result = stdout.read_line(&mut response);
			child.stdout = Some(stdout.into_inner());
			read_result.context("failed to read Roblox Studio identity verification response")?;
			anyhow::ensure!(
				response.trim() == "CARBON_STUDIO_LAUNCH_VERIFIED",
				"Roblox Studio launcher failed process identity verification"
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
		let (Some(studio_executable), Some(started_at_file_time), Some(helper_path)) =
			(&self.studio_executable, self.started_at_file_time, &self.helper_path)
		else {
			anyhow::bail!("Roblox Studio process identity is unavailable");
		};
		terminate_process(helper_path, self.process_id, studio_executable, started_at_file_time)
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
		let encoded_studio = BASE64_STANDARD.encode(windows_studio.as_bytes());
		let encoded_place = BASE64_STANDARD.encode(windows_place.as_bytes());
		let managed_str = if managed_place { "1" } else { "0" };
		let encoded_loader = BASE64_STANDARD.encode(windows_loader.as_bytes());
		let encoded_build = BASE64_STANDARD.encode(rml_launch.build_version().as_bytes());
		let encoded_dotnet_root = BASE64_STANDARD.encode(windows_dotnet_root.as_bytes());
		let encoded_bridge_id = BASE64_STANDARD.encode(rml_launch.bridge_id().as_bytes());
		let mut child = Command::new(rml_launch.helper_path())
			.args([
				"launch",
				&encoded_studio,
				&encoded_place,
				managed_str,
				&encoded_loader,
				&encoded_build,
				&encoded_dotnet_root,
				&encoded_bridge_id,
			])
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
					anyhow::Error::new(error).context("native helper returned an invalid Roblox Studio process ID");
				return Err(reap_failed_launcher(child, error));
			}
		};
		let started_at_file_time = match started_at_file_time.trim().parse::<u64>() {
			Ok(started_at_file_time) if started_at_file_time > 0 => started_at_file_time,
			Ok(_) => {
				return Err(reap_failed_launcher(
					child,
					anyhow::anyhow!("native helper returned an invalid Roblox Studio process creation time"),
				));
			}
			Err(error) => {
				let error = anyhow::Error::new(error)
					.context("native helper returned an invalid Roblox Studio process creation time");
				return Err(reap_failed_launcher(child, error));
			}
		};
		Ok(Some(DirectLaunch {
			process_id,
			child: Some(child),
			studio_executable: None,
			started_at_file_time: Some(started_at_file_time),
			helper_path: Some(rml_launch.helper_path().to_path_buf()),
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
			.env(rml::BRIDGE_ID_ENV, rml_launch.bridge_id())
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
if (-not [string]::IsNullOrWhiteSpace($env:DOTNET_ROOT_X64)) {
    $candidates += $env:DOTNET_ROOT_X64
}
if (-not [string]::IsNullOrWhiteSpace($env:ProgramW6432)) {
    $candidates += Join-Path $env:ProgramW6432 'dotnet'
}
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
	let output = powershell_command()?
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

pub fn wait_for_exit(process_id: u32) -> Result<()> {
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	{
		let script = format!(
			"$process = Get-Process -Id {process_id} -ErrorAction SilentlyContinue; if ($null -ne $process) {{ $process.WaitForExit() }}"
		);
		let status = powershell_command()?
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
fn terminate_process(
	helper_path: &Path,
	process_id: u32,
	studio_executable: &str,
	started_at_file_time: u64,
) -> Result<()> {
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	{
		let encoded_executable = BASE64_STANDARD.encode(studio_executable.as_bytes());
		let output = Command::new(helper_path)
			.args([
				"terminate",
				&process_id.to_string(),
				&started_at_file_time.to_string(),
				&encoded_executable,
			])
			.stdin(Stdio::null())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
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
		let _ = helper_path;
		let _ = studio_executable;
		let _ = started_at_file_time;
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

/// Focus exactly the managed Roblox Studio process identified at launch.
/// Process-name validation prevents a recycled native PID from targeting an
/// unrelated application after Studio exits.
pub fn focus_process(process_id: u32, creation_filetime: Option<u64>, studio_executable: Option<&str>) -> Result<()> {
	let metadata = match (creation_filetime, studio_executable) {
		(Some(creation_filetime), Some(studio_executable)) => Some((creation_filetime, studio_executable)),
		(None, None) => None,
		_ => {
			anyhow::bail!(
				"incomplete focus metadata: creation_filetime and studio_executable must both be present or both absent"
			);
		}
	};

	#[cfg(target_os = "windows")]
	{
		let (creation_filetime, studio_executable) = metadata.context(
			"incomplete focus metadata: creation_filetime and studio_executable are required for Roblox Studio focus",
		)?;
		crate::studio_windows::focus_process(process_id, creation_filetime, studio_executable)
	}

	#[cfg(target_os = "linux")]
	{
		anyhow::ensure!(
			std::env::var_os("WSL_DISTRO_NAME").is_some(),
			"Roblox Studio focus is supported on Linux only through WSL"
		);
		let (creation_filetime, studio_executable) = metadata.context(
			"incomplete focus metadata: creation_filetime and studio_executable are required for Roblox Studio focus",
		)?;
		let helper_path = crate::rml::helper_path()?;
		let encoded_executable = BASE64_STANDARD.encode(studio_executable.as_bytes());
		let output = Command::new(&helper_path)
			.args([
				"focus",
				&process_id.to_string(),
				&creation_filetime.to_string(),
				&encoded_executable,
			])
			.stdin(Stdio::null())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.output()
			.context("failed to focus the Roblox Studio window")?;
		anyhow::ensure!(
			output.status.success(),
			"Roblox Studio window activation failed: {}",
			String::from_utf8_lossy(&output.stderr).trim()
		);
		Ok(())
	}

	#[cfg(target_os = "macos")]
	{
		let _ = metadata;
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
		let _ = metadata;
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
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) fn powershell_command() -> Result<Command> {
	Ok(Command::new(powershell_path()?))
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) fn powershell_path() -> Result<PathBuf> {
	#[cfg(target_os = "linux")]
	{
		use std::sync::OnceLock;
		static RESOLVED_PATH: OnceLock<Result<PathBuf, String>> = OnceLock::new();
		let result = RESOLVED_PATH.get_or_init(|| {
			let output = Command::new("cmd.exe")
				.args(["/c", "echo", "%SystemRoot%"])
				.output()
				.map_err(|e| format!("failed to execute cmd.exe to resolve SystemRoot: {e}"))?;
			if !output.status.success() {
				return Err(format!(
					"cmd.exe failed to resolve SystemRoot: {}",
					String::from_utf8_lossy(&output.stderr).trim()
				));
			}
			let win_root =
				String::from_utf8(output.stdout).map_err(|e| format!("cmd.exe output invalid UTF-8: {e}"))?;
			let win_root = win_root.trim();
			if win_root.is_empty() || win_root.starts_with('%') {
				return Err("cmd.exe returned empty or unexpanded SystemRoot".to_string());
			}
			let syswow64 = format!(r"{win_root}\SysWOW64\WindowsPowerShell\v1.0\powershell.exe");
			let wsl_output = Command::new("wslpath")
				.arg(&syswow64)
				.output()
				.map_err(|e| format!("failed to run wslpath for SysWOW64 PowerShell: {e}"))?;
			if !wsl_output.status.success() {
				return Err(format!(
					"wslpath failed to translate SysWOW64 PowerShell path: {}",
					String::from_utf8_lossy(&wsl_output.stderr).trim()
				));
			}
			let lpath_str =
				String::from_utf8(wsl_output.stdout).map_err(|e| format!("wslpath output invalid UTF-8: {e}"))?;
			let lpath = PathBuf::from(lpath_str.trim());
			if !lpath.is_file() {
				return Err(format!(
					"resolved 32-bit SysWOW64 PowerShell executable does not exist: {}",
					lpath.display()
				));
			}
			Ok(lpath)
		});
		match result {
			Ok(path) => Ok(path.clone()),
			Err(err) => anyhow::bail!("{err}"),
		}
	}

	#[cfg(target_os = "windows")]
	{
		let sys_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
		let wow64_path = PathBuf::from(format!(r"{sys_root}\SysWOW64\WindowsPowerShell\v1.0\powershell.exe"));
		if wow64_path.is_file() {
			Ok(wow64_path)
		} else {
			let sys32_path = PathBuf::from(format!(r"{sys_root}\System32\WindowsPowerShell\v1.0\powershell.exe"));
			if sys32_path.is_file() {
				Ok(sys32_path)
			} else {
				anyhow::bail!(
					"PowerShell executable not found in SysWOW64 or System32 under {}",
					sys_root
				);
			}
		}
	}
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
	use super::*;
	use serde_json::json;
	use std::{
		io::{Read, Write},
		net::TcpListener,
		sync::Mutex,
	};

	static FOCUS_ENV_LOCK: Mutex<()> = Mutex::new(());

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
	fn automatic_wsl_lifecycle_does_not_bypass_suspended_mcp() {
		assert!(should_discover_mcp_lifecycle(LifecyclePreference::Auto));
		assert!(should_discover_mcp_lifecycle(LifecyclePreference::Mcp));
		assert!(!should_discover_mcp_lifecycle(LifecyclePreference::Direct));
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
			"0123456789abcdef0123456789abcdef",
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
			payload["process_environment"]["set"][rml::BRIDGE_ID_ENV],
			"0123456789abcdef0123456789abcdef"
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
	fn rml_helper_launch_args_contract() {
		let studio = r"C:\Roblox\version\RobloxStudioBeta.exe";
		let place = r"C:\places\place.rbxl";
		let loader = r"C:\Carbon\RobloxModLoader.dll";
		let build = "26.7.252257";
		let dotnet_root = r"C:\Program Files\dotnet";
		let bridge_id = "0123456789abcdef0123456789abcdef";
		let encoded_studio = BASE64_STANDARD.encode(studio.as_bytes());
		let encoded_place = BASE64_STANDARD.encode(place.as_bytes());
		let encoded_loader = BASE64_STANDARD.encode(loader.as_bytes());
		let encoded_build = BASE64_STANDARD.encode(build.as_bytes());
		let encoded_dotnet_root = BASE64_STANDARD.encode(dotnet_root.as_bytes());
		let encoded_bridge_id = BASE64_STANDARD.encode(bridge_id.as_bytes());
		let args = [
			"launch".to_string(),
			encoded_studio.clone(),
			encoded_place.clone(),
			"1".to_string(),
			encoded_loader.clone(),
			encoded_build.clone(),
			encoded_dotnet_root.clone(),
			encoded_bridge_id.clone(),
		];
		assert_eq!(args[0], "launch");
		assert_eq!(
			String::from_utf8(BASE64_STANDARD.decode(args[1].as_bytes()).unwrap()).unwrap(),
			studio
		);
		assert_eq!(
			String::from_utf8(BASE64_STANDARD.decode(args[2].as_bytes()).unwrap()).unwrap(),
			place
		);
		assert_eq!(args[3], "1");
		assert_eq!(
			String::from_utf8(BASE64_STANDARD.decode(args[4].as_bytes()).unwrap()).unwrap(),
			loader
		);
		assert_eq!(
			String::from_utf8(BASE64_STANDARD.decode(args[5].as_bytes()).unwrap()).unwrap(),
			build
		);
		assert_eq!(
			String::from_utf8(BASE64_STANDARD.decode(args[6].as_bytes()).unwrap()).unwrap(),
			dotnet_root
		);
		assert_eq!(
			String::from_utf8(BASE64_STANDARD.decode(args[7].as_bytes()).unwrap()).unwrap(),
			bridge_id
		);
	}

	#[cfg(any(target_os = "linux", target_os = "windows"))]
	#[test]
	fn powershell_resolver_uses_syswow64() {
		#[cfg(target_os = "linux")]
		if std::env::var_os("WSL_DISTRO_NAME").is_none() {
			return;
		}
		let path = powershell_path().unwrap();
		let path_str = path.to_string_lossy().to_lowercase();
		assert!(
			path_str.contains("syswow64"),
			"expected 32-bit SysWOW64 PowerShell path, got: {}",
			path.display()
		);
	}

	#[test]
	fn rml_helper_terminate_args_contract() {
		let studio = r"C:\Roblox\version\RobloxStudioBeta.exe";
		let encoded_studio = BASE64_STANDARD.encode(studio.as_bytes());
		let pid = 47_312_u32;
		let started_at_file_time = 133_700_123_456_u64;
		let args = [
			"terminate".to_string(),
			pid.to_string(),
			started_at_file_time.to_string(),
			encoded_studio.clone(),
		];
		assert_eq!(args[0], "terminate");
		assert_eq!(args[1], "47312");
		assert_eq!(args[2], "133700123456");
		assert_eq!(
			String::from_utf8(BASE64_STANDARD.decode(args[3].as_bytes()).unwrap()).unwrap(),
			studio
		);
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
		assert!(script.contains("$env:DOTNET_ROOT_X64"));
		assert!(script.contains("$env:ProgramW6432"));
		assert!(script.contains("$candidateVersion.Major -eq $requiredVersion.Major"));
		assert!(script.contains("$candidateVersion -ge $requiredVersion"));
		assert!(script.contains("host\\fxr\\*\\hostfxr.dll"));
	}

	#[cfg(target_os = "linux")]
	#[test]
	fn focus_process_uses_exact_helper_argv_when_helper_materialized() {
		let _environment = FOCUS_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		let unique = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap()
			.as_nanos();
		let temp_dir = std::env::temp_dir().join(format!("carbon-helper-argv-{unique}"));
		for relative in &[
			"dwmapi.dll",
			"RobloxModLoader/roblox_modloader.dll",
			"carbon-studio-helper.exe",
			"RobloxModLoader/config.toml",
			"RobloxModLoader/runtime/RML.Core.dll",
			"RobloxModLoader/runtime/RML.NativeHost.dll",
			"RobloxModLoader/runtime/Roblox.dll",
			"RobloxModLoader/runtime/nethost.dll",
			"RobloxModLoader/runtime/RML.runtimeconfig.json",
			"RobloxModLoader/mods/carbon/dotnet/Carbon.RmlBridge.dll",
		] {
			let path = temp_dir.join(relative);
			fs::create_dir_all(path.parent().unwrap()).unwrap();
			fs::write(&path, b"stub").unwrap();
		}
		let marker = serde_json::to_vec(&crate::rml::InstallMarker::current()).unwrap();
		let marker_path = temp_dir.join("RobloxModLoader/carbon-rml.json");
		fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
		fs::write(marker_path, marker).unwrap();
		let script_path = temp_dir.join("carbon-studio-helper.exe");
		let log_path = temp_dir.join("args.txt");
		fs::write(
			&script_path,
			format!("#!/bin/sh\necho \"$@\" > {}\nexit 0\n", log_path.display()),
		)
		.unwrap();
		use std::os::unix::fs::PermissionsExt;
		fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();

		let old_package = std::env::var_os("CARBON_RML_PACKAGE");
		let old_wsl = std::env::var_os("WSL_DISTRO_NAME");
		std::env::set_var("CARBON_RML_PACKAGE", &temp_dir);
		std::env::set_var("WSL_DISTRO_NAME", "Ubuntu");

		let exe_path = r"C:\Roblox\version\RobloxStudioBeta.exe";
		let expected_b64 = BASE64_STANDARD.encode(exe_path.as_bytes());
		let result = focus_process(54321, Some(133700123456), Some(exe_path));

		if let Some(old) = old_package {
			std::env::set_var("CARBON_RML_PACKAGE", old);
		} else {
			std::env::remove_var("CARBON_RML_PACKAGE");
		}
		if let Some(old) = old_wsl {
			std::env::set_var("WSL_DISTRO_NAME", old);
		} else {
			std::env::remove_var("WSL_DISTRO_NAME");
		}

		assert!(result.is_ok());
		let recorded_args = fs::read_to_string(&log_path).unwrap();
		assert_eq!(recorded_args.trim(), format!("focus 54321 133700123456 {expected_b64}"));
		let _ = fs::remove_dir_all(&temp_dir);
	}

	#[cfg(target_os = "linux")]
	#[test]
	fn focus_process_surfaces_helper_failures_without_fallback() {
		let _environment = FOCUS_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		let unique = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap()
			.as_nanos();
		let temp_dir = std::env::temp_dir().join(format!("carbon-helper-fail-{unique}"));
		for relative in &[
			"dwmapi.dll",
			"RobloxModLoader/roblox_modloader.dll",
			"carbon-studio-helper.exe",
			"RobloxModLoader/config.toml",
			"RobloxModLoader/runtime/RML.Core.dll",
			"RobloxModLoader/runtime/RML.NativeHost.dll",
			"RobloxModLoader/runtime/Roblox.dll",
			"RobloxModLoader/runtime/nethost.dll",
			"RobloxModLoader/runtime/RML.runtimeconfig.json",
			"RobloxModLoader/mods/carbon/dotnet/Carbon.RmlBridge.dll",
		] {
			let path = temp_dir.join(relative);
			fs::create_dir_all(path.parent().unwrap()).unwrap();
			fs::write(&path, b"stub").unwrap();
		}
		let marker = serde_json::to_vec(&crate::rml::InstallMarker::current()).unwrap();
		let marker_path = temp_dir.join("RobloxModLoader/carbon-rml.json");
		fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
		fs::write(marker_path, marker).unwrap();
		let script_path = temp_dir.join("carbon-studio-helper.exe");
		fs::write(
			&script_path,
			"#!/bin/sh\necho \"native focus helper failed\" >&2\nexit 1\n",
		)
		.unwrap();
		use std::os::unix::fs::PermissionsExt;
		fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();

		let old_package = std::env::var_os("CARBON_RML_PACKAGE");
		let old_wsl = std::env::var_os("WSL_DISTRO_NAME");
		std::env::set_var("CARBON_RML_PACKAGE", &temp_dir);
		std::env::set_var("WSL_DISTRO_NAME", "Ubuntu");

		let err = focus_process(
			54321,
			Some(133700123456),
			Some(r"C:\Roblox\version\RobloxStudioBeta.exe"),
		)
		.unwrap_err();

		if let Some(old) = old_package {
			std::env::set_var("CARBON_RML_PACKAGE", old);
		} else {
			std::env::remove_var("CARBON_RML_PACKAGE");
		}
		if let Some(old) = old_wsl {
			std::env::set_var("WSL_DISTRO_NAME", old);
		} else {
			std::env::remove_var("WSL_DISTRO_NAME");
		}

		assert!(err.to_string().contains("native focus helper failed"));
		let _ = fs::remove_dir_all(&temp_dir);
	}

	#[test]
	fn focus_process_fails_on_partial_metadata() {
		let err = focus_process(54321, None, None).unwrap_err();
		assert!(err.to_string().contains("incomplete focus metadata"));

		let err = focus_process(54321, Some(12345), None).unwrap_err();
		assert!(err.to_string().contains("incomplete focus metadata"));

		let err = focus_process(54321, None, Some("exe")).unwrap_err();
		assert!(err.to_string().contains("incomplete focus metadata"));
	}

	#[test]
	fn managed_studio_focus_metadata_exposure() {
		let direct = ManagedStudio {
			lifecycle: ManagedStudioLifecycle::Direct {
				process_id: 1234,
				data_model_name: "test".to_string(),
				studio_executable: "Studio.exe".to_string(),
				started_at_file_time: 1000000,
				helper_path: PathBuf::from("/path/helper"),
			},
			startup_guard: Arc::new(Mutex::new(None)),
			bridge_id: None,
		};

		#[cfg(any(target_os = "linux", target_os = "windows"))]
		{
			let meta = direct.focus_metadata().unwrap();
			assert_eq!(meta.studio_executable, "Studio.exe");
			assert_eq!(meta.creation_filetime, 1000000);
		}
		#[cfg(not(any(target_os = "linux", target_os = "windows")))]
		{
			assert!(direct.focus_metadata().is_none());
		}
	}
}
