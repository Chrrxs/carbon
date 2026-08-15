use anyhow::{Context, Result};
use clap::{ArgGroup, Parser};
use std::path::PathBuf;

use crate::studio;

use super::studio_session;

/// Move one managed Roblox Studio back to its configured parking desktop.
#[derive(Parser)]
#[command(group(
	ArgGroup::new("target")
		.required(true)
		.multiple(false)
		.args(["instance_id", "port", "worktree"])
))]
pub struct Park {
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

impl Park {
	pub fn main(self) -> Result<()> {
		let (session, target) = studio_session::resolve(self.instance_id, self.port, self.worktree)?;
		let desktop_name = session
			.studio_desktop
			.as_deref()
			.and_then(studio::requested_virtual_desktop_name)
			.with_context(|| {
				format!(
					"the Carbon serve session for {target} has no configured parking desktop; restart it with studio_desktop configured"
				)
			})?
			.to_owned();
		let process = studio_session::process_identity(&session, "the parked Carbon serve session")?;
		let studio_pid = process.process_id;
		let placement = studio::StudioDesktopPlacement {
			process,
			desktop_name: desktop_name.clone(),
		};
		let _focus_lock = studio::acquire_focus_lock()?;
		let guard = studio::set_studio_parking_policy(&placement.process, studio::StudioParkingPolicy::Parked)
			.with_context(|| format!("failed to guard parked Studio for {target}"))?;
		let report = match studio::park_studio(&placement) {
			Ok(report) => report,
			Err(error) => {
				return match studio::set_studio_parking_policy(
					&placement.process,
					studio::StudioParkingPolicy::Active,
				) {
					Ok(_) => Err(error.context(format!(
						"failed to park the Studio process registered for {target}; parking guard rollback completed"
					))),
					Err(rollback_error) => Err(error.context(format!(
						"failed to park the Studio process registered for {target}; parking guard rollback also failed: {rollback_error:#}"
					))),
				};
			}
		};
		for warning in report.warnings {
			crate::carbon_warn!("{warning}");
		}
		log::debug!(
			"Parked Studio guard protected {} UI thread(s), matched {} audio session(s), and changed {} mute state(s)",
			guard.guarded_threads,
			guard.audio.matched_sessions,
			guard.audio.changed_sessions
		);
		crate::carbon_info!(
			"Parked Roblox Studio PID {studio_pid} for {target} on Windows desktop {desktop_name:?}, guarded its focus and audio, and cleared attention from {} window(s)",
			report.attention_windows,
		);
		Ok(())
	}
}
