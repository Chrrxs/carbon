use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

use crate::{carbon_info, config::Config, studio};

/// Launch a new Roblox Studio instance
#[derive(Parser)]
pub struct Studio {
	/// Path to place or model to open
	#[arg()]
	path: Option<PathBuf>,

	/// Check if Roblox Studio is already running
	#[arg(short, long)]
	check: bool,
}

impl Studio {
	pub fn main(self) -> Result<()> {
		if self.check && studio::is_running(None)? {
			carbon_info!("Roblox Studio is already running!");
			return Ok(());
		}

		carbon_info!("Launching Roblox Studio..");

		let studio_desktop = Config::new().studio_desktop.clone();
		studio::launch(self.path, &studio_desktop)?;

		Ok(())
	}
}
