use anyhow::{Context, Result};
use bytes::Bytes;
use directories::BaseDirs;
use rbx_dom_weak::types::Variant;
use reqwest::blocking::Client;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
	collections::{HashMap, HashSet},
	env, fs,
	io::{self, Write},
	net::Ipv4Addr,
	path::{Path, PathBuf},
	sync::mpsc,
	thread,
	time::{Duration, SystemTime},
};

const DISCOVERY_ENV: &str = "CARBON_RML_BRIDGE_DISCOVERY";
const DISCOVERY_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const DISCOVERY_REQUEST_TIMEOUT: Duration = Duration::from_millis(750);

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Discovery {
	protocol_version: u32,
	rml_build_version: String,
	bridge_id: String,
	endpoint: String,
	wsl_endpoint: Option<String>,
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

#[derive(Clone)]
pub struct Bridge {
	discovery: Discovery,
	endpoint: String,
	client: Client,
}

impl Bridge {
	pub fn discover(bridge_id: &str) -> Result<Self> {
		validate_bridge_id(bridge_id)?;
		let path = discovery_path(bridge_id)?;
		Self::from_path(&path, Some(bridge_id))
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

	fn active_bridge_ids_for_process(process_id: Option<u32>) -> Result<HashSet<String>> {
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
					is_ready_bridge(&bridge.discovery, &capabilities, process_id)
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
		Self::active_bridge_ids_for_process(None)
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

	pub fn wait_for_process(process_id: u32, timeout: Duration) -> Result<Self> {
		let deadline = std::time::Instant::now() + timeout;
		loop {
			let candidates = Self::active_bridge_ids_for_process(Some(process_id))?
				.into_iter()
				.collect::<Vec<_>>();
			match candidates.as_slice() {
				[bridge_id] => return Self::discover(bridge_id),
				[] if std::time::Instant::now() < deadline => thread::sleep(Duration::from_millis(250)),
				[] => anyhow::bail!("timed out waiting for RML bridge process {process_id}"),
				_ => anyhow::bail!("multiple RML bridges claimed Studio process {process_id}"),
			}
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
		let endpoint = if env::var_os("WSL_DISTRO_NAME").is_some() {
			match discovery.wsl_endpoint.as_deref() {
				Some(endpoint) => validate_wsl_endpoint(endpoint)?,
				None => discovery.endpoint.clone(),
			}
		} else {
			discovery.endpoint.clone()
		};
		let client = Client::builder()
			.connect_timeout(connect_timeout)
			.timeout(request_timeout)
			.build()?;
		Ok(Self {
			discovery,
			endpoint,
			client,
		})
	}

	pub fn bridge_id(&self) -> &str {
		&self.discovery.bridge_id
	}

	pub fn process_id(&self) -> u32 {
		self.discovery.process_id
	}

	pub fn get<R: DeserializeOwned>(&self, path: &str) -> Result<R> {
		let response = self
			.client
			.get(self.url(path))
			.bearer_auth(&self.discovery.token)
			.send()
			.with_context(|| format!("failed to contact RML bridge process {}", self.discovery.process_id))?;
		Self::ensure_success(path, response)?
			.json()
			.context("failed to decode RML bridge response")
	}

	pub fn post<T: Serialize + ?Sized, R: DeserializeOwned>(&self, path: &str, body: &T) -> Result<R> {
		let response = self
			.client
			.post(self.url(path))
			.bearer_auth(&self.discovery.token)
			.json(body)
			.send()
			.with_context(|| format!("failed to contact RML bridge process {}", self.discovery.process_id))?;
		Self::ensure_success(path, response)?
			.json()
			.context("failed to decode RML bridge response")
	}

	pub fn post_bytes<R: DeserializeOwned>(&self, path: &str, body: Bytes) -> Result<R> {
		let response = self
			.client
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

	pub fn delete<R: DeserializeOwned>(&self, path: &str) -> Result<R> {
		let response = self
			.client
			.delete(self.url(path))
			.bearer_auth(&self.discovery.token)
			.send()
			.with_context(|| format!("failed to contact RML bridge process {}", self.discovery.process_id))?;
		Self::ensure_success(path, response)?
			.json()
			.context("failed to decode RML bridge response")
	}

	/// Stream a raw bridge artifact directly into a bounded-storage writer.
	///
	/// Capture payloads use this route so the RBXM never becomes a JSON/base64
	/// string or a second complete in-memory Rust buffer.
	pub fn get_to_writer<W: Write + ?Sized>(&self, path: &str, writer: &mut W) -> Result<u64> {
		let response = self
			.client
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

	pub fn get_range_to_writer<W: Write + ?Sized>(
		&self,
		path: &str,
		offset: u64,
		length: u64,
		writer: &mut W,
	) -> Result<u64> {
		if length == 0 {
			return Ok(0);
		}
		let end = offset
			.checked_add(length - 1)
			.context("RML bridge artifact range overflows u64")?;
		let response = self
			.client
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

fn is_ready_bridge(discovery: &Discovery, capabilities: &Capabilities, process_id: Option<u32>) -> bool {
	capabilities.bridge_id == discovery.bridge_id
		&& capabilities.process_id == discovery.process_id
		&& capabilities.engine_ready
		&& capabilities.engine_generation != 0
		&& process_id.is_none_or(|expected| capabilities.process_id == expected)
}

fn validate_wsl_endpoint(endpoint: &str) -> Result<String> {
	let gateway = wsl_default_gateway().context("WSL host gateway is unavailable")?;
	let parsed = reqwest::Url::parse(endpoint).context("invalid RML WSL bridge endpoint")?;
	anyhow::ensure!(parsed.scheme() == "http", "RML WSL bridge must use HTTP");
	anyhow::ensure!(
		parsed.host_str() == Some(&gateway.to_string()),
		"RML WSL bridge host is not the WSL gateway"
	);
	anyhow::ensure!(parsed.port().is_some(), "RML WSL bridge endpoint has no port");
	anyhow::ensure!(parsed.path() == "/", "RML WSL bridge endpoint has an invalid path");
	anyhow::ensure!(
		parsed.query().is_none() && parsed.fragment().is_none(),
		"RML WSL bridge endpoint has extra data"
	);
	anyhow::ensure!(
		parsed.username().is_empty() && parsed.password().is_none(),
		"RML WSL bridge endpoint has credentials"
	);
	Ok(endpoint.to_owned())
}

fn wsl_default_gateway() -> Option<Ipv4Addr> {
	parse_wsl_default_gateway(&fs::read_to_string("/proc/net/route").ok()?)
}

fn parse_wsl_default_gateway(routes: &str) -> Option<Ipv4Addr> {
	for line in routes.lines().skip(1) {
		let fields: Vec<_> = line.split_whitespace().collect();
		if fields.len() < 4 || fields[1] != "00000000" {
			continue;
		}
		let flags = u16::from_str_radix(fields[3], 16).ok()?;
		if flags & 0x2 == 0 {
			continue;
		}
		let raw = u32::from_str_radix(fields[2], 16).ok()?;
		return Some(Ipv4Addr::from(raw.to_le_bytes()));
	}
	None
}

fn validate_bridge_id(bridge_id: &str) -> Result<()> {
	anyhow::ensure!(
		bridge_id.len() == 32 && bridge_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
		"invalid RML bridge routing identity"
	);
	Ok(())
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

/// Remove only discovery records owned by one Studio process after capture has
/// observed that process exit. RML normally removes these records on unload,
/// but Studio can terminate without running managed unload hooks. The process
/// identity is part of the authenticated discovery contract, so cleanup never
/// guesses from timestamps or removes another open Studio's bridge.
pub fn cleanup_process_discovery(process_id: u32) -> Result<usize> {
	cleanup_discovery_records(discovery_record_paths()?, process_id)
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

fn cleanup_discovery_records(paths: Vec<PathBuf>, process_id: u32) -> Result<usize> {
	let mut removed = 0;
	for path in paths {
		let Some(discovery) = fs::read(&path)
			.ok()
			.and_then(|bytes| serde_json::from_slice::<Discovery>(&bytes).ok())
		else {
			continue;
		};
		if discovery.process_id != process_id {
			continue;
		}

		// Re-read immediately before deletion so a malformed/replaced file can
		// never be removed based on a stale observation.
		let still_owned = fs::read(&path)
			.ok()
			.and_then(|bytes| serde_json::from_slice::<Discovery>(&bytes).ok())
			.is_some_and(|current| current.process_id == process_id && current.bridge_id == discovery.bridge_id);
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
			wsl_endpoint: None,
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
			"wslEndpoint": null,
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
				"wslEndpoint": null,
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
				"wslEndpoint": null,
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
	fn parses_the_wsl_default_gateway() {
		let routes = "Iface\tDestination\tGateway\tFlags\neth0\t00000000\t015019AC\t0003\n";
		assert_eq!(parse_wsl_default_gateway(routes), Some(Ipv4Addr::new(172, 25, 80, 1)));
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
		capabilities.engine_ready = true;
		assert!(!is_ready_bridge(&discovery, &capabilities, Some(42)));
		capabilities.engine_generation = 1;
		assert!(is_ready_bridge(&discovery, &capabilities, Some(42)));
		assert!(!is_ready_bridge(&discovery, &capabilities, Some(7)));
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

		assert_eq!(
			cleanup_discovery_records(vec![main.clone(), route.clone(), other.clone()], 42).unwrap(),
			2
		);
		assert!(!main.exists());
		assert!(!route.exists());
		assert!(other.exists());
		fs::remove_dir_all(root).unwrap();
	}
}
