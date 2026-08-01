use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use std::{
	path::PathBuf,
	sync::{
		atomic::{AtomicBool, Ordering},
		Arc,
	},
};

use crate::{ext::PathExt, project};

fn project_path(value: &str) -> std::result::Result<PathBuf, String> {
	let path = PathBuf::from(value);
	if project::is_project_path(&path) {
		Ok(path)
	} else {
		Err("capture target must be a .carbon.json project".to_owned())
	}
}

fn binary_place_path(value: &str) -> std::result::Result<PathBuf, String> {
	let path = PathBuf::from(value);
	if path.extension().is_some_and(|extension| extension == "rbxl") {
		Ok(path)
	} else {
		Err("manual capture requires a binary .rbxl place".to_owned())
	}
}

/// Import a manually saved binary Studio place directly into a Carbon project.
#[derive(Parser)]
pub struct Capture {
	/// Explicit `*.carbon.json` project to update.
	#[arg(value_parser = project_path)]
	project: PathBuf,

	/// Binary place saved manually from a Carbon-managed Studio place.
	#[arg(value_parser = binary_place_path)]
	place: PathBuf,
}

impl Capture {
	pub fn main(self) -> Result<()> {
		let project_path = self.project.resolve()?;
		let place = self.place.resolve()?;
		if !project_path.is_file() {
			bail!("Carbon project does not exist: {}", project_path.display());
		}
		if !place.is_file() {
			bail!("manual capture place does not exist: {}", place.display());
		}

		let cancelled = Arc::new(AtomicBool::new(false));
		let interrupt = Arc::clone(&cancelled);
		ctrlc::set_handler(move || interrupt.store(true, Ordering::Release))
			.context("failed to install Capture Manifest cancellation handler")?;
		crate::carbon_info!(
			"Capturing manually saved place {} into {}",
			place.display().to_string().bold(),
			project_path.display().to_string().bold()
		);
		let report = project::capture_saved_place(&project_path, &place, &|| cancelled.load(Ordering::Acquire))?;
		crate::carbon_info!(
			"Capture Manifest committed project generation {}",
			report.generation.bold()
		);
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn capture_requires_a_project_and_binary_place_without_a_serve_target() {
		assert!(Capture::try_parse_from(["capture", "game.carbon.json", "saved.rbxl"]).is_ok());
		assert!(Capture::try_parse_from(["capture", "game.json", "saved.rbxl"]).is_err());
		assert!(Capture::try_parse_from(["capture", "game.carbon.json", "saved.rbxm"]).is_err());
		assert!(Capture::try_parse_from(["capture", "saved.rbxl", "7"]).is_err());
		assert!(Capture::try_parse_from(["capture", "game.carbon.json", "saved.rbxl", "7"]).is_err());
		assert!(Capture::try_parse_from(["capture", "game.carbon.json", "saved.rbxl", "--port", "8123"]).is_err());
		assert!(Capture::try_parse_from(["capture"]).is_err());
	}
}
