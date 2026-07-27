use anyhow::{Context, Result};
use clap::{ArgGroup, Parser};
use std::path::PathBuf;

use crate::{sessions, studio};

/// Focus the exact Roblox Studio window managed by a running serve session.
#[derive(Parser)]
#[command(group(
	ArgGroup::new("target")
		.required(true)
		.multiple(false)
		.args(["instance_id", "port", "worktree"])
))]
pub struct Focus {
	/// Identifier reported by `carbon serve`.
	#[arg()]
	instance_id: Option<String>,

	/// Port of the existing loopback `carbon serve` endpoint.
	#[arg(short = 'P', long)]
	port: Option<u16>,

	/// Any path inside the Git worktree served by the Studio instance.
	#[arg(long, value_name = "PATH")]
	worktree: Option<PathBuf>,
}

impl Focus {
	pub fn main(self) -> Result<()> {
		let (session, target) = match (self.instance_id, self.port, self.worktree) {
			(Some(instance_id), None, None) => {
				let session = sessions::get(Some(instance_id.clone()), None, None)?
					.with_context(|| format!("no running Carbon serve instance has ID {instance_id}"))?;
				(session, format!("instance ID {instance_id}"))
			}
			(None, Some(port), None) => {
				let session = sessions::get(None, None, Some(port))?
					.with_context(|| format!("no running Carbon serve instance uses port {port}"))?;
				(session, format!("port {port}"))
			}
			(None, None, Some(worktree)) => {
				let session = sessions::get_by_worktree(&worktree)?.with_context(|| {
					format!(
						"no running Carbon serve instance is registered for worktree {}",
						worktree.display()
					)
				})?;
				(session, format!("worktree {}", worktree.display()))
			}
			_ => unreachable!("clap requires exactly one focus target"),
		};

		let studio_pid = session.studio_pid.with_context(|| {
			format!(
				"the Carbon serve session for {target} does not record a managed Studio PID; restart that serve session with this Carbon version"
			)
		})?;
		studio::focus_process(
			studio_pid,
			session.creation_filetime,
			session.studio_executable.as_deref(),
		)
		.with_context(|| format!("failed to focus the Studio process registered for {target}"))?;
		crate::carbon_info!("Activated Roblox Studio PID {studio_pid} for {target} and restored the previous window");
		Ok(())
	}
}
