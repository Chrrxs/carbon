use anyhow::{Context, Result};
#[cfg(target_os = "linux")]
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::{
	fmt, fs,
	path::PathBuf,
	process::{Command, Stdio},
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
const MCP_PROTOCOL_VERSION: u64 = 1;
const MCP_PROBE_TIMEOUT: Duration = Duration::from_millis(750);
const MCP_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
enum ManagedStudioLifecycle {
	Direct {
		process_id: u32,
		data_model_name: String,
	},
	Mcp {
		endpoint: String,
		auth_token: Option<String>,
		launch_id: String,
		process_id: u32,
	},
}

#[derive(Clone)]
pub struct ManagedStudio {
	lifecycle: ManagedStudioLifecycle,
	startup_guard: Arc<Mutex<Option<studio_plugin::Installation>>>,
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
		match &self.lifecycle {
			ManagedStudioLifecycle::Direct { process_id, .. } => terminate(*process_id),
			ManagedStudioLifecycle::Mcp {
				endpoint,
				auth_token,
				launch_id,
				..
			} => {
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
			}
		}
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
	let (rml_launch, _plugin_launch) = prepare_launch()?;
	launch_prepared(path, rml_launch)
}

pub fn launch_managed(path: PathBuf) -> Result<ManagedStudio> {
	let (rml_launch, plugin_launch) = prepare_launch()?;
	let lifecycle = if let Some(mcp) = discover_mcp_lifecycle()? {
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
				let data_model_name = path
					.file_name()
					.context("managed Studio place path does not have a file name")?
					.to_string_lossy()
					.into_owned();
				let process_id = launch_prepared(Some(path), rml_launch)
					.with_context(|| {
						format!(
							"safe direct fallback failed after robloxstudio-mcp pre-dispatch failure: {broker_error}"
						)
					})?
					.context("Studio launcher did not return a process ID")?;
				ManagedStudioLifecycle::Direct {
					process_id,
					data_model_name,
				}
			}
			Err(error) => return Err(anyhow::Error::new(error)),
		}
	} else {
		let data_model_name = path
			.file_name()
			.context("managed Studio place path does not have a file name")?
			.to_string_lossy()
			.into_owned();
		let process_id =
			launch_prepared(Some(path), rml_launch)?.context("Studio launcher did not return a process ID")?;
		ManagedStudioLifecycle::Direct {
			process_id,
			data_model_name,
		}
	};
	Ok(ManagedStudio {
		lifecycle,
		startup_guard: Arc::new(Mutex::new(Some(plugin_launch))),
	})
}

fn prepare_launch() -> Result<(rml::Launch, studio_plugin::Installation)> {
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
	Ok((rml::prepare_launch(None, None)?, installation))
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

fn launch_through_mcp(
	path: PathBuf,
	rml_launch: &rml::Launch,
	mcp: &McpLifecycle,
) -> std::result::Result<ManagedStudioLifecycle, ManagedLaunchFailure> {
	let (studio_executable, loader_path) =
		mcp_launch_paths(rml_launch).map_err(|error| ManagedLaunchFailure::pre_dispatch(&mcp.endpoint, error))?;
	let payload = mcp_launch_request(&path, &studio_executable, &loader_path, rml_launch.build_version());
	let result = mcp_tool(
		&mcp.endpoint,
		mcp.auth_token.as_deref(),
		&payload,
		MCP_LIFECYCLE_TIMEOUT,
	)
	.map_err(ManagedLaunchFailure::from_request)?;
	(|| -> Result<ManagedStudioLifecycle> {
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
		anyhow::ensure!(
			result.get("managed").and_then(Value::as_bool) == Some(true)
				&& result.get("source").and_then(Value::as_str) == Some("local_file"),
			"robloxstudio-mcp did not return a managed local-file Studio launch"
		);
		anyhow::ensure!(
			result.get("process_running").and_then(Value::as_bool) == Some(true),
			"robloxstudio-mcp did not confirm that Studio launch {launch_id} is running"
		);
		Ok(ManagedStudioLifecycle::Mcp {
			endpoint: mcp.endpoint.clone(),
			auth_token: mcp.auth_token.clone(),
			launch_id,
			process_id,
		})
	})()
	.map_err(|error| ManagedLaunchFailure::invalid_response(&mcp.endpoint, error))
}

fn mcp_launch_request(path: &std::path::Path, studio: &str, loader: &str, build_version: &str) -> Value {
	json!({
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
		"wait_for_connection": false
	})
}

fn mcp_launch_paths(rml_launch: &rml::Launch) -> Result<(String, String)> {
	#[cfg(target_os = "linux")]
	{
		Ok((
			windows_path(rml_launch.studio_executable(), "Roblox Studio executable")?,
			windows_path(rml_launch.loader_path(), "RML loader")?,
		))
	}

	#[cfg(not(target_os = "linux"))]
	{
		Ok((
			rml_launch.studio_executable().to_string_lossy().into_owned(),
			rml_launch.loader_path().to_string_lossy().into_owned(),
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

fn launch_prepared(path: Option<PathBuf>, rml_launch: rml::Launch) -> Result<Option<u32>> {
	#[cfg(target_os = "linux")]
	{
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
		let script = powershell_launch_script(
			&windows_studio,
			&windows_loader,
			rml_launch.build_version(),
			&windows_place,
		);
		// Windows PowerShell 5 concatenates everything after `-Command` into the
		// script instead of exposing trailing values through `$args`. Only the
		// fixed base64 alphabet is interpolated here so arbitrary paths and build
		// versions can never become executable PowerShell source.
		let output = Command::new("powershell.exe")
			.args(["-NoProfile", "-Command", script.as_str()])
			.output()?;
		anyhow::ensure!(
			output.status.success(),
			"failed to launch Roblox Studio: {}",
			String::from_utf8_lossy(&output.stderr).trim()
		);
		let process_id = String::from_utf8(output.stdout)?
			.trim()
			.parse::<u32>()
			.context("PowerShell returned an invalid Roblox Studio process ID")?;
		Ok(Some(process_id))
	}

	#[cfg(not(target_os = "linux"))]
	{
		let child = Command::new(rml_launch.studio_executable())
			.arg(path.unwrap_or_default())
			.env(rml::LOADER_ENV, rml_launch.loader_path())
			.env(rml::EXPECTED_BUILD_ENV, rml_launch.build_version())
			.env_remove(rml::LOADED_BUILD_ENV)
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()?;
		Ok(Some(child.id()))
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

#[cfg(target_os = "linux")]
fn powershell_launch_script(studio: &str, loader: &str, build_version: &str, place: &str) -> String {
	let encoded_studio = BASE64_STANDARD.encode(studio.as_bytes());
	let encoded_loader = BASE64_STANDARD.encode(loader.as_bytes());
	let encoded_build_version = BASE64_STANDARD.encode(build_version.as_bytes());
	let encoded_place = BASE64_STANDARD.encode(place.as_bytes());
	r#"
$studio = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__CARBON_STUDIO_BASE64__'))
$loader = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__CARBON_LOADER_BASE64__'))
$buildVersion = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__CARBON_BUILD_VERSION_BASE64__'))
$place = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__CARBON_PLACE_BASE64__'))
if (-not [IO.Path]::IsPathRooted($loader)) { throw "Carbon supplied a non-absolute RML loader path" }
$env:CARBON_RML_LOADER = $loader
$env:CARBON_RML_BUILD_VERSION = $buildVersion
Remove-Item Env:CARBON_RML_LOADED_BUILD_VERSION -ErrorAction SilentlyContinue
if ([string]::IsNullOrEmpty($place)) {
    $process = Start-Process -FilePath $studio -PassThru
} else {
    $quotedPlace = '"' + $place + '"'
    $process = Start-Process -FilePath $studio -ArgumentList '--task', 'EditFile', '--localPlaceFile', $quotedPlace -PassThru
}
$process.Id
"#
	.replace("__CARBON_STUDIO_BASE64__", &encoded_studio)
	.replace("__CARBON_LOADER_BASE64__", &encoded_loader)
	.replace("__CARBON_BUILD_VERSION_BASE64__", &encoded_build_version)
	.replace("__CARBON_PLACE_BASE64__", &encoded_place)
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

/// Stop exactly the Studio process returned by [`launch`]. Managed launchers
/// own their child process; using the native PID avoids title- or array-order
/// heuristics when several worktrees have Studio open simultaneously.
pub fn terminate(process_id: u32) -> Result<()> {
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	{
		let script = format!(
			"$process = Get-Process -Id {process_id} -ErrorAction SilentlyContinue; if ($null -ne $process) {{ Stop-Process -Id {process_id} -Force -ErrorAction Stop; $process.WaitForExit() }}"
		);
		let status = Command::new("powershell.exe")
			.args(["-NoProfile", "-NonInteractive", "-Command", script.as_str()])
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status()
			.context("failed to stop the managed Roblox Studio process")?;
		anyhow::ensure!(status.success(), "managed Roblox Studio process did not stop cleanly");
		Ok(())
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
		Ok(())
	}
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
		br#"{"status":"ok","service":"robloxstudio-mcp","serverName":"robloxstudio-mcp","capabilities":{"studioLifecycle":{"protocolVersion":1,"endpoint":"/mcp/manage_instance"}}}"#
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
					"protocolVersion": 1,
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
					"protocolVersion": 1,
					"endpoint": "/mcp/manage_instance"
				}
			}
		});
		assert!(mcp_lifecycle_endpoint(DEFAULT_MCP_URL, &health).is_none());

		health["serverName"] = json!("robloxstudio-mcp");
		health["capabilities"]["studioLifecycle"]["protocolVersion"] = json!(2);
		assert!(mcp_lifecycle_endpoint(DEFAULT_MCP_URL, &health).is_none());

		health["capabilities"]["studioLifecycle"]["protocolVersion"] = json!(1);
		assert!(mcp_lifecycle_endpoint("http://192.0.2.1:58741", &health).is_none());
	}

	#[test]
	fn managed_launch_transfers_exact_rml_environment() {
		let payload = mcp_launch_request(
			std::path::Path::new("/tmp/carbon-managed.rbxl"),
			r"C:\Roblox\RobloxStudioBeta.exe",
			r"C:\Carbon\RobloxModLoader\roblox_modloader.dll",
			"0.0.0+build.test",
		);

		assert_eq!(payload["action"], "launch");
		assert_eq!(payload["source"], "local_file");
		assert_eq!(payload["local_place_file"], "/tmp/carbon-managed.rbxl");
		assert_eq!(payload["studio_executable"], r"C:\Roblox\RobloxStudioBeta.exe");
		assert_eq!(
			payload["process_environment"]["set"][rml::LOADER_ENV],
			r"C:\Carbon\RobloxModLoader\roblox_modloader.dll"
		);
		assert_eq!(
			payload["process_environment"]["set"][rml::EXPECTED_BUILD_ENV],
			"0.0.0+build.test"
		);
		assert_eq!(payload["process_environment"]["remove"], json!([rml::LOADED_BUILD_ENV]));
		assert_eq!(payload["wait_for_connection"], false);
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
	fn powershell_launch_is_pinned_to_one_studio_and_one_rml_package() {
		let studio = r#"C:\Roblox\Versions\worktree'; throw 'studio\RobloxStudioBeta.exe"#;
		let loader = r#"C:\Carbon\rml\worktree'; throw 'loader\RobloxModLoader\roblox_modloader.dll"#;
		let build = "0.0.0+worktree'; throw 'build";
		let place = r#"C:\places\worktree'; throw 'place.rbxl"#;
		let script = powershell_launch_script(studio, loader, build, place);

		for untrusted in [studio, loader, build, place] {
			assert!(!script.contains(untrusted));
		}
		assert!(script.contains("$env:CARBON_RML_LOADER = $loader"));
		assert!(script.contains("$env:CARBON_RML_BUILD_VERSION = $buildVersion"));
		assert!(script.contains("Remove-Item Env:CARBON_RML_LOADED_BUILD_VERSION"));
		assert!(script.contains("Start-Process -FilePath $studio"));
		assert!(!script.contains("Get-ChildItem"));
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
