use anyhow::{bail, ensure, Context, Result};
#[cfg(not(target_os = "linux"))]
use roblox_install::RobloxStudio;
use std::fs::{File, OpenOptions};
#[cfg(target_os = "linux")]
use std::process::Command;
use std::{
	borrow::Cow,
	env, fs,
	io::Write,
	path::{Path, PathBuf},
};

const PLUGIN_FILE: &str = "Carbon.rbxm";
const PLUGINS_DIR_ENV: &str = "MCP_PLUGINS_DIR";
#[cfg(not(carbon_bundled_studio_plugin))]
const BUNDLE_ENV: &str = "CARBON_STUDIO_PLUGIN_BUNDLE";
#[cfg(carbon_bundled_studio_plugin)]
const EMBEDDED_PLUGIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/Carbon.rbxm"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallStatus {
	Current,
	Installed,
	Updated,
}

#[derive(Debug)]
pub(crate) struct Installation {
	pub status: InstallStatus,
	pub path: PathBuf,
	_launch_lock: File,
}

pub(crate) fn ensure_current() -> Result<Installation> {
	let bundle = bundled_plugin()?;
	let destination = plugins_directory()?.join(PLUGIN_FILE);
	install_bundled_plugin_for_launch(&bundle, &destination)
}

#[cfg(carbon_bundled_studio_plugin)]
fn bundled_plugin() -> Result<Cow<'static, [u8]>> {
	Ok(Cow::Borrowed(EMBEDDED_PLUGIN))
}

#[cfg(not(carbon_bundled_studio_plugin))]
fn bundled_plugin() -> Result<Cow<'static, [u8]>> {
	let explicit = env::var_os(BUNDLE_ENV).map(PathBuf::from);
	let adjacent = env::current_exe()
		.ok()
		.and_then(|executable| executable.parent().map(|parent| parent.join(PLUGIN_FILE)));
	let source = explicit
		.into_iter()
		.chain(adjacent)
		.find(|candidate| candidate.is_file())
		.with_context(|| {
			format!(
				"this development Carbon build has no bundled {PLUGIN_FILE}; set {BUNDLE_ENV} or place {PLUGIN_FILE} beside the executable"
			)
		})?;
	let bytes = fs::read(&source)
		.with_context(|| format!("failed to read Carbon Studio plugin bundle {}", source.display()))?;
	ensure!(
		!bytes.is_empty(),
		"Carbon Studio plugin bundle is empty: {}",
		source.display()
	);
	Ok(Cow::Owned(bytes))
}

fn plugins_directory() -> Result<PathBuf> {
	if let Some(path) = env::var_os(PLUGINS_DIR_ENV).filter(|path| !path.is_empty()) {
		return Ok(PathBuf::from(path));
	}

	#[cfg(target_os = "linux")]
	{
		ensure!(
			env::var_os("WSL_DISTRO_NAME").is_some(),
			"automatic Carbon Studio plugin installation on Linux requires WSL or {PLUGINS_DIR_ENV}"
		);
		let local_app_data = crate::studio::powershell_command()?
			.args([
				"-NoProfile",
				"-NonInteractive",
				"-Command",
				r#"[Environment]::GetFolderPath("LocalApplicationData")"#,
			])
			.output()
			.context("failed to resolve Windows LOCALAPPDATA for the Studio plugin")?;
		ensure!(
			local_app_data.status.success(),
			"PowerShell could not resolve Windows LOCALAPPDATA for the Studio plugin"
		);
		let windows_path = String::from_utf8(local_app_data.stdout)?.trim().to_owned();
		ensure!(!windows_path.is_empty(), "Windows LOCALAPPDATA is empty");
		let translated = Command::new("wslpath")
			.args(["-u", windows_path.as_str()])
			.output()
			.context("failed to translate the Studio plugins directory into WSL")?;
		ensure!(
			translated.status.success(),
			"wslpath could not translate the Studio plugins directory"
		);
		Ok(PathBuf::from(String::from_utf8(translated.stdout)?.trim()).join("Roblox/Plugins"))
	}

	#[cfg(not(target_os = "linux"))]
	{
		Ok(RobloxStudio::locate()?.plugins_path().to_owned())
	}
}

fn install_bundled_plugin(bundle: &[u8], destination: &Path) -> Result<InstallStatus> {
	ensure!(
		!bundle.is_empty(),
		"refusing to install an empty Carbon Studio plugin bundle"
	);
	let current = match fs::read(destination) {
		Ok(current) => Some(current),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
		Err(error) => {
			return Err(error)
				.with_context(|| format!("failed to inspect installed Studio plugin {}", destination.display()))
		}
	};
	if current.as_deref() == Some(bundle) {
		return Ok(InstallStatus::Current);
	}

	let parent = destination
		.parent()
		.with_context(|| format!("Studio plugin destination has no parent: {}", destination.display()))?;
	fs::create_dir_all(parent)
		.with_context(|| format!("failed to create Studio plugins directory {}", parent.display()))?;
	let status = if current.is_some() {
		InstallStatus::Updated
	} else {
		InstallStatus::Installed
	};
	let temporary = parent.join(format!(".{PLUGIN_FILE}-{}.tmp", uuid::Uuid::new_v4().simple()));
	let install = (|| -> Result<()> {
		let mut output = OpenOptions::new()
			.create_new(true)
			.write(true)
			.open(&temporary)
			.with_context(|| format!("failed to stage Studio plugin {}", temporary.display()))?;
		output
			.write_all(bundle)
			.with_context(|| format!("failed to write staged Studio plugin {}", temporary.display()))?;
		output
			.sync_all()
			.with_context(|| format!("failed to flush staged Studio plugin {}", temporary.display()))?;
		drop(output);
		crate::artifact_store::install_artifact_file(&temporary, destination)
			.with_context(|| format!("failed to install Studio plugin {}", destination.display()))?;
		Ok(())
	})();
	let _ = fs::remove_file(&temporary);
	install?;
	let installed = fs::read(destination)
		.with_context(|| format!("failed to verify installed Studio plugin {}", destination.display()))?;
	if installed != bundle {
		bail!("installed Studio plugin did not match the Carbon bundle");
	}
	Ok(status)
}

fn install_bundled_plugin_for_launch(bundle: &[u8], destination: &Path) -> Result<Installation> {
	ensure!(
		!bundle.is_empty(),
		"refusing to install an empty Carbon Studio plugin bundle"
	);
	let parent = destination
		.parent()
		.with_context(|| format!("Studio plugin destination has no parent: {}", destination.display()))?;
	fs::create_dir_all(parent)
		.with_context(|| format!("failed to create Studio plugins directory {}", parent.display()))?;
	let lock_path = parent.join(format!(".{PLUGIN_FILE}.lock"));
	let lock = OpenOptions::new()
		.create(true)
		.truncate(false)
		.read(true)
		.write(true)
		.open(&lock_path)
		.with_context(|| format!("failed to open Studio plugin launch lock {}", lock_path.display()))?;
	let mut installed_status = None;

	loop {
		lock.lock_shared()
			.with_context(|| format!("failed to lock Studio plugin {} for launch", destination.display()))?;
		if fs::read(destination).ok().as_deref() == Some(bundle) {
			return Ok(Installation {
				status: installed_status.unwrap_or(InstallStatus::Current),
				path: destination.to_owned(),
				_launch_lock: lock,
			});
		}
		lock.unlock()
			.with_context(|| format!("failed to unlock Studio plugin {}", destination.display()))?;
		lock.lock()
			.with_context(|| format!("failed to lock Studio plugin {} for update", destination.display()))?;
		if fs::read(destination).ok().as_deref() != Some(bundle) {
			installed_status = Some(install_bundled_plugin(bundle, destination)?);
		}
		lock.unlock()
			.with_context(|| format!("failed to unlock updated Studio plugin {}", destination.display()))?;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::{env, fs, path::PathBuf, sync::mpsc, thread, time::Duration};
	use uuid::Uuid;

	fn fixture(name: &str) -> PathBuf {
		env::temp_dir().join(format!("carbon-studio-plugin-{name}-{}", Uuid::new_v4().simple()))
	}

	#[test]
	fn missing_plugin_is_installed_from_the_bundle() {
		let root = fixture("missing");
		let destination = root.join("Plugins/Carbon.rbxm");

		let result = install_bundled_plugin(b"matching plugin", &destination).unwrap();

		assert_eq!(result, InstallStatus::Installed);
		assert_eq!(fs::read(&destination).unwrap(), b"matching plugin");
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn different_plugin_is_replaced_by_the_bundle() {
		let root = fixture("different");
		let destination = root.join("Plugins/Carbon.rbxm");
		fs::create_dir_all(destination.parent().unwrap()).unwrap();
		fs::write(&destination, b"stale plugin").unwrap();

		let result = install_bundled_plugin(b"matching plugin", &destination).unwrap();

		assert_eq!(result, InstallStatus::Updated);
		assert_eq!(fs::read(&destination).unwrap(), b"matching plugin");
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn matching_plugin_is_not_rewritten() {
		let root = fixture("matching");
		let destination = root.join("Plugins/Carbon.rbxm");
		fs::create_dir_all(destination.parent().unwrap()).unwrap();
		fs::write(&destination, b"matching plugin").unwrap();

		let result = install_bundled_plugin(b"matching plugin", &destination).unwrap();

		assert_eq!(result, InstallStatus::Current);
		assert_eq!(fs::read(&destination).unwrap(), b"matching plugin");
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn parallel_serve_holds_each_plugin_version_stable_until_launch_finishes() {
		let root = fixture("parallel-launches");
		let destination = root.join("Plugins/Carbon.rbxm");
		let first = install_bundled_plugin_for_launch(b"first worktree plugin", &destination).unwrap();
		let same = install_bundled_plugin_for_launch(b"first worktree plugin", &destination).unwrap();
		let (sender, receiver) = mpsc::channel();
		let other_destination = destination.clone();
		let other = thread::spawn(move || {
			let installation =
				install_bundled_plugin_for_launch(b"second worktree plugin", &other_destination).unwrap();
			sender.send(installation).unwrap();
		});

		assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
		assert_eq!(fs::read(&destination).unwrap(), b"first worktree plugin");
		drop(first);
		drop(same);
		let second = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
		assert_eq!(fs::read(&destination).unwrap(), b"second worktree plugin");
		drop(second);
		other.join().unwrap();
		fs::remove_dir_all(root).unwrap();
	}
}
