use anyhow::{bail, ensure, Result};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const SUITE_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
	pub schema_version: u32,
	pub name: String,
	#[serde(default)]
	pub description: String,
	#[serde(default)]
	pub variables: BTreeMap<String, Value>,
	#[serde(default)]
	pub requirements: Requirements,
	#[serde(default)]
	pub policy: Policy,
	pub scenarios: Vec<Scenario>,
}

impl Suite {
	pub fn validate(&self) -> Result<()> {
		ensure!(
			self.schema_version == SUITE_SCHEMA_VERSION,
			"unsupported suite schema version {}; expected {}",
			self.schema_version,
			SUITE_SCHEMA_VERSION
		);
		ensure!(!self.name.trim().is_empty(), "suite name must not be empty");
		ensure!(
			self.scenarios.len() >= self.policy.minimum_scenarios,
			"suite has {} scenarios but policy requires at least {}",
			self.scenarios.len(),
			self.policy.minimum_scenarios
		);

		let mut names = BTreeSet::new();
		let mut tags = BTreeSet::new();
		for scenario in &self.scenarios {
			ensure!(!scenario.name.trim().is_empty(), "scenario name must not be empty");
			ensure!(
				names.insert(scenario.name.clone()),
				"duplicate scenario name {:?}",
				scenario.name
			);
			ensure!(
				scenario.repeats > 0,
				"scenario {:?} repeats must be positive",
				scenario.name
			);
			ensure!(
				!scenario.steps.is_empty(),
				"scenario {:?} must contain at least one step",
				scenario.name
			);
			for tag in &scenario.tags {
				ensure!(!tag.trim().is_empty(), "scenario {:?} has an empty tag", scenario.name);
				tags.insert(tag.clone());
			}

			let mut step_names = BTreeSet::new();
			for step in scenario.steps.iter().chain(&scenario.cleanup) {
				ensure!(
					!step.name.trim().is_empty(),
					"scenario {:?} has a step with an empty name",
					scenario.name
				);
				ensure!(
					step_names.insert(step.name.clone()),
					"scenario {:?} has duplicate step name {:?}",
					scenario.name,
					step.name
				);
				step.validate()
					.map_err(|error| anyhow::anyhow!("scenario {:?}, step {:?}: {error}", scenario.name, step.name))?;
			}
		}

		for required in &self.policy.required_tags {
			ensure!(
				tags.contains(required),
				"release policy requires tag {:?}, but no scenario carries it",
				required
			);
		}
		Ok(())
	}
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirements {
	#[serde(default)]
	pub operating_systems: Vec<String>,
	#[serde(default)]
	pub environment: Vec<String>,
	#[serde(default)]
	pub variables: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
	pub fail_on_command_warnings: bool,
	pub fail_on_runtime_warnings: bool,
	pub minimum_scenarios: usize,
	pub required_tags: Vec<String>,
	pub max_suite_seconds: Option<u64>,
}

impl Default for Policy {
	fn default() -> Self {
		Self {
			fail_on_command_warnings: true,
			fail_on_runtime_warnings: true,
			minimum_scenarios: 1,
			required_tags: Vec::new(),
			max_suite_seconds: None,
		}
	}
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
	pub name: String,
	#[serde(default)]
	pub description: String,
	#[serde(default)]
	pub tags: Vec<String>,
	#[serde(default = "one")]
	pub repeats: u32,
	#[serde(default)]
	pub max_p95_ms: Option<u64>,
	pub steps: Vec<Step>,
	#[serde(default)]
	pub cleanup: Vec<Step>,
}

fn one() -> u32 {
	1
}

#[derive(Clone, Debug)]
pub struct Step {
	pub name: String,
	pub max_duration_ms: Option<u64>,
	pub action: Action,
}

impl<'de> Deserialize<'de> for Step {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		let value = Value::deserialize(deserializer)?;
		let mut fields = value
			.as_object()
			.cloned()
			.ok_or_else(|| D::Error::custom("step must be a JSON object"))?;
		let name = fields.remove("name").ok_or_else(|| D::Error::missing_field("name"))?;
		let name = serde_json::from_value(name).map_err(D::Error::custom)?;
		let max_duration_ms = fields
			.remove("max_duration_ms")
			.map(serde_json::from_value)
			.transpose()
			.map_err(D::Error::custom)?;
		let action = serde_json::from_value(Value::Object(fields)).map_err(D::Error::custom)?;
		Ok(Self {
			name,
			max_duration_ms,
			action,
		})
	}
}

impl Step {
	fn validate(&self) -> Result<()> {
		match &self.action {
			Action::Command {
				program,
				allowed_diagnostic_contains,
				..
			} => {
				ensure!(!program.trim().is_empty(), "program must not be empty");
				ensure!(
					allowed_diagnostic_contains.iter().all(|value| !value.trim().is_empty()),
					"allowed command diagnostic substrings must not be empty"
				);
			}
			Action::Spawn { program, .. } => {
				ensure!(!program.trim().is_empty(), "program must not be empty");
			}
			Action::WaitProcess {
				process,
				expected_exit_codes,
				..
			} => {
				ensure!(!process.trim().is_empty(), "process name must not be empty");
				ensure!(!expected_exit_codes.is_empty(), "expected exit codes must not be empty");
			}
			Action::WaitProcessOutput {
				process,
				stdout_contains,
				stderr_contains,
				..
			} => {
				ensure!(!process.trim().is_empty(), "process name must not be empty");
				ensure!(
					!stdout_contains.is_empty() || !stderr_contains.is_empty(),
					"process output wait requires expected stdout or stderr text"
				);
				ensure!(
					stdout_contains
						.iter()
						.chain(stderr_contains)
						.all(|value| !value.is_empty()),
					"expected process output text must not be empty"
				);
			}
			Action::TerminateProcess { process } => {
				ensure!(!process.trim().is_empty(), "process name must not be empty");
			}
			Action::Mcp {
				tool,
				checks,
				capture,
				select,
				poll_interval_ms,
				..
			} => {
				ensure!(!tool.trim().is_empty(), "MCP tool must not be empty");
				if let Some(interval) = poll_interval_ms {
					ensure!(*interval > 0, "MCP poll interval must be positive");
					ensure!(
						!checks.is_empty() || select.is_some(),
						"a polled MCP step requires at least one check or selection"
					);
				}
				for check in checks {
					check.validate()?;
				}
				if let Some(select) = select {
					select.validate()?;
				}
				for (variable, pointer) in capture {
					ensure!(!variable.trim().is_empty(), "capture variable must not be empty");
					validate_pointer(pointer)?;
				}
			}
			Action::Sleep { milliseconds } => {
				ensure!(*milliseconds > 0, "sleep duration must be positive");
			}
			Action::SnapshotPath { snapshot, path } | Action::AssertPathUnchanged { snapshot, path, .. } => {
				ensure!(!snapshot.trim().is_empty(), "snapshot name must not be empty");
				ensure!(!path.trim().is_empty(), "snapshot path must not be empty");
			}
			Action::AssertNumericDelta {
				before,
				after,
				max_increase,
			} => {
				ensure!(!before.trim().is_empty(), "baseline variable must not be empty");
				ensure!(!after.trim().is_empty(), "current variable must not be empty");
				ensure!(
					max_increase.is_finite() && *max_increase >= 0.0,
					"maximum numeric increase must be finite and non-negative"
				);
			}
			Action::AssertPlaceInstance {
				path,
				instance_path,
				class_name,
				properties,
				attributes,
			} => {
				ensure!(!path.trim().is_empty(), "place path must not be empty");
				ensure!(!instance_path.is_empty(), "instance path must not be empty");
				ensure!(
					instance_path.iter().all(|segment| !segment.trim().is_empty()),
					"instance path segments must not be empty"
				);
				ensure!(!class_name.trim().is_empty(), "instance class must not be empty");
				ensure!(
					!properties.is_empty() || !attributes.is_empty(),
					"place assertion must include at least one property or attribute"
				);
				for (name, expected) in properties.iter().chain(attributes) {
					ensure!(
						!name.trim().is_empty(),
						"expected property or attribute name must not be empty"
					);
					validate_place_value(expected)?;
				}
			}
		}
		Ok(())
	}
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
	Command {
		program: String,
		#[serde(default)]
		args: Vec<String>,
		#[serde(default)]
		cwd: Option<String>,
		#[serde(default)]
		env: BTreeMap<String, String>,
		#[serde(default = "default_command_timeout")]
		timeout_seconds: u64,
		#[serde(default)]
		expected_exit_code: i32,
		#[serde(default)]
		stdout_contains: Vec<String>,
		#[serde(default)]
		stdout_not_contains: Vec<String>,
		#[serde(default)]
		stderr_contains: Vec<String>,
		#[serde(default)]
		stderr_not_contains: Vec<String>,
		#[serde(default)]
		allowed_diagnostic_contains: Vec<String>,
	},
	Spawn {
		process: String,
		program: String,
		#[serde(default)]
		args: Vec<String>,
		#[serde(default)]
		cwd: Option<String>,
		#[serde(default)]
		env: BTreeMap<String, String>,
	},
	WaitProcess {
		process: String,
		#[serde(default = "default_command_timeout")]
		timeout_seconds: u64,
		#[serde(default = "successful_exit_codes")]
		expected_exit_codes: Vec<i32>,
	},
	WaitProcessOutput {
		process: String,
		#[serde(default)]
		stdout_contains: Vec<String>,
		#[serde(default)]
		stderr_contains: Vec<String>,
		#[serde(default = "default_command_timeout")]
		timeout_seconds: u64,
	},
	TerminateProcess {
		process: String,
	},
	Mcp {
		tool: String,
		#[serde(default = "empty_object")]
		arguments: Value,
		#[serde(default = "default_mcp_timeout")]
		timeout_seconds: u64,
		#[serde(default)]
		poll_interval_ms: Option<u64>,
		#[serde(default)]
		checks: Vec<JsonCheck>,
		/// Select exactly one element from an array in the MCP response. Checks
		/// and captures are then evaluated against that element, while unrelated
		/// elements remain available to other concurrent qualification runs.
		#[serde(default)]
		select: Option<JsonSelection>,
		#[serde(default)]
		capture: BTreeMap<String, String>,
	},
	Sleep {
		milliseconds: u64,
	},
	SnapshotPath {
		snapshot: String,
		path: String,
	},
	AssertPathUnchanged {
		snapshot: String,
		path: String,
		#[serde(default = "default_true")]
		check_mtime: bool,
	},
	AssertNumericDelta {
		before: String,
		after: String,
		max_increase: f64,
	},
	AssertPlaceInstance {
		path: String,
		instance_path: Vec<String>,
		#[serde(rename = "class")]
		class_name: String,
		#[serde(default)]
		properties: BTreeMap<String, Value>,
		#[serde(default)]
		attributes: BTreeMap<String, Value>,
	},
}

fn validate_place_value(value: &Value) -> Result<()> {
	let supported = matches!(value, Value::Bool(_) | Value::Number(_) | Value::String(_))
		|| value
			.as_array()
			.is_some_and(|components| components.len() == 3 && components.iter().all(Value::is_number));
	ensure!(
		supported,
		"place values must be booleans, numbers, strings, or three-number vectors"
	);
	Ok(())
}

fn default_command_timeout() -> u64 {
	150
}

fn successful_exit_codes() -> Vec<i32> {
	vec![0]
}

fn default_mcp_timeout() -> u64 {
	60
}

fn default_true() -> bool {
	true
}

fn empty_object() -> Value {
	Value::Object(Default::default())
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonCheck {
	pub pointer: String,
	pub op: CheckOperation,
	#[serde(default)]
	pub value: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonSelection {
	pub pointer: String,
	pub checks: Vec<JsonCheck>,
}

impl JsonSelection {
	fn validate(&self) -> Result<()> {
		validate_pointer(&self.pointer)?;
		ensure!(!self.checks.is_empty(), "JSON selection requires at least one check");
		for check in &self.checks {
			check.validate()?;
		}
		Ok(())
	}
}

impl JsonCheck {
	fn validate(&self) -> Result<()> {
		validate_pointer(&self.pointer)?;
		match self.op {
			CheckOperation::Exists | CheckOperation::Absent => {
				ensure!(self.value.is_none(), "{:?} check must not have a value", self.op);
			}
			_ => ensure!(self.value.is_some(), "{:?} check requires a value", self.op),
		}
		Ok(())
	}
}

fn validate_pointer(pointer: &str) -> Result<()> {
	if pointer.is_empty() || pointer.starts_with('/') {
		Ok(())
	} else {
		bail!("JSON pointer {:?} must be empty or start with '/'", pointer)
	}
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOperation {
	Exists,
	Absent,
	Equals,
	NotEquals,
	Contains,
	LessThanOrEqual,
	GreaterThanOrEqual,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
	Pass,
	Fail,
}

#[derive(Clone, Debug, Serialize)]
pub struct QualificationReport {
	pub schema_version: u32,
	pub suite: String,
	pub description: String,
	pub started_at: String,
	pub duration_ms: u64,
	pub outcome: Outcome,
	pub failures: Vec<String>,
	pub scenarios: Vec<ScenarioReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScenarioReport {
	pub name: String,
	pub description: String,
	pub tags: Vec<String>,
	pub iteration: u32,
	pub duration_ms: u64,
	pub outcome: Outcome,
	pub failure: Option<String>,
	pub steps: Vec<StepReport>,
	pub cleanup: Vec<StepReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StepReport {
	pub name: String,
	pub kind: String,
	pub duration_ms: u64,
	pub outcome: Outcome,
	pub summary: String,
	pub artifacts: Vec<String>,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn validation_rejects_a_missing_required_release_area() {
		let suite: Suite = serde_json::from_value(serde_json::json!({
			"schema_version": 1,
			"name": "release",
			"policy": { "required_tags": ["runtime", "faults"] },
			"scenarios": [{
				"name": "runtime",
				"tags": ["runtime"],
				"steps": [{"name": "wait", "kind": "sleep", "milliseconds": 1}]
			}]
		}))
		.unwrap();
		let error = suite.validate().unwrap_err().to_string();
		assert!(error.contains("faults"));
	}

	#[test]
	fn validation_rejects_polling_without_a_condition() {
		let suite: Suite = serde_json::from_value(serde_json::json!({
			"schema_version": 1,
			"name": "release",
			"scenarios": [{
				"name": "runtime",
				"steps": [{
					"name": "poll",
					"kind": "mcp",
					"tool": "get_connected_instances",
					"poll_interval_ms": 10
				}]
			}]
		}))
		.unwrap();
		assert!(suite
			.validate()
			.unwrap_err()
			.to_string()
			.contains("requires at least one check"));
	}

	#[test]
	fn step_deserialization_rejects_misspelled_fields() {
		let error = serde_json::from_value::<Step>(serde_json::json!({
			"name": "command",
			"kind": "command",
			"program": "carbon",
			"timeout_second": 10
		}))
		.unwrap_err()
		.to_string();
		assert!(error.contains("timeout_second"));
	}

	#[test]
	fn step_deserialization_rejects_nonblocking_failures() {
		let error = serde_json::from_value::<Step>(serde_json::json!({
			"name": "release-gate",
			"kind": "sleep",
			"milliseconds": 1,
			"allow_failure": true
		}))
		.unwrap_err()
		.to_string();
		assert!(error.contains("allow_failure"));
	}

	#[test]
	fn validation_accepts_an_mcp_array_selector() {
		let suite: Suite = serde_json::from_value(serde_json::json!({
			"schema_version": 1,
			"name": "release",
			"scenarios": [{
				"name": "runtime",
				"steps": [{
					"name": "route",
					"kind": "mcp",
					"tool": "get_connected_instances",
					"select": {
						"pointer": "/instances",
						"checks": [{
							"pointer": "/dataModelName",
							"op": "equals",
							"value": "qualification-1"
						}]
					},
					"capture": {"instance": "/instanceId"}
				}]
			}]
		}))
		.unwrap();
		suite.validate().unwrap();
	}

	fn release_suite() -> Value {
		let suite_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
			.parent()
			.unwrap()
			.join("qualification/suites/carbon-release.json");
		serde_json::from_slice(&std::fs::read(suite_path).unwrap()).unwrap()
	}

	#[test]
	fn release_suite_exercises_auto_recovery_capture() {
		let suite = release_suite();
		let deterministic = suite["scenarios"]
			.as_array()
			.unwrap()
			.iter()
			.find(|scenario| scenario["name"] == "deterministic-installed-build")
			.expect("release suite is missing deterministic build coverage");
		let offline = deterministic["steps"]
			.as_array()
			.unwrap()
			.iter()
			.find(|step| step["name"] == "capture-offline-binary-place")
			.expect("release suite is missing offline binary-place capture");
		assert_eq!(
			offline["args"],
			serde_json::json!(["capture", "${project}", "${studio_launch_path}", "--color", "never"])
		);
		let scenario = suite["scenarios"]
			.as_array()
			.unwrap()
			.iter()
			.find(|scenario| scenario["name"] == "managed-studio-auto-recovery-capture")
			.expect("release suite is missing the auto-recovery scenario");
		let steps = scenario["steps"].as_array().unwrap();
		let capture = steps
			.iter()
			.find(|step| step["name"] == "stop-after-next-auto-recovery")
			.expect("release suite is missing recovery-backed stop");
		assert!(capture["timeout_seconds"].as_u64().unwrap() >= 390);
		assert!(capture["stderr_contains"]
			.as_array()
			.unwrap()
			.iter()
			.any(|value| value == "auto-recovery"));
		assert!(!capture["args"]
			.as_array()
			.unwrap()
			.iter()
			.any(|value| value == "--force-full"));
		assert_eq!(
			capture["args"],
			serde_json::json!(["stop", "${studio_instance}", "--color", "never"])
		);
		let focus = steps
			.iter()
			.find(|step| step["name"] == "focus-managed-studio")
			.expect("release suite is missing final-instance focus coverage");
		assert_eq!(
			focus["args"],
			serde_json::json!(["focus", "${studio_instance}", "--color", "never"])
		);
		let serve = steps
			.iter()
			.find(|step| step["name"] == "start-managed-serve")
			.expect("release suite is missing managed serve coverage");
		assert_eq!(serve["env"]["CARBON_STUDIO_MCP_URL"], "${mcp_url}");
		let ready_index = steps
			.iter()
			.position(|step| step["name"] == "wait-for-carbon-final-id-readiness")
			.expect("release suite does not wait for Carbon's final-ID readiness log");
		let focus_index = steps
			.iter()
			.position(|step| step["name"] == "focus-managed-studio")
			.unwrap();
		assert!(ready_index < focus_index);
		assert_eq!(steps[ready_index]["kind"], "wait_process_output");
		assert_eq!(
			steps[ready_index]["stderr_contains"],
			serde_json::json!(["instance ID: ${studio_instance}"])
		);
		assert!(steps
			.iter()
			.any(|step| step["name"] == "verify-rebuilt-place-contains-live-edit"));
	}

	#[test]
	fn release_suite_verifies_the_rebuilt_capture_state() {
		let suite = release_suite();
		let steps = suite["scenarios"]
			.as_array()
			.unwrap()
			.iter()
			.find(|scenario| scenario["name"] == "managed-studio-auto-recovery-capture")
			.unwrap()["steps"]
			.as_array()
			.unwrap();
		let assertion = steps
			.iter()
			.find(|step| step["name"] == "verify-rebuilt-capture-state")
			.expect("release suite does not inspect the rebuilt RBXL state");

		assert_eq!(assertion["kind"], "assert_place_instance");
		assert_eq!(assertion["path"], "${capture_rebuild_output}");
		assert_eq!(
			assertion["instance_path"],
			serde_json::json!(["Workspace", "CarbonAutoRecoveryProbe"])
		);
		assert_eq!(assertion["class"], "Part");
		assert_eq!(assertion["properties"]["Anchored"], true);
		assert_eq!(assertion["properties"]["Size"], serde_json::json!([7, 3, 5]));
		assert_eq!(assertion["attributes"]["CapturedThroughAutoRecovery"], true);
	}

	#[test]
	fn release_suite_requires_deterministic_builds() {
		let suite = release_suite();
		let scenario = suite["scenarios"]
			.as_array()
			.unwrap()
			.iter()
			.find(|scenario| scenario["name"] == "deterministic-installed-build")
			.expect("release suite is missing deterministic build coverage");
		let steps = scenario["steps"].as_array().unwrap();
		assert!(steps.iter().any(|step| step["name"] == "stable-build-bytes"));
		assert!(steps
			.iter()
			.any(|step| step["name"] == "stable-manifest-bytes-and-mtimes"));
	}
}
