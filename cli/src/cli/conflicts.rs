use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

use crate::artifact_resolution;

/// Inspect a conflicted Carbon artifact as typed semantic fields.
#[derive(Parser)]
#[command(
	after_long_help = "AGENT WORKFLOW:\n  1. Save the machine-readable document:\n       carbon conflicts --json > carbon-conflicts.json\n  2. Add exactly one entry to `decisions` for each conflict ID.\n  3. Apply and stage the validated result:\n       carbon resolve --plan carbon-conflicts.json\n  4. Continue Git yourself:\n       git merge --continue   # or: git rebase --continue\n\nThe document names Git's stages `base`, `current`, and `incoming`. Each conflict includes stable instance identity, path/class context for all three sides, typed values, and allowed choices. Carbon fingerprints the index objects and conflict descriptions, so a plan cannot be applied to a different conflict state.\n\nRun `carbon resolve --help` for decision JSON examples."
)]
pub struct Conflicts {
	/// Conflicted `state.carbon`; optional when exactly one Carbon artifact is unmerged.
	#[arg()]
	path: Option<PathBuf>,

	/// Emit the editable, machine-readable resolution document as JSON.
	#[arg(long)]
	json: bool,
}

impl Conflicts {
	pub fn main(self) -> Result<()> {
		let document = artifact_resolution::discover(self.path.as_deref())?;
		if self.json {
			println!("{}", serde_json::to_string_pretty(&document)?);
			return Ok(());
		}
		println!(
			"{} semantic conflict(s) in {} (token {})",
			document.conflicts.len(),
			document.path,
			document.token
		);
		for conflict in &document.conflicts {
			let details = &conflict.details;
			let subject = details
				.context
				.current
				.as_ref()
				.or(details.context.incoming.as_ref())
				.or(details.context.base.as_ref());
			let location = subject
				.map(|subject| {
					let path = if subject.path.is_empty() {
						"<root>".to_owned()
					} else {
						subject.path.join("/")
					};
					format!("{path} ({})", subject.class)
				})
				.unwrap_or_else(|| details.identity.clone());
			let field = details
				.field
				.name
				.as_ref()
				.map(|name| format!("{}.{}", details.field.kind, name))
				.unwrap_or_else(|| details.field.kind.clone());
			println!("\n{}  {}  {}", conflict.id, location, field);
			println!("  base:     {}", serde_json::to_string(&details.base)?);
			println!("  current:  {}", serde_json::to_string(&details.current)?);
			println!("  incoming: {}", serde_json::to_string(&details.incoming)?);
			println!("  allowed:  {}", details.allowed.join(", "));
		}
		println!("\nNext: `carbon conflicts --json > carbon-conflicts.json`, then edit `decisions` and run `carbon resolve --plan carbon-conflicts.json`.");
		println!("Help: `carbon conflicts --help` and `carbon resolve --help`.");
		Ok(())
	}
}
