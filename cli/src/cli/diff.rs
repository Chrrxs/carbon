use anyhow::{bail, ensure, Result};
use clap::Parser;
use std::path::PathBuf;

use crate::{ext::PathExt, place_diff};

/// Compare two binary places and classify gameplay-affecting differences.
#[derive(Parser)]
pub struct Diff {
	/// Baseline .rbxl/.rbxm file.
	#[arg()]
	before: PathBuf,

	/// Candidate .rbxl/.rbxm file.
	#[arg()]
	after: PathBuf,

	/// Emit the complete report as JSON.
	#[arg(long)]
	json: bool,

	/// Maximum number of detailed differences retained in the report.
	#[arg(long, default_value_t = 200)]
	max_differences: usize,

	/// Always exit successfully after printing the report.
	#[arg(long)]
	no_fail: bool,
}

impl Diff {
	pub fn main(self) -> Result<()> {
		let before = self.before.resolve()?;
		let after = self.after.resolve()?;
		for path in [&before, &after] {
			ensure!(path.is_file(), "binary place does not exist: {}", path.display());
			ensure!(
				matches!(path.get_ext(), "rbxl" | "rbxm"),
				"unsupported input extension {}; expected .rbxl or .rbxm",
				path.get_ext()
			);
		}
		let report = place_diff::compare(&before, &after, self.max_differences)?;
		if self.json {
			println!("{}", serde_json::to_string_pretty(&report)?);
		} else {
			println!("Compared {} -> {}", report.before.display(), report.after.display());
			println!(
				"instances: {} before, {} after, {} matched, {} added, {} removed",
				report.before_instances,
				report.after_instances,
				report.matched_instances,
				report.added_instances,
				report.removed_instances
			);
			println!(
				"differences: {} blocking, {} accepted non-gameplay{}",
				report.blocking_differences,
				report.non_gameplay_differences,
				if report.details_truncated {
					" (details truncated)"
				} else {
					""
				}
			);
			for difference in &report.differences {
				let marker = if difference.impact == place_diff::Impact::NonGameplay {
					"NON-GAMEPLAY"
				} else {
					"BLOCKING"
				};
				println!(
					"[{marker}] {} {}{}: {}",
					difference.kind,
					difference.path,
					difference
						.property
						.as_ref()
						.map(|property| format!(".{property}"))
						.unwrap_or_default(),
					difference.reason
				);
			}
		}
		if report.has_blockers() && !self.no_fail {
			bail!(
				"{} gameplay-affecting or unexplained differences found",
				report.blocking_differences
			);
		}
		Ok(())
	}
}
