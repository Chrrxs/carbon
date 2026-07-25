use anyhow::{bail, ensure, Context, Result};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use log::info;
use serde::{Deserialize, Serialize};
#[cfg(any(carbon_bundled_rml, test, target_os = "linux", target_os = "windows"))]
use std::io::Write;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::io::{BufRead, BufReader};
use std::process::Command;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::process::{Child, Stdio};
use std::{
	env, fs,
	fs::{File, OpenOptions},
	path::{Path, PathBuf},
};

use crate::util;

pub const BUILD_VERSION: &str = env!("CARBON_BUILD_IDENTITY");
pub const LOADER_ENV: &str = "CARBON_RML_LOADER";
pub const EXPECTED_BUILD_ENV: &str = "CARBON_RML_BUILD_VERSION";
pub const LOADED_BUILD_ENV: &str = "CARBON_RML_LOADED_BUILD_VERSION";
const MARKER_SCHEMA: u32 = 1;
const MARKER_PATH: &str = "RobloxModLoader/carbon-rml.json";
const BOOTSTRAP_PATH: &str = "dwmapi.dll";
const BOOTSTRAP_LOCK_PATH: &str = "RobloxModLoader/carbon-bootstrap.lock";
const LOADER_PATH: &str = "RobloxModLoader/roblox_modloader.dll";
#[cfg(any(carbon_bundled_rml, test))]
const CONFIG_PATH: &str = "RobloxModLoader/config.toml";
const RML_CACHE_DIR_ENV: &str = "CARBON_RML_CACHE_DIR";
#[cfg(carbon_bundled_rml)]
include!(concat!(env!("OUT_DIR"), "/carbon_rml_bundle.rs"));
const REQUIRED_FILES: &[&str] = &[
	BOOTSTRAP_PATH,
	LOADER_PATH,
	"RobloxModLoader/runtime/RML.Core.dll",
	"RobloxModLoader/runtime/RML.NativeHost.dll",
	"RobloxModLoader/runtime/Roblox.dll",
	"RobloxModLoader/runtime/nethost.dll",
	"RobloxModLoader/runtime/RML.runtimeconfig.json",
	"RobloxModLoader/mods/carbon/dotnet/Carbon.RmlBridge.dll",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallMarker {
	pub schema_version: u32,
	pub build_version: String,
}

impl InstallMarker {
	pub fn current() -> Self {
		Self {
			schema_version: MARKER_SCHEMA,
			build_version: BUILD_VERSION.to_owned(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
	Current,
	Missing,
	Outdated,
	Incomplete(&'static str),
}

#[cfg(any(carbon_bundled_rml, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BundleInstallStatus {
	Current,
	Installed,
	Updated,
}

impl Status {
	pub fn is_current(&self) -> bool {
		matches!(self, Self::Current)
	}
}

pub fn status(studio_dir: &Path) -> Status {
	let package = match package_dir(None) {
		Ok(package) => package,
		Err(_) => return Status::Missing,
	};
	bootstrap_status(&package, studio_dir)
}

#[derive(Debug)]
pub struct Launch {
	studio_executable: PathBuf,
	loader_path: PathBuf,
	bootstrap_updated: bool,
	_bootstrap_lock: File,
}

impl Launch {
	pub fn studio_executable(&self) -> &Path {
		&self.studio_executable
	}

	pub fn loader_path(&self) -> &Path {
		&self.loader_path
	}

	pub fn build_version(&self) -> &'static str {
		BUILD_VERSION
	}

	pub fn bootstrap_updated(&self) -> bool {
		self.bootstrap_updated
	}
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn powershell_injection_script(
	process_id: u32,
	loader: &str,
	studio_executable: &str,
	started_at_file_time: u64,
) -> String {
	let encoded_loader = BASE64_STANDARD.encode(loader.as_bytes());
	let encoded_studio = BASE64_STANDARD.encode(studio_executable.as_bytes());
	r#"
$processId = __CARBON_PROCESS_ID__
$startedAtFileTime = [uint64]__CARBON_STARTED_AT_FILE_TIME__
$loader = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__CARBON_LOADER_BASE64__'))
$studio = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__CARBON_STUDIO_BASE64__'))
if ($loader -notmatch '^(?:[A-Za-z]:\\|\\\\[^\\]+\\[^\\]+\\)') { throw 'Carbon supplied a non-absolute RML loader path' }
if ($studio -notmatch '^(?:[A-Za-z]:\\|\\\\[^\\]+\\[^\\]+\\)') { throw 'Carbon supplied a non-absolute Studio executable path' }
Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

public static class CarbonRmlInjector
{
    private const uint ProcessAccess = 0x0002 | 0x0008 | 0x0010 | 0x0020 | 0x0400 | 0x1000;
    private const uint MemCommit = 0x1000;
    private const uint MemReserve = 0x2000;
    private const uint MemRelease = 0x8000;
    private const uint PageReadWrite = 0x04;
    private const uint WaitObject0 = 0x00000000;
    private const uint WaitTimeout = 0x00000102;
    private const uint CreateSuspended = 0x00000004;
    private const uint ResumeFailed = 0xFFFFFFFF;
    [StructLayout(LayoutKind.Sequential)]
    private struct FileTime
    {
        public uint Low;
        public uint High;
    }


    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr OpenProcess(uint access, bool inheritHandle, uint processId);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool QueryFullProcessImageName(
        IntPtr process,
        uint flags,
        StringBuilder executable,
        ref uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetProcessTimes(
        IntPtr process,
        out FileTime creation,
        out FileTime exit,
        out FileTime kernel,
        out FileTime user);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr VirtualAllocEx(
        IntPtr process,
        IntPtr address,
        UIntPtr size,
        uint allocationType,
        uint protect);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool VirtualFreeEx(IntPtr process, IntPtr address, UIntPtr size, uint freeType);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool WriteProcessMemory(
        IntPtr process,
        IntPtr address,
        byte[] buffer,
        UIntPtr size,
        out UIntPtr written);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr GetModuleHandle(string moduleName);
    [DllImport("kernel32.dll", CharSet = CharSet.Ansi, ExactSpelling = true, SetLastError = true)]
    private static extern IntPtr GetProcAddress(IntPtr module, string name);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr CreateRemoteThread(
        IntPtr process,
        IntPtr attributes,
        UIntPtr stackSize,
        IntPtr startAddress,
        IntPtr parameter,
        uint flags,
        out uint threadId);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint ResumeThread(IntPtr thread);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool CloseHandle(IntPtr handle);

    private static string NormalizePath(string path)
    {
        if (path.StartsWith(@"\\?\UNC\", StringComparison.OrdinalIgnoreCase))
        {
            path = @"\\" + path.Substring(8);
        }
        else if (path.StartsWith(@"\\?\", StringComparison.Ordinal))
        {
            path = path.Substring(4);
        }
        return Path.GetFullPath(path).TrimEnd(Path.DirectorySeparatorChar);
    }

    private static void Check(bool condition, string operation)
    {
        if (!condition)
        {
            throw new Win32Exception(Marshal.GetLastWin32Error(), operation);
        }
    }

    public static void Inject(uint processId, string loader, string expectedExecutable, ulong expectedStartedAt)
    {
        byte[] path = Encoding.Unicode.GetBytes(loader + "\0");
        IntPtr process = OpenProcess(ProcessAccess, false, processId);
        Check(process != IntPtr.Zero, "OpenProcess");
        IntPtr remote = IntPtr.Zero;
        IntPtr thread = IntPtr.Zero;
        bool remoteThreadFinished = false;
        try
        {
            FileTime creation;
            FileTime exit;
            FileTime kernel;
            FileTime user;
            Check(GetProcessTimes(process, out creation, out exit, out kernel, out user), "GetProcessTimes");
            ulong startedAt = ((ulong)creation.High << 32) | creation.Low;
            if (startedAt != expectedStartedAt)
            {
                throw new InvalidOperationException(
                    "Carbon injector process creation time does not match the launched Studio process");
            }

            var executable = new StringBuilder(32768);
            uint executableLength = (uint)executable.Capacity;
            Check(
                QueryFullProcessImageName(process, 0, executable, ref executableLength),
                "QueryFullProcessImageName");
            if (!string.Equals(
                NormalizePath(executable.ToString()),
                NormalizePath(expectedExecutable),
                StringComparison.OrdinalIgnoreCase))
            {
                throw new InvalidOperationException(
                    "Carbon injector process identity does not match the launched Studio executable");
            }

            remote = VirtualAllocEx(
                process,
                IntPtr.Zero,
                (UIntPtr)path.Length,
                MemCommit | MemReserve,
                PageReadWrite);
            Check(remote != IntPtr.Zero, "VirtualAllocEx");

            UIntPtr written;
            Check(
                WriteProcessMemory(process, remote, path, (UIntPtr)path.Length, out written)
                    && written.ToUInt64() == (ulong)path.Length,
                "WriteProcessMemory");

            IntPtr kernel32 = GetModuleHandle("kernel32.dll");
            Check(kernel32 != IntPtr.Zero, "GetModuleHandle");
            IntPtr loadLibrary = GetProcAddress(kernel32, "LoadLibraryW");
            Check(loadLibrary != IntPtr.Zero, "GetProcAddress");

            uint threadId;
            thread = CreateRemoteThread(
                process,
                IntPtr.Zero,
                UIntPtr.Zero,
                loadLibrary,
                remote,
                CreateSuspended,
                out threadId);
            Check(thread != IntPtr.Zero, "CreateRemoteThread");

            Console.Out.WriteLine("CARBON_RML_INJECTOR_READY");
            Console.Out.Flush();
            if (!string.Equals(Console.In.ReadLine(), "CARBON_RML_INJECTOR_PROCEED", StringComparison.Ordinal))
            {
                throw new InvalidOperationException("Carbon injector authorization was not confirmed");
            }
            if (ResumeThread(thread) == ResumeFailed)
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "ResumeThread failed");
            }
            Console.Out.WriteLine("CARBON_RML_INJECTOR_STARTED");
            Console.Out.Flush();
            uint wait = WaitForSingleObject(thread, 30000);
            if (wait == WaitTimeout)
            {
                throw new TimeoutException("RML loader injection timed out");
            }
            Check(wait == WaitObject0, "WaitForSingleObject");
            remoteThreadFinished = true;
            // The remote thread exit API truncates the 64-bit HMODULE returned by LoadLibraryW.
            // Carbon verifies the loaded module through exact-build bridge attestation.

        }
        finally
        {
            if (thread != IntPtr.Zero)
            {
                CloseHandle(thread);
            }
            if (remote != IntPtr.Zero && (thread == IntPtr.Zero || remoteThreadFinished))
            {
                VirtualFreeEx(process, remote, UIntPtr.Zero, MemRelease);
            }
            CloseHandle(process);
        }
    }
}
'@
[CarbonRmlInjector]::Inject([uint32]$processId, $loader, $studio, $startedAtFileTime)
"#
	.replace("__CARBON_PROCESS_ID__", &process_id.to_string())
	.replace("__CARBON_STARTED_AT_FILE_TIME__", &started_at_file_time.to_string())
	.replace("__CARBON_LOADER_BASE64__", &encoded_loader)
	.replace("__CARBON_STUDIO_BASE64__", &encoded_studio)
}

#[cfg(target_os = "linux")]
fn windows_loader_path(loader: &Path) -> Result<String> {
	let output = Command::new("wslpath")
		.args(["-w"])
		.arg(loader)
		.output()
		.context("failed to translate the RML loader path for Windows")?;
	ensure!(
		output.status.success(),
		"failed to translate the RML loader path for Windows: {}",
		String::from_utf8_lossy(&output.stderr).trim()
	);
	let loader = String::from_utf8(output.stdout)?.trim().to_owned();
	ensure!(!loader.is_empty(), "Windows RML loader path is empty");
	Ok(loader)
}

#[cfg(target_os = "windows")]
fn windows_loader_path(loader: &Path) -> Result<String> {
	Ok(loader
		.to_str()
		.context("RML loader path is not valid UTF-8")?
		.to_owned())
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn reap_failed_injector(mut child: Child, error: anyhow::Error) -> anyhow::Error {
	child.stdin.take();
	match child.wait_with_output() {
		Ok(output) => {
			let stderr = String::from_utf8_lossy(&output.stderr);
			let stderr = stderr.trim();
			if stderr.is_empty() {
				error
			} else {
				error.context(format!("Carbon RML injector failed: {stderr}"))
			}
		}
		Err(wait_error) => error.context(format!("failed to reap the Carbon RML injector: {wait_error}")),
	}
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn inject_loader(
	process_id: u32,
	loader: &Path,
	studio_executable: &str,
	started_at_file_time: u64,
	authorize: impl FnOnce() -> Result<()>,
) -> Result<()> {
	ensure!(loader.is_file(), "RML loader is missing: {}", loader.display());
	let loader = windows_loader_path(loader)?;
	let script = powershell_injection_script(process_id, &loader, studio_executable, started_at_file_time);
	let mut child = Command::new("powershell.exe")
		.args(["-NoProfile", "-NonInteractive", "-Command", &script])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.context("failed to start the Carbon RML injector")?;
	let mut stdout = BufReader::new(
		child
			.stdout
			.take()
			.context("Carbon RML injector stdout is unavailable")?,
	);
	let mut ready = String::new();
	if let Err(error) = stdout.read_line(&mut ready) {
		child.stdout = Some(stdout.into_inner());
		let error = anyhow::Error::new(error).context("failed to read Carbon RML injector readiness");
		return Err(reap_failed_injector(child, error));
	}
	if ready.trim() != "CARBON_RML_INJECTOR_READY" {
		child.stdout = Some(stdout.into_inner());
		let error = anyhow::anyhow!("Carbon RML injector did not stage the exact launched Studio process");
		return Err(reap_failed_injector(child, error));
	}
	let write_result = match child.stdin.as_mut() {
		Some(stdin) => writeln!(stdin, "CARBON_RML_INJECTOR_PROCEED"),
		None => {
			child.stdout = Some(stdout.into_inner());
			return Err(reap_failed_injector(
				child,
				anyhow::anyhow!("Carbon RML injector stdin is unavailable"),
			));
		}
	};
	if let Err(error) = write_result {
		child.stdout = Some(stdout.into_inner());
		return Err(reap_failed_injector(
			child,
			anyhow::Error::new(error).context("failed to start the staged Carbon RML injector"),
		));
	}
	let mut started = String::new();
	let started_result = stdout.read_line(&mut started);
	child.stdout = Some(stdout.into_inner());
	if let Err(error) = started_result {
		let error = anyhow::Error::new(error).context("failed to read Carbon RML injector start confirmation");
		return Err(reap_failed_injector(child, error));
	}
	if started.trim() != "CARBON_RML_INJECTOR_STARTED" {
		let error = anyhow::anyhow!("Carbon RML injector did not start its staged remote thread");
		return Err(reap_failed_injector(child, error));
	}
	if let Err(error) = authorize() {
		return Err(reap_failed_injector(
			child,
			error.context("Carbon RML injection authorization failed"),
		));
	}
	child.stdin.take();
	let output = child
		.wait_with_output()
		.context("failed to wait for the Carbon RML injector")?;
	ensure!(
		output.status.success(),
		"failed to load Carbon RML into Roblox Studio process {process_id}: {}",
		String::from_utf8_lossy(&output.stderr).trim()
	);
	Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn inject_loader(
	_process_id: u32,
	_loader: &Path,
	_studio_executable: &str,
	_started_at_file_time: u64,
	_authorize: impl FnOnce() -> Result<()>,
) -> Result<()> {
	Ok(())
}

#[derive(Debug, Clone)]
pub struct StudioInfo {
	pub executable: PathBuf,
	pub version_text: String,
	pub version_components: [u32; 4],
	pub build_id: String,
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn windows_file_version(path: &str) -> Result<String> {
	let encoded_path = BASE64_STANDARD.encode(path.as_bytes());
	let script = format!(
		r#"$path = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded_path}')); (Get-Item -LiteralPath $path).VersionInfo.FileVersion"#
	);
	let output = Command::new("powershell.exe")
		.args(["-NoProfile", "-NonInteractive", "-Command", &script])
		.output()
		.context("failed to read Studio file version")?;
	ensure!(output.status.success(), "failed to read Studio file version");
	Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(target_os = "macos")]
fn macos_studio_bundle(executable: &Path) -> Result<&Path> {
	executable
		.ancestors()
		.find(|path| path.extension().is_some_and(|extension| extension == "app"))
		.with_context(|| {
			format!(
				"Studio executable is not inside an app bundle: {}",
				executable.display()
			)
		})
}

#[cfg(target_os = "macos")]
fn macos_studio_version(executable: &Path) -> Result<String> {
	let plist = macos_studio_bundle(executable)?.join("Contents/Info.plist");
	ensure!(
		plist.is_file(),
		"Info.plist not found for Studio app at {}",
		plist.display()
	);
	let output = Command::new("/usr/libexec/PlistBuddy")
		.args(["-c", "Print :CFBundleShortVersionString"])
		.arg(&plist)
		.output()
		.with_context(|| format!("failed to read Studio version from {}", plist.display()))?;
	ensure!(
		output.status.success(),
		"failed to read Studio version from {}",
		plist.display()
	);
	Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

pub fn get_studio_info() -> Result<StudioInfo> {
	if let Some(executable_env) = env::var_os("ROBLOX_STUDIO_EXE") {
		let executable = PathBuf::from(executable_env);
		ensure!(
			executable.is_file(),
			"ROBLOX_STUDIO_EXE does not exist: {}",
			executable.display()
		);
		#[cfg(target_os = "macos")]
		let build_id = macos_studio_bundle(&executable)?
			.file_name()
			.and_then(|name| name.to_str())
			.unwrap_or("custom")
			.to_owned();
		#[cfg(not(target_os = "macos"))]
		let build_id = executable
			.parent()
			.and_then(|p| p.file_name())
			.and_then(|n| n.to_str())
			.unwrap_or("custom")
			.to_owned();

		#[cfg(target_os = "linux")]
		let raw_version = {
			let win_path = Command::new("wslpath")
				.args(["-w", executable.to_str().unwrap()])
				.output()
				.context("failed to translate executable path with wslpath -w")?;
			ensure!(win_path.status.success(), "wslpath -w failed");
			let win_str = String::from_utf8(win_path.stdout)?.trim().to_owned();
			windows_file_version(&win_str)?
		};

		#[cfg(target_os = "macos")]
		let raw_version = macos_studio_version(&executable)?;

		#[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
		let raw_version = windows_file_version(
			executable
				.to_str()
				.context("Roblox Studio executable path is not valid UTF-8")?,
		)?;

		let (version_text, version_components) = parse_version(&raw_version)?;
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
			env::var_os("WSL_DISTRO_NAME").is_some(),
			"automatic Studio reflection on Linux requires WSL"
		);
		let output = Command::new("powershell.exe")
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
		let line = stdout.trim();
		let mut parts = line.split('\t');
		let windows_path = parts.next().context("missing Studio executable path")?.trim();
		let raw_version = parts.next().context("missing Studio file version")?.trim();

		let translated = Command::new("wslpath")
			.args(["-u", windows_path])
			.output()
			.context("failed to translate Roblox Studio path")?;
		ensure!(translated.status.success(), "failed to translate Roblox Studio path");
		let executable = PathBuf::from(String::from_utf8(translated.stdout)?.trim());

		let build_id = executable
			.parent()
			.and_then(|p| p.file_name())
			.and_then(|n| n.to_str())
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

	#[cfg(target_os = "macos")]
	{
		use roblox_install::RobloxStudio;
		let executable = RobloxStudio::locate()?.application_path().to_owned();
		ensure!(executable.is_file(), "Roblox Studio executable not found");

		let raw_version = macos_studio_version(&executable)?;
		let build_id = macos_studio_bundle(&executable)?
			.file_name()
			.and_then(|name| name.to_str())
			.unwrap_or("RobloxStudio.app")
			.to_owned();
		let (version_text, version_components) = parse_version(&raw_version)?;

		Ok(StudioInfo {
			executable,
			version_text,
			version_components,
			build_id,
		})
	}

	#[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
	{
		use roblox_install::RobloxStudio;
		let executable = RobloxStudio::locate()?.application_path().to_owned();
		ensure!(executable.is_file(), "Roblox Studio executable not found");

		let build_id = executable
			.parent()
			.and_then(|p| p.file_name())
			.and_then(|n| n.to_str())
			.unwrap_or("latest")
			.to_owned();

		let raw_version = windows_file_version(
			executable
				.to_str()
				.context("Roblox Studio executable path is not valid UTF-8")?,
		)?;
		let (version_text, version_components) = parse_version(&raw_version)?;

		Ok(StudioInfo {
			executable,
			version_text,
			version_components,
			build_id,
		})
	}
}

fn parse_version(raw: &str) -> Result<(String, [u32; 4])> {
	let sanitized = raw.replace(", ", ".").replace(',', ".").replace(' ', "");
	let parts: Vec<&str> = sanitized.split('.').collect();
	ensure!(
		parts.len() == 4,
		"Studio version string '{raw}' does not have 4 components"
	);
	let c0: u32 = parts[0].parse().context("invalid version component 0")?;
	let c1: u32 = parts[1].parse().context("invalid version component 1")?;
	let c2: u32 = parts[2].parse().context("invalid version component 2")?;
	let c3: u32 = parts[3].parse().context("invalid version component 3")?;

	let text = format!("{c0}.{c1}.{c2}.{c3}");
	Ok((text, [c0, c1, c2, c3]))
}

pub fn latest_studio_dir() -> Result<PathBuf> {
	let info = get_studio_info()?;
	info.executable
		.parent()
		.map(Path::to_owned)
		.context("Roblox Studio executable has no parent directory")
}

pub fn package_dir(explicit: Option<&Path>) -> Result<PathBuf> {
	let mut candidates = Vec::new();
	if let Some(path) = explicit {
		candidates.push(path.to_owned());
	}
	if let Some(path) = env::var_os("CARBON_RML_PACKAGE") {
		candidates.push(PathBuf::from(path));
	}
	if let Ok(executable) = env::current_exe() {
		if let Some(parent) = executable.parent() {
			candidates.push(parent.join("rml"));
		}
	}
	#[cfg(carbon_bundled_rml)]
	{
		candidates.push(ensure_bundled_package()?);
	}
	if let Ok(cache) = rml_cache_dir() {
		candidates.push(cache.join(BUILD_VERSION));
	}

	for candidate in candidates {
		if package_is_current(&candidate) {
			return Ok(candidate);
		}
	}
	bail!(
		"the Carbon {} RML bundle is unavailable; reinstall this Carbon build or set CARBON_RML_PACKAGE",
		BUILD_VERSION
	)
}

fn rml_cache_dir() -> Result<PathBuf> {
	if let Some(path) = env::var_os(RML_CACHE_DIR_ENV).filter(|path| !path.is_empty()) {
		fs::create_dir_all(&path)
			.with_context(|| format!("failed to create RML cache directory {}", Path::new(&path).display()))?;
		return Ok(PathBuf::from(path));
	}
	Ok(util::get_carbon_dir()?.join("rml"))
}

#[cfg(carbon_bundled_rml)]
fn ensure_bundled_package() -> Result<PathBuf> {
	ensure!(!EMBEDDED_RML_FILES.is_empty(), "the embedded RML bundle is empty");
	let cache = rml_cache_dir()?;
	fs::create_dir_all(&cache)?;
	let lock_path = cache.join(format!(".{BUILD_VERSION}.lock"));
	let lock = OpenOptions::new()
		.create(true)
		.truncate(false)
		.read(true)
		.write(true)
		.open(&lock_path)
		.with_context(|| format!("failed to open embedded RML lock {}", lock_path.display()))?;
	lock.lock()
		.with_context(|| format!("failed to lock embedded RML package {}", lock_path.display()))?;
	let package = cache.join(BUILD_VERSION);
	let status = install_bundled_files(EMBEDDED_RML_FILES, &package)?;
	ensure!(
		bundled_files_are_current(EMBEDDED_RML_FILES, &package),
		"embedded RML extraction did not verify"
	);
	match status {
		BundleInstallStatus::Current => {}
		BundleInstallStatus::Installed => info!("Installed embedded RML package at {}", package.display()),
		BundleInstallStatus::Updated => info!("Updated embedded RML package at {}", package.display()),
	}
	Ok(package)
}

#[cfg(any(carbon_bundled_rml, test))]
fn install_bundled_files(files: &[(&str, &[u8])], package: &Path) -> Result<BundleInstallStatus> {
	ensure!(!files.is_empty(), "refusing to install an empty RML bundle");
	let existed = package.exists();
	let mut changed = false;
	for (relative, bytes) in files {
		let relative = safe_bundle_path(relative)?;
		let destination = package.join(relative);
		if relative == Path::new(CONFIG_PATH) && destination.is_file() {
			continue;
		}
		if fs::read(&destination).ok().as_deref() == Some(*bytes) {
			continue;
		}
		if let Some(parent) = destination.parent() {
			fs::create_dir_all(parent)
				.with_context(|| format!("failed to create embedded RML directory {}", parent.display()))?;
		}
		let mut output = File::create(&destination)
			.with_context(|| format!("failed to create embedded RML file {}", destination.display()))?;
		output
			.write_all(bytes)
			.with_context(|| format!("failed to write embedded RML file {}", destination.display()))?;
		output
			.sync_all()
			.with_context(|| format!("failed to flush embedded RML file {}", destination.display()))?;
		changed = true;
	}
	if !changed {
		Ok(BundleInstallStatus::Current)
	} else if existed {
		Ok(BundleInstallStatus::Updated)
	} else {
		Ok(BundleInstallStatus::Installed)
	}
}

#[cfg(carbon_bundled_rml)]
fn bundled_files_are_current(files: &[(&str, &[u8])], package: &Path) -> bool {
	files.iter().all(|(relative, bytes)| {
		let Ok(relative) = safe_bundle_path(relative) else {
			return false;
		};
		let destination = package.join(relative);
		if relative == Path::new(CONFIG_PATH) {
			destination.is_file()
		} else {
			fs::read(destination).ok().as_deref() == Some(*bytes)
		}
	})
}

#[cfg(any(carbon_bundled_rml, test))]
fn safe_bundle_path(relative: &str) -> Result<&Path> {
	let path = Path::new(relative);
	ensure!(!path.is_absolute(), "embedded RML path must be relative: {relative}");
	ensure!(
		path.components()
			.all(|component| matches!(component, std::path::Component::Normal(_))),
		"embedded RML path is unsafe: {relative}"
	);
	Ok(path)
}

pub fn ensure_current(explicit_studio: Option<&Path>, explicit_package: Option<&Path>) -> Result<bool> {
	Ok(prepare_launch(explicit_studio, explicit_package)?.bootstrap_updated())
}

fn package_is_current(package: &Path) -> bool {
	let marker = fs::read(package.join(MARKER_PATH))
		.ok()
		.and_then(|bytes| serde_json::from_slice::<InstallMarker>(&bytes).ok());
	marker == Some(InstallMarker::current()) && REQUIRED_FILES.iter().all(|path| package.join(path).is_file())
}

pub fn prepare_launch(explicit_studio: Option<&Path>, explicit_package: Option<&Path>) -> Result<Launch> {
	let studio_dir = match explicit_studio {
		Some(path) => path.to_owned(),
		None => latest_studio_dir()?,
	};
	let studio_executable = studio_dir.join("RobloxStudioBeta.exe");
	ensure!(
		studio_executable.is_file(),
		"Roblox Studio was not found in {}",
		studio_dir.display()
	);
	let package = package_dir(explicit_package)?;
	ensure!(
		package_is_current(&package),
		"RML bundle is incomplete or belongs to another Carbon build"
	);

	fs::create_dir_all(studio_dir.join("RobloxModLoader"))?;
	let lock = OpenOptions::new()
		.create(true)
		.truncate(false)
		.read(true)
		.write(true)
		.open(studio_dir.join(BOOTSTRAP_LOCK_PATH))
		.context("failed to open the shared RML bootstrap lock")?;
	lock.lock_shared().context("failed to lock the shared RML bootstrap")?;

	let mut bootstrap_updated = false;
	if !bootstrap_matches(&package, &studio_dir) {
		lock.unlock()
			.context("failed to upgrade the shared RML bootstrap lock")?;
		lock.lock()
			.context("failed to lock the shared RML bootstrap for update")?;
		if !bootstrap_matches(&package, &studio_dir) {
			ensure_studio_closed()?;
			copy_file(&package.join(BOOTSTRAP_PATH), &studio_dir.join(BOOTSTRAP_PATH))?;
			bootstrap_updated = true;
		}
	}

	ensure!(
		bootstrap_matches(&package, &studio_dir),
		"RML bootstrap installation did not pass verification"
	);
	info!("RobloxModLoader {} will load from {}", BUILD_VERSION, package.display());
	Ok(Launch {
		studio_executable: fs::canonicalize(studio_executable)?,
		loader_path: fs::canonicalize(package.join(LOADER_PATH))?,
		bootstrap_updated,
		_bootstrap_lock: lock,
	})
}

fn bootstrap_status(package: &Path, studio_dir: &Path) -> Status {
	if !studio_dir.join(BOOTSTRAP_PATH).is_file() {
		return Status::Incomplete(BOOTSTRAP_PATH);
	}
	if bootstrap_matches(package, studio_dir) {
		Status::Current
	} else {
		Status::Outdated
	}
}

fn bootstrap_matches(package: &Path, studio_dir: &Path) -> bool {
	let packaged = package.join(BOOTSTRAP_PATH);
	let installed = studio_dir.join(BOOTSTRAP_PATH);
	match (fs::read(packaged), fs::read(installed)) {
		(Ok(packaged), Ok(installed)) => bootstrap_bytes_compatible(&packaged, &installed),
		_ => false,
	}
}

fn bootstrap_bytes_compatible(packaged: &[u8], installed: &[u8]) -> bool {
	if packaged == installed {
		return true;
	}
	let mut packaged = packaged.to_vec();
	let mut installed = installed.to_vec();
	normalize_pe_timestamps(&mut packaged) && normalize_pe_timestamps(&mut installed) && packaged == installed
}

/// MSVC writes the link time into both the COFF header and each PE debug
/// directory entry. Fresh worktree builds of the source-identical proxy differ
/// only in those fields; the proxy ABI and behavior remain identical. Ignore
/// exactly those documented timestamps so an already-running Studio does not
/// force every worktree to replace the process-global bootstrap.
fn normalize_pe_timestamps(bytes: &mut [u8]) -> bool {
	fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
		Some(u16::from_le_bytes(
			bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
		))
	}
	fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
		Some(u32::from_le_bytes(
			bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
		))
	}
	fn clear_u32(bytes: &mut [u8], offset: usize) -> Option<()> {
		bytes.get_mut(offset..offset.checked_add(4)?)?.fill(0);
		Some(())
	}

	if bytes.get(..2) != Some(b"MZ") {
		return false;
	}
	let Some(pe_offset) = read_u32(bytes, 0x3c).and_then(|offset| usize::try_from(offset).ok()) else {
		return false;
	};
	if bytes.get(pe_offset..pe_offset.saturating_add(4)) != Some(b"PE\0\0") {
		return false;
	}
	let Some(coff) = pe_offset.checked_add(4) else {
		return false;
	};
	let Some(section_count) = read_u16(bytes, coff + 2).map(usize::from) else {
		return false;
	};
	let Some(optional_size) = read_u16(bytes, coff + 16).map(usize::from) else {
		return false;
	};
	if clear_u32(bytes, coff + 4).is_none() {
		return false;
	}
	let Some(optional) = coff.checked_add(20) else {
		return false;
	};
	let Some((directory_count_offset, directories_offset)) = read_u16(bytes, optional).and_then(|magic| match magic {
		0x10b => Some((optional + 92, optional + 96)),
		0x20b => Some((optional + 108, optional + 112)),
		_ => None,
	}) else {
		return false;
	};
	let Some(directory_count) = read_u32(bytes, directory_count_offset) else {
		return false;
	};
	if directory_count <= 6 {
		return true;
	}
	let Some(debug_directory) = directories_offset.checked_add(6 * 8) else {
		return false;
	};
	let Some(debug_rva) = read_u32(bytes, debug_directory) else {
		return false;
	};
	let Some(debug_size) = read_u32(bytes, debug_directory + 4).and_then(|size| usize::try_from(size).ok()) else {
		return false;
	};
	if debug_rva == 0 && debug_size == 0 {
		return true;
	}
	if debug_rva == 0 || debug_size == 0 || debug_size % 28 != 0 {
		return false;
	}
	let Some(section_table) = optional.checked_add(optional_size) else {
		return false;
	};
	let debug_rva = u64::from(debug_rva);
	let mut debug_offset = None;
	for index in 0..section_count {
		let Some(section) = section_table.checked_add(index.saturating_mul(40)) else {
			return false;
		};
		let Some(virtual_size) = read_u32(bytes, section + 8) else {
			return false;
		};
		let Some(virtual_address) = read_u32(bytes, section + 12) else {
			return false;
		};
		let Some(raw_size) = read_u32(bytes, section + 16) else {
			return false;
		};
		let Some(raw_offset) = read_u32(bytes, section + 20) else {
			return false;
		};
		let start = u64::from(virtual_address);
		let end = start.saturating_add(u64::from(virtual_size.max(raw_size)));
		if (start..end).contains(&debug_rva) {
			let offset = u64::from(raw_offset).saturating_add(debug_rva - start);
			debug_offset = usize::try_from(offset).ok();
			break;
		}
	}
	let Some(debug_offset) = debug_offset else {
		return false;
	};
	for entry in (0..debug_size).step_by(28) {
		let Some(timestamp) = debug_offset.checked_add(entry).and_then(|offset| offset.checked_add(4)) else {
			return false;
		};
		if clear_u32(bytes, timestamp).is_none() {
			return false;
		}
	}
	true
}

fn copy_file(source: &Path, destination: &Path) -> Result<()> {
	ensure!(source.is_file(), "RML bundle file is missing: {}", source.display());
	if let Some(parent) = destination.parent() {
		fs::create_dir_all(parent)?;
	}
	fs::copy(source, destination)
		.with_context(|| format!("failed to copy {} to {}", source.display(), destination.display()))?;
	Ok(())
}

fn ensure_studio_closed() -> Result<()> {
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	{
		let status = Command::new("powershell.exe")
			.args([
				"-NoProfile",
				"-NonInteractive",
				"-Command",
				"if (Get-Process RobloxStudioBeta -ErrorAction SilentlyContinue) { exit 9 }",
			])
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status()
			.context("failed to inspect Roblox Studio processes")?;
		ensure!(
			status.success(),
			"Roblox Studio must be closed while Carbon updates RML"
		);
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use uuid::Uuid;

	fn temp(name: &str) -> PathBuf {
		let path = env::temp_dir().join(format!("carbon-rml-{name}-{}", Uuid::new_v4()));
		fs::create_dir_all(&path).unwrap();
		fs::canonicalize(path).unwrap()
	}

	fn fake_package(root: &Path) {
		for relative in REQUIRED_FILES {
			let path = root.join(relative);
			fs::create_dir_all(path.parent().unwrap()).unwrap();
			fs::write(path, relative.as_bytes()).unwrap();
		}
		fs::create_dir_all(root.join("RobloxModLoader/libraries")).unwrap();
		fs::write(root.join("RobloxModLoader/config.toml"), b"enabled = true\n").unwrap();
		fs::write(
			root.join(MARKER_PATH),
			serde_json::to_vec(&InstallMarker::current()).unwrap(),
		)
		.unwrap();
	}

	#[test]
	fn another_build_is_not_a_launchable_package() {
		let package = temp("outdated");
		fake_package(&package);
		fs::write(
			package.join(MARKER_PATH),
			serde_json::to_vec(&InstallMarker {
				schema_version: MARKER_SCHEMA,
				build_version: "0.0.0+gold".to_owned(),
			})
			.unwrap(),
		)
		.unwrap();
		assert!(!package_is_current(&package));
		fs::remove_dir_all(package).unwrap();
	}

	#[test]
	fn bundled_rml_files_install_missing_and_replace_stale_runtime_bytes() {
		let package = temp("bundled-install");
		let files: &[(&str, &[u8])] = &[
			("dwmapi.dll", b"bootstrap-v2"),
			("RobloxModLoader/roblox_modloader.dll", b"loader-v2"),
		];
		fs::write(package.join("dwmapi.dll"), b"bootstrap-v1").unwrap();

		let status = install_bundled_files(files, &package).unwrap();

		assert_eq!(status, BundleInstallStatus::Updated);
		assert_eq!(fs::read(package.join("dwmapi.dll")).unwrap(), b"bootstrap-v2");
		assert_eq!(
			fs::read(package.join("RobloxModLoader/roblox_modloader.dll")).unwrap(),
			b"loader-v2"
		);
		fs::remove_dir_all(package).unwrap();
	}

	#[test]
	fn bundled_rml_files_preserve_existing_user_configuration() {
		let package = temp("bundled-config");
		let config = package.join("RobloxModLoader/config.toml");
		fs::create_dir_all(config.parent().unwrap()).unwrap();
		fs::write(&config, b"user configuration\n").unwrap();
		let files: &[(&str, &[u8])] = &[
			("RobloxModLoader/config.toml", b"release default\n"),
			("RobloxModLoader/runtime/RML.Core.dll", b"runtime"),
		];

		install_bundled_files(files, &package).unwrap();

		assert_eq!(fs::read(config).unwrap(), b"user configuration\n");
		assert_eq!(
			fs::read(package.join("RobloxModLoader/runtime/RML.Core.dll")).unwrap(),
			b"runtime"
		);
		fs::remove_dir_all(package).unwrap();
	}

	#[test]
	fn launch_packages_are_process_scoped_and_do_not_replace_the_global_runtime() {
		let studio = temp("isolated-studio");
		let package_a = temp("isolated-package-a");
		let package_b = temp("isolated-package-b");
		fake_package(&package_a);
		fake_package(&package_b);
		fs::write(studio.join("RobloxStudioBeta.exe"), b"studio").unwrap();
		fs::write(studio.join("dwmapi.dll"), b"shared-bootstrap").unwrap();
		fs::create_dir_all(studio.join("RobloxModLoader")).unwrap();
		fs::write(studio.join("RobloxModLoader/roblox_modloader.dll"), b"global-sentinel").unwrap();
		fs::write(package_a.join("dwmapi.dll"), b"shared-bootstrap").unwrap();
		fs::write(package_b.join("dwmapi.dll"), b"shared-bootstrap").unwrap();
		fs::write(package_a.join("RobloxModLoader/roblox_modloader.dll"), b"worktree-a").unwrap();
		fs::write(package_b.join("RobloxModLoader/roblox_modloader.dll"), b"worktree-b").unwrap();

		let launch_a = prepare_launch(Some(&studio), Some(&package_a)).unwrap();
		let launch_b = prepare_launch(Some(&studio), Some(&package_b)).unwrap();

		assert_eq!(
			launch_a.loader_path(),
			package_a.join("RobloxModLoader/roblox_modloader.dll")
		);
		assert_eq!(
			launch_b.loader_path(),
			package_b.join("RobloxModLoader/roblox_modloader.dll")
		);
		assert_eq!(
			fs::read(studio.join("RobloxModLoader/roblox_modloader.dll")).unwrap(),
			b"global-sentinel"
		);

		drop(launch_a);
		drop(launch_b);
		fs::remove_dir_all(studio).unwrap();
		fs::remove_dir_all(package_a).unwrap();
		fs::remove_dir_all(package_b).unwrap();
	}

	#[test]
	fn source_identical_bootstraps_ignore_only_pe_build_timestamps() {
		fn bootstrap(coff_timestamp: u32, debug_timestamp: u32) -> Vec<u8> {
			let mut bytes = vec![0_u8; 512];
			bytes[..2].copy_from_slice(b"MZ");
			bytes[0x3c..0x40].copy_from_slice(&0x80_u32.to_le_bytes());
			bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
			let coff = 0x84;
			bytes[coff + 2..coff + 4].copy_from_slice(&1_u16.to_le_bytes());
			bytes[coff + 4..coff + 8].copy_from_slice(&coff_timestamp.to_le_bytes());
			bytes[coff + 16..coff + 18].copy_from_slice(&0xf0_u16.to_le_bytes());
			let optional = coff + 20;
			bytes[optional..optional + 2].copy_from_slice(&0x20b_u16.to_le_bytes());
			bytes[optional + 108..optional + 112].copy_from_slice(&7_u32.to_le_bytes());
			bytes[optional + 160..optional + 164].copy_from_slice(&0x1000_u32.to_le_bytes());
			bytes[optional + 164..optional + 168].copy_from_slice(&28_u32.to_le_bytes());
			let section = optional + 0xf0;
			bytes[section + 8..section + 12].copy_from_slice(&64_u32.to_le_bytes());
			bytes[section + 12..section + 16].copy_from_slice(&0x1000_u32.to_le_bytes());
			bytes[section + 16..section + 20].copy_from_slice(&64_u32.to_le_bytes());
			bytes[section + 20..section + 24].copy_from_slice(&448_u32.to_le_bytes());
			bytes[452..456].copy_from_slice(&debug_timestamp.to_le_bytes());
			bytes[500] = 42;
			bytes
		}

		let first = bootstrap(1, 2);
		let second = bootstrap(3, 4);
		assert!(bootstrap_bytes_compatible(&first, &second));
		let mut changed_code = second;
		changed_code[500] = 43;
		assert!(!bootstrap_bytes_compatible(&first, &changed_code));
	}
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	#[test]
	fn explicit_loader_injection_is_process_scoped_and_path_safe() {
		let loader = r"C:\Carbon\rml\loader'; throw 'unsafe\roblox_modloader.dll";
		let studio = r"C:\Roblox\version'; throw 'unsafe\RobloxStudioBeta.exe";
		let script = powershell_injection_script(47_312, loader, studio, 133_700_123_456);

		assert!(!script.contains(loader));
		assert!(!script.contains(studio));
		assert!(script.contains(&BASE64_STANDARD.encode(loader.as_bytes())));
		assert!(script.contains(&BASE64_STANDARD.encode(studio.as_bytes())));
		assert!(!script.contains("__CARBON_LOADER_BASE64__"));
		assert!(!script.contains("__CARBON_STUDIO_BASE64__"));
		assert!(!script.contains("GetExitCodeThread"));
		assert!(script.contains("$processId = 47312"));
		assert!(script.contains("$startedAtFileTime = [uint64]133700123456"));
		for operation in [
			"OpenProcess",
			"QueryFullProcessImageName",
			"GetProcessTimes",
			"VirtualAllocEx",
			"WriteProcessMemory",
			"CreateRemoteThread",
			"LoadLibraryW",
			"WaitForSingleObject",
			"VirtualFreeEx",
			"CARBON_RML_INJECTOR_READY",
			"CARBON_RML_INJECTOR_PROCEED",
			"CARBON_RML_INJECTOR_STARTED",
			"CreateSuspended",
			"ResumeThread",
		] {
			assert!(script.contains(operation), "missing {operation}");
		}
	}
}
