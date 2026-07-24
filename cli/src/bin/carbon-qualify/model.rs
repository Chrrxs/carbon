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
			Action::StartCrashWatch {
				watcher,
				crash_dir,
				rml_log_dir,
			} => {
				ensure!(!watcher.trim().is_empty(), "crash watcher name must not be empty");
				ensure!(!crash_dir.trim().is_empty(), "crash directory must not be empty");
				ensure!(!rml_log_dir.trim().is_empty(), "RML log directory must not be empty");
			}
			Action::AssertNoCrash { watcher } => {
				ensure!(!watcher.trim().is_empty(), "crash watcher name must not be empty");
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
			Action::ExportStudioPlace {
				endpoint,
				token,
				output,
				..
			} => {
				ensure!(!endpoint.trim().is_empty(), "Studio export endpoint must not be empty");
				ensure!(!token.trim().is_empty(), "Studio export token must not be empty");
				ensure!(!output.trim().is_empty(), "Studio export output must not be empty");
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
	StartCrashWatch {
		watcher: String,
		crash_dir: String,
		rml_log_dir: String,
	},
	AssertNoCrash {
		watcher: String,
	},
	AssertNumericDelta {
		before: String,
		after: String,
		max_increase: f64,
	},
	ExportStudioPlace {
		endpoint: String,
		token: String,
		output: String,
		#[serde(default = "default_mcp_timeout")]
		timeout_seconds: u64,
	},
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

	#[test]
	fn release_suite_contains_mapped_identity_live_regression() {
		let suite_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
			.parent()
			.unwrap()
			.join("qualification/suites/carbon-release.json");
		let suite: Value = serde_json::from_slice(&std::fs::read(suite_path).unwrap()).unwrap();
		let scenario = suite["scenarios"]
			.as_array()
			.unwrap()
			.iter()
			.find(|scenario| scenario["name"] == "managed-studio-runtime")
			.expect("release suite is missing the managed Studio scenario");
		assert!(
			scenario["tags"]
				.as_array()
				.unwrap()
				.iter()
				.any(|tag| tag == "mapped_identity"),
			"managed Studio scenario is missing the mapped_identity tag"
		);
		let step_names = scenario["steps"]
			.as_array()
			.unwrap()
			.iter()
			.filter_map(|step| step["name"].as_str())
			.collect::<std::collections::HashSet<_>>();
		for required in [
			"create-live-mapped-reference",
			"verify-live-mapped-identity-metadata",
			"rebuild-live-mapped-reference",
			"remove-live-mapped-identity",
			"missing-live-mapped-identity-blocks-build",
			"restore-live-mapped-identity",
			"capture-corrected-live-manifest",
			"build-after-live-manifest-correction",
			"corrected-live-manifest-rebuild-parity",
		] {
			assert!(step_names.contains(required), "release suite is missing {required}");
		}
	}

	#[test]
	fn release_suite_gates_optimized_capture_against_a_forced_full_oracle() {
		let suite_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
			.parent()
			.unwrap()
			.join("qualification/suites/carbon-release.json");
		let suite: Value = serde_json::from_slice(&std::fs::read(suite_path).unwrap()).unwrap();
		let steps = suite["scenarios"]
			.as_array()
			.unwrap()
			.iter()
			.find(|scenario| scenario["name"] == "managed-studio-runtime")
			.expect("release suite is missing the managed Studio scenario")["steps"]
			.as_array()
			.unwrap();
		let step = |name: &str| {
			steps
				.iter()
				.find(|step| step["name"] == name)
				.unwrap_or_else(|| panic!("release suite is missing {name}"))
		};

		let mutation = step("mutate-captured-datamodel")["arguments"]["code"].as_str().unwrap();
		for evidence in [
			".Source",
			"SetAttribute",
			"ObjectValue",
			".CFrame",
			".Parent",
			"Destroy",
			"AddTag",
			"Lighting",
		] {
			assert!(
				mutation.contains(evidence),
				"live capture mutation is missing {evidence}"
			);
		}
		assert!(step("forced-full-capture-oracle")["args"]
			.as_array()
			.unwrap()
			.iter()
			.any(|argument| argument == "--force-full"));
		assert!(step("unchanged-managed-launch-capture")["stderr_contains"]
			.as_array()
			.unwrap()
			.iter()
			.any(|output| output == "exact no-op"));
		assert_eq!(step("unchanged-managed-launch-preserves-baseline")["check_mtime"], true);
		let step_index = |name: &str| {
			steps
				.iter()
				.position(|candidate| candidate["name"] == name)
				.unwrap_or_else(|| panic!("release suite is missing {name}"))
		};
		assert!(
			step_index("unchanged-managed-launch-capture") < step_index("edit-datamodel-probe"),
			"the launch no-op gate must run before any edit-target execution"
		);
		assert_eq!(
			step("end-test-does-not-focus-managed-studio")["args"],
			serde_json::json!(["assert-not", "${studio_pid}"]),
			"the final focus check must only reject the managed test Studio"
		);
		assert_eq!(
			step("optimized-capture-matches-forced-full-oracle")["check_mtime"],
			false
		);
		assert_eq!(
			step("warm-capture-preserves-forced-full-fixed-point")["check_mtime"],
			true
		);
	}

	#[test]
	fn release_suite_requires_carbon_stop_to_report_automatic_capture() {
		let suite_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
			.parent()
			.unwrap()
			.join("qualification/suites/carbon-release.json");
		let suite: Value = serde_json::from_slice(&std::fs::read(suite_path).unwrap()).unwrap();
		let cleanup = suite["scenarios"]
			.as_array()
			.unwrap()
			.iter()
			.find(|scenario| scenario["name"] == "managed-studio-runtime")
			.expect("release suite is missing the managed Studio scenario")["cleanup"]
			.as_array()
			.unwrap();
		let stop = cleanup
			.iter()
			.find(|step| step["name"] == "stop-managed-serve-and-studio")
			.expect("release suite is missing managed Carbon stop");
		let output = stop["stderr_contains"]
			.as_array()
			.expect("managed Carbon stop must assert its capture result");
		assert!(output.iter().any(|value| value == "Capture Manifest completed"));
		assert!(output.iter().any(|value| value == "Carbon stopped successfully"));
	}

	#[test]
	fn release_suite_contains_procedural_mapped_generator_live_regression() {
		let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
		let suite: Value =
			serde_json::from_slice(&std::fs::read(root.join("qualification/suites/carbon-release.json")).unwrap())
				.unwrap();
		let scenario = suite["scenarios"]
			.as_array()
			.unwrap()
			.iter()
			.find(|scenario| scenario["name"] == "managed-studio-runtime")
			.expect("release suite is missing the managed Studio scenario");
		let steps = scenario["steps"].as_array().unwrap();
		let step_names = steps
			.iter()
			.filter_map(|step| step["name"].as_str())
			.collect::<std::collections::HashSet<_>>();
		for required in [
			"create-live-procedural-models-with-mapped-generator",
			"capture-live-procedural-generator-reference",
			"verify-live-procedural-generator-metadata",
			"verify-live-procedural-regeneration-after-capture",
			"missing-procedural-generator-id-blocks-build",
			"clear-one-of-two-procedural-generators",
			"remaining-procedural-generator-reference-still-blocks-build",
			"clear-final-procedural-generator",
			"corrected-procedural-manifest-builds-without-generator-id",
			"corrected-procedural-manifest-rebuild-parity",
		] {
			assert!(step_names.contains(required), "release suite is missing {required}");
		}
		let creation_code = steps
			.iter()
			.find(|step| step["name"] == "create-live-procedural-models-with-mapped-generator")
			.and_then(|step| step["arguments"]["code"].as_str())
			.expect("procedural model creation step has no Luau body");
		assert!(
			creation_code.contains(
				"assert(model:WaitForGenerationAsync(), name .. ' parameter generation failed: ' .. model.GenerationError)"
			),
			"every procedural model must await the dirty state created by parameter changes"
		);
		assert!(
			!creation_code.contains("task.wait"),
			"procedural model quiescence must use the engine await instead of a fixed delay"
		);

		let generator = std::fs::read_to_string(root.join("qualification/fixtures/ProceduralGenerator/init.luau"))
			.expect("release qualification is missing its mapped procedural generator");
		assert!(generator.contains("Attributes"));
		assert!(generator.contains("OnGenerate"));
	}
}
