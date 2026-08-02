use anyhow::{ensure, Context, Result};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use std::{
	path::{Path, PathBuf},
	process::{Command, Stdio},
	sync::{Arc, Mutex},
};

use crate::studio_plugin;

#[cfg(target_os = "windows")]
use winsafe::{co::SW, EnumWindows};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FocusMetadata {
	pub studio_executable: String,
	pub creation_filetime: u64,
}

#[derive(Debug, Clone)]
pub struct StudioInfo {
	pub executable: PathBuf,
	pub version_text: String,
	pub version_components: [u32; 4],
	pub build_id: String,
}

#[derive(Clone)]
pub struct ManagedStudio {
	process_id: u32,
	data_model_name: String,
	studio_executable: String,
	creation_filetime: u64,
	startup_guard: Arc<Mutex<Option<studio_plugin::Installation>>>,
}

impl ManagedStudio {
	pub fn process_id(&self) -> u32 {
		self.process_id
	}

	pub fn owner(&self) -> &'static str {
		"Carbon"
	}

	pub fn focus_metadata(&self) -> Option<FocusMetadata> {
		Some(FocusMetadata {
			studio_executable: self.studio_executable.clone(),
			creation_filetime: self.creation_filetime,
		})
	}

	pub fn finish_startup(&self) {
		self.startup_guard.lock().unwrap().take();
	}

	pub fn wait_for_instance_id(&self) -> Result<Option<String>> {
		let _ = &self.data_model_name;
		Ok(None)
	}

	pub fn stop(&self) -> Result<()> {
		terminate_process(self.process_id, &self.studio_executable, self.creation_filetime)
	}
}

fn ensure_plugin() -> Result<studio_plugin::Installation> {
	let installation = studio_plugin::ensure_current()?;
	match installation.status {
		studio_plugin::InstallStatus::Current => {}
		studio_plugin::InstallStatus::Installed => {
			crate::carbon_info!("Installed Carbon Studio plugin at {}", installation.path.display());
		}
		studio_plugin::InstallStatus::Updated => {
			crate::carbon_info!("Updated Carbon Studio plugin at {}", installation.path.display());
		}
	}
	Ok(installation)
}

fn parse_version(raw: &str) -> Result<(String, [u32; 4])> {
	let sanitized = raw.replace(", ", ".").replace(',', ".").replace(' ', "");
	let parts = sanitized.split('.').collect::<Vec<_>>();
	ensure!(
		parts.len() == 4,
		"Studio version string '{raw}' does not have 4 components"
	);
	let components = [
		parts[0].parse().context("invalid version component 0")?,
		parts[1].parse().context("invalid version component 1")?,
		parts[2].parse().context("invalid version component 2")?,
		parts[3].parse().context("invalid version component 3")?,
	];
	Ok((
		format!(
			"{}.{}.{}.{}",
			components[0], components[1], components[2], components[3]
		),
		components,
	))
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn windows_file_version(path: &str) -> Result<String> {
	let encoded = BASE64_STANDARD.encode(path.as_bytes());
	let script = format!(
		r#"$path = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded}')); (Get-Item -LiteralPath $path).VersionInfo.FileVersion"#
	);
	let output = powershell_command()?
		.args(["-NoProfile", "-NonInteractive", "-Command", &script])
		.output()
		.context("failed to read Roblox Studio file version")?;
	ensure!(
		output.status.success(),
		"failed to read Roblox Studio file version: {}",
		String::from_utf8_lossy(&output.stderr).trim()
	);
	Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

pub fn get_studio_info() -> Result<StudioInfo> {
	if let Some(executable) = std::env::var_os("ROBLOX_STUDIO_EXE").map(PathBuf::from) {
		ensure!(
			executable.is_file(),
			"ROBLOX_STUDIO_EXE does not exist: {}",
			executable.display()
		);
		#[cfg(target_os = "linux")]
		let native = windows_path(&executable, "Roblox Studio executable")?;
		#[cfg(not(target_os = "linux"))]
		let native = executable.to_string_lossy().into_owned();
		#[cfg(any(target_os = "linux", target_os = "windows"))]
		let raw_version = windows_file_version(&native)?;
		#[cfg(target_os = "macos")]
		let raw_version = macos_studio_version(&executable)?;
		let (version_text, version_components) = parse_version(&raw_version)?;
		let build_id = executable
			.parent()
			.and_then(Path::file_name)
			.and_then(|name| name.to_str())
			.unwrap_or("custom")
			.to_owned();
		return Ok(StudioInfo {
			executable,
			version_text,
			version_components,
			build_id,
		});
	}

	#[cfg(target_os = "linux")]
	{
		ensure!(
			std::env::var_os("WSL_DISTRO_NAME").is_some(),
			"automatic Studio discovery on Linux requires WSL"
		);
		let output = powershell_command()?
			.args([
				"-NoProfile",
				"-NonInteractive",
				"-Command",
				r#"$studio = Get-ChildItem "$env:LOCALAPPDATA/Roblox/Versions/*/RobloxStudioBeta.exe" -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1; if ($null -eq $studio) { exit 3 }; Write-Output ($studio.FullName + [char]9 + $studio.VersionInfo.FileVersion)"#,
			])
			.output()
			.context("failed to locate Roblox Studio through PowerShell")?;
		ensure!(output.status.success(), "Roblox Studio is not installed");
		let stdout = String::from_utf8(output.stdout)?;
		let mut parts = stdout.trim().split('\t');
		let native = parts.next().context("missing Studio executable path")?;
		let raw_version = parts.next().context("missing Studio file version")?;
		let translated = Command::new("wslpath")
			.args(["-u", native])
			.output()
			.context("failed to translate Roblox Studio path")?;
		ensure!(translated.status.success(), "failed to translate Roblox Studio path");
		let executable = PathBuf::from(String::from_utf8(translated.stdout)?.trim());
		let build_id = executable
			.parent()
			.and_then(Path::file_name)
			.and_then(|name| name.to_str())
			.unwrap_or("latest")
			.to_owned();
		let (version_text, version_components) = parse_version(raw_version)?;
		Ok(StudioInfo {
			executable,
			version_text,
			version_components,
			build_id,
		})
	}

	#[cfg(target_os = "windows")]
	{
		use roblox_install::RobloxStudio;
		let executable = RobloxStudio::locate()?.application_path().to_owned();
		ensure!(executable.is_file(), "Roblox Studio executable not found");
		let raw_version = windows_file_version(&executable.to_string_lossy())?;
		let (version_text, version_components) = parse_version(&raw_version)?;
		let build_id = executable
			.parent()
			.and_then(Path::file_name)
			.and_then(|name| name.to_str())
			.unwrap_or("latest")
			.to_owned();
		return Ok(StudioInfo {
			executable,
			version_text,
			version_components,
			build_id,
		});
	}

	#[cfg(target_os = "macos")]
	{
		use roblox_install::RobloxStudio;
		let executable = RobloxStudio::locate()?.application_path().to_owned();
		ensure!(executable.is_file(), "Roblox Studio executable not found");
		let raw_version = macos_studio_version(&executable)?;
		let (version_text, version_components) = parse_version(&raw_version)?;
		return Ok(StudioInfo {
			executable,
			version_text,
			version_components,
			build_id: "RobloxStudio.app".to_owned(),
		});
	}

	#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
	anyhow::bail!("Roblox Studio is unsupported on this platform")
}

#[cfg(target_os = "macos")]
fn macos_studio_version(executable: &Path) -> Result<String> {
	let bundle = executable
		.ancestors()
		.find(|path| path.extension().is_some_and(|extension| extension == "app"))
		.context("Studio executable is not inside an app bundle")?;
	let plist = bundle.join("Contents/Info.plist");
	let output = Command::new("/usr/libexec/PlistBuddy")
		.args(["-c", "Print :CFBundleShortVersionString"])
		.arg(&plist)
		.output()?;
	ensure!(
		output.status.success(),
		"failed to read Studio version from {}",
		plist.display()
	);
	Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(target_os = "linux")]
fn windows_path(path: &Path, description: &str) -> Result<String> {
	let output = Command::new("wslpath").arg("-w").arg(path).output()?;
	ensure!(
		output.status.success(),
		"failed to translate the {description} path for Windows"
	);
	Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn launch_process(path: Option<&Path>, studio: &StudioInfo) -> Result<(u32, String, u64)> {
	#[cfg(target_os = "linux")]
	let executable = windows_path(&studio.executable, "Roblox Studio executable")?;
	#[cfg(target_os = "windows")]
	let executable = studio.executable.to_string_lossy().into_owned();
	#[cfg(target_os = "linux")]
	let place = path.map(|path| windows_path(path, "Studio place")).transpose()?;
	#[cfg(target_os = "windows")]
	let place = path.map(|path| path.to_string_lossy().into_owned());
	let encoded_executable = BASE64_STANDARD.encode(executable.as_bytes());
	let encoded_place = BASE64_STANDARD.encode(place.as_deref().unwrap_or("").as_bytes());
	let script = format!(
		r#"
$studio = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded_executable}'))
$place = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded_place}'))
if ([string]::IsNullOrEmpty($place)) {{
    $process = Start-Process -FilePath $studio -PassThru
}} else {{
    $quotedPlace = '"' + $place + '"'
    $process = Start-Process -FilePath $studio -ArgumentList @('--task', 'EditFile', '--localPlaceFile', $quotedPlace) -PassThru
}}
$process.WaitForInputIdle(30000) | Out-Null
$process.Refresh()
Write-Output ($process.Id.ToString() + [char]9 + $process.StartTime.ToUniversalTime().ToFileTimeUtc().ToString())
"#
	);
	let output = powershell_command()?
		.args(["-NoProfile", "-NonInteractive", "-Command", &script])
		.output()
		.context("failed to launch Roblox Studio")?;
	ensure!(
		output.status.success(),
		"Roblox Studio launch failed: {}",
		String::from_utf8_lossy(&output.stderr).trim()
	);
	let stdout = String::from_utf8(output.stdout)?;
	let mut fields = stdout.trim().split('\t');
	let process_id = fields
		.next()
		.context("Studio launch omitted its process ID")?
		.parse()
		.context("Studio launch returned an invalid process ID")?;
	let creation_filetime = fields
		.next()
		.context("Studio launch omitted its process creation time")?
		.parse()
		.context("Studio launch returned an invalid process creation time")?;
	Ok((process_id, executable, creation_filetime))
}

#[cfg(target_os = "macos")]
fn launch_process(path: Option<&Path>, studio: &StudioInfo) -> Result<(u32, String, u64)> {
	let mut command = Command::new(&studio.executable);
	if let Some(path) = path {
		command.arg("--task").arg("EditFile").arg("--localPlaceFile").arg(path);
	}
	let child = command
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()?;
	Ok((child.id(), studio.executable.to_string_lossy().into_owned(), 0))
}

pub fn launch(path: Option<PathBuf>) -> Result<Option<u32>> {
	let _plugin = ensure_plugin()?;
	let studio = get_studio_info()?;
	let (process_id, _, _) = launch_process(path.as_deref(), &studio)?;
	Ok(Some(process_id))
}

pub fn launch_managed(path: PathBuf, _studio_dir: &Path) -> Result<ManagedStudio> {
	let installation = ensure_plugin()?;
	let studio = get_studio_info()?;
	let data_model_name = path
		.file_name()
		.context("managed Studio place path has no file name")?
		.to_string_lossy()
		.into_owned();
	let (process_id, studio_executable, creation_filetime) = launch_process(Some(&path), &studio)?;
	Ok(ManagedStudio {
		process_id,
		data_model_name,
		studio_executable,
		creation_filetime,
		startup_guard: Arc::new(Mutex::new(Some(installation))),
	})
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn validate_process_script(process_id: u32, executable: &str, creation_filetime: u64) -> String {
	let encoded = BASE64_STANDARD.encode(executable.as_bytes());
	format!(
		r#"
$expected = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded}'))
$process = Get-Process -Id {process_id} -ErrorAction SilentlyContinue
if ($null -eq $process) {{ exit 0 }}
$process.Refresh()
if (-not [string]::Equals($process.Path, $expected, [StringComparison]::OrdinalIgnoreCase)) {{ exit 3 }}
if ($process.StartTime.ToUniversalTime().ToFileTimeUtc() -ne {creation_filetime}) {{ exit 3 }}
"#
	)
}

fn terminate_process(process_id: u32, executable: &str, creation_filetime: u64) -> Result<()> {
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	{
		let script = format!(
			"{}\nStop-Process -Id {process_id} -Force -ErrorAction Stop\n",
			validate_process_script(process_id, executable, creation_filetime)
		);
		let output = powershell_command()?
			.args(["-NoProfile", "-NonInteractive", "-Command", &script])
			.output()?;
		ensure!(
			output.status.success(),
			"managed Roblox Studio process did not stop cleanly: {}",
			String::from_utf8_lossy(&output.stderr).trim()
		);
		Ok(())
	}

	#[cfg(target_os = "macos")]
	{
		let _ = executable;
		let _ = creation_filetime;
		let status = Command::new("kill").arg(process_id.to_string()).status()?;
		ensure!(status.success(), "managed Roblox Studio process did not stop cleanly");
		Ok(())
	}

	#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
	anyhow::bail!("Roblox Studio process termination is unsupported")
}

pub fn wait_for_exit(process_id: u32) -> Result<()> {
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	{
		let script = format!(
			"$process = Get-Process -Id {process_id} -ErrorAction SilentlyContinue; if ($null -ne $process) {{ $process.WaitForExit() }}"
		);
		let status = powershell_command()?
			.args(["-NoProfile", "-NonInteractive", "-Command", &script])
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status()?;
		ensure!(status.success(), "Roblox Studio process monitor failed");
		return Ok(());
	}

	#[cfg(target_os = "macos")]
	{
		loop {
			if !Command::new("kill")
				.args(["-0", &process_id.to_string()])
				.status()?
				.success()
			{
				return Ok(());
			}
			std::thread::sleep(std::time::Duration::from_millis(250));
		}
	}

	#[allow(unreachable_code)]
	Ok(())
}

pub fn focus_process(
	process_id: u32,
	creation_filetime: Option<u64>,
	studio_executable: Option<&str>,
	restore_previous: bool,
) -> Result<()> {
	let creation_filetime = creation_filetime.context("Studio process creation time is unavailable")?;
	let studio_executable = studio_executable.context("Studio executable identity is unavailable")?;
	#[cfg(target_os = "windows")]
	{
		return crate::studio_windows::focus_process(
			process_id,
			creation_filetime,
			studio_executable,
			restore_previous,
		);
	}
	#[cfg(target_os = "linux")]
	{
		let validation = validate_process_script(process_id, studio_executable, creation_filetime);
		let restore = if restore_previous {
			"if ($previous -ne [IntPtr]::Zero) { [CarbonWindow]::ShowWindow($previous, 9) | Out-Null; [CarbonWindow]::SetForegroundWindow($previous) | Out-Null }"
		} else {
			""
		};
		let script = format!(
			r#"
{validation}
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class CarbonWindow {{
    public delegate bool EnumProc(IntPtr hwnd, IntPtr param);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc proc, IntPtr param);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hwnd, int command);
}}
'@
$target = [IntPtr]::Zero
$previous = [CarbonWindow]::GetForegroundWindow()
[CarbonWindow]::EnumWindows({{
    param($hwnd, $state)
    [uint32]$windowProcessId = 0
    [CarbonWindow]::GetWindowThreadProcessId($hwnd, [ref]$windowProcessId) | Out-Null
    if ($windowProcessId -eq {process_id} -and [CarbonWindow]::IsWindowVisible($hwnd)) {{ $script:target = $hwnd; return $false }}
    return $true
}}, [IntPtr]::Zero) | Out-Null
if ($target -eq [IntPtr]::Zero) {{ throw 'Roblox Studio window was not found' }}
[CarbonWindow]::ShowWindow($target, 9) | Out-Null
if (-not [CarbonWindow]::SetForegroundWindow($target)) {{ throw 'Roblox Studio rejected foreground activation' }}
{restore}
"#
		);
		let output = powershell_command()?
			.args(["-NoProfile", "-NonInteractive", "-Command", &script])
			.output()?;
		ensure!(
			output.status.success(),
			"Roblox Studio window activation failed: {}",
			String::from_utf8_lossy(&output.stderr).trim()
		);
		Ok(())
	}
	#[cfg(target_os = "macos")]
	{
		let _ = creation_filetime;
		let _ = studio_executable;
		let script = format!(
			r#"tell application "System Events"
set matches to every process whose unix id is {process_id} and name is "RobloxStudio"
if (count of matches) is not 1 then error "Roblox Studio process is not running"
tell item 1 of matches
set frontmost to true
perform action "AXRaise" of window 1
end tell
end tell"#
		);
		let output = Command::new("osascript").args(["-e", &script]).output()?;
		ensure!(output.status.success(), "Roblox Studio window activation failed");
		return Ok(());
	}
	#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
	anyhow::bail!("Roblox Studio focus is unsupported on this platform")
}

#[allow(unused_variables)]
pub fn is_running(title: Option<String>) -> Result<bool> {
	#[cfg(target_os = "windows")]
	{
		let running = EnumWindows(|hwnd| -> bool {
			if !hwnd.IsWindowVisible() {
				return true;
			}
			if let Ok(text) = hwnd.GetWindowText() {
				if title
					.as_ref()
					.is_some_and(|title| text == format!("{title} - Roblox Studio"))
					|| title.is_none() && text.contains("Roblox Studio")
				{
					return false;
				}
			}
			true
		})
		.is_err();
		return Ok(running);
	}
	#[cfg(target_os = "linux")]
	{
		let output = powershell_command()?
			.args([
				"-NoProfile",
				"-NonInteractive",
				"-Command",
				"if (Get-Process RobloxStudioBeta -ErrorAction SilentlyContinue) { exit 0 } else { exit 1 }",
			])
			.output()?;
		return Ok(output.status.success());
	}
	#[cfg(target_os = "macos")]
	{
		let output = Command::new("pgrep").arg("-x").arg("RobloxStudio").output()?;
		return Ok(output.status.success());
	}
	#[allow(unreachable_code)]
	Ok(false)
}

#[allow(unused_variables)]
pub fn focus(title: Option<String>) -> Result<()> {
	#[cfg(target_os = "windows")]
	{
		let result = EnumWindows(|hwnd| -> bool {
			if !hwnd.IsWindowVisible() {
				return true;
			}
			if let Ok(text) = hwnd.GetWindowText() {
				if title
					.as_ref()
					.is_some_and(|title| text == format!("{title} - Roblox Studio"))
					|| title.is_none() && text.contains("Roblox Studio")
				{
					hwnd.SetForegroundWindow();
					hwnd.ShowWindow(SW::RESTORE);
					return false;
				}
			}
			true
		});
		if let Err(error) = result {
			if error.raw() != 0 {
				anyhow::bail!("Failed to focus Roblox Studio: {error}");
			}
		}
		return Ok(());
	}
	#[cfg(target_os = "macos")]
	{
		let output = Command::new("osascript")
			.args(["-e", "tell application \"RobloxStudio\" to activate"])
			.output()?;
		ensure!(output.status.success(), "failed to focus Roblox Studio");
		return Ok(());
	}
	#[cfg(target_os = "linux")]
	{
		anyhow::bail!("focus by window title is unavailable on WSL; use carbon focus with a managed session");
	}
	#[allow(unreachable_code)]
	Ok(())
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) fn powershell_command() -> Result<Command> {
	#[cfg(target_os = "linux")]
	{
		ensure!(
			std::env::var_os("WSL_DISTRO_NAME").is_some(),
			"PowerShell interoperability requires WSL"
		);
		Ok(Command::new("powershell.exe"))
	}
	#[cfg(target_os = "windows")]
	{
		Ok(Command::new("powershell.exe"))
	}
}
