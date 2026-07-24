use env_logger::WriteStyle;
use log::{debug, error, info, warn};
use puffin_http::Server;
use std::{
	env,
	io::{self, IsTerminal},
	mem::ManuallyDrop,
	process::ExitCode,
};

use carbon::{carbon_error, cli::Cli, config::Config, crash_handler, installer, logger};

const PROFILER_ADDRESS: &str = "localhost:8888";

fn main() -> ExitCode {
	crash_handler::hook();

	let config_kind = Config::load();
	let is_managed = installer::is_managed();
	let installation = installer::verify(is_managed);

	let cli = Cli::new();

	let yes = cli.yes();
	let backtrace = cli.backtrace();
	let verbosity = cli.verbosity();
	let log_style = cli.log_style();

	if log_style == WriteStyle::Auto && io::stdin().is_terminal() {
		env::set_var("RUST_LOG_STYLE", "always");
	} else {
		env::set_var(
			"RUST_LOG_STYLE",
			match log_style {
				WriteStyle::Always => "always",
				_ => "never",
			},
		)
	}

	env::set_var("RUST_VERBOSE", verbosity.as_str());
	env::set_var("RUST_YES", if yes { "1" } else { "0" });
	env::set_var("RUST_BACKTRACE", if backtrace { "1" } else { "0" });

	logger::init(verbosity, log_style);

	match config_kind {
		Ok(kind) => info!("{kind:?} config loaded"),
		Err(err) => error!("Failed to load config file: {err}"),
	}

	match installation {
		Ok(()) => info!("Carbon installation verified successfully!"),
		Err(err) => warn!("Failed to verify Carbon installation: {err}"),
	}

	if cfg!(debug_assertions) && cli.profile() {
		match Server::new(PROFILER_ADDRESS) {
			Ok(server) => {
				let _ = ManuallyDrop::new(server);

				info!("Profiler started at {PROFILER_ADDRESS}");
			}
			Err(err) => {
				error!("Failed to start profiler: {err}");
			}
		}

		puffin::set_scopes_on(true);
	}

	let exit_code = match cli.main() {
		Ok(()) => {
			debug!("Successfully executed command!");
			ExitCode::SUCCESS
		}
		Err(err) => {
			carbon_error!("{}", logger::render_error(&err));
			ExitCode::FAILURE
		}
	};

	exit_code
}
