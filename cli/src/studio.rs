use anyhow::{ensure, Context, Result};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde_json::{json, Value};
use std::{
	fs,
	path::{Path, PathBuf},
	process::{Command, Stdio},
	sync::{Arc, Mutex},
	thread,
	time::{Duration, Instant},
};

use crate::studio_plugin;

#[cfg(target_os = "windows")]
use winsafe::{co::SW, EnumWindows};

const MCP_LAUNCH_TIMEOUT: Duration = Duration::from_secs(120);
const MCP_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(15);
const MCP_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const MCP_INSTANCE_TIMEOUT: Duration = Duration::from_secs(120);
const MCP_PROTOCOL_VERSION: u64 = 3;
const DEFAULT_MCP_URL: &str = "http://127.0.0.1:58741";
const MCP_URL_ENV: &str = "CARBON_STUDIO_MCP_URL";

#[derive(Clone)]
struct McpLifecycle {
	endpoint: String,
	auth_token: Option<String>,
}

impl McpLifecycle {
	fn new(endpoint: String, auth_token: Option<String>) -> Self {
		Self { endpoint, auth_token }
	}

	fn launch(&self, path: &Path, studio_executable: &str) -> Result<ManagedLaunch> {
		let result = mcp_tool(
			&self.endpoint,
			self.auth_token.as_deref(),
			&mcp_launch_request(path, studio_executable),
			MCP_LAUNCH_TIMEOUT,
		)
		.context("robloxstudio-mcp could not launch managed Roblox Studio")?;
		let (launch_id, process_id, creation_filetime) = match managed_launch_identity(&result) {
			Ok(identity) => identity,
			Err(error) => {
				let cleanup = result
					.get("launch_id")
					.and_then(Value::as_str)
					.filter(|launch_id| !launch_id.is_empty())
					.map(|launch_id| {
						mcp_tool(
							&self.endpoint,
							self.auth_token.as_deref(),
							&json!({"action": "close", "launch_id": launch_id}),
							MCP_LIFECYCLE_TIMEOUT,
						)
					});
				return match cleanup {
					Some(Ok(_)) => Err(error.context("invalid managed launch attestation; broker cleanup completed")),
					Some(Err(cleanup_error)) => Err(error.context(format!(
						"invalid managed launch attestation and broker cleanup also failed: {cleanup_error:#}"
					))),
					None => Err(error.context(
						"invalid managed launch attestation omitted launch_id; broker retains its ownership-completion lease for cleanup",
					)),
				};
			}
		};
		Ok(ManagedLaunch {
			lifecycle: self.clone(),
			launch_id,
			process_id,
			creation_filetime,
		})
	}
}

fn discover_mcp_lifecycle() -> Result<McpLifecycle> {
	let base_url = std::env::var(MCP_URL_ENV).unwrap_or_else(|_| DEFAULT_MCP_URL.to_owned());
	let health_url = mcp_health_url(&base_url)
		.with_context(|| format!("invalid robloxstudio-mcp URL from {MCP_URL_ENV}: {base_url:?}"))?;
	let health: Value = reqwest::blocking::Client::builder()
		.connect_timeout(MCP_PROBE_TIMEOUT)
		.timeout(MCP_PROBE_TIMEOUT)
		.build()?
		.get(&health_url)
		.header("Accept", "application/json")
		.send()
		.with_context(|| {
			format!(
				"managed Carbon serve requires robloxstudio-mcp lifecycle protocol {MCP_PROTOCOL_VERSION}, but {health_url} was unavailable"
			)
		})?
		.error_for_status()
		.context("robloxstudio-mcp health request failed")?
		.json()
		.context("robloxstudio-mcp health returned invalid JSON")?;
	let endpoint = mcp_lifecycle_endpoint(&base_url, &health).with_context(|| {
		format!(
			"robloxstudio-mcp at {base_url} does not advertise lifecycle protocol {MCP_PROTOCOL_VERSION} with exact process identity"
		)
	})?;
	Ok(McpLifecycle::new(endpoint, load_mcp_auth_token()))
}

fn mcp_health_url(base_url: &str) -> Result<String> {
	let mut url = reqwest::Url::parse(base_url.trim())?;
	ensure!(url.scheme() == "http", "Studio lifecycle MCP URL must use http");
	ensure!(
		matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1")),
		"Studio lifecycle MCP URL must use a loopback host"
	);
	ensure!(
		url.username().is_empty() && url.password().is_none(),
		"Studio lifecycle MCP URL must not contain credentials"
	);
	url.set_query(None);
	url.set_fragment(None);
	url.set_path("/health");
	Ok(url.into())
}

fn mcp_lifecycle_endpoint(base_url: &str, health: &Value) -> Option<String> {
	if health.get("status").and_then(Value::as_str) != Some("ok")
		|| health.get("service").and_then(Value::as_str) != Some("robloxstudio-mcp")
		|| health.get("serverName").and_then(Value::as_str) != Some("robloxstudio-mcp")
	{
		return None;
	}
	let capability = health.pointer("/capabilities/studioLifecycle")?;
	if capability.get("protocolVersion").and_then(Value::as_u64) != Some(MCP_PROTOCOL_VERSION)
		|| capability.get("endpoint").and_then(Value::as_str) != Some("/mcp/manage_instance")
		|| capability
			.pointer("/processIdentity/supported")
			.and_then(Value::as_bool)
			!= Some(true)
	{
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
	url.set_path("/mcp/manage_instance");
	Some(url.into())
}

fn load_mcp_auth_token() -> Option<String> {
	if std::env::var("ROBLOX_STUDIO_NO_AUTH")
		.is_ok_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true"))
	{
		return None;
	}
	if let Ok(token) = std::env::var("ROBLOX_STUDIO_AUTH_TOKEN") {
		let token = token.trim().to_owned();
		if !token.is_empty() {
			return Some(token);
		}
	}
	let home = std::env::var_os("HOME")
		.or_else(|| std::env::var_os("USERPROFILE"))
		.map(PathBuf::from)
		.map(|path| path.join(".robloxstudio-mcp").join("auth-token"));
	home.and_then(|path| fs::read_to_string(path).ok())
		.map(|token| token.trim().to_owned())
		.filter(|token| !token.is_empty())
}

#[derive(Clone)]
struct ManagedLaunch {
	lifecycle: McpLifecycle,
	launch_id: String,
	process_id: u32,
	creation_filetime: u64,
}

impl ManagedLaunch {
	fn request(&self, action: &str) -> Result<Value> {
		mcp_tool(
			&self.lifecycle.endpoint,
			self.lifecycle.auth_token.as_deref(),
			&json!({"action": action, "launch_id": self.launch_id}),
			MCP_LIFECYCLE_TIMEOUT,
		)
		.with_context(|| format!("robloxstudio-mcp could not {action} Studio launch {}", self.launch_id))
	}

	fn has_exact_identity(&self, result: &Value) -> bool {
		result.get("launch_id").and_then(Value::as_str) == Some(self.launch_id.as_str())
			&& result.get("managed").and_then(Value::as_bool) == Some(true)
			&& result.get("source").and_then(Value::as_str) == Some("local_file")
			&& result.get("pid").and_then(Value::as_u64) == Some(u64::from(self.process_id))
			&& result
				.get("process_started_at_file_time")
				.and_then(Value::as_str)
				.and_then(|value| value.parse::<u64>().ok())
				== Some(self.creation_filetime)
			&& result.get("process_running").and_then(Value::as_bool) == Some(true)
	}

	fn authorize(&self) -> Result<()> {
		let result = self.request("authorize")?;
		ensure!(
			self.has_exact_identity(&result)
				&& matches!(
					result.get("state").and_then(Value::as_str),
					Some("launching" | "connected")
				) && result.get("process_authorized").and_then(Value::as_bool) == Some(true),
			"robloxstudio-mcp did not authorize the exact Studio process for launch {}",
			self.launch_id
		);
		Ok(())
	}

	fn complete(&self) -> Result<()> {
		let result = self.request("complete")?;
		ensure!(
			self.has_exact_identity(&result)
				&& matches!(
					result.get("state").and_then(Value::as_str),
					Some("launching" | "connected")
				) && result.get("process_authorized").and_then(Value::as_bool) == Some(true)
				&& result.get("process_ownership_released").and_then(Value::as_bool) == Some(true),
			"robloxstudio-mcp did not release ownership of the exact Studio process for launch {}",
			self.launch_id
		);
		Ok(())
	}

	fn connected_instance_id(&self) -> Result<String> {
		let deadline = Instant::now() + MCP_INSTANCE_TIMEOUT;
		loop {
			let request_error = match self.request("status") {
				Ok(result) => {
					if let Some(instance_id) = self.connected_instance_id_from_status(&result)? {
						return Ok(instance_id);
					}
					None
				}
				Err(error) => {
					log::debug!("waiting for Studio launch {} association: {error:#}", self.launch_id);
					Some(error)
				}
			};
			if Instant::now() >= deadline {
				let message = format!(
					"robloxstudio-mcp Studio launch {} did not report a final instance_id within {} seconds",
					self.launch_id,
					MCP_INSTANCE_TIMEOUT.as_secs()
				);
				return match request_error {
					Some(error) => Err(error).context(message),
					None => anyhow::bail!(message),
				};
			}
			thread::sleep(Duration::from_millis(100));
		}
	}

	fn connected_instance_id_from_status(&self, result: &Value) -> Result<Option<String>> {
		let state = result.get("state").and_then(Value::as_str).unwrap_or("unknown");
		ensure!(
			!matches!(state, "failed" | "exited"),
			"robloxstudio-mcp Studio launch {} entered state {state} before association",
			self.launch_id
		);
		ensure!(
			result.get("launch_id").and_then(Value::as_str) == Some(self.launch_id.as_str())
				&& result.get("managed").and_then(Value::as_bool) == Some(true)
				&& result.get("pid").and_then(Value::as_u64) == Some(u64::from(self.process_id))
				&& result.get("process_running").and_then(Value::as_bool) == Some(true),
			"robloxstudio-mcp did not report the exact running Studio process for launch {}",
			self.launch_id
		);
		if state != "connected" {
			return Ok(None);
		}
		Ok(Some(
			result
				.get("instance_id")
				.and_then(Value::as_str)
				.filter(|value| !value.is_empty())
				.map(str::to_owned)
				.context("robloxstudio-mcp connected Studio status did not include instance_id")?,
		))
	}

	fn close(&self) -> Result<()> {
		let result = self.request("close")?;
		ensure!(
			result.get("launch_id").and_then(Value::as_str) == Some(self.launch_id.as_str())
				&& matches!(
					result.get("close_status").and_then(Value::as_str),
					Some("closed" | "already_closed")
				) && result.get("process_running").and_then(Value::as_bool) == Some(false),
			"robloxstudio-mcp did not confirm exact closure of Studio launch {}",
			self.launch_id
		);
		Ok(())
	}
}

fn authorize_managed_launch(launch: &ManagedLaunch) -> Result<()> {
	if let Err(error) = launch.authorize() {
		return match launch.close() {
			Ok(()) => Err(error.context("managed Studio launch authorization failed; broker cleanup completed")),
			Err(cleanup_error) => Err(error.context(format!(
				"managed Studio launch authorization failed and broker cleanup also failed: {cleanup_error:#}"
			))),
		};
	}
	Ok(())
}

fn complete_managed_launch(launch: &ManagedLaunch) -> Result<()> {
	if let Err(error) = launch.complete() {
		return match launch.close() {
			Ok(()) => Err(error.context("managed Studio ownership completion failed; broker cleanup completed")),
			Err(cleanup_error) => Err(error.context(format!(
				"managed Studio ownership completion failed and broker cleanup also failed: {cleanup_error:#}"
			))),
		};
	}
	Ok(())
}

fn associate_managed_launch(launch: &ManagedLaunch) -> Result<String> {
	match launch.connected_instance_id() {
		Ok(instance_id) => Ok(instance_id),
		Err(error) => match launch.close() {
			Ok(()) => Err(error.context("managed Studio association failed; broker cleanup completed")),
			Err(cleanup_error) => Err(error.context(format!(
				"managed Studio association failed and broker cleanup also failed: {cleanup_error:#}"
			))),
		},
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FocusMetadata {
	pub studio_executable: String,
	pub creation_filetime: u64,
}

#[derive(Debug, Clone)]
pub struct StudioInfo {
	pub executable: PathBuf,
	pub version_text: String,
	pub version_components: [u32; 4],
	pub build_id: String,
}

#[derive(Clone)]
pub struct ManagedStudio {
	launch: ManagedLaunch,
	studio_executable: String,
	startup_guard: Arc<Mutex<Option<studio_plugin::Installation>>>,
}

impl ManagedStudio {
	pub fn process_id(&self) -> u32 {
		self.launch.process_id
	}

	pub fn launch_id(&self) -> &str {
		&self.launch.launch_id
	}

	pub fn owner(&self) -> &'static str {
		"robloxstudio-mcp"
	}

	pub fn focus_metadata(&self) -> Option<FocusMetadata> {
		Some(FocusMetadata {
			studio_executable: self.studio_executable.clone(),
			creation_filetime: self.launch.creation_filetime,
		})
	}

	fn finish_startup(&self) -> Result<()> {
		let mut startup_guard = self.startup_guard.lock().unwrap();
		if startup_guard.is_none() {
			return Ok(());
		}
		complete_managed_launch(&self.launch)?;
		startup_guard.take();
		Ok(())
	}

	pub fn establish_instance_id(&self) -> Result<String> {
		let established = (|| {
			self.finish_startup()?;
			associate_managed_launch(&self.launch)
		})();
		match established {
			Ok(instance_id) => Ok(instance_id),
			Err(error) => match self.stop() {
				Ok(()) => Err(error),
				Err(cleanup_error) => Err(error.context(format!(
					"managed Studio startup cleanup also failed for launch {}: {cleanup_error:#}",
					self.launch_id()
				))),
			},
		}
	}

	pub fn stop(&self) -> Result<()> {
		match self.launch.close() {
			Ok(()) => Ok(()),
			Err(broker_error) => terminate_process(
				self.process_id(),
				&self.studio_executable,
				self.launch.creation_filetime,
			)
			.with_context(|| {
				format!(
					"robloxstudio-mcp could not close Studio launch {} ({broker_error:#}); exact-process fallback also failed",
					self.launch_id()
				)
			}),
		}
	}
}

fn ensure_plugin() -> Result<studio_plugin::Installation> {
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
	Ok(installation)
}

fn mcp_launch_request(path: &Path, studio_executable: &str) -> Value {
	json!({
		"action": "launch",
		"source": "local_file",
		"local_place_file": path,
		"studio_executable": studio_executable,
		"require_process_identity": true,
		"wait_for_connection": false,
	})
}

fn managed_launch_identity(result: &Value) -> Result<(String, u32, u64)> {
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
	let creation_filetime = result
		.get("process_started_at_file_time")
		.and_then(Value::as_str)
		.and_then(|value| value.parse::<u64>().ok())
		.filter(|value| *value > 0)
		.context("robloxstudio-mcp launch response did not include the native Studio process creation time")?;
	ensure!(
		result.get("managed").and_then(Value::as_bool) == Some(true)
			&& result.get("source").and_then(Value::as_str) == Some("local_file")
			&& result.get("state").and_then(Value::as_str) == Some("launching")
			&& result.get("process_running").and_then(Value::as_bool) == Some(true)
			&& result.get("process_authorized").and_then(Value::as_bool) == Some(false),
		"robloxstudio-mcp did not return a suspended managed local-file launch awaiting Carbon authorization"
	);
	Ok((launch_id, process_id, creation_filetime))
}

fn mcp_tool(endpoint: &str, auth_token: Option<&str>, payload: &Value, timeout: Duration) -> Result<Value> {
	let client = reqwest::blocking::Client::builder()
		.connect_timeout(timeout)
		.timeout(timeout)
		.build()?;
	let mut request = client.post(endpoint).header("Accept", "application/json").json(payload);
	if let Some(token) = auth_token {
		request = request.header("X-MCP-Auth", token);
	}
	let response = request
		.send()
		.with_context(|| format!("robloxstudio-mcp lifecycle request to {endpoint} failed"))?;
	let status = response.status();
	let body = response
		.bytes()
		.context("failed to read robloxstudio-mcp lifecycle response")?;
	ensure!(
		status.is_success(),
		"robloxstudio-mcp lifecycle request returned HTTP {status}"
	);
	let envelope: Value =
		serde_json::from_slice(&body).context("robloxstudio-mcp lifecycle response was invalid JSON")?;
	let result = envelope
		.get("content")
		.and_then(Value::as_array)
		.into_iter()
		.flatten()
		.filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
		.filter_map(|item| item.get("text").and_then(Value::as_str))
		.find_map(|text| serde_json::from_str::<Value>(text).ok())
		.context("robloxstudio-mcp lifecycle response contained no JSON result")?;
	ensure!(
		envelope.get("isError").and_then(Value::as_bool) != Some(true)
			&& result.get("error").is_none_or(Value::is_null)
			&& result.get("success").and_then(Value::as_bool) != Some(false),
		"robloxstudio-mcp rejected the lifecycle request"
	);
	Ok(result)
}

fn parse_version(raw: &str) -> Result<(String, [u32; 4])> {
	let sanitized = raw.replace(", ", ".").replace(',', ".").replace(' ', "");
	let parts = sanitized.split('.').collect::<Vec<_>>();
	ensure!(
		parts.len() == 4,
		"Studio version string '{raw}' does not have 4 components"
	);
	let components = [
		parts[0].parse().context("invalid version component 0")?,
		parts[1].parse().context("invalid version component 1")?,
		parts[2].parse().context("invalid version component 2")?,
		parts[3].parse().context("invalid version component 3")?,
	];
	Ok((
		format!(
			"{}.{}.{}.{}",
			components[0], components[1], components[2], components[3]
		),
		components,
	))
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn windows_file_version(path: &str) -> Result<String> {
	let encoded = BASE64_STANDARD.encode(path.as_bytes());
	let script = format!(
		r#"$path = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded}')); (Get-Item -LiteralPath $path).VersionInfo.FileVersion"#
	);
	let output = powershell_command()?
		.args(["-NoProfile", "-NonInteractive", "-Command", &script])
		.output()
		.context("failed to read Roblox Studio file version")?;
	ensure!(
		output.status.success(),
		"failed to read Roblox Studio file version: {}",
		String::from_utf8_lossy(&output.stderr).trim()
	);
	Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

pub fn get_studio_info() -> Result<StudioInfo> {
	if let Some(executable) = std::env::var_os("ROBLOX_STUDIO_EXE").map(PathBuf::from) {
		ensure!(
			executable.is_file(),
			"ROBLOX_STUDIO_EXE does not exist: {}",
			executable.display()
		);
		#[cfg(target_os = "linux")]
		let native = windows_path(&executable, "Roblox Studio executable")?;
		#[cfg(not(target_os = "linux"))]
		let native = executable.to_string_lossy().into_owned();
		#[cfg(any(target_os = "linux", target_os = "windows"))]
		let raw_version = windows_file_version(&native)?;
		#[cfg(target_os = "macos")]
		let raw_version = macos_studio_version(&executable)?;
		let (version_text, version_components) = parse_version(&raw_version)?;
		let build_id = executable
			.parent()
			.and_then(Path::file_name)
			.and_then(|name| name.to_str())
			.unwrap_or("custom")
			.to_owned();
		return Ok(StudioInfo {
			executable,
			version_text,
			version_components,
			build_id,
		});
	}

	#[cfg(target_os = "linux")]
	{
		ensure!(
			std::env::var_os("WSL_DISTRO_NAME").is_some(),
			"automatic Studio discovery on Linux requires WSL"
		);
		let output = powershell_command()?
			.args([
				"-NoProfile",
				"-NonInteractive",
				"-Command",
				r#"$studio = Get-ChildItem "$env:LOCALAPPDATA/Roblox/Versions/*/RobloxStudioBeta.exe" -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1; if ($null -eq $studio) { exit 3 }; Write-Output ($studio.FullName + [char]9 + $studio.VersionInfo.FileVersion)"#,
			])
			.output()
			.context("failed to locate Roblox Studio through PowerShell")?;
		ensure!(output.status.success(), "Roblox Studio is not installed");
		let stdout = String::from_utf8(output.stdout)?;
		let mut parts = stdout.trim().split('\t');
		let native = parts.next().context("missing Studio executable path")?;
		let raw_version = parts.next().context("missing Studio file version")?;
		let translated = Command::new("wslpath")
			.args(["-u", native])
			.output()
			.context("failed to translate Roblox Studio path")?;
		ensure!(translated.status.success(), "failed to translate Roblox Studio path");
		let executable = PathBuf::from(String::from_utf8(translated.stdout)?.trim());
		let build_id = executable
			.parent()
			.and_then(Path::file_name)
			.and_then(|name| name.to_str())
			.unwrap_or("latest")
			.to_owned();
		let (version_text, version_components) = parse_version(raw_version)?;
		Ok(StudioInfo {
			executable,
			version_text,
			version_components,
			build_id,
		})
	}

	#[cfg(target_os = "windows")]
	{
		use roblox_install::RobloxStudio;
		let executable = RobloxStudio::locate()?.application_path().to_owned();
		ensure!(executable.is_file(), "Roblox Studio executable not found");
		let raw_version = windows_file_version(&executable.to_string_lossy())?;
		let (version_text, version_components) = parse_version(&raw_version)?;
		let build_id = executable
			.parent()
			.and_then(Path::file_name)
			.and_then(|name| name.to_str())
			.unwrap_or("latest")
			.to_owned();
		return Ok(StudioInfo {
			executable,
			version_text,
			version_components,
			build_id,
		});
	}

	#[cfg(target_os = "macos")]
	{
		use roblox_install::RobloxStudio;
		let executable = RobloxStudio::locate()?.application_path().to_owned();
		ensure!(executable.is_file(), "Roblox Studio executable not found");
		let raw_version = macos_studio_version(&executable)?;
		let (version_text, version_components) = parse_version(&raw_version)?;
		return Ok(StudioInfo {
			executable,
			version_text,
			version_components,
			build_id: "RobloxStudio.app".to_owned(),
		});
	}

	#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
	anyhow::bail!("Roblox Studio is unsupported on this platform")
}

#[cfg(target_os = "macos")]
fn macos_studio_version(executable: &Path) -> Result<String> {
	let bundle = executable
		.ancestors()
		.find(|path| path.extension().is_some_and(|extension| extension == "app"))
		.context("Studio executable is not inside an app bundle")?;
	let plist = bundle.join("Contents/Info.plist");
	let output = Command::new("/usr/libexec/PlistBuddy")
		.args(["-c", "Print :CFBundleShortVersionString"])
		.arg(&plist)
		.output()?;
	ensure!(
		output.status.success(),
		"failed to read Studio version from {}",
		plist.display()
	);
	Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(target_os = "linux")]
fn windows_path(path: &Path, description: &str) -> Result<String> {
	let output = Command::new("wslpath").arg("-w").arg(path).output()?;
	ensure!(
		output.status.success(),
		"failed to translate the {description} path for Windows"
	);
	Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn broker_studio_executable(studio: &StudioInfo) -> Result<String> {
	#[cfg(target_os = "linux")]
	{
		windows_path(&studio.executable, "Roblox Studio executable")
	}
	#[cfg(not(target_os = "linux"))]
	{
		Ok(studio.executable.to_string_lossy().into_owned())
	}
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn launch_process(path: Option<&Path>, studio: &StudioInfo) -> Result<(u32, String, u64)> {
	#[cfg(target_os = "linux")]
	let executable = windows_path(&studio.executable, "Roblox Studio executable")?;
	#[cfg(target_os = "windows")]
	let executable = studio.executable.to_string_lossy().into_owned();
	#[cfg(target_os = "linux")]
	let place = path.map(|path| windows_path(path, "Studio place")).transpose()?;
	#[cfg(target_os = "windows")]
	let place = path.map(|path| path.to_string_lossy().into_owned());
	let encoded_executable = BASE64_STANDARD.encode(executable.as_bytes());
	let encoded_place = BASE64_STANDARD.encode(place.as_deref().unwrap_or("").as_bytes());
	let script = format!(
		r#"
$studio = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded_executable}'))
$place = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded_place}'))
if ([string]::IsNullOrEmpty($place)) {{
    $process = Start-Process -FilePath $studio -PassThru
}} else {{
    $quotedPlace = '"' + $place + '"'
    $process = Start-Process -FilePath $studio -ArgumentList @('--task', 'EditFile', '--localPlaceFile', $quotedPlace) -PassThru
}}
$process.WaitForInputIdle(30000) | Out-Null
$process.Refresh()
Write-Output ($process.Id.ToString() + [char]9 + $process.StartTime.ToUniversalTime().ToFileTimeUtc().ToString())
"#
	);
	let output = powershell_command()?
		.args(["-NoProfile", "-NonInteractive", "-Command", &script])
		.output()
		.context("failed to launch Roblox Studio")?;
	ensure!(
		output.status.success(),
		"Roblox Studio launch failed: {}",
		String::from_utf8_lossy(&output.stderr).trim()
	);
	let stdout = String::from_utf8(output.stdout)?;
	let mut fields = stdout.trim().split('\t');
	let process_id = fields
		.next()
		.context("Studio launch omitted its process ID")?
		.parse()
		.context("Studio launch returned an invalid process ID")?;
	let creation_filetime = fields
		.next()
		.context("Studio launch omitted its process creation time")?
		.parse()
		.context("Studio launch returned an invalid process creation time")?;
	Ok((process_id, executable, creation_filetime))
}

#[cfg(target_os = "macos")]
fn launch_process(path: Option<&Path>, studio: &StudioInfo) -> Result<(u32, String, u64)> {
	let mut command = Command::new(&studio.executable);
	if let Some(path) = path {
		command.arg("--task").arg("EditFile").arg("--localPlaceFile").arg(path);
	}
	let child = command
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()?;
	Ok((child.id(), studio.executable.to_string_lossy().into_owned(), 0))
}

pub fn launch(path: Option<PathBuf>) -> Result<Option<u32>> {
	let _plugin = ensure_plugin()?;
	let studio = get_studio_info()?;
	let (process_id, _, _) = launch_process(path.as_deref(), &studio)?;
	Ok(Some(process_id))
}

pub fn launch_managed(path: PathBuf, _studio_dir: &Path) -> Result<ManagedStudio> {
	let installation = ensure_plugin()?;
	let studio = get_studio_info()?;
	let studio_executable = broker_studio_executable(&studio)?;
	let lifecycle = discover_mcp_lifecycle()?;
	let launch = lifecycle.launch(&path, &studio_executable)?;
	authorize_managed_launch(&launch)?;
	Ok(ManagedStudio {
		launch,
		studio_executable,
		startup_guard: Arc::new(Mutex::new(Some(installation))),
	})
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn validate_process_script(process_id: u32, executable: &str, creation_filetime: u64) -> String {
	let encoded = BASE64_STANDARD.encode(executable.as_bytes());
	format!(
		r#"
$expected = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded}'))
$process = Get-Process -Id {process_id} -ErrorAction SilentlyContinue
if ($null -eq $process) {{ exit 0 }}
$process.Refresh()
if (-not [string]::Equals($process.Path, $expected, [StringComparison]::OrdinalIgnoreCase)) {{ exit 3 }}
if ($process.StartTime.ToUniversalTime().ToFileTimeUtc() -ne {creation_filetime}) {{ exit 3 }}
"#
	)
}

fn terminate_process(process_id: u32, executable: &str, creation_filetime: u64) -> Result<()> {
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	{
		let script = format!(
			"{}\nStop-Process -Id {process_id} -Force -ErrorAction Stop\n",
			validate_process_script(process_id, executable, creation_filetime)
		);
		let output = powershell_command()?
			.args(["-NoProfile", "-NonInteractive", "-Command", &script])
			.output()?;
		ensure!(
			output.status.success(),
			"managed Roblox Studio process did not stop cleanly: {}",
			String::from_utf8_lossy(&output.stderr).trim()
		);
		Ok(())
	}

	#[cfg(target_os = "macos")]
	{
		let _ = executable;
		let _ = creation_filetime;
		let status = Command::new("kill").arg(process_id.to_string()).status()?;
		ensure!(status.success(), "managed Roblox Studio process did not stop cleanly");
		Ok(())
	}

	#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
	anyhow::bail!("Roblox Studio process termination is unsupported")
}

pub fn wait_for_exit(process_id: u32) -> Result<()> {
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	{
		let script = format!(
			"$process = Get-Process -Id {process_id} -ErrorAction SilentlyContinue; if ($null -ne $process) {{ $process.WaitForExit() }}"
		);
		let status = powershell_command()?
			.args(["-NoProfile", "-NonInteractive", "-Command", &script])
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status()?;
		ensure!(status.success(), "Roblox Studio process monitor failed");
		return Ok(());
	}

	#[cfg(target_os = "macos")]
	{
		loop {
			if !Command::new("kill")
				.args(["-0", &process_id.to_string()])
				.status()?
				.success()
			{
				return Ok(());
			}
			std::thread::sleep(std::time::Duration::from_millis(250));
		}
	}

	#[allow(unreachable_code)]
	Ok(())
}

#[cfg(target_os = "linux")]
fn wsl_focus_script(process_id: u32, validation: &str, restore_previous: bool) -> String {
	let restore = if restore_previous {
		"if ($previous -ne [IntPtr]::Zero) { [CarbonWindow]::ShowWindow($previous, 9) | Out-Null; [CarbonWindow]::SetForegroundWindow($previous) | Out-Null }"
	} else {
		""
	};
	format!(
		r#"
{validation}
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class CarbonWindow {{
    public delegate bool EnumProc(IntPtr hwnd, IntPtr param);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc proc, IntPtr param);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hwnd, int command);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern IntPtr SetFocus(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool attach);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
}}
'@
$target = [IntPtr]::Zero
$previous = [CarbonWindow]::GetForegroundWindow()
[CarbonWindow]::EnumWindows({{
    param($hwnd, $state)
    [uint32]$windowProcessId = 0
    [CarbonWindow]::GetWindowThreadProcessId($hwnd, [ref]$windowProcessId) | Out-Null
    if ($windowProcessId -eq {process_id} -and [CarbonWindow]::IsWindowVisible($hwnd)) {{ $script:target = $hwnd; return $false }}
    return $true
}}, [IntPtr]::Zero) | Out-Null
if ($target -eq [IntPtr]::Zero) {{ throw 'Roblox Studio window was not found' }}
[CarbonWindow]::ShowWindow($target, 9) | Out-Null
[CarbonWindow]::SetForegroundWindow($target) | Out-Null
if ([CarbonWindow]::GetForegroundWindow() -ne $target) {{
    $currentThread = [CarbonWindow]::GetCurrentThreadId()
    [uint32]$targetProcessId = 0
    $targetThread = [CarbonWindow]::GetWindowThreadProcessId($target, [ref]$targetProcessId)
    $foreground = [CarbonWindow]::GetForegroundWindow()
    [uint32]$foregroundProcessId = 0
    $foregroundThread = if ($foreground -ne [IntPtr]::Zero) {{ [CarbonWindow]::GetWindowThreadProcessId($foreground, [ref]$foregroundProcessId) }} else {{ 0 }}
    $attachedForeground = $false
    $attachedTarget = $false
    try {{
        if ($foregroundThread -ne 0 -and $foregroundThread -ne $currentThread) {{
            $attachedForeground = [CarbonWindow]::AttachThreadInput($currentThread, $foregroundThread, $true)
        }}
        if ($targetThread -ne 0 -and $targetThread -ne $currentThread -and $targetThread -ne $foregroundThread) {{
            $attachedTarget = [CarbonWindow]::AttachThreadInput($currentThread, $targetThread, $true)
        }}
        [CarbonWindow]::BringWindowToTop($target) | Out-Null
        [CarbonWindow]::SetForegroundWindow($target) | Out-Null
        [CarbonWindow]::SetFocus($target) | Out-Null
    }} finally {{
        if ($attachedTarget) {{ [CarbonWindow]::AttachThreadInput($currentThread, $targetThread, $false) | Out-Null }}
        if ($attachedForeground) {{ [CarbonWindow]::AttachThreadInput($currentThread, $foregroundThread, $false) | Out-Null }}
    }}
}}
for ($attempt = 0; $attempt -lt 20 -and [CarbonWindow]::GetForegroundWindow() -ne $target; $attempt++) {{
    Start-Sleep -Milliseconds 10
}}
if ([CarbonWindow]::GetForegroundWindow() -ne $target) {{ throw 'Roblox Studio rejected foreground activation' }}
{restore}
"#
	)
}

pub fn focus_process(
	process_id: u32,
	creation_filetime: Option<u64>,
	studio_executable: Option<&str>,
	restore_previous: bool,
) -> Result<()> {
	let creation_filetime = creation_filetime.context("Studio process creation time is unavailable")?;
	let studio_executable = studio_executable.context("Studio executable identity is unavailable")?;
	#[cfg(target_os = "windows")]
	{
		return crate::studio_windows::focus_process(
			process_id,
			creation_filetime,
			studio_executable,
			restore_previous,
		);
	}
	#[cfg(target_os = "linux")]
	{
		let validation = validate_process_script(process_id, studio_executable, creation_filetime);
		let script = wsl_focus_script(process_id, &validation, restore_previous);
		let output = powershell_command()?
			.args(["-NoProfile", "-NonInteractive", "-Command", &script])
			.output()?;
		ensure!(
			output.status.success(),
			"Roblox Studio window activation failed: {}",
			String::from_utf8_lossy(&output.stderr).trim()
		);
		Ok(())
	}
	#[cfg(target_os = "macos")]
	{
		let _ = creation_filetime;
		let _ = studio_executable;
		let script = format!(
			r#"tell application "System Events"
set matches to every process whose unix id is {process_id} and name is "RobloxStudio"
if (count of matches) is not 1 then error "Roblox Studio process is not running"
tell item 1 of matches
set frontmost to true
perform action "AXRaise" of window 1
end tell
end tell"#
		);
		let output = Command::new("osascript").args(["-e", &script]).output()?;
		ensure!(output.status.success(), "Roblox Studio window activation failed");
		return Ok(());
	}
	#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
	anyhow::bail!("Roblox Studio focus is unsupported on this platform")
}

#[allow(unused_variables)]
pub fn is_running(title: Option<String>) -> Result<bool> {
	#[cfg(target_os = "windows")]
	{
		let running = EnumWindows(|hwnd| -> bool {
			if !hwnd.IsWindowVisible() {
				return true;
			}
			if let Ok(text) = hwnd.GetWindowText() {
				if title
					.as_ref()
					.is_some_and(|title| text == format!("{title} - Roblox Studio"))
					|| title.is_none() && text.contains("Roblox Studio")
				{
					return false;
				}
			}
			true
		})
		.is_err();
		return Ok(running);
	}
	#[cfg(target_os = "linux")]
	{
		let output = powershell_command()?
			.args([
				"-NoProfile",
				"-NonInteractive",
				"-Command",
				"if (Get-Process RobloxStudioBeta -ErrorAction SilentlyContinue) { exit 0 } else { exit 1 }",
			])
			.output()?;
		return Ok(output.status.success());
	}
	#[cfg(target_os = "macos")]
	{
		let output = Command::new("pgrep").arg("-x").arg("RobloxStudio").output()?;
		return Ok(output.status.success());
	}
	#[allow(unreachable_code)]
	Ok(false)
}

#[allow(unused_variables)]
pub fn focus(title: Option<String>) -> Result<()> {
	#[cfg(target_os = "windows")]
	{
		let result = EnumWindows(|hwnd| -> bool {
			if !hwnd.IsWindowVisible() {
				return true;
			}
			if let Ok(text) = hwnd.GetWindowText() {
				if title
					.as_ref()
					.is_some_and(|title| text == format!("{title} - Roblox Studio"))
					|| title.is_none() && text.contains("Roblox Studio")
				{
					hwnd.SetForegroundWindow();
					hwnd.ShowWindow(SW::RESTORE);
					return false;
				}
			}
			true
		});
		if let Err(error) = result {
			if error.raw() != 0 {
				anyhow::bail!("Failed to focus Roblox Studio: {error}");
			}
		}
		return Ok(());
	}
	#[cfg(target_os = "macos")]
	{
		let output = Command::new("osascript")
			.args(["-e", "tell application \"RobloxStudio\" to activate"])
			.output()?;
		ensure!(output.status.success(), "failed to focus Roblox Studio");
		return Ok(());
	}
	#[cfg(target_os = "linux")]
	{
		anyhow::bail!("focus by window title is unavailable on WSL; use carbon focus with a managed session");
	}
	#[allow(unreachable_code)]
	Ok(())
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) fn powershell_command() -> Result<Command> {
	#[cfg(target_os = "linux")]
	{
		ensure!(
			std::env::var_os("WSL_DISTRO_NAME").is_some(),
			"PowerShell interoperability requires WSL"
		);
		Ok(Command::new("powershell.exe"))
	}
	#[cfg(target_os = "windows")]
	{
		Ok(Command::new("powershell.exe"))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;
	use std::{
		io::{Read, Write},
		net::{TcpListener, TcpStream},
		sync::{Arc, Mutex as StdMutex},
		thread,
	};

	fn read_json_request(stream: &mut TcpStream) -> Value {
		let mut bytes = Vec::new();
		let mut buffer = [0_u8; 4096];
		loop {
			let read = stream.read(&mut buffer).unwrap();
			bytes.extend_from_slice(&buffer[..read]);
			let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
				continue;
			};
			let headers = String::from_utf8_lossy(&bytes[..header_end]);
			let content_length = headers
				.lines()
				.find_map(|line| {
					line.to_ascii_lowercase()
						.strip_prefix("content-length: ")
						.map(str::to_owned)
				})
				.unwrap()
				.parse::<usize>()
				.unwrap();
			let body_start = header_end + 4;
			if bytes.len() >= body_start + content_length {
				return serde_json::from_slice(&bytes[body_start..body_start + content_length]).unwrap();
			}
		}
	}

	fn write_json_result(mut stream: TcpStream, result: Value) {
		let body = serde_json::to_vec(&json!({
			"content": [{"type": "text", "text": result.to_string()}],
		}))
		.unwrap();
		write!(
			stream,
			"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
			body.len()
		)
		.unwrap();
		stream.write_all(&body).unwrap();
	}

	fn write_json_error(mut stream: TcpStream, status: &str) {
		let body = br#"{"error":"synthetic lifecycle failure"}"#;
		write!(
			stream,
			"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
			body.len()
		)
		.unwrap();
		stream.write_all(body).unwrap();
	}

	#[test]
	fn managed_launch_requires_broker_process_identity_before_connection() {
		let request = mcp_launch_request(Path::new("/tmp/carbon-managed.rbxl"), r"C:\Roblox\RobloxStudioBeta.exe");

		assert_eq!(
			request,
			json!({
				"action": "launch",
				"source": "local_file",
				"local_place_file": "/tmp/carbon-managed.rbxl",
				"studio_executable": r"C:\Roblox\RobloxStudioBeta.exe",
				"require_process_identity": true,
				"wait_for_connection": false,
			})
		);
	}

	#[test]
	fn managed_launch_accepts_only_an_exact_suspended_broker_identity() {
		let response = json!({
			"launch_id": "launch-carbon-a",
			"managed": true,
			"source": "local_file",
			"state": "launching",
			"pid": 47312,
			"process_started_at_file_time": "133700123456",
			"process_running": true,
			"process_authorized": false,
		});

		assert_eq!(
			managed_launch_identity(&response).unwrap(),
			("launch-carbon-a".to_owned(), 47_312, 133_700_123_456)
		);

		let mut already_authorized = response.clone();
		already_authorized["process_authorized"] = json!(true);
		assert!(managed_launch_identity(&already_authorized).is_err());

		let mut no_creation_identity = response;
		no_creation_identity
			.as_object_mut()
			.unwrap()
			.remove("process_started_at_file_time");
		assert!(managed_launch_identity(&no_creation_identity).is_err());
	}

	#[test]
	fn managed_lifecycle_returns_only_the_final_broker_instance_id() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let endpoint = format!("http://{}/mcp/manage_instance", listener.local_addr().unwrap());
		let observed = Arc::new(StdMutex::new(Vec::new()));
		let server_observed = Arc::clone(&observed);
		let server = thread::spawn(move || {
			let responses = [
				json!({
					"launch_id": "launch-carbon-a", "managed": true, "source": "local_file",
					"state": "launching", "pid": 47312,
					"process_started_at_file_time": "133700123456",
					"process_running": true, "process_authorized": false,
				}),
				json!({
					"launch_id": "launch-carbon-a", "managed": true, "source": "local_file",
					"state": "launching", "pid": 47312,
					"process_started_at_file_time": "133700123456",
					"process_running": true, "process_authorized": true,
				}),
				json!({
					"launch_id": "launch-carbon-a", "managed": true, "source": "local_file",
					"state": "launching", "pid": 47312,
					"process_started_at_file_time": "133700123456",
					"process_running": true, "process_authorized": true,
					"process_ownership_released": true,
				}),
				json!({
					"launch_id": "launch-carbon-a", "managed": true,
					"state": "launching", "pid": 47312,
					"process_running": true,
				}),
				json!({
					"launch_id": "launch-carbon-a", "instance_id": "anon:mcp-final",
					"managed": true, "state": "connected", "pid": 47312,
					"process_running": true, "roles": ["edit"],
				}),
			];
			for response in responses {
				let (mut stream, _) = listener.accept().unwrap();
				server_observed.lock().unwrap().push(read_json_request(&mut stream));
				write_json_result(stream, response);
			}
		});

		let lifecycle = McpLifecycle::new(endpoint, None);
		let launch = lifecycle
			.launch(Path::new("/tmp/carbon-managed.rbxl"), r"C:\Roblox\RobloxStudioBeta.exe")
			.unwrap();
		launch.authorize().unwrap();
		launch.complete().unwrap();
		assert_eq!(launch.connected_instance_id().unwrap(), "anon:mcp-final");
		server.join().unwrap();

		let actions = observed
			.lock()
			.unwrap()
			.iter()
			.map(|request| request["action"].as_str().unwrap().to_owned())
			.collect::<Vec<_>>();
		assert_eq!(actions, ["launch", "authorize", "complete", "status", "status"]);
	}

	#[test]
	fn managed_lifecycle_discovery_requires_protocol_v3_process_identity() {
		let mut health = json!({
			"status": "ok",
			"service": "robloxstudio-mcp",
			"serverName": "robloxstudio-mcp",
			"capabilities": {
				"studioLifecycle": {
					"protocolVersion": 3,
					"endpoint": "/mcp/manage_instance",
					"processIdentity": {"supported": true},
				}
			}
		});

		assert_eq!(
			mcp_lifecycle_endpoint("http://127.0.0.1:58741", &health).as_deref(),
			Some("http://127.0.0.1:58741/mcp/manage_instance")
		);

		health["capabilities"]["studioLifecycle"]["processIdentity"]["supported"] = json!(false);
		assert!(mcp_lifecycle_endpoint("http://127.0.0.1:58741", &health).is_none());
		health["capabilities"]["studioLifecycle"]["processIdentity"]["supported"] = json!(true);
		health["capabilities"]["studioLifecycle"]["protocolVersion"] = json!(2);
		assert!(mcp_lifecycle_endpoint("http://127.0.0.1:58741", &health).is_none());
	}

	#[test]
	fn managed_authorization_failure_closes_the_exact_broker_launch() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let endpoint = format!("http://{}/mcp/manage_instance", listener.local_addr().unwrap());
		let observed = Arc::new(StdMutex::new(Vec::new()));
		let server_observed = Arc::clone(&observed);
		let server = thread::spawn(move || {
			let (mut launch_stream, _) = listener.accept().unwrap();
			server_observed
				.lock()
				.unwrap()
				.push(read_json_request(&mut launch_stream));
			write_json_result(
				launch_stream,
				json!({
					"launch_id": "launch-cleanup", "managed": true, "source": "local_file",
					"state": "launching", "pid": 47312,
					"process_started_at_file_time": "133700123456",
					"process_running": true, "process_authorized": false,
				}),
			);

			let (mut authorize_stream, _) = listener.accept().unwrap();
			server_observed
				.lock()
				.unwrap()
				.push(read_json_request(&mut authorize_stream));
			write_json_error(authorize_stream, "500 Internal Server Error");

			let (mut close_stream, _) = listener.accept().unwrap();
			server_observed
				.lock()
				.unwrap()
				.push(read_json_request(&mut close_stream));
			write_json_result(
				close_stream,
				json!({
					"launch_id": "launch-cleanup", "managed": true, "source": "local_file",
					"state": "failed", "pid": 47312,
					"process_started_at_file_time": "133700123456",
					"process_running": false, "close_status": "closed",
				}),
			);
		});

		let lifecycle = McpLifecycle::new(endpoint, None);
		let launch = lifecycle
			.launch(Path::new("/tmp/carbon-managed.rbxl"), r"C:\Roblox\RobloxStudioBeta.exe")
			.unwrap();
		let error = authorize_managed_launch(&launch).unwrap_err();
		assert!(format!("{error:#}").contains("authorization failed"));
		server.join().unwrap();

		let actions = observed
			.lock()
			.unwrap()
			.iter()
			.map(|request| request["action"].as_str().unwrap().to_owned())
			.collect::<Vec<_>>();
		assert_eq!(actions, ["launch", "authorize", "close"]);
	}

	#[test]
	fn managed_completion_failure_closes_the_exact_broker_launch() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let endpoint = format!("http://{}/mcp/manage_instance", listener.local_addr().unwrap());
		let observed = Arc::new(StdMutex::new(Vec::new()));
		let server_observed = Arc::clone(&observed);
		let server = thread::spawn(move || {
			let (mut launch_stream, _) = listener.accept().unwrap();
			server_observed
				.lock()
				.unwrap()
				.push(read_json_request(&mut launch_stream));
			write_json_result(
				launch_stream,
				json!({
					"launch_id": "launch-completion-cleanup", "managed": true, "source": "local_file",
					"state": "launching", "pid": 47312,
					"process_started_at_file_time": "133700123456",
					"process_running": true, "process_authorized": false,
				}),
			);

			let (mut complete_stream, _) = listener.accept().unwrap();
			server_observed
				.lock()
				.unwrap()
				.push(read_json_request(&mut complete_stream));
			write_json_error(complete_stream, "500 Internal Server Error");

			let (mut close_stream, _) = listener.accept().unwrap();
			server_observed
				.lock()
				.unwrap()
				.push(read_json_request(&mut close_stream));
			write_json_result(
				close_stream,
				json!({
					"launch_id": "launch-completion-cleanup", "managed": true, "source": "local_file",
					"state": "failed", "pid": 47312,
					"process_started_at_file_time": "133700123456",
					"process_running": false, "close_status": "already_closed",
				}),
			);
		});

		let lifecycle = McpLifecycle::new(endpoint, None);
		let launch = lifecycle
			.launch(Path::new("/tmp/carbon-managed.rbxl"), r"C:\Roblox\RobloxStudioBeta.exe")
			.unwrap();
		let error = complete_managed_launch(&launch).unwrap_err();
		assert!(format!("{error:#}").contains("ownership completion failed"));
		server.join().unwrap();

		let actions = observed
			.lock()
			.unwrap()
			.iter()
			.map(|request| request["action"].as_str().unwrap().to_owned())
			.collect::<Vec<_>>();
		assert_eq!(actions, ["launch", "complete", "close"]);
	}

	#[test]
	fn managed_association_failure_closes_the_exact_broker_launch() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let endpoint = format!("http://{}/mcp/manage_instance", listener.local_addr().unwrap());
		let observed = Arc::new(StdMutex::new(Vec::new()));
		let server_observed = Arc::clone(&observed);
		let server = thread::spawn(move || {
			let (mut launch_stream, _) = listener.accept().unwrap();
			server_observed
				.lock()
				.unwrap()
				.push(read_json_request(&mut launch_stream));
			write_json_result(
				launch_stream,
				json!({
					"launch_id": "launch-association-cleanup", "managed": true, "source": "local_file",
					"state": "launching", "pid": 47312,
					"process_started_at_file_time": "133700123456",
					"process_running": true, "process_authorized": false,
				}),
			);

			let (mut status_stream, _) = listener.accept().unwrap();
			server_observed
				.lock()
				.unwrap()
				.push(read_json_request(&mut status_stream));
			write_json_result(
				status_stream,
				json!({
					"launch_id": "launch-association-cleanup", "managed": true,
					"state": "failed", "pid": 47312, "process_running": false,
					"failure_reason": "ambiguous association",
				}),
			);

			let (mut close_stream, _) = listener.accept().unwrap();
			server_observed
				.lock()
				.unwrap()
				.push(read_json_request(&mut close_stream));
			write_json_result(
				close_stream,
				json!({
					"launch_id": "launch-association-cleanup", "managed": true, "source": "local_file",
					"state": "failed", "pid": 47312,
					"process_started_at_file_time": "133700123456",
					"process_running": false, "close_status": "already_closed",
				}),
			);
		});

		let lifecycle = McpLifecycle::new(endpoint, None);
		let launch = lifecycle
			.launch(Path::new("/tmp/carbon-managed.rbxl"), r"C:\Roblox\RobloxStudioBeta.exe")
			.unwrap();
		let error = associate_managed_launch(&launch).unwrap_err();
		assert!(format!("{error:#}").contains("association failed"));
		server.join().unwrap();

		let actions = observed
			.lock()
			.unwrap()
			.iter()
			.map(|request| request["action"].as_str().unwrap().to_owned())
			.collect::<Vec<_>>();
		assert_eq!(actions, ["launch", "status", "close"]);
	}

	#[test]
	fn invalid_launch_attestation_closes_by_launch_id() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let endpoint = format!("http://{}/mcp/manage_instance", listener.local_addr().unwrap());
		let observed = Arc::new(StdMutex::new(Vec::new()));
		let server_observed = Arc::clone(&observed);
		let server = thread::spawn(move || {
			let (mut launch_stream, _) = listener.accept().unwrap();
			server_observed
				.lock()
				.unwrap()
				.push(read_json_request(&mut launch_stream));
			write_json_result(
				launch_stream,
				json!({
					"launch_id": "launch-invalid-attestation", "managed": true, "source": "local_file",
					"state": "launching", "pid": 47312,
					"process_started_at_file_time": "133700123456",
					"process_running": true, "process_authorized": true,
				}),
			);

			listener.set_nonblocking(true).unwrap();
			let deadline = Instant::now() + Duration::from_millis(500);
			while Instant::now() < deadline {
				match listener.accept() {
					Ok((mut close_stream, _)) => {
						server_observed
							.lock()
							.unwrap()
							.push(read_json_request(&mut close_stream));
						write_json_result(
							close_stream,
							json!({
								"launch_id": "launch-invalid-attestation", "managed": true,
								"state": "failed", "pid": 47312,
								"process_running": false, "close_status": "closed",
							}),
						);
						return;
					}
					Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
						thread::sleep(Duration::from_millis(10));
					}
					Err(error) => panic!("unexpected fake broker accept error: {error}"),
				}
			}
		});

		let lifecycle = McpLifecycle::new(endpoint, None);
		assert!(lifecycle
			.launch(Path::new("/tmp/carbon-managed.rbxl"), r"C:\Roblox\RobloxStudioBeta.exe")
			.is_err());
		server.join().unwrap();

		let actions = observed
			.lock()
			.unwrap()
			.iter()
			.map(|request| request["action"].as_str().unwrap().to_owned())
			.collect::<Vec<_>>();
		assert_eq!(actions, ["launch", "close"]);
	}

	#[cfg(target_os = "linux")]
	#[test]
	fn wsl_focus_verifies_foreground_state_and_uses_thread_input_fallback() {
		let script = wsl_focus_script(47_312, "# exact process validation", false);

		assert!(script.contains("GetForegroundWindow() -ne $target"));
		assert!(script.contains("AttachThreadInput"));
		assert!(script.contains("BringWindowToTop"));
		assert!(script.contains("SetFocus"));
	}
}
