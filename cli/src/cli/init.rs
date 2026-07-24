use anyhow::{ensure, Result};
use clap::Parser;
use std::path::PathBuf;

use crate::{carbon_info, ext::PathExt, project};

/// Initialize a canonical Carbon project without a source place.
#[derive(Parser)]
pub struct Init {
	/// Destination strict `*.carbon.json` project.
	#[arg(short, long, default_value = "game.carbon.json")]
	output: PathBuf,

	/// Root name. Defaults to the project stem.
	#[arg(short, long)]
	name: Option<String>,
}

impl Init {
	pub fn main(self) -> Result<()> {
		let output = self.output.resolve()?;
		ensure!(
			project::is_project_path(&output),
			"output must be a .carbon.json project"
		);
		ensure!(!output.exists(), "Carbon project already exists: {}", output.display());
		let name = self.name.unwrap_or_else(|| {
			output
				.file_name()
				.and_then(|name| name.to_str())
				.unwrap_or("game")
				.trim_end_matches(".carbon.json")
				.to_owned()
		});
		let report = project::initialize(&output, name)?;
		carbon_info!(
			"Initialized mapped starter project with {} Studio-owned instances in {} artifact",
			report.instances,
			report.artifacts
		);
		Ok(())
	}
}
