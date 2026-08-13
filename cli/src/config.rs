use anyhow::Result;
use colored::Colorize;
use config_derive::{Get, Iter, Set, Val};
use documented::DocumentedFields;
use lazy_static::lazy_static;
use log::{debug, info};
use optfield::optfield;
use serde::{ser::SerializeMap, Deserialize, Serialize, Serializer};
use std::{
	env,
	fmt::{self, Debug, Display, Formatter},
	fs, mem,
	path::{Path, PathBuf},
	sync::{RwLock, RwLockReadGuard, RwLockWriteGuard},
};
use toml;

use crate::{carbon_error, logger::Table, util};

lazy_static! {
	static ref CONFIG: RwLock<Config> = RwLock::new(Config::default());
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub enum ConfigKind {
	#[default]
	Default,
	Global(PathBuf),
	Workspace(PathBuf),
}

#[optfield(OptConfig, merge_fn, attrs = (derive(Deserialize)))]
#[derive(Debug, Clone, Deserialize, DocumentedFields, Val, Iter, Get, Set)]
pub struct Config {
	/// Default server host name
	pub host: String,
	/// Default server port number
	pub port: u16,
	/// Run Carbon asynchronously, freeing up the terminal
	pub run_async: bool,
	/// Scan for the first available port if selected one is in use
	pub scan_ports: bool,
	/// Parking desktop for Studio windows and automatic focus routing (empty disables)
	pub studio_desktop: String,

	/// Maximum number of unsynced changes before showing a warning
	pub max_unsynced_changes: usize,

	#[serde(skip)]
	/// Internal
	kind: ConfigKind,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			host: String::from("localhost"),
			port: 8000,
			run_async: false,
			scan_ports: true,
			studio_desktop: String::new(),

			max_unsynced_changes: 10,

			kind: ConfigKind::default(),
		}
	}
}

impl ConfigKind {
	pub fn path(&self) -> Option<&Path> {
		match self {
			Self::Default => None,
			Self::Global(path) | Self::Workspace(path) => Some(path),
		}
	}
}

impl Config {
	pub fn new() -> RwLockReadGuard<'static, Self> {
		CONFIG.read().unwrap()
	}

	pub fn new_mut() -> RwLockWriteGuard<'static, Self> {
		CONFIG.try_write().expect("Failed to acquire write lock on config")
	}

	pub fn load() -> Result<ConfigKind> {
		let mut config = Self::default();

		let config_kind = || -> Result<ConfigKind> {
			let workspace_config = env::current_dir()?.join("carbon.toml");
			let global_config = util::get_carbon_dir()?.join("config.toml");

			let kind = if workspace_config.exists() {
				ConfigKind::Workspace(workspace_config)
			} else if global_config.exists() {
				ConfigKind::Global(global_config)
			} else {
				ConfigKind::Default
			};

			if let Some(path) = kind.path() {
				config.merge_opt(toml::from_str(&fs::read_to_string(path)?)?);
			}

			config.kind = kind.clone();

			Ok(kind)
		}();

		*CONFIG.write().unwrap() = config;

		config_kind
	}

	pub fn load_virtual(kind: ConfigKind) -> Result<()> {
		let kind = match kind {
			ConfigKind::Default => ConfigKind::Global(util::get_carbon_dir()?.join("config.toml")),
			_ => kind,
		};

		if kind.path().unwrap().exists() {
			Self::load_specific(kind);
		} else {
			*CONFIG.write().unwrap() = Config {
				kind,
				..Default::default()
			};
		}

		Ok(())
	}

	pub fn load_workspace(path: &Path) {
		Self::load_specific(ConfigKind::Workspace(path.join("carbon.toml")))
	}

	#[inline]
	fn load_specific(kind: ConfigKind) {
		if mem::discriminant(&kind) == mem::discriminant(&CONFIG.read().unwrap().kind) {
			debug!("{kind} config file already loaded");
			return;
		}

		let path = kind.path().unwrap();

		if !path.exists() {
			debug!("{kind} config file not found");
			return;
		}

		let mut config = Self::default();

		let load_result = || -> Result<()> {
			config.merge_opt(toml::from_str(&fs::read_to_string(path)?)?);

			config.kind = match kind {
				ConfigKind::Global(_) => ConfigKind::Global(path.to_owned()),
				ConfigKind::Workspace(_) => ConfigKind::Workspace(path.to_owned()),
				_ => ConfigKind::Default,
			};

			*CONFIG.write().unwrap() = config;

			Ok(())
		}();

		match load_result {
			Ok(()) => info!("{kind} config file loaded"),
			Err(err) => {
				carbon_error!("Failed to load {} config file: {}", kind.to_string().bold(), err);
			}
		}
	}

	pub fn save(&self, path: &Path) -> Result<()> {
		fs::write(path, toml::to_string(self)?)?;

		Ok(())
	}

	pub fn has_setting(&self, setting: &str) -> bool {
		self.get(setting).is_some()
	}

	pub fn list(&self) -> Table {
		let defaults = Self::default();
		let mut table = Table::new();
		let defaults_only = self == &defaults;

		if defaults_only {
			table.set_header(vec!["Setting", "Default", "Description"]);
		} else {
			table.set_header(vec!["Setting", "Default", "Current", "Description"]);
		}

		for (setting, default) in &defaults {
			if let Ok(doc) = Self::get_field_docs(setting) {
				if defaults_only {
					table.add_row(vec![setting.to_owned(), default.to_string(), doc.trim().to_owned()]);
				} else {
					let default = default.to_string();
					let mut current = self.get(setting).map(|v| v.to_string()).unwrap();

					if current == default {
						current = String::new();
					}

					table.add_row(vec![setting.to_owned(), default, current, doc.trim().to_owned()]);
				}
			}
		}

		table
	}

	pub fn kind(&self) -> &ConfigKind {
		&self.kind
	}
}

impl Display for ConfigKind {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"{}",
			match self {
				Self::Default => "Default",
				Self::Global(_) => "Global",
				Self::Workspace(_) => "Workspace",
			}
		)
	}
}

impl PartialEq for Config {
	fn eq(&self, other: &Self) -> bool {
		for (k, v) in self {
			if other.get(k) != Some(v) {
				return false;
			}
		}

		true
	}
}

impl Serialize for Config {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		let mut map = serializer.serialize_map(None)?;
		let defaults = Self::default();

		for (k, v) in self {
			if v == defaults.get(k).unwrap() {
				continue;
			}

			map.serialize_entry(&k, &v)?;
		}

		map.end()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn workspace_config_accepts_a_named_studio_desktop() {
		let configured: OptConfig = toml::from_str("studio_desktop = \"Studios\"\n").unwrap();
		let mut config = Config::default();

		config.merge_opt(configured);

		assert_eq!(config.studio_desktop, "Studios");
		assert_eq!(
			config.get("studio_desktop").map(|value| value.to_string()),
			Some(String::from("Studios"))
		);
	}
}
