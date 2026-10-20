use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::{sessions, studio};

pub(super) fn resolve(
	instance_id: Option<String>,
	port: Option<u16>,
	worktree: Option<PathBuf>,
) -> Result<(sessions::Session, String)> {
	match (instance_id, port, worktree) {
		(Some(instance_id), None, None) => {
			let Some(session) = sessions::get(Some(instance_id.clone()), None, None)? else {
				anyhow::bail!(missing_instance_message(&instance_id));
			};
			Ok((session, format!("instance ID {instance_id}")))
		}
		(None, Some(port), None) => {
			let session = sessions::get(None, None, Some(port))?
				.with_context(|| format!("no running Carbon serve instance uses port {port}"))?;
			Ok((session, format!("port {port}")))
		}
		(None, None, Some(worktree)) => {
			let session = sessions::get_by_worktree(&worktree)?.with_context(|| {
				format!(
					"no running Carbon serve instance is registered for worktree {}",
					worktree.display()
				)
			})?;
			Ok((session, format!("worktree {}", worktree.display())))
		}
		_ => unreachable!("clap requires exactly one managed Studio target"),
	}
}

/// A bare number is more likely a port than an instance ID; say so.
fn missing_instance_message(instance_id: &str) -> String {
	match instance_id.parse::<u16>() {
		Ok(port) => format!(
			"no running Carbon serve instance has ID {instance_id}; to select the serve session on port {port}, pass `--port {port}`"
		),
		Err(_) => format!("no running Carbon serve instance has ID {instance_id}"),
	}
}

pub(super) fn process_identity(
	session: &sessions::Session,
	description: &str,
) -> Result<studio::StudioProcessIdentity> {
	Ok(studio::StudioProcessIdentity {
		process_id: session
			.studio_pid
			.with_context(|| format!("{description} does not record a managed Studio PID"))?,
		studio_executable: session
			.studio_executable
			.clone()
			.with_context(|| format!("{description} does not record the Studio executable identity"))?,
		creation_filetime: session
			.creation_filetime
			.with_context(|| format!("{description} does not record the Studio process creation time"))?,
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn numeric_instance_ids_suggest_the_port_flag() {
		assert!(missing_instance_message("8002").contains("pass `--port 8002`"));
		assert!(!missing_instance_message("anon:studio-a").contains("--port"));
		assert!(!missing_instance_message("99999").contains("--port"));
	}
}
