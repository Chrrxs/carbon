use anyhow::{Context, Result};
use clap::Parser;
use std::{
	fs,
	io::{self, Read},
	path::{Path, PathBuf},
};

use crate::artifact_resolution;

/// Apply an agent-authored Carbon semantic conflict plan and stage the result.
#[derive(Parser)]
#[command(
	after_long_help = "DECISION JSON:\n  Take one Git side:\n    {\"conflict\":\"c_...\",\"action\":\"take\",\"side\":\"incoming\"}\n\n  Set a custom typed value (only when `allowed` contains `custom`):\n    {\"conflict\":\"c_...\",\"action\":\"set\",\"value\":{...}}\n\n  Remove a property or metadata key (only when `allowed` contains `remove`):\n    {\"conflict\":\"c_...\",\"action\":\"remove\"}\n\nValid take sides are `base`, `current`, and `incoming`. Copy canonical typed property JSON from a conflict value when constructing a custom value. Conflict IDs may be written in full or as an unambiguous prefix of at least eight characters. Every conflict requires exactly one decision.\n\nCarbon rebuilds the merge from Git's index, rejects stale or modified documents, validates the complete artifact and its blobs, atomically installs it, and stages only its required paths. Carbon never runs `git merge --continue` or `git rebase --continue`; the agent must inspect the staged result and continue Git explicitly.\n\nRun `carbon conflicts --help` for discovery and document-field details."
)]
pub struct Resolve {
	/// JSON plan from `carbon conflicts --json`, or `-` to read standard input.
	#[arg(long, value_name = "FILE")]
	plan: PathBuf,

	/// Emit the apply report as JSON.
	#[arg(long)]
	json: bool,
}

impl Resolve {
	pub fn main(self) -> Result<()> {
		let bytes = if self.plan == Path::new("-") {
			let mut bytes = Vec::new();
			io::stdin().read_to_end(&mut bytes)?;
			bytes
		} else {
			fs::read(&self.plan).with_context(|| format!("failed to read resolution plan {}", self.plan.display()))?
		};
		let document = artifact_resolution::parse_document(&bytes)?;
		let report = artifact_resolution::apply(document)?;
		if self.json {
			println!("{}", serde_json::to_string_pretty(&report)?);
		} else {
			println!(
				"Resolved {} conflict(s); staged {} instance(s) and {} propert(ies) in {}",
				report.resolved_conflicts, report.instances, report.properties, report.path
			);
			println!("Staged: {}", report.staged.join(", "));
			println!("Next: {}", report.next);
			println!("Help: {}.", report.help.join(" and "));
		}
		Ok(())
	}
}
