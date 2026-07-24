use anyhow::{bail, Context, Result};
use clap::{ArgGroup, Parser};
use colored::Colorize;
use reqwest::blocking::Client;
use std::{process, thread, time::Duration};

use crate::{core::ManifestCaptureStatus, sessions};

const CAPTURE_HOST: &str = "127.0.0.1";

/// Ask an already-running serve session to capture its connected Studio place.
#[derive(Parser)]
#[command(group(
	ArgGroup::new("target")
		.required(true)
		.multiple(false)
		.args(["instance_id", "port"])
))]
pub struct Capture {
	/// Identifier reported by `carbon serve`.
	#[arg()]
	instance_id: Option<String>,

	/// Port of the existing loopback `carbon serve` endpoint.
	#[arg(short = 'P', long)]
	port: Option<u16>,

	/// Bypass exact no-op reuse and rebuild the complete captured artifact.
	#[arg(long)]
	force_full: bool,
}

impl Capture {
	pub fn main(self) -> Result<()> {
		let endpoint = match (self.instance_id, self.port) {
			(Some(instance_id), None) => sessions::get(Some(instance_id.clone()), None, None)?
				.with_context(|| format!("no running Carbon serve instance has ID {instance_id}"))?
				.get_address()
				.with_context(|| format!("Carbon serve instance {instance_id} does not have an address"))?,
			(None, Some(port)) => format!("http://{CAPTURE_HOST}:{port}"),
			_ => unreachable!("clap requires exactly one capture target"),
		};
		let client = Client::builder()
			.connect_timeout(Duration::from_secs(2))
			.timeout(Duration::from_secs(5))
			.build()
			.context("failed to create Capture Manifest client")?;
		let response = client
			.post(format!("{endpoint}/capture/request?forceFull={}", self.force_full))
			.send()
			.with_context(|| format!("could not connect to a Carbon serve session on {endpoint}"))?;
		if !response.status().is_success() {
			let status = response.status();
			let message = response.text().unwrap_or_else(|_| "capture request failed".to_owned());
			bail!("Capture Manifest request was rejected ({status}): {message}");
		}
		let started: ManifestCaptureStatus = response
			.json()
			.context("serve returned an invalid Capture Manifest response")?;
		crate::carbon_info!(
			"Capture Manifest {} started for served generation {}",
			started.request_id.bold(),
			started.source_generation
		);
		let cancel_client = client.clone();
		let cancel_url = format!("{endpoint}/capture/cancel/{}", started.request_id);
		ctrlc::set_handler(move || {
			let result = cancel_client.post(&cancel_url).send();
			match result {
				Ok(response) if response.status().is_success() => {
					eprintln!("ERROR: Capture Manifest cancelled; the previous manifest remains active");
					process::exit(130);
				}
				Ok(response) => {
					let status = response.status();
					let message = response
						.text()
						.unwrap_or_else(|_| "capture cancellation was rejected".to_owned());
					eprintln!("ERROR: Capture Manifest cancellation failed ({status}): {message}");
					process::exit(1);
				}
				Err(error) => {
					eprintln!("ERROR: Capture Manifest cancellation failed: {error}");
					process::exit(1);
				}
			}
		})
		.context("failed to install Capture Manifest cancellation handler")?;

		loop {
			let response = client
				.get(format!("{endpoint}/capture/status/{}", started.request_id))
				.send()
				.context("lost the serve endpoint while waiting for Capture Manifest")?;
			if !response.status().is_success() {
				let status = response.status();
				let message = response.text().unwrap_or_else(|_| "status request failed".to_owned());
				bail!("Capture Manifest status failed ({status}): {message}");
			}
			let status: ManifestCaptureStatus = response
				.json()
				.context("serve returned an invalid Capture Manifest status")?;
			match status.terminal_result()? {
				Some(message) => {
					crate::carbon_info!("Capture Manifest completed: {}", message.bold());
					return Ok(());
				}
				None => thread::sleep(Duration::from_millis(100)),
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn capture_accepts_an_instance_id_or_port() {
		assert!(Capture::try_parse_from(["capture", "7"]).is_ok());
		assert!(Capture::try_parse_from(["capture", "--port", "8123"]).is_ok());
		assert!(Capture::try_parse_from(["capture", "--port", "8123", "--force-full"]).is_ok());
		assert!(Capture::try_parse_from(["capture"]).is_err());
		assert!(Capture::try_parse_from(["capture", "7", "--port", "8123"]).is_err());
	}

	#[test]
	fn committed_but_restart_required_status_is_a_cli_error() {
		let error = ManifestCaptureStatus {
			request_id: "capture".to_owned(),
			state: "failed".to_owned(),
			source_generation: "committed-generation".to_owned(),
			message: Some(
				"Manifest capture committed atomically, but post-commit RML finalization failed; hard restart serve"
					.to_owned(),
			),
		}
		.terminal_result()
		.unwrap_err()
		.to_string();
		assert!(error.contains("Capture Manifest failed"));
		assert!(error.contains("committed atomically"));
		assert!(error.contains("hard restart serve"));
	}
}
