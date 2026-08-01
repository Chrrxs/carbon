use anyhow::{Context, Result};
use base64::prelude::*;
use bytes::Bytes;
use directories::BaseDirs;
use rbx_dom_weak::types::Variant;
use reqwest::blocking::Client;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
	collections::{HashMap, HashSet},
	env, fs,
	io::{self, Read, Write},
	path::{Path, PathBuf},
	process::{Command, Stdio},
	sync::{mpsc, Arc},
	thread,
	time::{Duration, Instant, SystemTime},
};

const DISCOVERY_ENV: &str = "CARBON_RML_BRIDGE_DISCOVERY";
const DISCOVERY_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const DISCOVERY_REQUEST_TIMEOUT: Duration = Duration::from_millis(750);
const EXACT_DISCOVERY_ATTEMPTS: usize = 5;
const EXACT_DISCOVERY_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Discovery {
	protocol_version: u32,
	rml_build_version: String,
	bridge_id: String,
	endpoint: String,
	token: String,
	process_id: u32,
	#[serde(default)]
	studio_session_id: Option<String>,
	#[serde(default)]
	instance_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
	pub protocol_version: u32,
	pub bridge_id: String,
	pub process_id: u32,
	#[serde(default)]
	pub engine_ready: bool,
	#[serde(default)]
	pub engine_generation: u64,
	#[serde(default)]
	pub studio_session_id: String,
	#[serde(default)]
	pub instance_id: String,
	#[serde(default)]
	pub hierarchy_sequence: u64,
	pub change_sequence: u64,
	pub binary_types: Vec<String>,
	pub scalar_types: Vec<String>,
	#[serde(default)]
	pub blittable_types: Vec<String>,
	#[serde(default)]
	pub raw_types: Vec<String>,
	pub native_observation: bool,
	pub engine_creation: bool,
	pub per_root_availability: bool,
	#[serde(default)]
	pub serialized_references: bool,
	#[serde(default)]
	pub managed_hierarchy_attachment: bool,
	#[serde(default)]
	pub managed_contract_id: String,
	#[serde(default)]
	pub managed_contract_source_instances: u32,
	#[serde(default)]
	pub manifest_identity_ledger: bool,
	#[serde(default)]
	pub manifest_identities_authoritative: bool,
	#[serde(default)]
	pub capture_lease_protocol: u16,
	#[serde(default)]
	pub local_place_save_diagnostic: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedHierarchyAttachment {
	pub attached: bool,
	pub source_instances: u32,
	pub hierarchy_sequence: u64,
	pub change_sequence: u64,
	#[serde(default)]
	pub excluded_source_ids: Vec<String>,
	#[serde(default)]
	pub source_root_debug_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedHierarchyStage {
	pub contract_id: String,
	pub source_instances: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedIdentityResolution {
	pub source_id: String,
	pub debug_id: String,
	pub marker_name: String,
	pub root_debug_id: String,
	pub root_source_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedIdentityResolutions {
	#[serde(default)]
	pub pending: bool,
	#[serde(default)]
	pub identities: Vec<ManagedIdentityResolution>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertyRead {
	pub type_name: String,
	pub value: String,
	#[serde(default)]
	pub model: Option<String>,
	#[serde(default)]
	pub model_root_debug_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertyBatchRead {
	pub type_name: Option<String>,
	pub value: Option<String>,
	pub error: Option<String>,
	#[serde(default)]
	pub model_root_debug_id: Option<String>,
	#[serde(skip)]
	pub serialized_value: Option<Variant>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertyBatchReads {
	pub values: Vec<PropertyBatchRead>,
	#[serde(default)]
	pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceBatchRead {
	pub target_debug_id: Option<String>,
	#[serde(default)]
	pub source_id: Option<String>,
	pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceBatchReads {
	pub values: Vec<ReferenceBatchRead>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertyChange {
	pub sequence: u64,
	pub debug_id: String,
	pub property: String,
	#[serde(default)]
	pub kind: String,
	#[serde(default)]
	pub root_debug_id: Option<String>,
	#[serde(default)]
	pub source_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Changes {
	pub changes: Vec<PropertyChange>,
	#[serde(default)]
	pub diagnostics: Vec<BridgeDiagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeDiagnostic {
	pub sequence: u64,
	pub severity: String,
	pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Root {
	pub class_name: String,
	pub name: String,
	pub debug_id: String,
	#[serde(default)]
	pub initially_present: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Roots {
	pub roots: Vec<Root>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootModel {
	pub model: String,
	pub roots: Vec<Root>,
	pub model_root_parent_debug_ids: Vec<String>,
	pub instance_debug_ids: Vec<String>,
	#[serde(default)]
	pub root_property_carriers: HashMap<String, String>,
	#[serde(default)]
	pub root_property_carrier_instance_debug_ids: HashMap<String, Vec<String>>,
	pub change_sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedInstance {
	pub debug_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceDebugIdentity {
	pub source_id: String,
	pub debug_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootApplyModelResponse {
	pub source_instances: Vec<SourceDebugIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioIdentity {
	pub studio_session_id: String,
	pub instance_id: String,
	pub bridge_id: String,
	pub process_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LiveSessionRequest {
	pub endpoint: String,
	pub project: String,
	pub session: String,
	pub generation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LiveSessionStatus {
	pub installed: bool,
	pub engine_generation: u64,
}

struct HelperExecutionRequest<'a> {
	port: u16,
	method: &'a str,
	path_b64: &'a str,
	content_type: &'a str,
	range: &'a str,
	timeout_ms: u64,
	stdin_payload: &'a [u8],
	process_id: u32,
}

trait HelperExecutor: Send + Sync + std::fmt::Debug {
	fn execute(&self, request: HelperExecutionRequest<'_>, writer: &mut dyn Write) -> Result<HelperExecutionResult>;
}

#[derive(Debug)]
struct HelperExecutionResult {
	status: u32,
	content_length: Option<u64>,
	content_range: Option<String>,
	copied_bytes: u64,
	error_body: Option<Vec<u8>>,
}

#[derive(Debug)]
struct ProcessHelperExecutor;

impl HelperExecutor for ProcessHelperExecutor {
	fn execute(&self, request: HelperExecutionRequest<'_>, writer: &mut dyn Write) -> Result<HelperExecutionResult> {
		let HelperExecutionRequest {
			port,
			method,
			path_b64,
			content_type,
			range,
			timeout_ms,
			stdin_payload,
			process_id,
		} = request;
		let helper_path = crate::rml::helper_path()
			.with_context(|| format!("failed to find RML helper for bridge process {process_id}"))?;

		let child = Command::new(&helper_path)
			.args([
				"bridge-request",
				&port.to_string(),
				method,
				path_b64,
				content_type,
				range,
				&timeout_ms.to_string(),
			])
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.with_context(|| {
				format!(
					"failed to spawn RML helper process for bridge process {process_id} ({})",
					helper_path.display()
				)
			})?;

		execute_helper_child(
			child,
			stdin_payload,
			writer,
			process_id,
			Duration::from_millis(timeout_ms),
		)
	}
}

fn execute_helper_child(
	mut child: std::process::Child,
	stdin_payload: &[u8],
	writer: &mut dyn Write,
	process_id: u32,
	timeout: Duration,
) -> Result<HelperExecutionResult> {
	let mut stdin_pipe = child.stdin.take();
	let mut stdout_pipe = child
		.stdout
		.take()
		.with_context(|| format!("RML helper process stdout is unavailable for bridge process {process_id}"))?;
	let stderr_pipe = child.stderr.take();
	let deadline = Instant::now()
		.checked_add(timeout)
		.context("RML helper request timeout is too large")?;

	thread::scope(|scope| -> Result<HelperExecutionResult> {
		let stderr_handle = scope.spawn(move || {
			let mut buf = Vec::new();
			if let Some(mut stderr) = stderr_pipe {
				let _ = io::copy(&mut stderr, &mut buf);
			}
			buf
		});

		let (output_sender, output_receiver) = mpsc::sync_channel(1);
		let io_handle = scope.spawn(move || {
			let body_sender = output_sender.clone();
			let result = (|| {
				if let Some(mut stdin) = stdin_pipe.take() {
					stdin.write_all(stdin_payload).with_context(|| {
						format!("failed to write request framing to RML helper process for bridge process {process_id}")
					})?;
				}
				let mut channel_writer = HelperChannelWriter { sender: body_sender };
				parse_helper_stdout(&mut stdout_pipe, &mut channel_writer, process_id)
			})();
			let _ = output_sender.send(HelperOutputMessage::Finished(result));
		});

		let parsed = loop {
			let remaining = deadline.saturating_duration_since(Instant::now());
			if remaining.is_zero() {
				break Err(anyhow::anyhow!(
					"RML helper process for bridge process {process_id} timed out after {} ms",
					timeout.as_millis()
				));
			}
			match output_receiver.recv_timeout(remaining) {
				Ok(HelperOutputMessage::Body(chunk)) => {
					if let Err(error) = writer.write_all(&chunk) {
						break Err(error).context(format!(
							"failed to stream RML helper response for bridge process {process_id}"
						));
					}
				}
				Ok(HelperOutputMessage::Finished(result)) => break result,
				Err(mpsc::RecvTimeoutError::Timeout) => {
					break Err(anyhow::anyhow!(
						"RML helper process for bridge process {process_id} timed out after {} ms",
						timeout.as_millis()
					));
				}
				Err(mpsc::RecvTimeoutError::Disconnected) => {
					break Err(anyhow::anyhow!(
						"RML helper process I/O worker stopped unexpectedly for bridge process {process_id}"
					));
				}
			}
		};

		let mut outcome = parsed.and_then(|result| {
			let exit_status = loop {
				if let Some(status) = child
					.try_wait()
					.with_context(|| format!("failed to wait for RML helper process for bridge process {process_id}"))?
				{
					break status;
				}
				let remaining = deadline.saturating_duration_since(Instant::now());
				if remaining.is_zero() {
					anyhow::bail!(
						"RML helper process for bridge process {process_id} timed out after {} ms",
						timeout.as_millis()
					);
				}
				thread::sleep(remaining.min(Duration::from_millis(5)));
			};

			anyhow::ensure!(
				exit_status.success(),
				"RML helper process for bridge process {process_id} failed with exit status {:?}",
				exit_status.code()
			);
			Ok(result)
		});

		drop(output_receiver);
		if outcome.is_err() {
			let _ = child.kill();
			let _ = child.wait();
		}
		if io_handle.join().is_err() && outcome.is_ok() {
			outcome = Err(anyhow::anyhow!(
				"RML helper process I/O worker panicked for bridge process {process_id}"
			));
		}
		let stderr_bytes = stderr_handle.join().unwrap_or_default();

		match outcome {
			Err(error) if !stderr_bytes.is_empty() => {
				let stderr_msg = String::from_utf8_lossy(&stderr_bytes);
				Err(error).with_context(|| format!("RML helper process stderr: {stderr_msg}"))
			}
			other => other,
		}
	})
}

enum HelperOutputMessage {
	Body(Vec<u8>),
	Finished(Result<HelperExecutionResult>),
}

struct HelperChannelWriter {
	sender: mpsc::SyncSender<HelperOutputMessage>,
}

impl Write for HelperChannelWriter {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		if buf.is_empty() {
			return Ok(0);
		}
		self.sender
			.send(HelperOutputMessage::Body(buf.to_vec()))
			.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "RML helper response consumer stopped"))?;
		Ok(buf.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

fn parse_helper_stdout(
	stdout_read: &mut dyn Read,
	writer: &mut dyn Write,
	process_id: u32,
) -> Result<HelperExecutionResult> {
	let mut header_buf = [0_u8; 24];
	if let Err(err) = stdout_read.read_exact(&mut header_buf) {
		anyhow::bail!("RML helper process for bridge process {process_id} failed to return framing header: {err}");
	}

	if &header_buf[0..8] != b"CBRS0001" {
		anyhow::bail!("RML bridge process {process_id} helper returned invalid framing magic");
	}

	let status = u32::from_le_bytes(header_buf[8..12].try_into().unwrap());
	let raw_len = u64::from_le_bytes(header_buf[12..20].try_into().unwrap());
	let content_length = if raw_len == u64::MAX { None } else { Some(raw_len) };
	let range_len = u32::from_le_bytes(header_buf[20..24].try_into().unwrap()) as usize;

	let content_range = if range_len > 0 {
		let mut range_bytes = vec![0_u8; range_len];
		if let Err(err) = stdout_read.read_exact(&mut range_bytes) {
			anyhow::bail!("failed to read Content-Range header from RML helper for bridge process {process_id}: {err}");
		}
		Some(
			String::from_utf8(range_bytes).with_context(|| {
				format!("invalid Content-Range UTF-8 from RML helper for bridge process {process_id}")
			})?,
		)
	} else {
		None
	};

	let is_success_status = (200..=299).contains(&status);
	if is_success_status {
		let copied = io::copy(stdout_read, writer).with_context(|| {
			format!("failed to stream response body from RML helper for bridge process {process_id}")
		})?;
		Ok(HelperExecutionResult {
			status,
			content_length,
			content_range,
			copied_bytes: copied,
			error_body: None,
		})
	} else {
		let mut err_body = Vec::new();
		stdout_read.read_to_end(&mut err_body).with_context(|| {
			format!("failed to read error response body from RML helper for bridge process {process_id}")
		})?;
		Ok(HelperExecutionResult {
			status,
			content_length,
			content_range,
			copied_bytes: 0,
			error_body: Some(err_body),
		})
	}
}

fn encode_request_path(path: &str) -> Result<String> {
	let normalized = format!("/{}", path.trim_start_matches('/'));
	anyhow::ensure!(
		!normalized.contains('\r') && !normalized.contains('\n') && !normalized.contains('\0'),
		"RML bridge request path contains control characters"
	);
	anyhow::ensure!(
		!normalized.starts_with("//") && !normalized.contains("://"),
		"RML bridge request path cannot supply a host"
	);
	Ok(BASE64_STANDARD.encode(normalized.as_bytes()))
}

fn build_stdin_payload(token: &str, body: &[u8]) -> Vec<u8> {
	let token_bytes = token.as_bytes();
	let token_len = token_bytes.len() as u32;
	let mut payload = Vec::with_capacity(8 + 4 + token_bytes.len() + body.len());
	payload.extend_from_slice(b"CBRQ0001");
	payload.extend_from_slice(&token_len.to_le_bytes());
	payload.extend_from_slice(token_bytes);
	payload.extend_from_slice(body);
	payload
}

fn parse_loopback_port(endpoint: &str) -> Result<u16> {
	anyhow::ensure!(
		endpoint.starts_with("http://127.0.0.1:"),
		"RML bridge is not loopback-only"
	);
	let url = reqwest::Url::parse(endpoint).context("invalid RML bridge endpoint")?;
	anyhow::ensure!(url.scheme() == "http", "RML bridge endpoint must use HTTP");
	anyhow::ensure!(
		url.host_str() == Some("127.0.0.1"),
		"RML bridge endpoint is not loopback"
	);
	let port = url.port().context("RML bridge endpoint has no port")?;
	anyhow::ensure!(port >= 1, "invalid RML bridge loopback port");
	Ok(port)
}

#[derive(Clone)]
enum Transport {
	Reqwest {
		client: Client,
	},
	Helper {
		port: u16,
		token: String,
		process_id: u32,
		request_timeout: Duration,
		executor: Arc<dyn HelperExecutor>,
	},
}

#[derive(Clone)]
pub struct Bridge {
	discovery: Discovery,
	endpoint: String,
	transport: Transport,
}

impl Bridge {
	pub fn discover(bridge_id: &str) -> Result<Self> {
		validate_bridge_id(bridge_id)?;
		retry_exact_bridge_discovery(
			|| {
				let path = discovery_path(bridge_id)?;
				Self::from_path(&path, Some(bridge_id))
			},
			thread::sleep,
		)
	}

	pub fn discover_studio(studio_session_id: &str, instance_id: &str) -> Result<Self> {
		anyhow::ensure!(!studio_session_id.is_empty(), "Carbon Studio session identity is empty");
		anyhow::ensure!(!instance_id.is_empty(), "Carbon Studio instance identity is empty");

		for attempt in 0..3 {
			let mut matched = None;
			let paths = routed_discovery_paths(studio_session_id, instance_id)?
				.into_iter()
				.filter(|path| {
					fs::read(path)
						.ok()
						.and_then(|bytes| serde_json::from_slice::<Discovery>(&bytes).ok())
						.is_some_and(|discovery| discovery_claims_studio(&discovery, studio_session_id, instance_id))
				});
			let (sender, receiver) = mpsc::channel();
			for path in paths {
				let sender = sender.clone();
				thread::spawn(move || {
					let result = (|| {
						let bridge = Self::from_path_with_timeouts(
							&path,
							None,
							DISCOVERY_CONNECT_TIMEOUT,
							DISCOVERY_REQUEST_TIMEOUT,
						)?;
						let identity = bridge.get::<StudioIdentity>("v1/identity")?;
						Ok::<_, anyhow::Error>((bridge.discovery, identity))
					})()
					.ok();
					sender.send(result).ok();
				});
			}
			drop(sender);
			for result in receiver {
				let Some((discovery, identity)) = result else {
					continue;
				};
				if identity.studio_session_id != studio_session_id || identity.instance_id != instance_id {
					continue;
				}
				anyhow::ensure!(
					identity.bridge_id == discovery.bridge_id && identity.process_id == discovery.process_id,
					"RML bridge identity response does not match its discovery record"
				);
				anyhow::ensure!(
					matched.is_none(),
					"multiple RML bridges claimed the same Carbon Studio session"
				);
				matched = Some(discovery.bridge_id);
			}
			if let Some(bridge_id) = matched {
				return Self::discover(&bridge_id);
			}
			if attempt < 2 {
				thread::sleep(Duration::from_millis(100));
			}
		}

		anyhow::bail!("RML bridge for Studio session {studio_session_id} ({instance_id}) is unavailable")
	}

	fn active_bridge_ids_for_process(process_id: Option<u32>, require_engine_ready: bool) -> Result<HashSet<String>> {
		let mut active = HashSet::new();
		let (sender, receiver) = mpsc::channel();
		for path in discovery_paths()? {
			// A launched capture has an exact OS process identity. Filter registry
			// records before attempting HTTP so stale bridges cannot add a full
			// discovery timeout while the edit scheduler is still active at startup.
			if let Some(process_id) = process_id {
				let matches_process = fs::read(&path)
					.ok()
					.and_then(|bytes| serde_json::from_slice::<Discovery>(&bytes).ok())
					.is_some_and(|discovery| discovery.process_id == process_id);
				if !matches_process {
					continue;
				}
			}
			let sender = sender.clone();
			thread::spawn(move || {
				let bridge_id = (|| {
					let bridge = Self::from_path_with_timeouts(
						&path,
						None,
						DISCOVERY_CONNECT_TIMEOUT,
						DISCOVERY_REQUEST_TIMEOUT,
					)
					.ok()?;
					let capabilities = bridge.get::<Capabilities>("v1/capabilities").ok()?;
					let attested = if require_engine_ready {
						is_ready_bridge(&bridge.discovery, &capabilities, process_id)
					} else {
						is_loaded_bridge(&bridge.discovery, &capabilities, process_id)
					};
					attested
						.then_some(capabilities)
						.map(|capabilities| capabilities.bridge_id)
				})();
				sender.send(bridge_id).ok();
			});
		}
		drop(sender);
		for bridge_id in receiver.into_iter().flatten() {
			active.insert(bridge_id);
		}
		Ok(active)
	}

	pub fn active_bridge_ids() -> Result<HashSet<String>> {
		Self::active_bridge_ids_for_process(None, true)
	}

	pub fn wait_for_active(excluded: &HashSet<String>, timeout: Duration) -> Result<Self> {
		let deadline = std::time::Instant::now() + timeout;
		loop {
			let candidates = Self::active_bridge_ids()?
				.into_iter()
				.filter(|bridge_id| !excluded.contains(bridge_id))
				.collect::<Vec<_>>();
			match candidates.as_slice() {
				[bridge_id] => return Self::discover(bridge_id),
				[] if std::time::Instant::now() < deadline => thread::sleep(Duration::from_millis(250)),
				[] => anyhow::bail!("timed out waiting for an active RML Studio bridge"),
				_ => anyhow::bail!(
					"multiple active RML Studio bridges are available; close unrelated Studio processes and retry"
				),
			}
		}
	}

	pub fn wait_for_loaded_process(bridge_id: &str, process_id: u32, timeout: Duration) -> Result<Self> {
		validate_bridge_id(bridge_id)?;
		let deadline = std::time::Instant::now() + timeout;
		loop {
			if let Ok(bridge) = Self::discover(bridge_id) {
				if bridge
					.get::<Capabilities>("v1/capabilities")
					.ok()
					.is_some_and(|capabilities| is_loaded_bridge(&bridge.discovery, &capabilities, Some(process_id)))
				{
					return Ok(bridge);
				}
			}
			if std::time::Instant::now() >= deadline {
				anyhow::bail!("timed out waiting for RML bridge {bridge_id} process {process_id}");
			}
			thread::sleep(Duration::from_millis(250));
		}
	}

	pub fn wait_until_process_ready(&self, process_id: u32, timeout: Duration) -> Result<()> {
		let deadline = std::time::Instant::now() + timeout;
		loop {
			if self
				.get::<Capabilities>("v1/capabilities")
				.ok()
				.is_some_and(|capabilities| is_ready_bridge(&self.discovery, &capabilities, Some(process_id)))
			{
				return Ok(());
			}
			if std::time::Instant::now() >= deadline {
				anyhow::bail!(
					"timed out waiting for RML bridge {} process {process_id} to attach an engine generation",
					self.bridge_id()
				);
			}
			thread::sleep(Duration::from_millis(250));
		}
	}

	fn from_path(path: &Path, expected_bridge_id: Option<&str>) -> Result<Self> {
		Self::from_path_with_timeouts(
			path,
			expected_bridge_id,
			Duration::from_secs(2),
			Duration::from_secs(60),
		)
	}

	fn from_path_with_timeouts(
		path: &Path,
		expected_bridge_id: Option<&str>,
		connect_timeout: Duration,
		request_timeout: Duration,
	) -> Result<Self> {
		Self::from_path_with_timeouts_for_platform(
			path,
			expected_bridge_id,
			connect_timeout,
			request_timeout,
			env::var_os("WSL_DISTRO_NAME").is_some(),
		)
	}

	fn from_path_with_timeouts_for_platform(
		path: &Path,
		expected_bridge_id: Option<&str>,
		connect_timeout: Duration,
		request_timeout: Duration,
		is_wsl: bool,
	) -> Result<Self> {
		let bytes = fs::read(path).with_context(|| format!("RML Carbon bridge is unavailable ({})", path.display()))?;
		let discovery: Discovery = serde_json::from_slice(&bytes)
			.with_context(|| format!("invalid RML Carbon bridge discovery file: {}", path.display()))?;
		anyhow::ensure!(
			discovery.protocol_version == 2,
			"unsupported RML Carbon bridge protocol"
		);
		anyhow::ensure!(
			discovery.rml_build_version == crate::rml::BUILD_VERSION,
			"RML bridge build mismatch: expected {}, loaded {}",
			crate::rml::BUILD_VERSION,
			discovery.rml_build_version
		);
		validate_bridge_id(&discovery.bridge_id)?;
		if let Some(expected_bridge_id) = expected_bridge_id {
			anyhow::ensure!(
				discovery.bridge_id == expected_bridge_id,
				"RML bridge routing identity mismatch"
			);
		}
		anyhow::ensure!(
			discovery.endpoint.starts_with("http://127.0.0.1:"),
			"RML bridge is not loopback-only"
		);
		anyhow::ensure!(!discovery.token.is_empty(), "RML bridge discovery token is empty");

		let loopback_port = parse_loopback_port(&discovery.endpoint)?;

		if is_wsl {
			let endpoint = discovery.endpoint.clone();
			let transport = Transport::Helper {
				port: loopback_port,
				token: discovery.token.clone(),
				process_id: discovery.process_id,
				request_timeout,
				executor: Arc::new(ProcessHelperExecutor),
			};
			Ok(Self {
				discovery,
				endpoint,
				transport,
			})
		} else {
			let endpoint = discovery.endpoint.clone();
			let client = Client::builder()
				.connect_timeout(connect_timeout)
				.timeout(request_timeout)
				.build()?;
			let transport = Transport::Reqwest { client };
			Ok(Self {
				discovery,
				endpoint,
				transport,
			})
		}
	}

	#[cfg(test)]
	fn with_test_transport(discovery: Discovery, transport: Transport) -> Self {
		let endpoint = discovery.endpoint.clone();
		Self {
			discovery,
			endpoint,
			transport,
		}
	}

	fn request_helper(
		&self,
		method: &str,
		path: &str,
		content_type: &str,
		range: &str,
		body: &[u8],
		writer: &mut dyn Write,
	) -> Result<HelperExecutionResult> {
		let Transport::Helper {
			port,
			token,
			process_id,
			request_timeout,
			executor,
		} = &self.transport
		else {
			anyhow::bail!("not using helper transport");
		};
		let path_b64 = encode_request_path(path)?;
		let stdin_payload = build_stdin_payload(token, body);
		let timeout_ms = request_timeout.as_millis() as u64;
		executor.execute(
			HelperExecutionRequest {
				port: *port,
				method,
				path_b64: &path_b64,
				content_type,
				range,
				timeout_ms,
				stdin_payload: &stdin_payload,
				process_id: *process_id,
			},
			writer,
		)
	}

	pub fn bridge_id(&self) -> &str {
		&self.discovery.bridge_id
	}

	pub fn process_id(&self) -> u32 {
		self.discovery.process_id
	}

	pub fn get<R: DeserializeOwned>(&self, path: &str) -> Result<R> {
		match &self.transport {
			Transport::Reqwest { client } => {
				let response = client
					.get(self.url(path))
					.bearer_auth(&self.discovery.token)
					.send()
					.with_context(|| format!("failed to contact RML bridge process {}", self.discovery.process_id))?;
				Self::ensure_success(path, response)?
					.json()
					.context("failed to decode RML bridge response")
			}
			Transport::Helper { .. } => {
				let mut body_buf = Vec::new();
				let res = self.request_helper("GET", path, "-", "-", &[], &mut body_buf)?;
				if !(200..=299).contains(&res.status) {
					let detail = res
						.error_body
						.as_deref()
						.map(|b| String::from_utf8_lossy(b).into_owned())
						.unwrap_or_else(String::new);
					anyhow::bail!("RML bridge {path} returned {}: {detail}", res.status);
				}
				if let Some(declared) = res.content_length {
					anyhow::ensure!(
						res.copied_bytes == declared,
						"RML bridge artifact length mismatch: declared {declared}, received {}",
						res.copied_bytes
					);
				}
				serde_json::from_slice(&body_buf).context("failed to decode RML bridge response")
			}
		}
	}

	pub fn post<T: Serialize + ?Sized, R: DeserializeOwned>(&self, path: &str, body: &T) -> Result<R> {
		match &self.transport {
			Transport::Reqwest { client } => {
				let response = client
					.post(self.url(path))
					.bearer_auth(&self.discovery.token)
					.json(body)
					.send()
					.with_context(|| format!("failed to contact RML bridge process {}", self.discovery.process_id))?;
				Self::ensure_success(path, response)?
					.json()
					.context("failed to decode RML bridge response")
			}
			Transport::Helper { .. } => {
				let req_bytes = serde_json::to_vec(body).context("failed to serialize RML bridge request body")?;
				let mut body_buf = Vec::new();
				let res = self.request_helper("POST", path, "application/json", "-", &req_bytes, &mut body_buf)?;
				if !(200..=299).contains(&res.status) {
					let detail = res
						.error_body
						.as_deref()
						.map(|b| String::from_utf8_lossy(b).into_owned())
						.unwrap_or_else(String::new);
					anyhow::bail!("RML bridge {path} returned {}: {detail}", res.status);
				}
				if let Some(declared) = res.content_length {
					anyhow::ensure!(
						res.copied_bytes == declared,
						"RML bridge artifact length mismatch: declared {declared}, received {}",
						res.copied_bytes
					);
				}
				serde_json::from_slice(&body_buf).context("failed to decode RML bridge response")
			}
		}
	}

	pub fn post_bytes<R: DeserializeOwned>(&self, path: &str, body: Bytes) -> Result<R> {
		match &self.transport {
			Transport::Reqwest { client } => {
				let response = client
					.post(self.url(path))
					.bearer_auth(&self.discovery.token)
					.header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
					.body(body)
					.send()
					.with_context(|| format!("failed to contact RML bridge process {}", self.discovery.process_id))?;
				Self::ensure_success(path, response)?
					.json()
					.context("failed to decode RML bridge response")
			}
			Transport::Helper { .. } => {
				let mut body_buf = Vec::new();
				let res = self.request_helper("POST", path, "application/octet-stream", "-", &body, &mut body_buf)?;
				if !(200..=299).contains(&res.status) {
					let detail = res
						.error_body
						.as_deref()
						.map(|b| String::from_utf8_lossy(b).into_owned())
						.unwrap_or_else(String::new);
					anyhow::bail!("RML bridge {path} returned {}: {detail}", res.status);
				}
				if let Some(declared) = res.content_length {
					anyhow::ensure!(
						res.copied_bytes == declared,
						"RML bridge artifact length mismatch: declared {declared}, received {}",
						res.copied_bytes
					);
				}
				serde_json::from_slice(&body_buf).context("failed to decode RML bridge response")
			}
		}
	}

	pub fn delete<R: DeserializeOwned>(&self, path: &str) -> Result<R> {
		match &self.transport {
			Transport::Reqwest { client } => {
				let response = client
					.delete(self.url(path))
					.bearer_auth(&self.discovery.token)
					.send()
					.with_context(|| format!("failed to contact RML bridge process {}", self.discovery.process_id))?;
				Self::ensure_success(path, response)?
					.json()
					.context("failed to decode RML bridge response")
			}
			Transport::Helper { .. } => {
				let mut body_buf = Vec::new();
				let res = self.request_helper("DELETE", path, "-", "-", &[], &mut body_buf)?;
				if !(200..=299).contains(&res.status) {
					let detail = res
						.error_body
						.as_deref()
						.map(|b| String::from_utf8_lossy(b).into_owned())
						.unwrap_or_else(String::new);
					anyhow::bail!("RML bridge {path} returned {}: {detail}", res.status);
				}
				if let Some(declared) = res.content_length {
					anyhow::ensure!(
						res.copied_bytes == declared,
						"RML bridge artifact length mismatch: declared {declared}, received {}",
						res.copied_bytes
					);
				}
				serde_json::from_slice(&body_buf).context("failed to decode RML bridge response")
			}
		}
	}

	/// Stream a raw bridge artifact directly into a bounded-storage writer.
	///
	/// Capture payloads use this route so the RBXM never becomes a JSON/base64
	/// string or a second complete in-memory Rust buffer.
	pub fn get_to_writer<W: Write + ?Sized>(&self, path: &str, mut writer: &mut W) -> Result<u64> {
		match &self.transport {
			Transport::Reqwest { client } => {
				let response = client
					.get(self.url(path))
					.bearer_auth(&self.discovery.token)
					.send()
					.with_context(|| format!("failed to contact RML bridge process {}", self.discovery.process_id))?;
				let mut response = Self::ensure_success(path, response)?;
				let declared = response.content_length();
				let copied = io::copy(&mut response, writer).context("failed to stream RML bridge artifact")?;
				if let Some(declared) = declared {
					anyhow::ensure!(
						copied == declared,
						"RML bridge artifact length mismatch: declared {declared}, received {copied}"
					);
				}
				Ok(copied)
			}
			Transport::Helper { .. } => {
				let res = self.request_helper("GET", path, "-", "-", &[], &mut writer)?;
				if !(200..=299).contains(&res.status) {
					let detail = res
						.error_body
						.as_deref()
						.map(|b| String::from_utf8_lossy(b).into_owned())
						.unwrap_or_else(String::new);
					anyhow::bail!("RML bridge {path} returned {}: {detail}", res.status);
				}
				if let Some(declared) = res.content_length {
					anyhow::ensure!(
						res.copied_bytes == declared,
						"RML bridge artifact length mismatch: declared {declared}, received {}",
						res.copied_bytes
					);
				}
				Ok(res.copied_bytes)
			}
		}
	}

	pub fn get_range_to_writer<W: Write + ?Sized>(
		&self,
		path: &str,
		offset: u64,
		length: u64,
		mut writer: &mut W,
	) -> Result<u64> {
		if length == 0 {
			return Ok(0);
		}
		let end = offset
			.checked_add(length - 1)
			.context("RML bridge artifact range overflows u64")?;

		match &self.transport {
			Transport::Reqwest { client } => {
				let response = client
					.get(self.url(path))
					.bearer_auth(&self.discovery.token)
					.header(reqwest::header::RANGE, format!("bytes={offset}-{end}"))
					.send()
					.with_context(|| format!("failed to contact RML bridge process {}", self.discovery.process_id))?;
				let mut response = Self::ensure_success(path, response)?;
				anyhow::ensure!(
					response.status() == reqwest::StatusCode::PARTIAL_CONTENT,
					"RML bridge ignored the requested artifact byte range"
				);
				let content_range = response
					.headers()
					.get(reqwest::header::CONTENT_RANGE)
					.context("RML bridge range response has no Content-Range header")?
					.to_str()
					.context("RML bridge returned a non-text Content-Range header")?;
				let expected_prefix = format!("bytes {offset}-{end}/");
				let total = content_range
					.strip_prefix(&expected_prefix)
					.context("RML bridge returned the wrong artifact byte range")?
					.parse::<u64>()
					.context("RML bridge returned an invalid artifact range length")?;
				anyhow::ensure!(total > end, "RML bridge artifact range exceeds its published length");
				let copied = io::copy(&mut response, writer).context("failed to stream RML bridge artifact range")?;
				anyhow::ensure!(
					copied == length,
					"RML bridge artifact range length mismatch: requested {length}, received {copied}"
				);
				Ok(copied)
			}
			Transport::Helper { .. } => {
				let range_header = format!("bytes={offset}-{end}");
				let res = self.request_helper("GET", path, "-", &range_header, &[], &mut writer)?;
				if res.status != 206 {
					if (200..=299).contains(&res.status) {
						anyhow::bail!("RML bridge ignored the requested artifact byte range");
					}
					let detail = res
						.error_body
						.as_deref()
						.map(|b| String::from_utf8_lossy(b).into_owned())
						.unwrap_or_else(String::new);
					anyhow::bail!("RML bridge {path} returned {}: {detail}", res.status);
				}
				let content_range = res
					.content_range
					.as_deref()
					.context("RML bridge range response has no Content-Range header")?;
				let expected_prefix = format!("bytes {offset}-{end}/");
				let total = content_range
					.strip_prefix(&expected_prefix)
					.context("RML bridge returned the wrong artifact byte range")?
					.parse::<u64>()
					.context("RML bridge returned an invalid artifact range length")?;
				anyhow::ensure!(total > end, "RML bridge artifact range exceeds its published length");
				anyhow::ensure!(
					res.copied_bytes == length,
					"RML bridge artifact range length mismatch: requested {length}, received {}",
					res.copied_bytes
				);
				Ok(res.copied_bytes)
			}
		}
	}

	fn ensure_success(path: &str, response: reqwest::blocking::Response) -> Result<reqwest::blocking::Response> {
		let status = response.status();
		if status.is_success() {
			return Ok(response);
		}
		let detail = response
			.text()
			.unwrap_or_else(|error| format!("failed to read error response: {error}"));
		anyhow::bail!("RML bridge {path} returned {status}: {detail}")
	}

	fn url(&self, path: &str) -> String {
		format!("{}{}", self.endpoint, path.trim_start_matches('/'))
	}
}

fn is_loaded_bridge(discovery: &Discovery, capabilities: &Capabilities, process_id: Option<u32>) -> bool {
	capabilities.bridge_id == discovery.bridge_id
		&& capabilities.process_id == discovery.process_id
		&& process_id.is_none_or(|expected| capabilities.process_id == expected)
}

fn is_ready_bridge(discovery: &Discovery, capabilities: &Capabilities, process_id: Option<u32>) -> bool {
	is_loaded_bridge(discovery, capabilities, process_id)
		&& capabilities.engine_ready
		&& capabilities.engine_generation != 0
}

fn validate_bridge_id(bridge_id: &str) -> Result<()> {
	anyhow::ensure!(
		bridge_id.len() == 32 && bridge_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
		"invalid RML bridge routing identity"
	);
	Ok(())
}

fn retry_exact_bridge_discovery<T, Discover, Wait>(mut discover: Discover, mut wait: Wait) -> Result<T>
where
	Discover: FnMut() -> Result<T>,
	Wait: FnMut(Duration),
{
	for attempt in 1..=EXACT_DISCOVERY_ATTEMPTS {
		match discover() {
			Ok(value) => return Ok(value),
			Err(error) if attempt < EXACT_DISCOVERY_ATTEMPTS && discovery_record_is_missing(&error) => {
				wait(EXACT_DISCOVERY_RETRY_DELAY);
			}
			Err(error) => return Err(error),
		}
	}
	unreachable!("exact bridge discovery attempts are nonzero")
}

fn discovery_record_is_missing(error: &anyhow::Error) -> bool {
	error.chain().any(|cause| {
		cause
			.downcast_ref::<io::Error>()
			.is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
	})
}

fn discovery_claims_studio(discovery: &Discovery, studio_session_id: &str, instance_id: &str) -> bool {
	discovery.studio_session_id.as_deref() == Some(studio_session_id)
		&& discovery.instance_id.as_deref() == Some(instance_id)
}

fn studio_route_key(studio_session_id: &str, instance_id: &str) -> String {
	let mut hash = 14_695_981_039_346_656_037_u64;
	for value in studio_session_id
		.bytes()
		.chain(std::iter::once(b'\n'))
		.chain(instance_id.bytes())
	{
		hash ^= u64::from(value);
		hash = hash.wrapping_mul(1_099_511_628_211);
	}
	format!("{hash:016x}")
}

fn discovery_path(bridge_id: &str) -> Result<PathBuf> {
	if let Some(path) = env::var_os(DISCOVERY_ENV) {
		return Ok(PathBuf::from(path));
	}
	let base = BaseDirs::new().context("local application data directory is unavailable")?;
	let native = base
		.data_local_dir()
		.join("RobloxModLoader/carbon-bridges/v1")
		.join(format!("{bridge_id}.json"));
	if native.exists() || env::var_os("WSL_DISTRO_NAME").is_none() {
		return Ok(native);
	}

	// Studio runs on Windows while Carbon commonly runs inside WSL. In that
	// arrangement each process has a different LocalApplicationData directory.
	// Select the exact bridge identity visible through the mounted Windows users
	// tree. Multiple Studio processes therefore never compete for one record.
	Ok(wsl_discovery_path(Path::new("/mnt/c/Users"), bridge_id).unwrap_or(native))
}

fn discovery_paths() -> Result<Vec<PathBuf>> {
	if let Some(path) = env::var_os(DISCOVERY_ENV) {
		return Ok(vec![PathBuf::from(path)]);
	}

	let base = BaseDirs::new().context("local application data directory is unavailable")?;
	let native_registry = base.data_local_dir().join("RobloxModLoader/carbon-bridges/v1");
	let mut paths = registry_paths(&native_registry);
	if env::var_os("WSL_DISTRO_NAME").is_some() {
		paths.extend(wsl_discovery_paths(Path::new("/mnt/c/Users")));
	}
	paths.sort_by_key(|path| {
		std::cmp::Reverse(
			path.metadata()
				.and_then(|metadata| metadata.modified())
				.unwrap_or(SystemTime::UNIX_EPOCH),
		)
	});
	paths.dedup();
	Ok(paths)
}

/// Remove only discovery records carrying the bridge identity attested for
/// this Studio launch. A later Studio may reuse the PID but never this bridge
/// ID, so cleanup cannot erase another live launch.
pub fn cleanup_bridge_discovery(process_id: u32, bridge_id: &str) -> Result<usize> {
	validate_bridge_id(bridge_id)?;
	cleanup_discovery_records(discovery_record_paths()?, process_id, bridge_id)
}

fn discovery_record_paths() -> Result<Vec<PathBuf>> {
	if let Some(path) = env::var_os(DISCOVERY_ENV) {
		return Ok(vec![PathBuf::from(path)]);
	}

	let base = BaseDirs::new().context("local application data directory is unavailable")?;
	let mut roots = vec![base.data_local_dir().join("RobloxModLoader/carbon-bridges")];
	if env::var_os("WSL_DISTRO_NAME").is_some() {
		roots.extend(
			fs::read_dir("/mnt/c/Users")
				.into_iter()
				.flatten()
				.filter_map(|entry| entry.ok())
				.map(|entry| entry.path().join("AppData/Local/RobloxModLoader/carbon-bridges")),
		);
	}

	let mut paths = Vec::new();
	for root in roots {
		paths.extend(registry_paths(&root.join("v1")));
		paths.extend(route_registry_paths(&root.join("routes/v1")));
	}
	paths.sort();
	paths.dedup();
	Ok(paths)
}

fn route_registry_paths(routes: &Path) -> Vec<PathBuf> {
	fs::read_dir(routes)
		.into_iter()
		.flatten()
		.filter_map(|entry| entry.ok())
		.filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
		.flat_map(|entry| registry_paths(&entry.path()))
		.collect()
}

fn cleanup_discovery_records(paths: Vec<PathBuf>, process_id: u32, bridge_id: &str) -> Result<usize> {
	let mut removed = 0;
	for path in paths {
		// Re-read immediately before deletion so a malformed/replaced file or a
		// later Studio launch that reused the PID can never lose its record.
		let still_owned = fs::read(&path)
			.ok()
			.and_then(|bytes| serde_json::from_slice::<Discovery>(&bytes).ok())
			.is_some_and(|current| current.process_id == process_id && current.bridge_id == bridge_id);
		if !still_owned {
			continue;
		}
		fs::remove_file(&path)
			.with_context(|| format!("failed to remove exited RML bridge record {}", path.display()))?;
		removed += 1;
	}
	Ok(removed)
}

fn routed_discovery_paths(studio_session_id: &str, instance_id: &str) -> Result<Vec<PathBuf>> {
	let route_key = studio_route_key(studio_session_id, instance_id);
	let base = BaseDirs::new().context("local application data directory is unavailable")?;
	let mut paths = registry_paths(
		&base
			.data_local_dir()
			.join("RobloxModLoader/carbon-bridges/routes/v1")
			.join(&route_key),
	);
	if env::var_os("WSL_DISTRO_NAME").is_some() {
		paths.extend(
			fs::read_dir("/mnt/c/Users")
				.into_iter()
				.flatten()
				.filter_map(|entry| entry.ok())
				.flat_map(|entry| {
					registry_paths(
						&entry
							.path()
							.join("AppData/Local/RobloxModLoader/carbon-bridges/routes/v1")
							.join(&route_key),
					)
				}),
		);
	}
	paths.sort();
	paths.dedup();
	Ok(paths)
}

fn registry_paths(registry: &Path) -> Vec<PathBuf> {
	fs::read_dir(registry)
		.into_iter()
		.flatten()
		.filter_map(|entry| {
			let path = entry.ok()?.path();
			(path.extension().and_then(|extension| extension.to_str()) == Some("json")).then_some(path)
		})
		.collect()
}

fn wsl_discovery_paths(users_root: &Path) -> Vec<PathBuf> {
	fs::read_dir(users_root)
		.into_iter()
		.flatten()
		.filter_map(|entry| entry.ok())
		.flat_map(|entry| registry_paths(&entry.path().join("AppData/Local/RobloxModLoader/carbon-bridges/v1")))
		.collect()
}

fn wsl_discovery_path(users_root: &Path, bridge_id: &str) -> Option<PathBuf> {
	fs::read_dir(users_root)
		.ok()?
		.filter_map(|entry| {
			let path = entry
				.ok()?
				.path()
				.join("AppData/Local/RobloxModLoader/carbon-bridges/v1")
				.join(format!("{bridge_id}.json"));
			let metadata = path.metadata().ok()?;
			metadata
				.is_file()
				.then(|| (path, metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH)))
		})
		.max_by_key(|(_, modified)| *modified)
		.map(|(path, _)| path)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn discovery_and_capabilities(engine_ready: bool, engine_generation: u64) -> (Discovery, Capabilities) {
		let discovery = Discovery {
			protocol_version: 2,
			rml_build_version: crate::rml::BUILD_VERSION.to_owned(),
			bridge_id: "0123456789abcdef0123456789abcdef".to_owned(),
			endpoint: "http://127.0.0.1:1234/".to_owned(),
			token: "secret".to_owned(),
			process_id: 42,
			studio_session_id: None,
			instance_id: None,
		};
		let capabilities = Capabilities {
			protocol_version: 2,
			bridge_id: discovery.bridge_id.clone(),
			process_id: discovery.process_id,
			engine_ready,
			engine_generation,
			studio_session_id: String::new(),
			instance_id: String::new(),
			hierarchy_sequence: 0,
			change_sequence: 0,
			binary_types: Vec::new(),
			scalar_types: Vec::new(),
			blittable_types: Vec::new(),
			raw_types: Vec::new(),
			native_observation: true,
			engine_creation: true,
			per_root_availability: true,
			serialized_references: true,
			managed_hierarchy_attachment: true,
			managed_contract_id: String::new(),
			managed_contract_source_instances: 0,
			manifest_identity_ledger: true,
			manifest_identities_authoritative: true,
			capture_lease_protocol: crate::capture_provider::CAPTURE_ENVELOPE_VERSION,
			local_place_save_diagnostic: true,
		};
		(discovery, capabilities)
	}

	#[test]
	fn capabilities_preserve_the_serialized_reference_flag() {
		let (_, capabilities) = discovery_and_capabilities(true, 3);
		let value = serde_json::to_value(capabilities).unwrap();
		assert_eq!(value["serializedReferences"], true);
	}

	#[test]
	fn exact_discovery_retries_a_transient_record_replacement() {
		let mut attempts = 0;
		let mut waits = 0;
		let result = retry_exact_bridge_discovery(
			|| {
				attempts += 1;
				if attempts < 3 {
					return Err(anyhow::Error::new(io::Error::from(io::ErrorKind::NotFound)));
				}
				Ok("bridge")
			},
			|_| waits += 1,
		)
		.unwrap();

		assert_eq!(result, "bridge");
		assert_eq!(attempts, 3);
		assert_eq!(waits, 2);
	}

	#[test]
	fn exact_discovery_does_not_retry_an_invalid_record() {
		let mut attempts = 0;
		let mut waits = 0;
		let result = retry_exact_bridge_discovery::<(), _, _>(
			|| {
				attempts += 1;
				anyhow::bail!("invalid discovery record")
			},
			|_| waits += 1,
		);

		assert_eq!(result.unwrap_err().to_string(), "invalid discovery record");
		assert_eq!(attempts, 1);
		assert_eq!(waits, 0);
	}

	#[test]
	fn reference_reads_preserve_the_optional_managed_source_identity() {
		let source_id = "0123456789abcdef0123456789abcdef";
		let response: ReferenceBatchReads = serde_json::from_value(serde_json::json!({
			"values": [{
				"targetDebugId": "runtime-target",
				"sourceId": source_id,
				"error": null
			}]
		}))
		.unwrap();
		assert_eq!(response.values[0].target_debug_id.as_deref(), Some("runtime-target"));
		assert_eq!(response.values[0].source_id.as_deref(), Some(source_id));
	}

	#[test]
	fn discovery_rejects_non_loopback_endpoint() {
		let parsed: Discovery = serde_json::from_value(serde_json::json!({
			"protocolVersion": 2,
			"rmlBuildVersion": crate::rml::BUILD_VERSION,
			"bridgeId": "0123456789abcdef0123456789abcdef",
			"endpoint": "http://example.com/",
			"token": "secret",
			"processId": 1
		}))
		.unwrap();
		assert!(!parsed.endpoint.starts_with("http://127.0.0.1:"));
	}

	#[test]
	fn discovery_rejects_the_pre_guid_bridge_protocol_before_connecting() {
		let path = env::temp_dir().join(format!(
			"carbon-old-rml-protocol-{}-{}.json",
			std::process::id(),
			uuid::Uuid::new_v4().simple()
		));
		fs::write(
			&path,
			serde_json::to_vec(&serde_json::json!({
				"protocolVersion": 1,
				"rmlBuildVersion": crate::rml::BUILD_VERSION,
				"bridgeId": "0123456789abcdef0123456789abcdef",
				"endpoint": "http://127.0.0.1:1/",
				"token": "secret",
				"processId": 1
			}))
			.unwrap(),
		)
		.unwrap();

		let error =
			match Bridge::from_path_with_timeouts(&path, None, Duration::from_millis(1), Duration::from_millis(1)) {
				Ok(_) => panic!("old bridge protocol was accepted"),
				Err(error) => error.to_string(),
			};

		assert!(error.contains("unsupported RML Carbon bridge protocol"), "{error}");
		fs::remove_file(path).unwrap();
	}

	#[test]
	fn discovery_rejects_a_different_native_rml_build_before_connecting() {
		let path = env::temp_dir().join(format!(
			"carbon-wrong-rml-build-{}-{}.json",
			std::process::id(),
			uuid::Uuid::new_v4().simple()
		));
		fs::write(
			&path,
			serde_json::to_vec(&serde_json::json!({
				"protocolVersion": 2,
				"rmlBuildVersion": "0.0.0+different-worktree",
				"bridgeId": "0123456789abcdef0123456789abcdef",
				"endpoint": "http://127.0.0.1:1/",
				"token": "secret",
				"processId": 1
			}))
			.unwrap(),
		)
		.unwrap();

		let error =
			match Bridge::from_path_with_timeouts(&path, None, Duration::from_millis(1), Duration::from_millis(1)) {
				Ok(_) => panic!("another worktree's RML bridge was accepted"),
				Err(error) => error.to_string(),
			};

		assert!(error.contains("RML bridge build mismatch"), "{error}");
		fs::remove_file(path).unwrap();
	}

	#[test]
	fn discovery_route_claim_requires_both_exact_identities() {
		let (mut discovery, _) = discovery_and_capabilities(true, 3);
		discovery.studio_session_id = Some("studio-session".to_owned());
		discovery.instance_id = Some("studio-instance".to_owned());

		assert!(discovery_claims_studio(&discovery, "studio-session", "studio-instance"));
		assert!(!discovery_claims_studio(&discovery, "other-session", "studio-instance"));
		assert!(!discovery_claims_studio(&discovery, "studio-session", "other-instance"));
	}

	#[test]
	fn studio_route_key_matches_the_bridge_index_contract() {
		assert_eq!(
			studio_route_key("studio-session", "studio-instance"),
			"fc03bbceded1831a"
		);
		assert_ne!(
			studio_route_key("studio-session", "studio-instance"),
			studio_route_key("studio-session", "other-instance")
		);
	}

	#[test]
	fn wsl_discovery_routes_to_the_exact_studio_bridge() {
		let root = env::temp_dir().join(format!("carbon-wsl-discovery-{}", std::process::id()));
		let first_id = "0123456789abcdef0123456789abcdef";
		let second_id = "fedcba9876543210fedcba9876543210";
		let registry = root.join("developer/AppData/Local/RobloxModLoader/carbon-bridges/v1");
		fs::create_dir_all(&registry).unwrap();
		fs::write(registry.join(format!("{first_id}.json")), b"{}").unwrap();
		let discovery = registry.join(format!("{second_id}.json"));
		fs::write(&discovery, b"{}").unwrap();

		assert_eq!(wsl_discovery_path(&root, second_id), Some(discovery));
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn bridge_ids_are_path_safe_and_fixed_width() {
		assert!(validate_bridge_id("0123456789abcdef0123456789abcdef").is_ok());
		assert!(validate_bridge_id("../carbon-bridge").is_err());
		assert!(validate_bridge_id("0123456789abcdef").is_err());
	}

	#[test]
	fn discovery_waits_until_the_exact_process_has_an_attached_engine_generation() {
		let (discovery, mut capabilities) = discovery_and_capabilities(false, 0);
		assert!(!is_ready_bridge(&discovery, &capabilities, Some(42)));
		assert!(is_loaded_bridge(&discovery, &capabilities, Some(42)));
		assert!(!is_loaded_bridge(&discovery, &capabilities, Some(7)));
		capabilities.engine_ready = true;
		assert!(!is_ready_bridge(&discovery, &capabilities, Some(42)));
		capabilities.engine_generation = 1;
		assert!(is_ready_bridge(&discovery, &capabilities, Some(42)));
		assert!(!is_ready_bridge(&discovery, &capabilities, Some(7)));
	}

	#[test]
	fn ready_attestation_reuses_the_exact_discovered_bridge_without_registry_access() {
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let (mut discovery, capabilities) = discovery_and_capabilities(true, 1);
		discovery.endpoint = format!("http://{}/", listener.local_addr().unwrap());
		let endpoint = discovery.endpoint.clone();
		let client = Client::builder()
			.connect_timeout(Duration::from_millis(100))
			.timeout(Duration::from_millis(100))
			.build()
			.unwrap();
		let bridge = Bridge {
			discovery,
			endpoint,
			transport: Transport::Reqwest { client },
		};

		let server = thread::spawn(move || {
			let (mut stream, _) = listener.accept().unwrap();
			let mut request = [0_u8; 2048];
			std::io::Read::read(&mut stream, &mut request).unwrap();
			let body = serde_json::to_vec(&capabilities).unwrap();
			write!(
				stream,
				"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
				body.len()
			)
			.unwrap();
			stream.write_all(&body).unwrap();
		});

		bridge.wait_until_process_ready(42, Duration::from_millis(100)).unwrap();
		server.join().unwrap();
	}

	#[test]
	fn cleanup_removes_only_the_exited_process_discovery_records() {
		let root = env::temp_dir().join(format!(
			"carbon-discovery-cleanup-{}-{}",
			std::process::id(),
			uuid::Uuid::new_v4().simple()
		));
		let main = root.join("v1/0123456789abcdef0123456789abcdef.json");
		let route = root.join("routes/v1/route/0123456789abcdef0123456789abcdef.json");
		let other = root.join("v1/fedcba9876543210fedcba9876543210.json");
		fs::create_dir_all(main.parent().unwrap()).unwrap();
		fs::create_dir_all(route.parent().unwrap()).unwrap();
		let (exited, _) = discovery_and_capabilities(true, 3);
		let mut open = exited.clone();
		open.bridge_id = "fedcba9876543210fedcba9876543210".to_owned();
		open.process_id = 43;
		let exited_json = serde_json::to_vec(&exited).unwrap();
		fs::write(&main, &exited_json).unwrap();
		fs::write(&route, &exited_json).unwrap();
		fs::write(&other, serde_json::to_vec(&open).unwrap()).unwrap();

		let mut reused = open.clone();
		reused.process_id = 42;
		fs::write(&route, serde_json::to_vec(&reused).unwrap()).unwrap();
		assert_eq!(
			cleanup_discovery_records(vec![main.clone(), route.clone(), other.clone()], 42, &exited.bridge_id,)
				.unwrap(),
			1
		);
		assert!(!main.exists());
		assert!(route.exists());
		assert!(other.exists());
		fs::remove_dir_all(root).unwrap();
	}
	#[derive(Debug)]
	struct MockHelperExecutor {
		raw_stdout: Vec<u8>,
	}

	impl HelperExecutor for MockHelperExecutor {
		fn execute(
			&self,
			request: HelperExecutionRequest<'_>,
			writer: &mut dyn Write,
		) -> Result<HelperExecutionResult> {
			let mut cursor = io::Cursor::new(&self.raw_stdout);
			parse_helper_stdout(&mut cursor, writer, request.process_id)
		}
	}

	#[derive(Debug)]
	struct AuthenticatedHelperExecutor {
		raw_stdout: Vec<u8>,
	}

	impl HelperExecutor for AuthenticatedHelperExecutor {
		fn execute(
			&self,
			request: HelperExecutionRequest<'_>,
			writer: &mut dyn Write,
		) -> Result<HelperExecutionResult> {
			assert_eq!(request.method, "GET");
			assert_eq!(BASE64_STANDARD.decode(request.path_b64).unwrap(), b"/v1/capabilities");
			assert_eq!(request.port, 1234);
			assert_eq!(request.stdin_payload, b"CBRQ0001\x06\x00\x00\x00secret");
			let mut cursor = io::Cursor::new(&self.raw_stdout);
			parse_helper_stdout(&mut cursor, writer, request.process_id)
		}
	}

	fn make_helper_stdout(
		status: u32,
		content_length: Option<u64>,
		content_range: Option<&str>,
		body: &[u8],
	) -> Vec<u8> {
		let mut buf = Vec::new();
		buf.extend_from_slice(b"CBRS0001");
		buf.extend_from_slice(&status.to_le_bytes());
		let raw_len = content_length.unwrap_or(u64::MAX);
		buf.extend_from_slice(&raw_len.to_le_bytes());
		let range_bytes = content_range.unwrap_or("").as_bytes();
		let range_len = range_bytes.len() as u32;
		buf.extend_from_slice(&range_len.to_le_bytes());
		if range_len > 0 {
			buf.extend_from_slice(range_bytes);
		}
		buf.extend_from_slice(body);
		buf
	}

	fn stalling_helper_child() -> std::process::Child {
		#[cfg(unix)]
		let mut command = {
			let mut command = Command::new("sleep");
			command.arg("0.30");
			command
		};
		#[cfg(windows)]
		let mut command = {
			let mut command = Command::new("powershell.exe");
			command.args([
				"-NoProfile",
				"-NonInteractive",
				"-Command",
				"Start-Sleep -Milliseconds 300",
			]);
			command
		};

		command
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap()
	}

	#[test]
	fn helper_process_timeout_bounds_the_parent_side_io_and_wait() {
		let started = std::time::Instant::now();
		let error = execute_helper_child(
			stalling_helper_child(),
			b"",
			&mut Vec::new(),
			42,
			Duration::from_millis(40),
		)
		.unwrap_err();

		assert!(error.to_string().contains("timed out"), "{error:#}");
		assert!(started.elapsed() < Duration::from_millis(250));
	}

	#[test]
	fn backend_selection_uses_helper_under_wsl() {
		let (discovery, _) = discovery_and_capabilities(true, 1);
		let path = env::temp_dir().join(format!(
			"carbon-backend-selection-{}-{}.json",
			std::process::id(),
			uuid::Uuid::new_v4().simple()
		));
		fs::write(&path, serde_json::to_vec(&discovery).unwrap()).unwrap();

		let bridge = Bridge::from_path_with_timeouts_for_platform(
			&path,
			None,
			Duration::from_millis(100),
			Duration::from_millis(100),
			true,
		)
		.unwrap();

		fs::remove_file(&path).unwrap();

		match bridge.transport {
			Transport::Helper { port, .. } => assert_eq!(port, 1234),
			Transport::Reqwest { .. } => panic!("expected Helper transport under WSL"),
		}
	}

	#[test]
	fn helper_transport_frames_the_bearer_token() {
		let (discovery, capabilities) = discovery_and_capabilities(true, 1);
		let body = serde_json::to_vec(&capabilities).unwrap();
		let mock = Arc::new(AuthenticatedHelperExecutor {
			raw_stdout: make_helper_stdout(200, Some(body.len() as u64), None, &body),
		});
		let bridge = Bridge::with_test_transport(
			discovery,
			Transport::Helper {
				port: 1234,
				token: "secret".to_owned(),
				process_id: 42,
				request_timeout: Duration::from_secs(1),
				executor: mock,
			},
		);

		assert_eq!(
			bridge.get::<Capabilities>("v1/capabilities").unwrap().bridge_id,
			bridge.bridge_id()
		);
	}

	#[test]
	fn helper_framing_failure_invalid_magic() {
		let raw_stdout = b"CBRX0001\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00".to_vec();
		let mock = Arc::new(MockHelperExecutor { raw_stdout });
		let (discovery, _) = discovery_and_capabilities(true, 1);
		let bridge = Bridge::with_test_transport(
			discovery,
			Transport::Helper {
				port: 1234,
				token: "secret".to_owned(),
				process_id: 42,
				request_timeout: Duration::from_secs(1),
				executor: mock,
			},
		);

		let err = bridge.get::<serde_json::Value>("v1/capabilities").unwrap_err();
		assert!(err.to_string().contains("invalid framing magic"));
	}

	#[test]
	fn helper_framing_failure_truncated_header() {
		let raw_stdout = b"CBRS0001short".to_vec();
		let mock = Arc::new(MockHelperExecutor { raw_stdout });
		let (discovery, _) = discovery_and_capabilities(true, 1);
		let bridge = Bridge::with_test_transport(
			discovery,
			Transport::Helper {
				port: 1234,
				token: "secret".to_owned(),
				process_id: 42,
				request_timeout: Duration::from_secs(1),
				executor: mock,
			},
		);

		let err = bridge.get::<serde_json::Value>("v1/capabilities").unwrap_err();
		assert!(err.to_string().contains("failed to return framing header"));
	}

	#[test]
	fn helper_status_failure_returns_error_detail() {
		let raw_stdout = make_helper_stdout(500, None, None, b"internal server error");
		let mock = Arc::new(MockHelperExecutor { raw_stdout });
		let (discovery, _) = discovery_and_capabilities(true, 1);
		let bridge = Bridge::with_test_transport(
			discovery,
			Transport::Helper {
				port: 1234,
				token: "secret".to_owned(),
				process_id: 42,
				request_timeout: Duration::from_secs(1),
				executor: mock,
			},
		);

		let err = bridge.get::<serde_json::Value>("v1/capabilities").unwrap_err();
		assert!(err.to_string().contains("500: internal server error"));
	}

	#[test]
	fn helper_header_failure_missing_content_range_for_range_request() {
		let raw_stdout = make_helper_stdout(206, Some(10), None, b"0123456789");
		let mock = Arc::new(MockHelperExecutor { raw_stdout });
		let (discovery, _) = discovery_and_capabilities(true, 1);
		let bridge = Bridge::with_test_transport(
			discovery,
			Transport::Helper {
				port: 1234,
				token: "secret".to_owned(),
				process_id: 42,
				request_timeout: Duration::from_secs(1),
				executor: mock,
			},
		);

		let mut writer = Vec::new();
		let err = bridge
			.get_range_to_writer("v1/artifact", 0, 10, &mut writer)
			.unwrap_err();
		assert!(err.to_string().contains("no Content-Range header"));
	}

	#[test]
	fn helper_truncation_failure_content_length_mismatch() {
		let raw_stdout = make_helper_stdout(200, Some(100), None, b"short");
		let mock = Arc::new(MockHelperExecutor { raw_stdout });
		let (discovery, _) = discovery_and_capabilities(true, 1);
		let bridge = Bridge::with_test_transport(
			discovery,
			Transport::Helper {
				port: 1234,
				token: "secret".to_owned(),
				process_id: 42,
				request_timeout: Duration::from_secs(1),
				executor: mock,
			},
		);

		let err = bridge.get::<serde_json::Value>("v1/capabilities").unwrap_err();
		assert!(err
			.to_string()
			.contains("RML bridge artifact length mismatch: declared 100, received 5"));
	}
}
