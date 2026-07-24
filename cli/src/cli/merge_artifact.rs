use anyhow::{bail, ensure, Result};
use clap::Parser;
use std::path::PathBuf;

use crate::{
	artifact_resolution::{materialize_git_merge_inputs, CONFLICT_GUIDANCE},
	artifact_store,
	ext::PathExt,
};

/// Three-way semantic merge driver for a Carbon artifact.
#[derive(Parser)]
pub struct MergeArtifact {
	/// Common-ancestor artifact supplied by Git as %O.
	#[arg()]
	base: PathBuf,

	/// Current artifact supplied by Git as %A; replaced on success.
	#[arg()]
	current: PathBuf,

	/// Incoming artifact supplied by Git as %B.
	#[arg()]
	incoming: PathBuf,

	/// Canonical conflicted worktree path supplied by Git as %P.
	#[arg()]
	path: PathBuf,

	/// Emit semantic conflicts as JSON.
	#[arg(long)]
	json: bool,
}

impl MergeArtifact {
	pub fn main(self) -> Result<()> {
		let base = self.base.resolve()?;
		let current = self.current.resolve()?;
		let incoming = self.incoming.resolve()?;
		let path = self.path.resolve()?;
		for path in [&base, &current, &incoming] {
			ensure!(path.is_file(), "Carbon merge input does not exist: {}", path.display());
		}
		ensure!(
			path.is_file(),
			"Carbon worktree artifact does not exist: {}",
			path.display()
		);
		let inputs = materialize_git_merge_inputs(&base, &current, &incoming, &path)?;
		match artifact_store::merge_git_artifacts(&inputs.base, &inputs.current, &inputs.incoming, &current, &path)? {
			artifact_store::MergeOutcome::Merged(report) => {
				println!(
					"Merged {} instances and {} properties into {}",
					report.instances,
					report.properties,
					current.display()
				);
				Ok(())
			}
			artifact_store::MergeOutcome::Conflicted(conflicts) => {
				if self.json {
					eprintln!("{}", serde_json::to_string_pretty(&conflicts)?);
				} else {
					for conflict in &conflicts {
						eprintln!(
							"{} {}: current={}, incoming={}, base={}",
							conflict.identity, conflict.field, conflict.current, conflict.incoming, conflict.base
						);
					}
				}
				bail!(
					"{} semantic conflict(s); current artifact was not modified\n{}",
					conflicts.len(),
					CONFLICT_GUIDANCE
				)
			}
		}
	}
}
