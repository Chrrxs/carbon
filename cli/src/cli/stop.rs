use anyhow::{bail, Result};
use clap::Parser;
use colored::Colorize;
use reqwest::blocking::Client;

use crate::{carbon_info, carbon_warn, logger::Table, sessions, util};

/// Stop Carbon serve instances by address, instance ID, or all running instances.
#[derive(Parser)]
pub struct Stop {
	/// Instance identifiers reported by `carbon serve`.
	#[arg()]
	instances: Vec<String>,

	/// Server host name
	#[arg(short = 'H', long)]
	host: Option<String>,

	/// Server port
	#[arg(short = 'P', long)]
	port: Option<u16>,

	/// Stop all running session
	#[arg(short, long)]
	all: bool,

	/// List all running session
	#[arg(short, long)]
	list: bool,
}

impl Stop {
	pub fn main(self) -> Result<()> {
		if self.list {
			let sessions = sessions::get_all()?;

			if sessions.is_empty() {
				carbon_warn!("There are no running sessions");
				return Ok(());
			}

			let mut table = Table::new();
			table.set_header(vec!["ID", "Host", "Port", "PID"]);

			for (id, session) in sessions {
				table.add_row(vec![
					id,
					session.host.unwrap_or("None".into()),
					session.port.map(|p| p.to_string()).unwrap_or("None".into()),
					session.pid.to_string(),
				]);
			}

			carbon_info!("All running sessions:\n\n{}", table);

			return Ok(());
		}

		if self.all {
			let sessions = sessions::get_all()?;

			if sessions.is_empty() {
				carbon_warn!("There are no running sessions");
				return Ok(());
			}

			for session in sessions.values() {
				if let Some(address) = session.get_address() {
					Self::make_request(&address, session.pid)?;
				} else {
					Self::kill_process(session.pid);
				}
			}

			return sessions::remove_matching(&sessions);
		}

		if self.instances.is_empty() {
			if let Some(session) = sessions::get(None, self.host, self.port)? {
				if let Some(address) = session.get_address() {
					Self::make_request(&address, session.pid)?;
				} else {
					Self::kill_process(session.pid);
				}

				sessions::remove(&session)?;
			} else {
				carbon_warn!("There is no matching session to stop");
			}
		} else {
			let sessions = sessions::get_multiple(&self.instances)?;

			if sessions.is_empty() {
				carbon_warn!("There are no running sessions with provided IDs");
			} else {
				for session in sessions.values() {
					if let Some(address) = session.get_address() {
						Self::make_request(&address, session.pid)?;
					} else {
						Self::kill_process(session.pid);
					}
				}

				sessions::remove_matching(&sessions)?;
			}
		}

		Ok(())
	}

	fn make_request(address: &str, pid: u32) -> Result<()> {
		let url = format!("{address}/stop");

		match Client::new().post(url).send() {
			Ok(response) => {
				let status = response.status();
				let message = response
					.text()
					.unwrap_or_else(|_| "Carbon stop returned no readable result".to_owned());
				if !status.is_success() {
					bail!("Carbon stop failed ({status}): {message}");
				}
				carbon_info!("{} ({})", message, address.bold());
				Ok(())
			}
			Err(_) => {
				Self::kill_process(pid);
				Ok(())
			}
		}
	}

	fn kill_process(pid: u32) {
		util::kill_process(pid);
		carbon_info!("Stopped Carbon process with PID: {}", pid.to_string().bold())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn stop_accepts_reported_instance_ids() {
		let parsed = Stop::try_parse_from(["stop", "7", "12"]).unwrap();
		assert_eq!(parsed.instances, ["7", "12"]);
	}
}
