use crate::{
	model::{
		Action, CheckOperation, JsonCheck, JsonSelection, Outcome, QualificationReport, Scenario, ScenarioReport, Step,
		StepReport, Suite,
	},
	place_assertion,
	runtime::{CommandRequest, Observation, RuntimeAction, RuntimeAdapter, SpawnRequest},
	snapshot::PathSnapshot,
	template::Variables,
	warning::{runtime_log_failures, warning_lines},
};
use anyhow::{bail, ensure, Context, Result};
use chrono::Utc;
use serde_json::Value;
use std::{
	collections::{BTreeMap, HashMap},
	path::{Path, PathBuf},
	time::{Duration, Instant},
};

pub struct QualificationRunner<'a, R: RuntimeAdapter> {
	runtime: &'a mut R,
	suite_path: PathBuf,
	artifact_dir: PathBuf,
	overrides: BTreeMap<String, Value>,
}

impl<'a, R: RuntimeAdapter> QualificationRunner<'a, R> {
	pub fn new(
		runtime: &'a mut R,
		suite_path: PathBuf,
		artifact_dir: PathBuf,
		overrides: BTreeMap<String, Value>,
	) -> Self {
		Self {
			runtime,
			suite_path,
			artifact_dir,
			overrides,
		}
	}

	pub fn run(&mut self, suite: &Suite) -> QualificationReport {
		let started_wall = Utc::now();
		let started = Instant::now();
		let mut failures = Vec::new();
		let mut reports = Vec::new();

		if let Err(error) = self.check_requirements(suite) {
			failures.push(format!("environment requirements failed: {error:#}"));
		} else {
			for scenario in &suite.scenarios {
				let first_report = reports.len();
				for iteration in 1..=scenario.repeats {
					reports.push(self.run_scenario(suite, scenario, iteration));
				}
				if let Some(limit) = scenario.max_p95_ms {
					if !reports[first_report..]
						.iter()
						.all(|report| report.outcome == Outcome::Pass)
					{
						continue;
					}
					let mut durations = reports[first_report..]
						.iter()
						.map(|report| report.duration_ms)
						.collect::<Vec<_>>();
					durations.sort_unstable();
					let index = ((durations.len() * 95).div_ceil(100)).saturating_sub(1);
					let p95 = durations[index];
					if p95 > limit {
						let failure = format!(
							"scenario {:?} p95 duration was {p95} ms, above {limit} ms",
							scenario.name
						);
						if let Some(report) = reports.get_mut(first_report) {
							report.outcome = Outcome::Fail;
							report.failure = Some(failure);
						}
					}
				}
			}
		}

		for report in &reports {
			if report.outcome == Outcome::Fail {
				failures.push(format!(
					"scenario {:?} iteration {} failed: {}",
					report.name,
					report.iteration,
					report.failure.as_deref().unwrap_or("unknown failure")
				));
			}
		}
		let duration_ms = elapsed_ms(started);
		if let Some(limit_seconds) = suite.policy.max_suite_seconds {
			let limit_ms = limit_seconds.saturating_mul(1000);
			if duration_ms > limit_ms {
				failures.push(format!(
					"suite took {duration_ms} ms, above the {limit_ms} ms release budget"
				));
			}
		}

		QualificationReport {
			schema_version: 1,
			suite: suite.name.clone(),
			description: suite.description.clone(),
			started_at: started_wall.to_rfc3339(),
			duration_ms,
			outcome: if failures.is_empty() {
				Outcome::Pass
			} else {
				Outcome::Fail
			},
			failures,
			scenarios: reports,
		}
	}

	fn check_requirements(&self, suite: &Suite) -> Result<()> {
		if !suite.requirements.operating_systems.is_empty() {
			let current = std::env::consts::OS;
			ensure!(
				suite
					.requirements
					.operating_systems
					.iter()
					.any(|value| value.eq_ignore_ascii_case(current)),
				"suite supports {:?}, current operating system is {current:?}",
				suite.requirements.operating_systems
			);
		}
		for name in &suite.requirements.environment {
			ensure!(
				std::env::var_os(name).is_some_and(|value| !value.is_empty()),
				"required environment variable {name:?} is not set"
			);
		}
		let variables = self.initial_variables(suite);
		for name in &suite.requirements.variables {
			let value = variables
				.get(name)
				.with_context(|| format!("required suite variable {name:?} was not provided"))?;
			ensure!(
				!value.is_null() && !value.as_str().is_some_and(|value| value.trim().is_empty()),
				"required suite variable {name:?} is empty"
			);
		}
		Ok(())
	}

	fn run_scenario(&mut self, suite: &Suite, scenario: &Scenario, iteration: u32) -> ScenarioReport {
		let started = Instant::now();
		let mut variables = self.initial_variables(suite);
		variables.insert("scenario", Value::String(scenario.name.clone()));
		variables.insert("iteration", Value::from(iteration));
		let mut snapshots = HashMap::new();
		let mut steps = Vec::new();
		let mut cleanup = Vec::new();
		let mut failure = None;

		for step in &scenario.steps {
			let (report, step_failure) =
				self.run_step(suite, scenario, iteration, step, &mut variables, &mut snapshots, false);
			steps.push(report);
			if let Some(step_failure) = step_failure {
				failure = Some(step_failure);
				break;
			}
		}

		for step in &scenario.cleanup {
			let (report, step_failure) =
				self.run_step(suite, scenario, iteration, step, &mut variables, &mut snapshots, true);
			cleanup.push(report);
			if let Some(step_failure) = step_failure {
				let cleanup_failure = format!("cleanup failed: {step_failure}");
				failure = Some(match failure {
					Some(primary) => format!("{primary}; {cleanup_failure}"),
					None => cleanup_failure,
				});
			}
		}

		ScenarioReport {
			name: scenario.name.clone(),
			description: scenario.description.clone(),
			tags: scenario.tags.clone(),
			iteration,
			duration_ms: elapsed_ms(started),
			outcome: if failure.is_some() {
				Outcome::Fail
			} else {
				Outcome::Pass
			},
			failure,
			steps,
			cleanup,
		}
	}

	#[allow(clippy::too_many_arguments)]
	fn run_step(
		&mut self,
		suite: &Suite,
		scenario: &Scenario,
		iteration: u32,
		step: &Step,
		variables: &mut Variables,
		snapshots: &mut HashMap<String, PathSnapshot>,
		cleanup: bool,
	) -> (StepReport, Option<String>) {
		let started = Instant::now();
		let kind = action_kind(&step.action).to_owned();
		let stem = format!(
			"{}-{}-{}-{}",
			scenario.name,
			iteration,
			if cleanup { "cleanup" } else { "step" },
			step.name
		);
		let result = self.execute_action(suite, &step.action, variables, snapshots, &stem);
		let duration_ms = elapsed_ms(started);
		let result = result.and_then(|(summary, artifacts)| {
			if let Some(limit) = step.max_duration_ms {
				ensure!(
					duration_ms <= limit,
					"step took {duration_ms} ms, above its {limit} ms budget"
				);
			}
			Ok((summary, artifacts))
		});

		match result {
			Ok((summary, artifacts)) => (
				StepReport {
					name: step.name.clone(),
					kind,
					duration_ms,
					outcome: Outcome::Pass,
					summary,
					artifacts,
				},
				None,
			),
			Err(error) => {
				let message = format!("step {:?}: {error:#}", step.name);
				let artifacts = self.artifacts_for_stem(&stem);
				(
					StepReport {
						name: step.name.clone(),
						kind,
						duration_ms,
						outcome: Outcome::Fail,
						summary: message.clone(),
						artifacts,
					},
					Some(message),
				)
			}
		}
	}

	fn execute_action(
		&mut self,
		suite: &Suite,
		action: &Action,
		variables: &mut Variables,
		snapshots: &mut HashMap<String, PathSnapshot>,
		artifact_stem: &str,
	) -> Result<(String, Vec<String>)> {
		match action {
			Action::Command {
				program,
				args,
				cwd,
				env,
				timeout_seconds,
				expected_exit_code,
				stdout_contains,
				stdout_not_contains,
				stderr_contains,
				stderr_not_contains,
				allowed_diagnostic_contains,
			} => {
				let request = CommandRequest {
					program: variables.resolve_string(program)?,
					args: resolve_strings(variables, args)?,
					cwd: resolve_optional_path(variables, cwd.as_deref(), self.suite_dir())?,
					env: resolve_map(variables, env)?,
					timeout: Duration::from_secs(*timeout_seconds),
					artifact_stem: artifact_stem.to_owned(),
				};
				let observation = self.runtime.execute(RuntimeAction::Command(request))?;
				let Observation::Command {
					exit_code,
					stdout,
					stderr,
					timed_out,
					artifacts,
				} = observation
				else {
					bail!("runtime returned the wrong observation for a command");
				};
				validate_command(
					exit_code,
					timed_out,
					&stdout,
					&stderr,
					*expected_exit_code,
					stdout_contains,
					stdout_not_contains,
					stderr_contains,
					stderr_not_contains,
					allowed_diagnostic_contains,
					suite.policy.fail_on_command_warnings,
				)?;
				Ok((format!("command exited with {exit_code}"), artifacts))
			}
			Action::Spawn {
				process,
				program,
				args,
				cwd,
				env,
			} => {
				let process = variables.resolve_string(process)?;
				let request = SpawnRequest {
					process: process.clone(),
					program: variables.resolve_string(program)?,
					args: resolve_strings(variables, args)?,
					cwd: resolve_optional_path(variables, cwd.as_deref(), self.suite_dir())?,
					env: resolve_map(variables, env)?,
					artifact_stem: artifact_stem.to_owned(),
				};
				match self.runtime.execute(RuntimeAction::Spawn(request))? {
					Observation::Spawned { process, artifacts } => {
						Ok((format!("spawned background process {process:?}"), artifacts))
					}
					_ => bail!("runtime returned the wrong observation for spawn"),
				}
			}
			Action::WaitProcess {
				process,
				timeout_seconds,
				expected_exit_codes,
			} => {
				let process = variables.resolve_string(process)?;
				let observation = self.runtime.execute(RuntimeAction::WaitProcess {
					process: process.clone(),
					timeout: Duration::from_secs(*timeout_seconds),
				})?;
				let Observation::Command {
					exit_code,
					stdout,
					stderr,
					timed_out,
					artifacts,
				} = observation
				else {
					bail!("runtime returned the wrong observation while waiting for a process");
				};
				ensure!(!timed_out, "background process {process:?} did not exit before timeout");
				ensure!(
					expected_exit_codes.contains(&exit_code),
					"background process {process:?} exited with {exit_code}, expected one of {expected_exit_codes:?}"
				);
				if suite.policy.fail_on_command_warnings {
					let failures = warning_lines(&stdout)
						.into_iter()
						.chain(warning_lines(&stderr))
						.collect::<Vec<_>>();
					ensure!(
						failures.is_empty(),
						"background process emitted warning/error diagnostics:\n{}",
						failures.join("\n")
					);
				}
				Ok((
					format!("background process {process:?} exited with {exit_code}"),
					artifacts,
				))
			}
			Action::TerminateProcess { process } => {
				let process = variables.resolve_string(process)?;
				match self.runtime.execute(RuntimeAction::TerminateProcess {
					process: process.clone(),
				})? {
					Observation::Terminated {
						process,
						existed,
						artifacts,
					} => Ok((
						format!(
							"background process {process:?} {}",
							if existed { "terminated" } else { "was already absent" }
						),
						artifacts,
					)),
					_ => bail!("runtime returned the wrong observation for termination"),
				}
			}
			Action::Mcp {
				tool,
				arguments,
				timeout_seconds,
				poll_interval_ms,
				checks,
				select,
				capture,
			} => {
				let tool = variables.resolve_string(tool)?;
				let arguments = variables.resolve_value(arguments)?;
				let checks = resolve_json_checks(variables, checks)?;
				let select = select
					.as_ref()
					.map(|select| resolve_json_selection(variables, select))
					.transpose()?;
				let deadline = Instant::now() + Duration::from_secs(*timeout_seconds);
				let (result, selected, artifacts) = loop {
					let remaining = deadline.saturating_duration_since(Instant::now());
					if remaining.is_zero() {
						bail!("timed out waiting for MCP checks for tool {tool:?}");
					}
					let observation = self.runtime.execute(RuntimeAction::Mcp {
						tool: tool.clone(),
						arguments: arguments.clone(),
						timeout: remaining,
						artifact_stem: artifact_stem.to_owned(),
					})?;
					let Observation::Mcp { result, artifacts } = observation else {
						bail!("runtime returned the wrong observation for MCP");
					};
					let selected = select
						.as_ref()
						.map(|select| select_json_value(&result, select).cloned())
						.transpose();
					let validation = selected.and_then(|selected| {
						validate_json_checks(&result, &checks)?;
						Ok(selected)
					});
					match validation {
						Ok(selected) => break (result, selected, artifacts),
						Err(error) if poll_interval_ms.is_some() && Instant::now() < deadline => {
							let interval = Duration::from_millis(poll_interval_ms.unwrap());
							std::thread::sleep(interval.min(deadline.saturating_duration_since(Instant::now())));
							let _ = error;
						}
						Err(error) => return Err(error),
					}
				};

				if suite.policy.fail_on_runtime_warnings && tool == "get_runtime_logs" {
					let failures = runtime_log_failures(&result);
					ensure!(
						failures.is_empty(),
						"Studio runtime emitted warning/error diagnostics:\n{}",
						failures.join("\n")
					);
				}
				let capture_root = selected.as_ref().unwrap_or(&result);
				for (name, pointer) in capture {
					let value = capture_root
						.pointer(pointer)
						.with_context(|| format!("capture pointer {pointer:?} was absent"))?;
					variables.insert(name.clone(), value.clone());
				}
				Ok((format!("MCP tool {tool:?} completed"), artifacts))
			}
			Action::Sleep { milliseconds } => {
				self.runtime
					.execute(RuntimeAction::Sleep(Duration::from_millis(*milliseconds)))?;
				Ok((format!("slept for {milliseconds} ms"), Vec::new()))
			}
			Action::SnapshotPath { snapshot, path } => {
				let snapshot_name = variables.resolve_string(snapshot)?;
				let path = resolve_path(&variables.resolve_string(path)?, self.suite_dir());
				let value = PathSnapshot::capture(&path)?;
				let summary = format!(
					"captured {} entries from {} with digest {}",
					value.entry_count(),
					path.display(),
					value.digest
				);
				snapshots.insert(snapshot_name, value);
				Ok((summary, Vec::new()))
			}
			Action::AssertPathUnchanged {
				snapshot,
				path,
				check_mtime,
			} => {
				let snapshot_name = variables.resolve_string(snapshot)?;
				let path = resolve_path(&variables.resolve_string(path)?, self.suite_dir());
				let snapshot = snapshots
					.get(&snapshot_name)
					.with_context(|| format!("snapshot {snapshot_name:?} has not been captured"))?;
				snapshot.compare(&path, *check_mtime)?;
				Ok((format!("{} remained unchanged", path.display()), Vec::new()))
			}
			Action::AssertNumericDelta {
				before,
				after,
				max_increase,
			} => {
				let before_value = variables
					.get(before)
					.and_then(Value::as_f64)
					.with_context(|| format!("baseline variable {before:?} is absent or non-numeric"))?;
				let after_value = variables
					.get(after)
					.and_then(Value::as_f64)
					.with_context(|| format!("current variable {after:?} is absent or non-numeric"))?;
				let increase = after_value - before_value;
				ensure!(
					increase <= *max_increase,
					"numeric value increased by {increase:.3} from {before_value:.3} to {after_value:.3}, above the {max_increase:.3} budget"
				);
				Ok((
					format!("numeric value changed by {increase:.3} from {before_value:.3} to {after_value:.3}"),
					Vec::new(),
				))
			}
			Action::AssertPlaceInstance {
				path,
				instance_path,
				class_name,
				properties,
				attributes,
			} => {
				let path = resolve_path(&variables.resolve_string(path)?, self.suite_dir());
				let instance_path = resolve_strings(variables, instance_path)?;
				let class_name = variables.resolve_string(class_name)?;
				let properties = resolve_value_map(variables, properties)?;
				let attributes = resolve_value_map(variables, attributes)?;
				place_assertion::assert_place_instance(&path, &instance_path, &class_name, &properties, &attributes)?;
				Ok((
					format!(
						"verified {} as {} in {}",
						instance_path.join("/"),
						class_name,
						path.display()
					),
					Vec::new(),
				))
			}
		}
	}

	fn initial_variables(&self, suite: &Suite) -> Variables {
		let mut values = suite.variables.clone();
		values.extend(self.overrides.clone());
		values.insert(
			"suite_dir".to_owned(),
			Value::String(self.suite_dir().to_string_lossy().into_owned()),
		);
		values.insert(
			"artifact_dir".to_owned(),
			Value::String(self.artifact_dir.to_string_lossy().into_owned()),
		);
		values
			.entry("qualification_token".to_owned())
			.or_insert_with(|| Value::String(uuid::Uuid::new_v4().simple().to_string()));
		Variables::new(values)
	}

	fn suite_dir(&self) -> &Path {
		self.suite_path.parent().unwrap_or_else(|| Path::new("."))
	}

	fn artifacts_for_stem(&self, stem: &str) -> Vec<String> {
		let prefix = sanitize(stem);
		let mut artifacts = std::fs::read_dir(&self.artifact_dir)
			.into_iter()
			.flatten()
			.filter_map(|entry| entry.ok())
			.filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
			.map(|entry| entry.path().to_string_lossy().into_owned())
			.collect::<Vec<_>>();
		artifacts.sort();
		artifacts
	}
}

#[allow(clippy::too_many_arguments)]
fn validate_command(
	exit_code: i32,
	timed_out: bool,
	stdout: &str,
	stderr: &str,
	expected_exit_code: i32,
	stdout_contains: &[String],
	stdout_not_contains: &[String],
	stderr_contains: &[String],
	stderr_not_contains: &[String],
	allowed_diagnostic_contains: &[String],
	fail_on_warnings: bool,
) -> Result<()> {
	ensure!(!timed_out, "command timed out");
	ensure!(
		exit_code == expected_exit_code,
		"command exited with {exit_code}, expected {expected_exit_code}"
	);
	for expected in stdout_contains {
		ensure!(stdout.contains(expected), "stdout did not contain {expected:?}");
	}
	for forbidden in stdout_not_contains {
		ensure!(
			!stdout.contains(forbidden),
			"stdout contained forbidden text {forbidden:?}"
		);
	}
	for expected in stderr_contains {
		ensure!(stderr.contains(expected), "stderr did not contain {expected:?}");
	}
	for forbidden in stderr_not_contains {
		ensure!(
			!stderr.contains(forbidden),
			"stderr contained forbidden text {forbidden:?}"
		);
	}
	if fail_on_warnings {
		let failures = warning_lines(stdout)
			.into_iter()
			.chain(warning_lines(stderr))
			.filter(|line| !allowed_diagnostic_contains.iter().any(|allowed| line.contains(allowed)))
			.collect::<Vec<_>>();
		ensure!(
			failures.is_empty(),
			"command emitted warning/error diagnostics:\n{}",
			failures.join("\n")
		);
	}
	Ok(())
}

fn validate_json_checks(result: &Value, checks: &[JsonCheck]) -> Result<()> {
	for check in checks {
		let actual = result.pointer(&check.pointer);
		match check.op {
			CheckOperation::Exists => ensure!(actual.is_some(), "JSON pointer {:?} was absent", check.pointer),
			CheckOperation::Absent => ensure!(actual.is_none(), "JSON pointer {:?} was present", check.pointer),
			CheckOperation::Equals => ensure!(
				actual == check.value.as_ref(),
				"JSON pointer {:?} was {}, expected {}",
				check.pointer,
				actual.unwrap_or(&Value::Null),
				check.value.as_ref().unwrap_or(&Value::Null)
			),
			CheckOperation::NotEquals => ensure!(
				actual != check.value.as_ref(),
				"JSON pointer {:?} unexpectedly equalled {}",
				check.pointer,
				check.value.as_ref().unwrap_or(&Value::Null)
			),
			CheckOperation::Contains => {
				let expected = check.value.as_ref().expect("validated check value");
				let contains = match (actual, expected) {
					(Some(Value::String(actual)), Value::String(expected)) => actual.contains(expected),
					(Some(Value::Array(actual)), expected) => actual.contains(expected),
					(Some(Value::Object(actual)), Value::String(expected)) => actual.contains_key(expected),
					_ => false,
				};
				ensure!(
					contains,
					"JSON pointer {:?} did not contain {}",
					check.pointer,
					expected
				);
			}
			CheckOperation::LessThanOrEqual | CheckOperation::GreaterThanOrEqual => {
				let actual = actual
					.and_then(Value::as_f64)
					.with_context(|| format!("JSON pointer {:?} was not numeric", check.pointer))?;
				let expected = check
					.value
					.as_ref()
					.and_then(Value::as_f64)
					.context("numeric JSON check had a non-numeric expected value")?;
				match check.op {
					CheckOperation::LessThanOrEqual => ensure!(
						actual <= expected,
						"JSON pointer {:?} was {actual}, above {expected}",
						check.pointer
					),
					CheckOperation::GreaterThanOrEqual => ensure!(
						actual >= expected,
						"JSON pointer {:?} was {actual}, below {expected}",
						check.pointer
					),
					_ => unreachable!(),
				}
			}
		}
	}
	Ok(())
}

fn resolve_json_checks(variables: &Variables, checks: &[JsonCheck]) -> Result<Vec<JsonCheck>> {
	checks
		.iter()
		.map(|check| {
			Ok(JsonCheck {
				pointer: variables.resolve_string(&check.pointer)?,
				op: check.op,
				value: check
					.value
					.as_ref()
					.map(|value| variables.resolve_value(value))
					.transpose()?,
			})
		})
		.collect()
}

fn resolve_json_selection(variables: &Variables, selection: &JsonSelection) -> Result<JsonSelection> {
	Ok(JsonSelection {
		pointer: variables.resolve_string(&selection.pointer)?,
		checks: resolve_json_checks(variables, &selection.checks)?,
	})
}

fn select_json_value<'a>(result: &'a Value, selection: &JsonSelection) -> Result<&'a Value> {
	let values = result
		.pointer(&selection.pointer)
		.with_context(|| format!("selection pointer {:?} was absent", selection.pointer))?
		.as_array()
		.with_context(|| format!("selection pointer {:?} is not an array", selection.pointer))?;
	let matches = values
		.iter()
		.filter(|candidate| validate_json_checks(candidate, &selection.checks).is_ok())
		.collect::<Vec<_>>();
	ensure!(
		matches.len() == 1,
		"selection pointer {:?} matched {} elements, expected exactly one",
		selection.pointer,
		matches.len()
	);
	Ok(matches[0])
}

fn resolve_strings(variables: &Variables, values: &[String]) -> Result<Vec<String>> {
	values.iter().map(|value| variables.resolve_string(value)).collect()
}

fn resolve_map(variables: &Variables, values: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
	values
		.iter()
		.map(|(key, value)| Ok((key.clone(), variables.resolve_string(value)?)))
		.collect()
}

fn resolve_value_map(variables: &Variables, values: &BTreeMap<String, Value>) -> Result<BTreeMap<String, Value>> {
	values
		.iter()
		.map(|(key, value)| Ok((key.clone(), variables.resolve_value(value)?)))
		.collect()
}

fn resolve_optional_path(variables: &Variables, value: Option<&str>, base: &Path) -> Result<Option<PathBuf>> {
	value
		.map(|value| variables.resolve_string(value).map(|value| resolve_path(&value, base)))
		.transpose()
}

fn resolve_path(value: &str, base: &Path) -> PathBuf {
	let path = PathBuf::from(value);
	if path.is_absolute() {
		path
	} else {
		base.join(path)
	}
}

fn action_kind(action: &Action) -> &'static str {
	match action {
		Action::Command { .. } => "command",
		Action::Spawn { .. } => "spawn",
		Action::WaitProcess { .. } => "wait_process",
		Action::TerminateProcess { .. } => "terminate_process",
		Action::Mcp { .. } => "mcp",
		Action::Sleep { .. } => "sleep",
		Action::SnapshotPath { .. } => "snapshot_path",
		Action::AssertPathUnchanged { .. } => "assert_path_unchanged",
		Action::AssertNumericDelta { .. } => "assert_numeric_delta",
		Action::AssertPlaceInstance { .. } => "assert_place_instance",
	}
}

fn elapsed_ms(started: Instant) -> u64 {
	started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
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

#[cfg(test)]
mod tests {
	use super::*;
	use crate::runtime::ScriptedRuntime;
	use serde_json::json;
	use std::fs;

	fn suite_with_steps(steps: Value, cleanup: Value) -> Suite {
		serde_json::from_value(json!({
			"schema_version": 1,
			"name": "test",
			"scenarios": [{
				"name": "scenario",
				"steps": steps,
				"cleanup": cleanup
			}]
		}))
		.unwrap()
	}

	fn run_with(runtime: &mut ScriptedRuntime, suite: &Suite) -> QualificationReport {
		let root = std::env::temp_dir().join(format!("carbon-qualify-engine-{}", uuid::Uuid::new_v4()));
		fs::create_dir_all(&root).unwrap();
		let suite_path = root.join("suite.json");
		let mut runner = QualificationRunner::new(runtime, suite_path, root.clone(), BTreeMap::new());
		let report = runner.run(suite);
		fs::remove_dir_all(root).unwrap();
		report
	}

	#[test]
	fn command_warning_fails_and_cleanup_still_runs() {
		let suite = suite_with_steps(
			json!([{"name": "command", "kind": "command", "program": "test"}]),
			json!([{"name": "cleanup", "kind": "sleep", "milliseconds": 1}]),
		);
		let mut runtime = ScriptedRuntime::new(vec![
			Ok(Observation::Command {
				exit_code: 0,
				stdout: "WARNING: bad".into(),
				stderr: String::new(),
				timed_out: false,
				artifacts: vec![],
			}),
			Ok(Observation::Slept),
		]);
		let report = run_with(&mut runtime, &suite);
		assert_eq!(report.outcome, Outcome::Fail);
		assert_eq!(runtime.actions.len(), 2);
		assert!(report.failures.iter().any(|failure| failure.contains("WARNING")));
	}

	#[test]
	fn expected_command_diagnostics_do_not_hide_unrelated_failures() {
		let expected = "modify the relevant manifest reference".to_owned();
		validate_command(
			1,
			false,
			"",
			"error: restore the mapped source identity or modify the relevant manifest reference",
			1,
			&[],
			&[],
			std::slice::from_ref(&expected),
			&[],
			std::slice::from_ref(&expected),
			true,
		)
		.unwrap();

		let error = validate_command(
			1,
			false,
			"WARNING: unrelated",
			"error: restore the mapped source identity or modify the relevant manifest reference",
			1,
			&[],
			&[],
			&["modify the relevant manifest reference".to_owned()],
			&[],
			&["modify the relevant manifest reference".to_owned()],
			true,
		)
		.unwrap_err()
		.to_string();
		assert!(error.contains("WARNING: unrelated"));
	}

	#[test]
	fn captured_mcp_values_feed_later_steps() {
		let suite = suite_with_steps(
			json!([
				{
					"name": "connect",
					"kind": "mcp",
					"tool": "get_connected_instances",
					"capture": {"instance": "/instances/0/instanceId"}
				},
				{
					"name": "probe",
					"kind": "mcp",
					"tool": "execute_luau",
					"arguments": {"instance_id": "${instance}", "code": "return true"}
				}
			]),
			json!([]),
		);
		let mut runtime = ScriptedRuntime::new(vec![
			Ok(Observation::Mcp {
				result: json!({"instances": [{"instanceId": "studio-1"}]}),
				artifacts: vec![],
			}),
			Ok(Observation::Mcp {
				result: json!({"success": true}),
				artifacts: vec![],
			}),
		]);
		let report = run_with(&mut runtime, &suite);
		assert_eq!(report.outcome, Outcome::Pass);
		let RuntimeAction::Mcp { arguments, .. } = &runtime.actions[1] else {
			panic!("expected MCP action");
		};
		assert_eq!(arguments["instance_id"], "studio-1");
	}

	#[test]
	fn selected_mcp_instance_ignores_other_connected_worktrees() {
		let suite = suite_with_steps(
			json!([
				{
					"name": "connect",
					"kind": "mcp",
					"tool": "get_connected_instances",
					"select": {
						"pointer": "/instances",
						"checks": [
							{"pointer": "/dataModelName", "op": "equals", "value": "${studio_name}"},
							{"pointer": "/role", "op": "equals", "value": "edit"}
						]
					},
					"capture": {"instance": "/instanceId"}
				},
				{
					"name": "probe",
					"kind": "mcp",
					"tool": "execute_luau",
					"arguments": {"instance_id": "${instance}", "code": "return true"}
				}
			]),
			json!([]),
		);
		let mut runtime = ScriptedRuntime::new(vec![
			Ok(Observation::Mcp {
				result: json!({"instances": [
					{"instanceId": "other", "dataModelName": "other-worktree", "role": "edit"},
					{"instanceId": "mine", "dataModelName": "qualification-7", "role": "edit"}
				]}),
				artifacts: vec![],
			}),
			Ok(Observation::Mcp {
				result: json!({"success": true}),
				artifacts: vec![],
			}),
		]);
		let root = std::env::temp_dir().join(format!("carbon-qualify-engine-{}", uuid::Uuid::new_v4()));
		fs::create_dir_all(&root).unwrap();
		let suite_path = root.join("suite.json");
		let mut overrides = BTreeMap::new();
		overrides.insert("studio_name".to_owned(), Value::String("qualification-7".to_owned()));
		let mut runner = QualificationRunner::new(&mut runtime, suite_path, root.clone(), overrides);
		let report = runner.run(&suite);
		fs::remove_dir_all(root).unwrap();
		assert_eq!(report.outcome, Outcome::Pass);
		let RuntimeAction::Mcp { arguments, .. } = &runtime.actions[1] else {
			panic!("expected MCP action");
		};
		assert_eq!(arguments["instance_id"], "mine");
	}

	#[test]
	fn runtime_warning_entries_fail_even_when_the_tool_succeeds() {
		let suite = suite_with_steps(
			json!([{"name": "logs", "kind": "mcp", "tool": "get_runtime_logs"}]),
			json!([]),
		);
		let mut runtime = ScriptedRuntime::new(vec![Ok(Observation::Mcp {
			result: json!({"entries": [{"level": "WARN", "message": "unsafe"}]}),
			artifacts: vec![],
		})]);
		let report = run_with(&mut runtime, &suite);
		assert_eq!(report.outcome, Outcome::Fail);
		assert!(report.failures.iter().any(|failure| failure.contains("unsafe")));
	}

	#[test]
	fn numeric_growth_budget_uses_values_captured_from_mcp() {
		let suite = suite_with_steps(
			json!([
				{
					"name": "before",
					"kind": "mcp",
					"tool": "get_memory_breakdown",
					"capture": {"before_mb": "/total_mb"}
				},
				{
					"name": "after",
					"kind": "mcp",
					"tool": "get_memory_breakdown",
					"capture": {"after_mb": "/total_mb"}
				},
				{
					"name": "budget",
					"kind": "assert_numeric_delta",
					"before": "before_mb",
					"after": "after_mb",
					"max_increase": 10.0
				}
			]),
			json!([]),
		);
		let mut runtime = ScriptedRuntime::new(vec![
			Ok(Observation::Mcp {
				result: json!({"total_mb": 100.0}),
				artifacts: vec![],
			}),
			Ok(Observation::Mcp {
				result: json!({"total_mb": 112.0}),
				artifacts: vec![],
			}),
		]);
		let report = run_with(&mut runtime, &suite);
		assert_eq!(report.outcome, Outcome::Fail);
		assert!(report
			.failures
			.iter()
			.any(|failure| failure.contains("above the 10.000 budget")));
	}
}
