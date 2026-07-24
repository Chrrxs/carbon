use anyhow::Result;
use std::{
	env,
	path::PathBuf,
	process::{Child, Command},
};

use crate::{ext::WriteStyleExt, util};

pub enum ProgramName {
	Carbon,
}

pub struct Program {
	args: Vec<String>,
}

impl Program {
	pub fn new(_program: ProgramName) -> Self {
		Self { args: Vec::new() }
	}

	pub fn arg<S: Into<String>>(&mut self, arg: S) -> &mut Self {
		let arg = arg.into();
		if !arg.is_empty() {
			self.args.push(arg);
		}
		self
	}

	pub fn args<I, S>(&mut self, args: I) -> &mut Self
	where
		I: IntoIterator<Item = S>,
		S: Into<String>,
	{
		for arg in args {
			self.arg(arg);
		}
		self
	}

	pub fn spawn(&mut self) -> Result<Option<Child>> {
		let mut command = Command::new(env::current_exe().unwrap_or_else(|_| PathBuf::from("carbon")));
		command
			.args(&self.args)
			.arg("--carbon-spawn")
			.env("RUST_VERBOSE", util::env_verbosity().as_str())
			.env("RUST_LOG_STYLE", util::env_log_style().to_string())
			.env("RUST_BACKTRACE", if util::env_backtrace() { "1" } else { "0" })
			.env("RUST_YES", if util::env_yes() { "1" } else { "0" });
		Ok(Some(command.spawn()?))
	}
}
