use anyhow::{Context, Result};
use clap::{ArgGroup, Parser};
use std::path::PathBuf;

use crate::{sessions, studio, studio_desktop};

use super::studio_session;

#[derive(Debug, PartialEq, Eq)]
struct DesktopRoutingPlan {
	target: studio::StudioProcessIdentity,
	peers: Vec<studio::StudioDesktopPlacement>,
	warnings: Vec<String>,
}

fn session_description(session: &sessions::Session) -> String {
	if let Some(worktree) = &session.worktree {
		return format!("worktree {}", worktree.display());
	}
	if let Some(port) = session.port {
		return format!("serve session on port {port}");
	}
	format!("serve process PID {}", session.pid)
}

fn desktop_routing_plan(
	target: &sessions::Session,
	peers: Vec<sessions::Session>,
) -> Result<Option<DesktopRoutingPlan>> {
	if target
		.studio_desktop
		.as_deref()
		.and_then(studio::requested_virtual_desktop_name)
		.is_none()
	{
		return Ok(None);
	}
	let target_identity = studio_session::process_identity(target, "the focused Carbon serve session")?;
	let mut placements = Vec::new();
	let mut warnings = Vec::new();
	for peer in peers {
		let description = session_description(&peer);
		let desktop_name = peer
			.studio_desktop
			.as_deref()
			.and_then(studio::requested_virtual_desktop_name)
			.map(str::to_owned);
		let identity = studio_session::process_identity(&peer, &description);
		match (identity, desktop_name) {
			(Ok(process), Some(desktop_name)) => placements.push(studio::StudioDesktopPlacement {
				process,
				desktop_name,
			}),
			(Err(error), _) => warnings.push(format!("Did not park sibling Studio for {description}: {error:#}")),
			(Ok(_), None) => warnings.push(format!(
				"Did not park sibling Studio for {description}: its launch-time studio_desktop is unavailable; restart that serve session"
			)),
		}
	}
	Ok(Some(DesktopRoutingPlan {
		target: target_identity,
		peers: placements,
		warnings,
	}))
}

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

	/// Restore the previously foreground window after activating Studio.
	#[arg(long)]
	restore: bool,
}

impl Focus {
	pub fn main(self) -> Result<()> {
		let restore = self.restore;
		let (session, target) = studio_session::resolve(self.instance_id, self.port, self.worktree)?;

		let studio_pid = session.studio_pid.with_context(|| {
			format!(
				"the Carbon serve session for {target} does not record a managed Studio PID; restart that serve session with this Carbon version"
			)
		})?;
		let _focus_lock = studio::acquire_focus_lock()?;
		let routing = if session
			.studio_desktop
			.as_deref()
			.and_then(studio::requested_virtual_desktop_name)
			.is_some()
		{
			let peers = sessions::get_repository_peers(&session)?;
			desktop_routing_plan(&session, peers)?
		} else {
			None
		};
		let report = match &routing {
			Some(plan) => {
				for warning in &plan.warnings {
					crate::carbon_warn!("{warning}");
				}
				studio_desktop::focus(
					&plan.target,
					studio_desktop::FocusRouting::Desktops { peers: &plan.peers },
					restore,
				)
			}
			None => {
				let identity = studio_session::process_identity(&session, "the focused Carbon serve session")?;
				studio_desktop::focus(&identity, studio_desktop::FocusRouting::None, restore)
			}
		}
		.with_context(|| format!("failed to focus the Studio process registered for {target}"))?;
		for warning in &report.warnings {
			crate::carbon_warn!("{warning}");
		}
		log::debug!(
			"Focus restored {} audio session(s) with {} mute change(s); sibling guards matched {} session(s), changed {} mute state(s), protected {} UI thread(s), and cleared attention from {} then {} window(s)",
			report.target_audio_sessions,
			report.target_audio_changes,
			report.peer_audio_sessions,
			report.peer_audio_changes,
			report.peer_guarded_threads,
			report.attention_windows,
			report.post_focus_attention_windows
		);
		let siblings = match report.parked {
			0 => String::new(),
			parked => format!(" and parked {parked} sibling Studio(s)"),
		};
		if restore {
			crate::carbon_info!(
				"Activated Roblox Studio PID {studio_pid} for {target}{siblings}, then restored the previous window"
			);
		} else {
			crate::carbon_info!("Focused Roblox Studio PID {studio_pid} for {target}{siblings}");
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::PathBuf;

	fn session(studio_pid: u32, worktree: &str, desktop: Option<&str>) -> sessions::Session {
		sessions::Session {
			pid: studio_pid + 1_000,
			host: Some("127.0.0.1".to_owned()),
			port: Some(8_000 + studio_pid as u16),
			studio_pid: Some(studio_pid),
			worktree: Some(PathBuf::from(worktree)),
			git_common_dir: Some(PathBuf::from("/tmp/game/.git")),
			studio_desktop: desktop.map(str::to_owned),
			studio_executable: Some(r"C:\Roblox\RobloxStudioBeta.exe".to_owned()),
			creation_filetime: Some(133_700_000_000 + u64::from(studio_pid)),
			launch_id: Some(format!("launch-{studio_pid}")),
		}
	}

	#[test]
	fn restoring_the_previous_window_is_opt_in() {
		let focused = Focus::try_parse_from(["focus", "session-id"]).unwrap();
		assert!(!focused.restore);

		let restored = Focus::try_parse_from(["focus", "--restore", "session-id"]).unwrap();
		assert!(restored.restore);
	}

	#[test]
	fn desktop_routing_uses_each_siblings_recorded_parking_desktop() {
		let target = session(101, "/tmp/game-main", Some("Studios"));
		let sibling = session(102, "/tmp/game-feature", Some("Feature Studios"));
		let mut legacy = session(103, "/tmp/game-legacy", None);
		legacy.studio_executable = None;

		let plan = desktop_routing_plan(&target, vec![sibling, legacy]).unwrap().unwrap();

		assert_eq!(plan.target.process_id, 101);
		assert_eq!(plan.peers.len(), 1);
		assert_eq!(plan.peers[0].process.process_id, 102);
		assert_eq!(plan.peers[0].desktop_name, "Feature Studios");
		assert_eq!(plan.warnings.len(), 1);
		assert!(plan.warnings[0].contains("game-legacy"));
	}

	#[test]
	fn sessions_without_a_parking_desktop_keep_focus_only_behavior() {
		let mut target = session(101, "/tmp/game-main", None);
		target.studio_executable = None;
		target.creation_filetime = None;

		assert_eq!(desktop_routing_plan(&target, Vec::new()).unwrap(), None);
	}
}
