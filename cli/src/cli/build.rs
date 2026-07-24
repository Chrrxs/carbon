use anyhow::{bail, Result};
use clap::Parser;
use colored::Colorize;
use roblox_install::RobloxStudio;
use std::{
	fs,
	path::{Path, PathBuf},
};

use crate::{artifact_store, carbon_info, ext::PathExt, project, source};

/// Build canonical Carbon artifact into a Roblox place or model.
#[derive(Parser)]
pub struct Build {
	/// Explicit `*.carbon.json` project, or a directory containing exactly one.
	#[arg()]
	source: Option<PathBuf>,

	/// Output .rbxl/.rbxm path or existing directory.
	#[arg(short, long)]
	output: Option<PathBuf>,

	/// Build a model directly into the Studio plugins directory.
	#[arg(short, long)]
	plugin: bool,

	/// Managed worktree endpoint embedded into Workspace.
	#[arg(long, requires_all = ["worktree_project", "worktree_id", "session_token"])]
	worktree_endpoint: Option<String>,

	/// Exact managed project name expected from the worktree server.
	#[arg(long, requires_all = ["worktree_endpoint", "worktree_id", "session_token"])]
	worktree_project: Option<String>,

	/// Stable worktree identity embedded into Workspace.
	#[arg(long, requires_all = ["worktree_endpoint", "worktree_project", "session_token"])]
	worktree_id: Option<String>,

	/// Random launch token embedded into Workspace.
	#[arg(long, requires_all = ["worktree_endpoint", "worktree_project", "worktree_id"])]
	session_token: Option<String>,
}

impl Build {
	pub fn main(self) -> Result<()> {
		let manifest_path = source::resolve(self.source.unwrap_or_default())?;
		let inspection = project::inspect(&manifest_path)?;
		let default_extension = output_extension(inspection.is_place());
		let default_file = PathBuf::from(format!("{}.{}", inspection.name, default_extension));

		let output = if self.plugin {
			if inspection.is_place() {
				bail!("cannot build a place source as a Studio plugin");
			}
			RobloxStudio::locate()?.plugins_path().join(&default_file)
		} else if let Some(output) = self.output {
			if output.is_dir() {
				output.join(&default_file)
			} else {
				let extension = output.get_ext();
				if extension != "rbxl" && extension != "rbxm" {
					bail!("invalid output extension {extension}; expected rbxl or rbxm");
				}
				validate_output_kind(&output, inspection.is_place())?;
				if let Some(parent) = output.parent() {
					fs::create_dir_all(parent)?;
				}
				output
			}
		} else {
			default_file
		}
		.resolve()?;

		let contract = self.worktree_endpoint.map(|endpoint| artifact_store::WorktreeContract {
			endpoint,
			project: self.worktree_project.expect("required by clap"),
			worktree_id: self.worktree_id.expect("required by clap"),
			session_token: self.session_token.expect("required by clap"),
			identity_exclusions: Default::default(),
		});
		let report = project::compile(&manifest_path, &output, contract.as_ref())?;
		carbon_info!(
			"Built {} instances and {} properties from {} artifact to {}",
			report.instances,
			report.properties,
			report.artifacts,
			output.to_string().bold()
		);
		Ok(())
	}
}

fn output_extension(is_place: bool) -> &'static str {
	if is_place {
		"rbxl"
	} else {
		"rbxm"
	}
}

fn validate_output_kind(path: &Path, is_place: bool) -> Result<()> {
	let extension = path.get_ext();
	if extension.starts_with("rbxm") && is_place {
		bail!("cannot build a place source as a model");
	}
	if extension.starts_with("rbxl") && !is_place {
		bail!("cannot build a model source as a place");
	}
	Ok(())
}
