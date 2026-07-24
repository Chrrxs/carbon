use crate::model::{Outcome, QualificationReport, ScenarioReport};
use anyhow::{Context, Result};
use std::{fs, path::Path};

pub fn write_reports(report: &QualificationReport, artifact_dir: &Path) -> Result<()> {
	fs::create_dir_all(artifact_dir)?;
	let json_path = artifact_dir.join("report.json");
	fs::write(&json_path, serde_json::to_vec_pretty(report)?)
		.with_context(|| format!("failed to write {}", json_path.display()))?;
	let junit_path = artifact_dir.join("junit.xml");
	fs::write(&junit_path, junit(report)).with_context(|| format!("failed to write {}", junit_path.display()))?;
	Ok(())
}

fn junit(report: &QualificationReport) -> String {
	let failures = report
		.scenarios
		.iter()
		.filter(|scenario| scenario.outcome == Outcome::Fail)
		.count();
	let suite_failure = report.outcome == Outcome::Fail && failures == 0;
	let mut output = format!(
		"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" time=\"{:.3}\" timestamp=\"{}\">\n",
		xml_escape(&report.suite),
		report.scenarios.len() + usize::from(suite_failure),
		failures + usize::from(suite_failure),
		report.duration_ms as f64 / 1000.0,
		xml_escape(&report.started_at),
	);
	for scenario in &report.scenarios {
		write_case(&mut output, scenario);
	}
	if suite_failure {
		let failure = report.failures.join("\n");
		output.push_str(&format!(
			"  <testcase classname=\"carbon.production_readiness\" name=\"suite policy\" time=\"0\">\n    <failure message=\"{}\">{}</failure>\n  </testcase>\n",
			xml_escape(&failure),
			xml_escape(&failure)
		));
	}
	if !report.failures.is_empty() {
		output.push_str("  <system-err>");
		output.push_str(&xml_escape(&report.failures.join("\n")));
		output.push_str("</system-err>\n");
	}
	output.push_str("</testsuite>\n");
	output
}

fn write_case(output: &mut String, scenario: &ScenarioReport) {
	output.push_str(&format!(
		"  <testcase classname=\"carbon.production_readiness\" name=\"{} [iteration {}]\" time=\"{:.3}\">\n",
		xml_escape(&scenario.name),
		scenario.iteration,
		scenario.duration_ms as f64 / 1000.0,
	));
	if let Some(failure) = &scenario.failure {
		output.push_str(&format!(
			"    <failure message=\"{}\">{}</failure>\n",
			xml_escape(failure),
			xml_escape(failure)
		));
	}
	let evidence = scenario
		.steps
		.iter()
		.chain(&scenario.cleanup)
		.map(|step| {
			format!(
				"{} [{}] {:?} {} ms: {}{}",
				step.name,
				step.kind,
				step.outcome,
				step.duration_ms,
				step.summary,
				if step.artifacts.is_empty() {
					String::new()
				} else {
					format!(" artifacts={}", step.artifacts.join(","))
				}
			)
		})
		.collect::<Vec<_>>()
		.join("\n");
	output.push_str("    <system-out>");
	output.push_str(&xml_escape(&evidence));
	output.push_str("</system-out>\n  </testcase>\n");
}

fn xml_escape(value: &str) -> String {
	value
		.replace('&', "&amp;")
		.replace('<', "&lt;")
		.replace('>', "&gt;")
		.replace('"', "&quot;")
		.replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::model::{QualificationReport, ScenarioReport};

	#[test]
	fn junit_escapes_failure_evidence() {
		let report = QualificationReport {
			schema_version: 1,
			suite: "a & b".into(),
			description: String::new(),
			started_at: "now".into(),
			duration_ms: 1,
			outcome: Outcome::Fail,
			failures: vec!["x < y".into()],
			scenarios: vec![ScenarioReport {
				name: "case".into(),
				description: String::new(),
				tags: vec![],
				iteration: 1,
				duration_ms: 1,
				outcome: Outcome::Fail,
				failure: Some("x < y".into()),
				steps: vec![],
				cleanup: vec![],
			}],
		};
		let xml = junit(&report);
		assert!(xml.contains("a &amp; b"));
		assert!(xml.contains("x &lt; y"));
	}

	#[test]
	fn suite_level_failure_is_a_failing_junit_case() {
		let report = QualificationReport {
			schema_version: 1,
			suite: "release".into(),
			description: String::new(),
			started_at: "now".into(),
			duration_ms: 0,
			outcome: Outcome::Fail,
			failures: vec!["Windows runner required".into()],
			scenarios: vec![],
		};
		let xml = junit(&report);
		assert!(xml.contains("tests=\"1\" failures=\"1\""));
		assert!(xml.contains("Windows runner required"));
	}
}
