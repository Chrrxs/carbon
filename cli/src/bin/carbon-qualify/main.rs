mod engine;
mod model;
mod report;
mod runtime;
mod snapshot;
mod template;
mod warning;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use engine::QualificationRunner;
use model::{Outcome, Suite};
use runtime::RealRuntime;
use serde_json::Value;
use std::{
	collections::BTreeMap,
	fs,
	path::{Path, PathBuf},
	process::ExitCode,
	time::{SystemTime, UNIX_EPOCH},
};

const DEFAULT_MCP_URL: &str = "http://127.0.0.1:58741";

#[derive(Parser)]
#[command(
	name = "carbon-qualify",
	version = env!("CARBON_BUILD_VERSION"),
	about = "Rust production-readiness runner for the Carbon Roblox stack"
)]
struct Cli {
	#[command(subcommand)]
	command: Commands,
}

#[derive(Subcommand)]
enum Commands {
	/// Parse and structurally validate a qualification suite without running it.
	Validate {
		#[arg(value_name = "SUITE.json")]
		suite: PathBuf,
	},
	/// Run every required scenario and emit JSON plus JUnit evidence.
	Run {
		#[arg(value_name = "SUITE.json")]
		suite: PathBuf,
		#[arg(long, value_name = "DIR")]
		artifacts: Option<PathBuf>,
		#[arg(long, default_value = DEFAULT_MCP_URL)]
		mcp_url: String,
		#[arg(long = "var", value_name = "NAME=VALUE")]
		variables: Vec<String>,
	},
}

fn main() -> ExitCode {
	match execute() {
		Ok(true) => ExitCode::SUCCESS,
		Ok(false) => ExitCode::FAILURE,
		Err(error) => {
			eprintln!("carbon-qualify: {error:#}");
			ExitCode::FAILURE
		}
	}
}

fn execute() -> Result<bool> {
	match Cli::parse().command {
		Commands::Validate { suite } => {
			let suite = load_suite(&suite)?;
			println!(
				"PASS: suite {:?} contains {} structurally valid scenarios",
				suite.name,
				suite.scenarios.len()
			);
			Ok(true)
		}
		Commands::Run {
			suite,
			artifacts,
			mcp_url,
			variables,
		} => {
			let suite_path = absolute_path(&suite)?;
			let suite = load_suite(&suite_path)?;
			let artifact_dir = match artifacts {
				Some(path) => absolute_output_path(&path)?,
				None => default_artifact_dir()?,
			};
			fs::create_dir_all(&artifact_dir)
				.with_context(|| format!("failed to create artifact directory {}", artifact_dir.display()))?;
			let overrides = parse_variables(&variables)?;
			let mut runtime = RealRuntime::new(artifact_dir.clone(), &mcp_url)?;
			let mut runner = QualificationRunner::new(&mut runtime, suite_path, artifact_dir.clone(), overrides);
			let report = runner.run(&suite);
			report::write_reports(&report, &artifact_dir)?;
			println!(
				"{}: {} scenarios in {} ms; evidence: {}",
				if report.outcome == Outcome::Pass {
					"PASS"
				} else {
					"FAIL"
				},
				report.scenarios.len(),
				report.duration_ms,
				artifact_dir.display()
			);
			for failure in &report.failures {
				eprintln!("- {failure}");
			}
			Ok(report.outcome == Outcome::Pass)
		}
	}
}

fn load_suite(path: &Path) -> Result<Suite> {
	let bytes = fs::read(path).with_context(|| format!("failed to read qualification suite {}", path.display()))?;
	let suite: Suite = serde_json::from_slice(&bytes)
		.with_context(|| format!("failed to parse qualification suite {}", path.display()))?;
	suite.validate()?;
	Ok(suite)
}

fn parse_variables(values: &[String]) -> Result<BTreeMap<String, Value>> {
	let mut parsed = BTreeMap::new();
	for item in values {
		let Some((name, value)) = item.split_once('=') else {
			bail!("suite variable {item:?} must use NAME=VALUE syntax");
		};
		if name.trim().is_empty() {
			bail!("suite variable name must not be empty");
		}
		if parsed
			.insert(name.to_owned(), Value::String(value.to_owned()))
			.is_some()
		{
			bail!("suite variable {name:?} was provided more than once");
		}
	}
	Ok(parsed)
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
	path.canonicalize()
		.with_context(|| format!("failed to resolve {}", path.display()))
}

fn absolute_output_path(path: &Path) -> Result<PathBuf> {
	if path.is_absolute() {
		Ok(path.to_owned())
	} else {
		Ok(std::env::current_dir()?.join(path))
	}
}

fn default_artifact_dir() -> Result<PathBuf> {
	let timestamp = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.context("system clock is before the Unix epoch")?
		.as_millis();
	Ok(std::env::current_dir()?
		.join("qualification-artifacts")
		.join(format!("run-{timestamp}-{}", std::process::id())))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn command_line_variables_remain_strings() {
		let variables = parse_variables(&["port=48125".into(), "path=C:\\Game".into()]).unwrap();
		assert_eq!(variables["port"], "48125");
		assert_eq!(variables["path"], "C:\\Game");
	}
}
