use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

use crate::{ext::PathExt, project, source};

/// Generate a Luau tooling sourcemap from file-backed scripts.
#[derive(Parser)]
pub struct Sourcemap {
	/// Explicit `*.carbon.json` project, or a directory containing exactly one.
	#[arg()]
	source: Option<PathBuf>,

	/// Output sourcemap JSON path.
	#[arg(short, long, default_value = "sourcemap.json")]
	output: PathBuf,
}

impl Sourcemap {
	pub fn main(self) -> Result<()> {
		let manifest = source::resolve(self.source.unwrap_or_default())?;
		let output = self.output.resolve()?;
		let instances = project::write_sourcemap(&manifest, &output)?;
		crate::carbon_info!(
			"Mapped {} script instances and ancestors to {}",
			instances,
			output.display()
		);
		Ok(())
	}
}
