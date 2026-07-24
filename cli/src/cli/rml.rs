use anyhow::Result;
use clap::{Args, Subcommand};
use std::path::PathBuf;

use crate::rml;

/// Inspect or install the RobloxModLoader bootstrap used by Carbon.
#[derive(Args)]
pub struct Rml {
	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand)]
enum Command {
	/// Report whether Studio has the bootstrap required by this Carbon build.
	Status {
		#[arg(long, value_name = "DIR")]
		studio_dir: Option<PathBuf>,
	},
	/// Install the stable Studio bootstrap; the versioned RML package stays isolated.
	Ensure {
		#[arg(long, value_name = "DIR")]
		studio_dir: Option<PathBuf>,
		#[arg(long, value_name = "DIR")]
		package: Option<PathBuf>,
	},
}

impl Rml {
	pub fn main(self) -> Result<()> {
		match self.command {
			Command::Status { studio_dir } => {
				let studio_dir = studio_dir.map_or_else(rml::latest_studio_dir, Ok)?;
				println!("{}: {:?}", studio_dir.display(), rml::status(&studio_dir));
				Ok(())
			}
			Command::Ensure { studio_dir, package } => {
				let changed = rml::ensure_current(studio_dir.as_deref(), package.as_deref())?;
				println!(
					"RobloxModLoader {} bootstrap is {}",
					rml::BUILD_VERSION,
					if changed { "installed" } else { "compatible" }
				);
				Ok(())
			}
		}
	}
}
