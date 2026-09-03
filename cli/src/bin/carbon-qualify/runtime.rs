use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::{
	collections::{BTreeMap, HashMap},
	fs::{self, File},
	io::{Read, Write},
	path::{Path, PathBuf},
	process::{Child, Command, Stdio},
	thread,
	time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct CommandRequest {
	pub program: String,
	pub args: Vec<String>,
	pub cwd: Option<PathBuf>,
	pub env: BTreeMap<String, String>,
	pub timeout: Duration,
	pub artifact_stem: String,
}

#[derive(Clone, Debug)]
pub struct SpawnRequest {
	pub process: String,
	pub program: String,
	pub args: Vec<String>,
	pub cwd: Option<PathBuf>,
	pub env: BTreeMap<String, String>,
	pub artifact_stem: String,
}

#[derive(Clone, Debug)]
pub enum RuntimeAction {
	Command(CommandRequest),
	Spawn(SpawnRequest),
	WaitProcess {
		process: String,
		timeout: Duration,
	},
	WaitProcessOutput {
		process: String,
		stdout_contains: Vec<String>,
		stderr_contains: Vec<String>,
		timeout: Duration,
	},
	TerminateProcess {
		process: String,
	},
	Mcp {
		tool: String,
		arguments: Value,
		timeout: Duration,
		artifact_stem: String,
	},
	Sleep(Duration),
}

#[derive(Clone, Debug)]
pub enum Observation {
	Command {
		exit_code: i32,
		stdout: String,
		stderr: String,
		timed_out: bool,
		artifacts: Vec<String>,
	},
	Spawned {
		process: String,
		artifacts: Vec<String>,
	},
	ProcessOutput {
		process: String,
		stdout: String,
		stderr: String,
		artifacts: Vec<String>,
	},
	Mcp {
		result: Value,
		artifacts: Vec<String>,
	},
	Terminated {
		process: String,
		existed: bool,
		artifacts: Vec<String>,
	},
	Slept,
}

pub trait RuntimeAdapter {
	fn execute(&mut self, action: RuntimeAction) -> Result<Observation>;
}

struct BackgroundProcess {
	child: Child,
	stdout_path: PathBuf,
	stderr_path: PathBuf,
}

pub struct RealRuntime {
	artifact_dir: PathBuf,
	mcp: McpClient,
	processes: HashMap<String, BackgroundProcess>,
}

impl RealRuntime {
	pub fn new(artifact_dir: PathBuf, mcp_url: &str) -> Result<Self> {
		fs::create_dir_all(&artifact_dir)
			.with_context(|| format!("failed to create artifact directory {}", artifact_dir.display()))?;
		Ok(Self {
			mcp: McpClient::new(mcp_url, artifact_dir.clone())?,
			artifact_dir,
			processes: HashMap::new(),
		})
	}

	fn command(&self, request: CommandRequest) -> Result<Observation> {
		let stdout_path = self.artifact_path(&request.artifact_stem, "stdout.txt");
		let stderr_path = self.artifact_path(&request.artifact_stem, "stderr.txt");
		let mut command = build_command(&request.program, &request.args, request.cwd.as_deref(), &request.env);
		command.stdout(Stdio::piped()).stderr(Stdio::piped());
		let mut child = command
			.spawn()
			.with_context(|| format!("failed to start command {:?}", request.program))?;
		let stdout = child.stdout.take().context("failed to capture command stdout")?;
		let stderr = child.stderr.take().context("failed to capture command stderr")?;
		let stdout_reader = thread::spawn(move || read_stream(stdout));
		let stderr_reader = thread::spawn(move || read_stream(stderr));

		let deadline = Instant::now() + request.timeout;
		let (status, timed_out) = loop {
			if let Some(status) = child.try_wait()? {
				break (status, false);
			}
			if Instant::now() >= deadline {
				let _ = child.kill();
				break (child.wait()?, true);
			}
			thread::sleep(Duration::from_millis(20));
		};
		let stdout = stdout_reader
			.join()
			.map_err(|_| anyhow::anyhow!("stdout reader panicked"))??;
		let stderr = stderr_reader
			.join()
			.map_err(|_| anyhow::anyhow!("stderr reader panicked"))??;
		fs::write(&stdout_path, stdout.as_bytes())?;
		fs::write(&stderr_path, stderr.as_bytes())?;
		Ok(Observation::Command {
			exit_code: status.code().unwrap_or(-1),
			stdout,
			stderr,
			timed_out,
			artifacts: vec![display_path(&stdout_path), display_path(&stderr_path)],
		})
	}

	fn spawn(&mut self, request: SpawnRequest) -> Result<Observation> {
		ensure!(
			!self.processes.contains_key(&request.process),
			"background process {:?} is already registered",
			request.process
		);
		let stdout_path = self.artifact_path(&request.artifact_stem, "stdout.txt");
		let stderr_path = self.artifact_path(&request.artifact_stem, "stderr.txt");
		let stdout = File::create(&stdout_path)?;
		let stderr = File::create(&stderr_path)?;
		let mut command = build_command(&request.program, &request.args, request.cwd.as_deref(), &request.env);
		let child = command
			.stdout(Stdio::from(stdout))
			.stderr(Stdio::from(stderr))
			.spawn()
			.with_context(|| format!("failed to start background command {:?}", request.program))?;
		self.processes.insert(
			request.process.clone(),
			BackgroundProcess {
				child,
				stdout_path: stdout_path.clone(),
				stderr_path: stderr_path.clone(),
			},
		);
		Ok(Observation::Spawned {
			process: request.process,
			artifacts: vec![display_path(&stdout_path), display_path(&stderr_path)],
		})
	}

	fn wait_process(&mut self, process: &str, timeout: Duration) -> Result<Observation> {
		let deadline = Instant::now() + timeout;
		loop {
			let background = self
				.processes
				.get_mut(process)
				.with_context(|| format!("background process {process:?} is not registered"))?;
			if let Some(status) = background.child.try_wait()? {
				let background = self.processes.remove(process).expect("process was present");
				let stdout = read_text(&background.stdout_path)?;
				let stderr = read_text(&background.stderr_path)?;
				return Ok(Observation::Command {
					exit_code: status.code().unwrap_or(-1),
					stdout,
					stderr,
					timed_out: false,
					artifacts: vec![
						display_path(&background.stdout_path),
						display_path(&background.stderr_path),
					],
				});
			}
			if Instant::now() >= deadline {
				let background = self.processes.get(process).expect("process was present");
				return Ok(Observation::Command {
					exit_code: -1,
					stdout: read_text(&background.stdout_path)?,
					stderr: read_text(&background.stderr_path)?,
					timed_out: true,
					artifacts: vec![
						display_path(&background.stdout_path),
						display_path(&background.stderr_path),
					],
				});
			}
			thread::sleep(Duration::from_millis(20));
		}
	}

	fn wait_process_output(
		&mut self,
		process: &str,
		stdout_contains: &[String],
		stderr_contains: &[String],
		timeout: Duration,
	) -> Result<Observation> {
		let deadline = Instant::now() + timeout;
		loop {
			let (stdout_path, stderr_path, status) = {
				let background = self
					.processes
					.get_mut(process)
					.with_context(|| format!("background process {process:?} is not registered"))?;
				(
					background.stdout_path.clone(),
					background.stderr_path.clone(),
					background.child.try_wait()?,
				)
			};
			let stdout = read_text(&stdout_path)?;
			let stderr = read_text(&stderr_path)?;
			if stdout_contains.iter().all(|expected| stdout.contains(expected))
				&& stderr_contains.iter().all(|expected| stderr.contains(expected))
			{
				return Ok(Observation::ProcessOutput {
					process: process.to_owned(),
					stdout,
					stderr,
					artifacts: vec![display_path(&stdout_path), display_path(&stderr_path)],
				});
			}
			if let Some(status) = status {
				self.processes.remove(process);
				anyhow::bail!(
					"background process {process:?} exited with {} before emitting the expected output",
					status.code().unwrap_or(-1)
				);
			}
			if Instant::now() >= deadline {
				anyhow::bail!("background process {process:?} did not emit the expected output before timeout");
			}
			thread::sleep(Duration::from_millis(20));
		}
	}

	fn terminate_process(&mut self, process: &str) -> Result<Observation> {
		let Some(mut background) = self.processes.remove(process) else {
			return Ok(Observation::Terminated {
				process: process.to_owned(),
				existed: false,
				artifacts: Vec::new(),
			});
		};
		if background.child.try_wait()?.is_none() {
			background.child.kill()?;
			let _ = background.child.wait();
		}
		Ok(Observation::Terminated {
			process: process.to_owned(),
			existed: true,
			artifacts: vec![
				display_path(&background.stdout_path),
				display_path(&background.stderr_path),
			],
		})
	}

	fn artifact_path(&self, stem: &str, suffix: &str) -> PathBuf {
		self.artifact_dir.join(format!("{}-{suffix}", sanitize(stem)))
	}
}

impl RuntimeAdapter for RealRuntime {
	fn execute(&mut self, action: RuntimeAction) -> Result<Observation> {
		match action {
			RuntimeAction::Command(request) => self.command(request),
			RuntimeAction::Spawn(request) => self.spawn(request),
			RuntimeAction::WaitProcess { process, timeout } => self.wait_process(&process, timeout),
			RuntimeAction::WaitProcessOutput {
				process,
				stdout_contains,
				stderr_contains,
				timeout,
			} => self.wait_process_output(&process, &stdout_contains, &stderr_contains, timeout),
			RuntimeAction::TerminateProcess { process } => self.terminate_process(&process),
			RuntimeAction::Mcp {
				tool,
				arguments,
				timeout,
				artifact_stem,
			} => {
				let response = self.mcp.tool(&tool, arguments, timeout, &artifact_stem)?;
				let result_path = self.artifact_path(&artifact_stem, "mcp.json");
				let mut artifacts = response.attachments;
				fs::write(&result_path, serde_json::to_vec_pretty(&response.result)?)?;
				artifacts.insert(0, display_path(&result_path));
				Ok(Observation::Mcp {
					result: response.result,
					artifacts,
				})
			}
			RuntimeAction::Sleep(duration) => {
				thread::sleep(duration);
				Ok(Observation::Slept)
			}
		}
	}
}

impl Drop for RealRuntime {
	fn drop(&mut self) {
		for (_, mut background) in self.processes.drain() {
			if background.child.try_wait().ok().flatten().is_none() {
				let _ = background.child.kill();
				let _ = background.child.wait();
			}
		}
	}
}

fn build_command(
	program: &str,
	args: &[String],
	cwd: Option<&Path>,
	environment: &BTreeMap<String, String>,
) -> Command {
	let mut command = Command::new(program);
	command.args(args);
	if let Some(cwd) = cwd {
		command.current_dir(cwd);
	}
	command.envs(environment);
	command
}

fn read_stream(mut stream: impl Read) -> Result<String> {
	let mut bytes = Vec::new();
	stream.read_to_end(&mut bytes)?;
	Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_text(path: &Path) -> Result<String> {
	Ok(fs::read_to_string(path)
		.unwrap_or_else(|_| String::from_utf8_lossy(&fs::read(path).unwrap_or_default()).into_owned()))
}

fn display_path(path: &Path) -> String {
	path.to_string_lossy().into_owned()
}

fn sanitize(value: &str) -> String {
	let sanitized = value
		.chars()
		.map(|character| {
			if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
				character
			} else {
				'-'
			}
		})
		.collect::<String>();
	if sanitized.is_empty() {
		"step".to_owned()
	} else {
		sanitized
	}
}

struct McpResponse {
	result: Value,
	attachments: Vec<String>,
}

struct McpClient {
	base_url: String,
	auth_token: Option<String>,
	attachment_dir: PathBuf,
}

impl McpClient {
	fn new(base_url: &str, attachment_dir: PathBuf) -> Result<Self> {
		let base_url = normalize_mcp_url(base_url)?;
		fs::create_dir_all(&attachment_dir)?;
		Ok(Self {
			base_url,
			auth_token: load_auth_token(),
			attachment_dir,
		})
	}

	fn tool(&self, name: &str, payload: Value, timeout: Duration, artifact_stem: &str) -> Result<McpResponse> {
		ensure!(
			name.chars()
				.all(|character| character.is_ascii_alphanumeric() || character == '_'),
			"invalid MCP tool name {name:?}"
		);
		let url = format!("{}/mcp/{name}", self.base_url.trim_end_matches('/'));
		let client = Client::builder().timeout(timeout).build()?;
		let mut request = client.post(&url).header("Accept", "application/json").json(&payload);
		if let Some(token) = &self.auth_token {
			request = request.header("X-MCP-Auth", token);
		}
		let response = request.send().with_context(|| {
			format!(
				"could not connect to Roblox Studio MCP at {} while calling {name}",
				self.base_url
			)
		})?;
		let status = response.status();
		let bytes = response.bytes()?;
		let response_path = self
			.attachment_dir
			.join(format!("{}-mcp.json", sanitize(artifact_stem)));
		fs::write(&response_path, &bytes)
			.with_context(|| format!("failed to retain MCP response evidence at {}", response_path.display()))?;
		if !status.is_success() {
			bail!(
				"MCP tool {name} failed with HTTP {status}: {}; evidence: {}",
				String::from_utf8_lossy(&bytes),
				response_path.display()
			);
		}
		let envelope: Value = serde_json::from_slice(&bytes).with_context(|| {
			format!(
				"MCP tool {name} returned invalid JSON; evidence: {}",
				response_path.display()
			)
		})?;
		ensure!(
			envelope.get("isError").and_then(Value::as_bool) != Some(true),
			"MCP tool {name} reported an error"
		);
		let has_call_tool_result_fields =
			envelope.get("content").is_some() || envelope.get("structuredContent").is_some();
		let content = envelope
			.get("content")
			.map(|value| {
				value
					.as_array()
					.context("MCP response contained an invalid content field")
			})
			.transpose()?;
		let mut result = envelope
			.get("structuredContent")
			.filter(|value| value.as_object().is_some_and(|object| !object.is_empty()))
			.cloned();
		if !has_call_tool_result_fields {
			ensure!(
				envelope.as_object().is_some_and(|object| !object.is_empty()),
				"MCP response contained no structured JSON result"
			);
			result = Some(envelope.clone());
		}
		ensure!(
			result.is_some() || content.is_some(),
			"MCP response contained no structured JSON result"
		);
		let mut text_items = Vec::new();
		let mut attachments = Vec::new();
		for (index, item) in content.into_iter().flatten().enumerate() {
			match item.get("type").and_then(Value::as_str) {
				Some("text") => {
					let text = item.get("text").and_then(Value::as_str).unwrap_or_default();
					match serde_json::from_str::<Value>(text) {
						Ok(value) if result.is_none() => result = Some(value),
						_ => text_items.push(text.to_owned()),
					}
				}
				Some("image") => {
					let data = item
						.get("data")
						.and_then(Value::as_str)
						.context("MCP image content had no data")?;
					let mime = item
						.get("mimeType")
						.and_then(Value::as_str)
						.unwrap_or("application/octet-stream");
					let extension = match mime {
						"image/png" => "png",
						"image/jpeg" => "jpg",
						_ => "bin",
					};
					let path = self.attachment_dir.join(format!(
						"{}-{}-{}.{}",
						sanitize(name),
						uuid::Uuid::new_v4(),
						index,
						extension
					));
					File::create(&path)?.write_all(&BASE64.decode(data)?)?;
					attachments.push(display_path(&path));
				}
				_ => text_items.push(item.to_string()),
			}
		}
		let mut result = result.unwrap_or_else(|| json!({"text": text_items}));
		if !text_items.is_empty() {
			if let Some(object) = result.as_object_mut() {
				object.insert(
					"_text".to_owned(),
					Value::Array(text_items.into_iter().map(Value::String).collect()),
				);
			}
		}
		if result.get("success").and_then(Value::as_bool) == Some(false) {
			bail!("MCP tool {name} reported failure: {result}");
		}
		if let Some(error) = result.get("error").filter(|value| !value.is_null()) {
			bail!("MCP tool {name} reported error: {error}");
		}
		if name == "get_connected_instances" {
			self.hydrate_connected_instances(&client, &mut result, artifact_stem, &mut attachments)?;
		}
		Ok(McpResponse { result, attachments })
	}

	fn hydrate_connected_instances(
		&self,
		client: &Client,
		result: &mut Value,
		artifact_stem: &str,
		attachments: &mut Vec<String>,
	) -> Result<()> {
		let Some(compact_instances) = result.get("instances").and_then(Value::as_array).cloned() else {
			return Ok(());
		};
		if compact_instances.is_empty() {
			return Ok(());
		}
		let compact_instances = compact_instances
			.into_iter()
			.map(|instance| {
				if instance.get("instanceId").is_some() {
					return None;
				}
				let instance_id = instance.get("id").and_then(Value::as_str)?.to_owned();
				let data_model_name = instance.get("name").and_then(Value::as_str).map(str::to_owned);
				if data_model_name.is_none() && instance.get("placeName").and_then(Value::as_str).is_none() {
					return None;
				}
				let roles = if let Some(roles) = instance.get("roles").and_then(Value::as_array) {
					roles
						.iter()
						.map(|role| role.as_str().map(str::to_owned))
						.collect::<Option<Vec<_>>>()?
				} else {
					instance
						.get("peers")
						.and_then(Value::as_object)?
						.keys()
						.cloned()
						.collect()
				};
				if roles.is_empty() {
					return None;
				}
				Some((instance, instance_id, data_model_name, roles))
			})
			.collect::<Option<Vec<_>>>();
		let Some(compact_instances) = compact_instances else {
			return Ok(());
		};

		let health_url = format!("{}/health", self.base_url.trim_end_matches('/'));
		let mut request = client.get(&health_url).header("Accept", "application/json");
		if let Some(token) = &self.auth_token {
			request = request.header("X-MCP-Auth", token);
		}
		let response = request
			.send()
			.with_context(|| format!("could not read Roblox Studio MCP health at {health_url}"))?;
		let status = response.status();
		let bytes = response.bytes()?;
		let health_path = self
			.attachment_dir
			.join(format!("{}-mcp-health.json", sanitize(artifact_stem)));
		fs::write(&health_path, &bytes)
			.with_context(|| format!("failed to retain MCP health evidence at {}", health_path.display()))?;
		attachments.push(display_path(&health_path));
		ensure!(
			status.is_success(),
			"Roblox Studio MCP health returned HTTP {status}; evidence: {}",
			health_path.display()
		);
		let health: Value = serde_json::from_slice(&bytes).with_context(|| {
			format!(
				"Roblox Studio MCP health returned invalid JSON; evidence: {}",
				health_path.display()
			)
		})?;
		ensure!(
			health.get("status").and_then(Value::as_str) == Some("ok")
				&& health.get("service").and_then(Value::as_str) == Some("robloxstudio-mcp"),
			"Roblox Studio MCP health did not identify a ready broker; evidence: {}",
			health_path.display()
		);
		let health_instances = health
			.get("instances")
			.and_then(Value::as_array)
			.context("Roblox Studio MCP health contained no instances array")?;
		let mut detailed_instances = Vec::new();
		for instance in health_instances {
			if instance.get("instanceId").and_then(Value::as_str).is_some() {
				detailed_instances.push(instance);
			}
			if let Some(peers) = instance.get("peers").and_then(Value::as_array) {
				detailed_instances.extend(peers);
			}
		}

		let mut hydrated = Vec::new();
		for (compact, instance_id, data_model_name, roles) in compact_instances {
			for role in roles {
				if let Some(detailed) = detailed_instances.iter().copied().find(|instance| {
					instance.get("instanceId").and_then(Value::as_str) == Some(instance_id.as_str())
						&& instance.get("role").and_then(Value::as_str) == Some(role.as_str())
				}) {
					let mut detailed = detailed.clone();
					if detailed.get("versionMismatch").is_none() {
						let version_mismatch = match (
							detailed.get("pluginVersion").and_then(Value::as_str),
							detailed.get("serverVersion").and_then(Value::as_str),
						) {
							(Some(plugin_version), Some(server_version)) => Some(plugin_version != server_version),
							_ => None,
						};
						if let Some(version_mismatch) = version_mismatch {
							detailed["versionMismatch"] = Value::Bool(version_mismatch);
						}
					}
					hydrated.push(detailed);
				} else {
					let peer_id = compact
						.get("peers")
						.and_then(Value::as_object)
						.and_then(|peers| peers.get(&role))
						.and_then(Value::as_str)
						.map(str::to_owned);
					let mut projected = compact.clone();
					if let Some(object) = projected.as_object_mut() {
						object.insert("instanceId".to_owned(), Value::String(instance_id.clone()));
						if let Some(data_model_name) = &data_model_name {
							object.insert("dataModelName".to_owned(), Value::String(data_model_name.clone()));
						}
						object.insert("role".to_owned(), Value::String(role));
						if let Some(peer_id) = peer_id {
							object.insert("peerId".to_owned(), Value::String(peer_id));
						}
					}
					hydrated.push(projected);
				}
			}
		}
		result["instances"] = Value::Array(hydrated);
		Ok(())
	}
}

fn normalize_mcp_url(raw: &str) -> Result<String> {
	let raw = raw.trim().trim_end_matches('/');
	ensure!(
		raw.starts_with("http://") || raw.starts_with("https://"),
		"invalid MCP URL {raw:?}"
	);
	Ok(raw
		.replace("http://localhost", "http://127.0.0.1")
		.replace("http://0.0.0.0", "http://127.0.0.1"))
}

fn load_auth_token() -> Option<String> {
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

#[cfg(test)]
pub struct ScriptedRuntime {
	responses: std::collections::VecDeque<Result<Observation>>,
	pub actions: Vec<RuntimeAction>,
}

#[cfg(test)]
impl ScriptedRuntime {
	pub fn new(responses: Vec<Result<Observation>>) -> Self {
		Self {
			responses: responses.into(),
			actions: Vec::new(),
		}
	}
}

#[cfg(test)]
impl RuntimeAdapter for ScriptedRuntime {
	fn execute(&mut self, action: RuntimeAction) -> Result<Observation> {
		self.actions.push(action);
		self.responses
			.pop_front()
			.unwrap_or_else(|| bail!("scripted runtime exhausted"))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::net::{TcpListener, TcpStream};

	fn temporary_directory(label: &str) -> PathBuf {
		let path = std::env::temp_dir().join(format!("carbon-qualify-{label}-{}", uuid::Uuid::new_v4()));
		fs::create_dir_all(&path).unwrap();
		path
	}

	fn write_json_response(mut stream: TcpStream, body: &Value) {
		let body = serde_json::to_vec(body).unwrap();
		write!(
			stream,
			"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
			body.len()
		)
		.unwrap();
		stream.write_all(&body).unwrap();
	}

	#[test]
	fn direct_structured_mcp_responses_are_accepted() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let address = listener.local_addr().unwrap();
		let expected = json!({"placeId": 123});
		let server_expected = expected.clone();
		let server = thread::spawn(move || {
			let (mut stream, _) = listener.accept().unwrap();
			let mut request = [0_u8; 4096];
			let _ = stream.read(&mut request).unwrap();
			write_json_response(stream, &server_expected);
		});

		let root = temporary_directory("mcp-direct-structured");
		let client = McpClient::new(&format!("http://{address}"), root.clone()).unwrap();
		let response = client
			.tool("get_place_info", json!({}), Duration::from_secs(2), "direct-mcp")
			.unwrap();
		server.join().unwrap();

		assert_eq!(response.result, expected);
		assert!(response.attachments.is_empty());
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn compact_connected_instances_are_hydrated_from_health() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let address = listener.local_addr().unwrap();
		let compact = json!({
			"instances": [{
				"id": "anon:qualification-compact",
				"name": "CarbonQualification.rbxl",
				"roles": ["edit"],
			}],
		});
		let detailed = json!({
			"instanceId": "anon:qualification-compact",
			"role": "edit",
			"dataModelName": "CarbonQualification.rbxl",
			"versionMismatch": false,
		});
		let server_detailed = detailed.clone();
		let server = thread::spawn(move || {
			let (mut stream, _) = listener.accept().unwrap();
			let mut request = [0_u8; 4096];
			let _ = stream.read(&mut request).unwrap();
			write_json_response(stream, &compact);

			listener.set_nonblocking(true).unwrap();
			let deadline = Instant::now() + Duration::from_millis(500);
			while Instant::now() < deadline {
				match listener.accept() {
					Ok((mut stream, _)) => {
						let _ = stream.read(&mut request).unwrap();
						write_json_response(
							stream,
							&json!({
								"status": "ok",
								"service": "robloxstudio-mcp",
								"instances": [server_detailed],
							}),
						);
						return;
					}
					Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
						thread::sleep(Duration::from_millis(10));
					}
					Err(error) => panic!("health request failed: {error}"),
				}
			}
		});

		let root = temporary_directory("mcp-compact-instances");
		let client = McpClient::new(&format!("http://{address}"), root.clone()).unwrap();
		let response = client
			.tool(
				"get_connected_instances",
				json!({}),
				Duration::from_secs(2),
				"compact-instances",
			)
			.unwrap();
		server.join().unwrap();

		assert_eq!(response.result["instances"], json!([detailed]));
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn peer_map_connected_instances_are_hydrated_from_nested_health() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let address = listener.local_addr().unwrap();
		let compact = json!({
			"instances": [{
				"id": "instance:qualification-peer-map",
				"placeId": 0,
				"placeName": "CarbonQualification.rbxl",
				"peers": {
					"edit": "peer:qualification-edit",
				},
			}],
			"multiplayerGroups": [],
		});
		let detailed = json!({
			"instanceId": "instance:qualification-peer-map",
			"role": "edit",
			"placeId": 0,
			"placeName": "CarbonQualification.rbxl",
			"dataModelName": "CarbonQualification.rbxl",
			"isRunning": false,
			"pluginVersion": "3.1.0",
			"pluginVariant": "main",
			"serverVersion": "3.1.0",
			"lastActivity": 1_788_477_988_254_u64,
			"connectedAt": 1_788_477_478_130_u64,
		});
		let expected = json!({
			"instanceId": "instance:qualification-peer-map",
			"role": "edit",
			"placeId": 0,
			"placeName": "CarbonQualification.rbxl",
			"dataModelName": "CarbonQualification.rbxl",
			"isRunning": false,
			"pluginVersion": "3.1.0",
			"pluginVariant": "main",
			"serverVersion": "3.1.0",
			"versionMismatch": false,
			"lastActivity": 1_788_477_988_254_u64,
			"connectedAt": 1_788_477_478_130_u64,
		});
		let server = thread::spawn(move || {
			let (mut stream, _) = listener.accept().unwrap();
			let mut request = [0_u8; 4096];
			let _ = stream.read(&mut request).unwrap();
			write_json_response(stream, &compact);

			listener.set_nonblocking(true).unwrap();
			let deadline = Instant::now() + Duration::from_millis(500);
			while Instant::now() < deadline {
				match listener.accept() {
					Ok((mut stream, _)) => {
						let _ = stream.read(&mut request).unwrap();
						write_json_response(
							stream,
							&json!({
								"status": "ok",
								"service": "robloxstudio-mcp",
								"instances": [{
									"id": "instance:qualification-peer-map",
									"placeId": 0,
									"placeName": "CarbonQualification.rbxl",
									"peers": [detailed],
								}],
							}),
						);
						return;
					}
					Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
						thread::sleep(Duration::from_millis(10));
					}
					Err(error) => panic!("health request failed: {error}"),
				}
			}
		});

		let root = temporary_directory("mcp-peer-map-instances");
		let client = McpClient::new(&format!("http://{address}"), root.clone()).unwrap();
		let response = client
			.tool(
				"get_connected_instances",
				json!({}),
				Duration::from_secs(2),
				"peer-map-instances",
			)
			.unwrap();
		server.join().unwrap();

		assert_eq!(response.result["instances"], json!([expected]));
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn peer_map_fallback_does_not_invent_a_data_model_name() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let address = listener.local_addr().unwrap();
		let compact = json!({
			"instances": [{
				"id": "instance:qualification-disconnected",
				"placeId": 123,
				"placeName": "Published Place Name",
				"peers": {
					"edit": "peer:qualification-disconnected",
				},
			}],
			"multiplayerGroups": [],
		});
		let server = thread::spawn(move || {
			let (mut stream, _) = listener.accept().unwrap();
			let mut request = [0_u8; 4096];
			let _ = stream.read(&mut request).unwrap();
			write_json_response(stream, &compact);

			listener.set_nonblocking(true).unwrap();
			let deadline = Instant::now() + Duration::from_millis(500);
			while Instant::now() < deadline {
				match listener.accept() {
					Ok((mut stream, _)) => {
						let _ = stream.read(&mut request).unwrap();
						write_json_response(
							stream,
							&json!({
								"status": "ok",
								"service": "robloxstudio-mcp",
								"instances": [],
							}),
						);
						return;
					}
					Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
						thread::sleep(Duration::from_millis(10));
					}
					Err(error) => panic!("health request failed: {error}"),
				}
			}
		});

		let root = temporary_directory("mcp-peer-map-fallback");
		let client = McpClient::new(&format!("http://{address}"), root.clone()).unwrap();
		let response = client
			.tool(
				"get_connected_instances",
				json!({}),
				Duration::from_secs(2),
				"peer-map-fallback",
			)
			.unwrap();
		server.join().unwrap();

		let projected = &response.result["instances"][0];
		assert_eq!(projected["instanceId"], "instance:qualification-disconnected");
		assert_eq!(projected["role"], "edit");
		assert_eq!(projected["placeName"], "Published Place Name");
		assert!(projected.get("dataModelName").is_none());
		fs::remove_dir_all(root).unwrap();
	}
	#[test]
	fn failed_mcp_responses_are_retained_as_evidence() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let address = listener.local_addr().unwrap();
		let server = thread::spawn(move || {
			let (mut stream, _) = listener.accept().unwrap();
			let mut request = [0_u8; 4096];
			let _ = stream.read(&mut request).unwrap();
			let body = br#"{"error":"synthetic MCP failure"}"#;
			write!(
				stream,
				"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
				body.len()
			)
			.unwrap();
			stream.write_all(body).unwrap();
		});

		let root = temporary_directory("mcp-failure");
		let mut runtime = RealRuntime::new(root.clone(), &format!("http://{address}")).unwrap();
		let error = runtime
			.execute(RuntimeAction::Mcp {
				tool: "synthetic_failure".into(),
				arguments: json!({}),
				timeout: Duration::from_secs(2),
				artifact_stem: "failed-mcp".into(),
			})
			.unwrap_err()
			.to_string();
		server.join().unwrap();
		let evidence = root.join("failed-mcp-mcp.json");
		assert!(error.contains("500 Internal Server Error"));
		assert_eq!(fs::read(&evidence).unwrap(), br#"{"error":"synthetic MCP failure"}"#);
		fs::remove_dir_all(root).unwrap();
	}

	#[cfg(unix)]
	#[test]
	fn process_output_wait_observes_a_running_background_process() {
		let root = temporary_directory("process-output");
		let mut runtime = RealRuntime::new(root.clone(), "http://127.0.0.1:1").unwrap();
		runtime
			.execute(RuntimeAction::Spawn(SpawnRequest {
				process: "serve".to_owned(),
				program: "sh".to_owned(),
				args: vec![
					"-c".to_owned(),
					"printf 'instance ID: anon:mcp-final\\n' >&2; sleep 30".to_owned(),
				],
				cwd: None,
				env: BTreeMap::new(),
				artifact_stem: "serve".to_owned(),
			}))
			.unwrap();

		let observation = runtime
			.execute(RuntimeAction::WaitProcessOutput {
				process: "serve".to_owned(),
				stdout_contains: Vec::new(),
				stderr_contains: vec!["instance ID: anon:mcp-final".to_owned()],
				timeout: Duration::from_secs(2),
			})
			.unwrap();

		let Observation::ProcessOutput { stderr, .. } = observation else {
			panic!("expected process output observation");
		};
		assert!(stderr.contains("instance ID: anon:mcp-final"));
		runtime
			.execute(RuntimeAction::TerminateProcess {
				process: "serve".to_owned(),
			})
			.unwrap();
		fs::remove_dir_all(root).unwrap();
	}
}
