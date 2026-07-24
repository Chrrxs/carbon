use anyhow::Result;
use clap::{ColorChoice, Parser, Subcommand};
use clap_verbosity_flag::Verbosity;
use env_logger::fmt::WriteStyle;
use log::LevelFilter;
use std::env;

use crate::util;

mod build;
mod capture;
mod config;
mod conflicts;
mod diff;
mod focus;
mod init;
mod merge_artifact;
mod migrate;
mod resolve;
mod rml;
mod serve;
mod sourcemap;
mod stop;
mod studio;

macro_rules! about {
	() => {
		concat!("Carbon ", env!("CARBON_BUILD_VERSION"))
	};
}

macro_rules! long_about {
	() => {
		concat!(
			"Carbon ",
			env!("CARBON_BUILD_VERSION"),
			"\n",
			env!("CARGO_PKG_DESCRIPTION"),
			"\n",
			"Made with <3 by ",
			env!("CARGO_PKG_AUTHORS")
		)
	};
}

#[derive(Parser)]
#[clap(about = about!(), long_about = long_about!(), version = env!("CARBON_BUILD_VERSION"))]
pub struct Cli {
	#[command(subcommand)]
	command: Commands,

	#[command(flatten)]
	verbose: Verbosity,

	/// Automatically answer to any prompts
	#[arg(short, long, global = true)]
	yes: bool,

	/// Print full backtrace on panic
	#[arg(short = 'B', long, global = true)]
	backtrace: bool,

	#[arg(long, hide = true, global = true)]
	profile: bool,

	/// Output coloring: auto, always, never
	#[arg(
		long,
		short = 'C',
		global = true,
		value_name = "WHEN",
		default_value = "auto",
		hide_default_value = true,
		hide_possible_values = true
	)]
	pub color: ColorChoice,
}

impl Cli {
	pub fn new() -> Cli {
		Cli::parse()
	}

	pub fn profile(&self) -> bool {
		self.profile
	}

	pub fn yes(&self) -> bool {
		if env::var("RUST_YES").is_ok() {
			return util::env_yes();
		}

		self.yes
	}

	pub fn backtrace(&self) -> bool {
		if env::var("RUST_BACKTRACE").is_ok() {
			return util::env_backtrace();
		}

		self.backtrace
	}

	pub fn verbosity(&self) -> LevelFilter {
		if env::var("RUST_VERBOSE").is_ok() {
			return util::env_verbosity();
		}

		self.verbose.log_level_filter()
	}

	pub fn log_style(&self) -> WriteStyle {
		if env::var("RUST_LOG_STYLE").is_ok() {
			return util::env_log_style();
		}

		match self.color {
			ColorChoice::Always => WriteStyle::Always,
			ColorChoice::Never => WriteStyle::Never,
			_ => WriteStyle::Auto,
		}
	}

	pub fn main(self) -> Result<()> {
		match self.command {
			Commands::Init(command) => command.main(),
			Commands::Migrate(command) => command.main(),
			Commands::MergeArtifact(command) => command.main(),
			Commands::Conflicts(command) => command.main(),
			Commands::Resolve(command) => command.main(),
			Commands::Build(command) => command.main(),
			Commands::Capture(command) => command.main(),
			Commands::Serve(command) => command.main(),
			Commands::Stop(command) => command.main(),
			Commands::Studio(command) => command.main(),
			Commands::Diff(command) => command.main(),
			Commands::Focus(command) => command.main(),
			Commands::Sourcemap(command) => command.main(),
			Commands::Config(command) => command.main(),
			Commands::Rml(command) => command.main(),
		}
	}
}

#[derive(Subcommand)]
pub enum Commands {
	Init(init::Init),
	Migrate(migrate::Migrate),
	MergeArtifact(merge_artifact::MergeArtifact),
	Conflicts(conflicts::Conflicts),
	Resolve(resolve::Resolve),
	Build(build::Build),
	Capture(capture::Capture),
	Serve(serve::Serve),
	Stop(stop::Stop),
	Studio(studio::Studio),
	Diff(diff::Diff),
	Focus(focus::Focus),
	Sourcemap(sourcemap::Sourcemap),
	Config(config::Config),
	Rml(rml::Rml),
}

#[cfg(test)]
mod tests {
	use super::*;
	use clap::CommandFactory;

	#[test]
	fn public_cli_excludes_debug_exec_and_describes_rml() {
		let command = Cli::command();
		assert!(command.find_subcommand("debug").is_none());
		assert!(command.find_subcommand("exec").is_none());

		let rml = command.find_subcommand("rml").expect("rml command");
		assert_eq!(
			rml.get_about().map(|about| about.to_string()).as_deref(),
			Some("Inspect or install the RobloxModLoader bootstrap used by Carbon")
		);
	}

	#[test]
	fn serve_and_capture_are_separate_entrypoints() {
		let cli = Cli::try_parse_from(["carbon", "capture", "--port", "8000"]).unwrap();
		assert!(matches!(cli.command, Commands::Capture(_)));
		assert!(matches!(
			Cli::try_parse_from(["carbon", "serve", "game.carbon.json"])
				.unwrap()
				.command,
			Commands::Serve(_)
		));
		assert!(Cli::try_parse_from(["carbon", "extract"]).is_err());
		assert!(matches!(
			Cli::try_parse_from(["carbon", "merge-artifact", "base", "current", "other", "path"])
				.unwrap()
				.command,
			Commands::MergeArtifact(_)
		));
	}

	#[test]
	fn migrate_command_accepts_a_place_and_project_output() {
		assert!(matches!(
			Cli::try_parse_from(["carbon", "migrate", "existing.rbxl", "--output", "game.carbon.json",])
				.unwrap()
				.command,
			Commands::Migrate(_)
		));
	}

	#[test]
	fn agents_can_discover_and_apply_carbon_conflicts() {
		assert!(matches!(
			Cli::try_parse_from(["carbon", "conflicts", "--json"]).unwrap().command,
			Commands::Conflicts(_)
		));
		assert!(matches!(
			Cli::try_parse_from(["carbon", "resolve", "--plan", "decisions.json"])
				.unwrap()
				.command,
			Commands::Resolve(_)
		));
	}

	#[test]
	fn focus_command_requires_one_supported_target() {
		assert!(Cli::try_parse_from(["carbon", "focus", "anon:studio-a"]).is_ok());
		assert!(Cli::try_parse_from(["carbon", "focus", "--port", "8123"]).is_ok());
		assert!(Cli::try_parse_from(["carbon", "focus", "--worktree", "/tmp/carbon-worktree"]).is_ok());
		assert!(Cli::try_parse_from(["carbon", "focus"]).is_err());
		assert!(Cli::try_parse_from(["carbon", "focus", "anon:studio-a", "--port", "8123"]).is_err());
		assert!(Cli::try_parse_from([
			"carbon",
			"focus",
			"--port",
			"8123",
			"--worktree",
			"/tmp/carbon-worktree",
		])
		.is_err());
	}
}
