//! Native Linux hosts that run the Windows Roblox Studio build under Wine.
//!
//! Wine host mode is active only outside WSL when `ROBLOX_STUDIO_WINE_LAUNCHER`
//! names an executable file. The launcher is invoked as
//! `<launcher> <studio-exe-unix-path> <studio args...>` and must `exec` Wine on
//! the Studio executable, so the Studio process keeps the launcher's PID.

use anyhow::{anyhow, bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use std::{
	collections::BTreeSet,
	env,
	ffi::{OsStr, OsString},
	fs::{self, File},
	io::{Read, Seek, SeekFrom},
	os::{
		fd::{AsRawFd, FromRawFd, OwnedFd},
		unix::{fs::PermissionsExt, process::CommandExt},
	},
	path::{Path, PathBuf},
	process::{Child, Command, Stdio},
	sync::{LazyLock, OnceLock},
	thread,
	time::{Duration, Instant},
};

pub(crate) const LAUNCHER_ENV: &str = "ROBLOX_STUDIO_WINE_LAUNCHER";
const PREFIX_ENV: &str = "WINEPREFIX";
const STUDIO_EXE_ENV: &str = "ROBLOX_STUDIO_EXE";
/// Display and session variables Studio's Wine graphics driver may need.
const FORWARDED_DISPLAY_ENV: [&str; 4] = ["DISPLAY", "XAUTHORITY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR"];
const STUDIO_EXECUTABLE_NAME: &str = "RobloxStudioBeta.exe";
const STUDIO_EXEC_TIMEOUT: Duration = Duration::from_secs(30);
const STUDIO_STOP_GRACE: Duration = Duration::from_secs(10);
const STUDIO_KILL_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;
const FILETIME_TICKS_PER_SECOND: u64 = 10_000_000;
const DEFAULT_PROC_CLOCK_TICKS_PER_SECOND: u64 = 100;
const START_TIME_TOLERANCE: u64 = 2 * FILETIME_TICKS_PER_SECOND;
const MAX_RESOURCE_SECTION: u32 = 64 * 1024 * 1024;
const FIXED_FILE_INFO_SIGNATURE: [u8; 8] = [0xBD, 0x04, 0xEF, 0xFE, 0x00, 0x00, 0x01, 0x00];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WineHost {
	launcher: PathBuf,
}

impl WineHost {
	/// The host for this Carbon process, resolved once from its environment.
	pub(crate) fn current() -> Result<Option<&'static Self>> {
		static CURRENT: LazyLock<Result<Option<WineHost>, String>> =
			LazyLock::new(|| WineHost::from_env().map_err(|error| format!("{error:#}")));
		match &*CURRENT {
			Ok(host) => Ok(host.as_ref()),
			Err(error) => Err(anyhow!("{error}")),
		}
	}

	fn from_env() -> Result<Option<Self>> {
		Self::detect(env::var_os("WSL_DISTRO_NAME").is_some(), env::var_os(LAUNCHER_ENV))
	}

	/// A set launcher on native Linux must be usable; silently falling back to
	/// the WSL path would report unrelated PowerShell or `wslpath` failures.
	fn detect(wsl: bool, launcher: Option<OsString>) -> Result<Option<Self>> {
		let Some(launcher) = launcher.filter(|launcher| !launcher.is_empty()) else {
			return Ok(None);
		};
		if wsl {
			return Ok(None);
		}
		let launcher = PathBuf::from(launcher);
		let metadata = fs::metadata(&launcher)
			.with_context(|| format!("{LAUNCHER_ENV} names {}, which cannot be read", launcher.display()))?;
		ensure!(
			metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
			"{LAUNCHER_ENV} must name an executable file, but {} is not one",
			launcher.display()
		);
		Ok(Some(Self { launcher }))
	}

	/// The launcher command for one Studio invocation, before Studio arguments.
	pub(crate) fn command(&self, studio_executable: &Path) -> Command {
		let mut command = Command::new(&self.launcher);
		command.arg(studio_executable);
		command
	}

	/// Studio's `LOCALAPPDATA` inside the Wine prefix named by `WINEPREFIX`.
	pub(crate) fn local_app_data(&self, override_env: &str) -> Result<PathBuf> {
		local_app_data(&prefix(env::var_os(PREFIX_ENV))?, override_env)
	}

	/// The newest Studio the Roblox installer placed in the prefix, matching
	/// Windows discovery so Studio's self-updates are picked up.
	pub(crate) fn installed_studio(&self) -> Result<PathBuf> {
		newest_studio_executable(&self.local_app_data(STUDIO_EXE_ENV)?.join("Roblox/Versions"))
	}

	/// Launch an unmanaged Studio and return its exact PID and creation FILETIME.
	pub(crate) fn launch(&self, studio_executable: &Path, arguments: &[OsString]) -> Result<(u32, u64)> {
		let studio = self.spawn(studio_executable, arguments)?;
		Ok((studio.process_id, studio.creation_filetime))
	}

	/// Launch a Studio that this process keeps as its child until `stop`.
	pub(crate) fn spawn(&self, studio_executable: &Path, arguments: &[OsString]) -> Result<OwnedStudio> {
		launch(
			ProcFs::system(),
			self.command(studio_executable).args(arguments),
			STUDIO_EXEC_TIMEOUT,
		)
	}
}

/// The most recently written `<version>/RobloxStudioBeta.exe` under `versions`.
fn newest_studio_executable(versions: &Path) -> Result<PathBuf> {
	let entries = fs::read_dir(versions).with_context(|| {
		format!(
			"failed to read Roblox Studio versions {}; set {STUDIO_EXE_ENV}",
			versions.display()
		)
	})?;
	let mut newest = None;
	for entry in entries {
		let entry = entry.with_context(|| format!("failed to read Roblox Studio versions {}", versions.display()))?;
		let executable = entry.path().join(STUDIO_EXECUTABLE_NAME);
		let Ok(metadata) = fs::metadata(&executable) else {
			continue;
		};
		if !metadata.is_file() {
			continue;
		}
		let modified = metadata
			.modified()
			.with_context(|| format!("failed to read the modification time of {}", executable.display()))?;
		if newest.as_ref().is_none_or(|(newest, _)| modified > *newest) {
			newest = Some((modified, executable));
		}
	}
	newest.map(|(_, executable)| executable).with_context(|| {
		format!(
			"Roblox Studio is not installed under {}; install it in the Wine prefix or set {STUDIO_EXE_ENV}",
			versions.display()
		)
	})
}

fn prefix(value: Option<OsString>) -> Result<PathBuf> {
	let prefix = PathBuf::from(
		value
			.filter(|value| !value.is_empty())
			.with_context(|| format!("Roblox Studio on a Linux Wine host requires {PREFIX_ENV}"))?,
	);
	ensure!(
		prefix.is_absolute(),
		"{PREFIX_ENV} must be an absolute path: {}",
		prefix.display()
	);
	Ok(prefix)
}

/// Resolve `drive_c/users/<user>/AppData/Local` for the single Wine profile
/// that owns one. Proton links `<$USER>` to `steamuser`, so candidates are
/// deduplicated by their canonical path.
fn local_app_data(prefix: &Path, override_env: &str) -> Result<PathBuf> {
	let users = prefix.join("drive_c/users");
	let entries =
		fs::read_dir(&users).with_context(|| format!("failed to read Wine prefix users {}", users.display()))?;
	let mut candidates = BTreeSet::new();
	for entry in entries {
		let entry = entry.with_context(|| format!("failed to read Wine prefix users {}", users.display()))?;
		if entry.file_name().eq_ignore_ascii_case("Public") {
			continue;
		}
		let local = entry.path().join("AppData/Local");
		if local.is_dir() {
			candidates.insert(
				fs::canonicalize(&local)
					.with_context(|| format!("failed to resolve Wine profile {}", local.display()))?,
			);
		}
	}
	let mut candidates = candidates.into_iter();
	match (candidates.next(), candidates.next()) {
		(Some(local), None) => Ok(local),
		(None, _) => bail!(
			"no Wine user profile under {} contains AppData/Local; set {override_env}",
			users.display()
		),
		(Some(first), Some(second)) => {
			let all = [first, second]
				.into_iter()
				.chain(candidates)
				.map(|path| path.display().to_string())
				.collect::<Vec<_>>();
			bail!(
				"several Wine user profiles under {} contain AppData/Local ({}); set {override_env}",
				users.display(),
				all.join(", ")
			)
		}
	}
}

/// The `process_environment` patch that runs a broker-launched Studio in
/// Carbon's Wine prefix and display.
pub(crate) fn process_environment(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Value> {
	let prefix = prefix(lookup(PREFIX_ENV))?;
	let mut set = Map::new();
	set.insert(PREFIX_ENV.to_owned(), json!(utf8(prefix.as_os_str(), PREFIX_ENV)?));
	for name in FORWARDED_DISPLAY_ENV {
		if let Some(value) = lookup(name).filter(|value| !value.is_empty()) {
			set.insert(name.to_owned(), json!(utf8(&value, name)?));
		}
	}
	Ok(json!({ "set": set }))
}

fn utf8<'a>(value: &'a OsStr, name: &str) -> Result<&'a str> {
	value.to_str().with_context(|| format!("{name} is not valid UTF-8"))
}

/// Wine maps drive `Z:` to `/`.
pub(crate) fn wine_path(path: &Path) -> Result<String> {
	ensure!(
		path.is_absolute(),
		"Wine path translation requires an absolute path: {}",
		path.display()
	);
	let path = path
		.to_str()
		.with_context(|| format!("path is not valid UTF-8: {}", path.display()))?;
	Ok(format!("Z:{}", path.replace('/', "\\")))
}

/// Read the `StringFileInfo` `FileVersion` of a Windows executable without Windows.
pub(crate) fn studio_file_version(executable: &Path) -> Result<String> {
	let mut file = File::open(executable)
		.with_context(|| format!("failed to open Roblox Studio executable {}", executable.display()))?;
	let resources =
		pe_resources(&mut file).with_context(|| format!("failed to read PE resources of {}", executable.display()))?;
	version_info_file_version(&resources).with_context(|| {
		format!(
			"failed to read the Roblox Studio file version of {}",
			executable.display()
		)
	})
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
	let field = bytes.get(offset..offset + 2).context("PE structure is truncated")?;
	Ok(u16::from_le_bytes([field[0], field[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
	let field = bytes.get(offset..offset + 4).context("PE structure is truncated")?;
	Ok(u32::from_le_bytes([field[0], field[1], field[2], field[3]]))
}

fn read_at(reader: &mut (impl Read + Seek), offset: u64, length: usize) -> Result<Vec<u8>> {
	reader.seek(SeekFrom::Start(offset))?;
	let mut bytes = vec![0; length];
	reader.read_exact(&mut bytes).context("PE file is truncated")?;
	Ok(bytes)
}

/// Read only the resource data directory of a PE image.
fn pe_resources(reader: &mut (impl Read + Seek)) -> Result<Vec<u8>> {
	let dos = read_at(reader, 0, 64)?;
	ensure!(dos.starts_with(b"MZ"), "missing DOS header");
	let pe_offset = u64::from(read_u32(&dos, 0x3C)?);
	let file_header = read_at(reader, pe_offset, 24)?;
	ensure!(file_header.starts_with(b"PE\0\0"), "missing PE signature");
	let sections = usize::from(read_u16(&file_header, 6)?);
	let optional_size = usize::from(read_u16(&file_header, 20)?);
	let optional = read_at(reader, pe_offset + 24, optional_size)?;
	let directories = match read_u16(&optional, 0)? {
		0x10B => 96,
		0x20B => 112,
		magic => bail!("unsupported PE optional header magic {magic:#x}"),
	};
	ensure!(
		read_u32(&optional, directories - 4)? > 2,
		"PE image has no resource directory"
	);
	let resource_rva = read_u32(&optional, directories + 16)?;
	let resource_size = read_u32(&optional, directories + 20)?;
	ensure!(resource_rva != 0 && resource_size != 0, "PE image has no resources");
	ensure!(
		resource_size <= MAX_RESOURCE_SECTION,
		"PE resource directory is implausibly large"
	);
	let table = read_at(reader, pe_offset + 24 + optional_size as u64, sections * 40)?;
	for section in table.chunks_exact(40) {
		let virtual_size = read_u32(section, 8)?;
		let virtual_address = read_u32(section, 12)?;
		let raw_size = read_u32(section, 16)?;
		let raw_offset = read_u32(section, 20)?;
		let Some(start) = resource_rva.checked_sub(virtual_address) else {
			continue;
		};
		if start >= virtual_size.max(raw_size) {
			continue;
		}
		ensure!(
			u64::from(start) + u64::from(resource_size) <= u64::from(raw_size),
			"PE resource directory exceeds its section"
		);
		return read_at(reader, u64::from(raw_offset) + u64::from(start), resource_size as usize);
	}
	bail!("PE resource directory is outside every section")
}

fn utf16_key(key: &str) -> Vec<u8> {
	key.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

/// Return the `FileVersion` string from the `VS_VERSIONINFO` resource. Windows
/// reports this string (for example `0, 740, 19, 7400003`) as the file version;
/// `VS_FIXEDFILEINFO` truncates each component to 16 bits, so it only
/// corroborates the string.
fn version_info_file_version(resources: &[u8]) -> Result<String> {
	let signature = resources
		.windows(FIXED_FILE_INFO_SIGNATURE.len())
		.position(|window| window == FIXED_FILE_INFO_SIGNATURE)
		.context("no VS_FIXEDFILEINFO resource")?;
	let block_start = signature
		.checked_sub(40)
		.context("VS_VERSIONINFO header is truncated")?;
	let block_length = usize::from(read_u16(resources, block_start)?);
	let block = resources
		.get(block_start..block_start + block_length)
		.context("VS_VERSIONINFO block is truncated")?;
	ensure!(
		block.get(6..38) == Some(utf16_key("VS_VERSION_INFO").as_slice()),
		"VS_FIXEDFILEINFO is not inside VS_VERSIONINFO"
	);
	let fixed = [
		read_u32(block, 48)? >> 16,
		read_u32(block, 48)? & 0xFFFF,
		read_u32(block, 52)? >> 16,
		read_u32(block, 52)? & 0xFFFF,
	];

	let key = utf16_key("FileVersion");
	let key_offset = (40 + 52..block.len().saturating_sub(key.len()))
		.step_by(2)
		.find(|offset| block[*offset..].starts_with(&key))
		.context("VS_VERSIONINFO has no FileVersion string")?;
	// Some toolchains write the value length in bytes instead of words, so
	// read at most to the end of the block and stop at the terminator.
	let value_words = usize::from(read_u16(block, key_offset - 4)?);
	let value_offset = (key_offset + key.len() + 3) & !3;
	let value_end = (value_offset + value_words * 2).min(block.len());
	let value = block
		.get(value_offset..value_end)
		.context("FileVersion string is truncated")?
		.chunks_exact(2)
		.map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
		.take_while(|unit| *unit != 0)
		.collect::<Vec<_>>();
	let value = String::from_utf16(&value).context("FileVersion string is not valid UTF-16")?;
	let value = value.trim().to_owned();

	let (_, components) = crate::studio::parse_version(&value)?;
	ensure!(
		components[..3] == fixed[..3] && components[3] & 0xFFFF == fixed[3],
		"FileVersion string {value:?} disagrees with VS_FIXEDFILEINFO {}.{}.{}.{}",
		fixed[0],
		fixed[1],
		fixed[2],
		fixed[3]
	);
	Ok(value)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcStat {
	state: char,
	start_ticks: u64,
}

fn parse_proc_stat(contents: &str) -> Result<ProcStat> {
	let (_, fields) = contents
		.rsplit_once(')')
		.context("process stat has no command terminator")?;
	let fields = fields.split_whitespace().collect::<Vec<_>>();
	// Field 3 (state) is the first field after the command; field 22 is starttime.
	let state = fields
		.first()
		.and_then(|state| state.chars().next())
		.context("process stat has no state")?;
	let start_ticks = fields
		.get(19)
		.context("process stat has no start time")?
		.parse()
		.context("process stat has an invalid start time")?;
	Ok(ProcStat { state, start_ticks })
}

fn parse_boot_time(contents: &str) -> Result<u64> {
	contents
		.lines()
		.find_map(|line| line.strip_prefix("btime "))
		.context("kernel stat has no btime")?
		.trim()
		.parse()
		.context("kernel stat has an invalid btime")
}

fn start_filetime(boot_time: u64, start_ticks: u64, clock_ticks_per_second: u64) -> u64 {
	FILETIME_UNIX_EPOCH
		+ boot_time * FILETIME_TICKS_PER_SECOND
		+ start_ticks * FILETIME_TICKS_PER_SECOND / clock_ticks_per_second
}

/// `/proc` start times are clock ticks since boot, so both Carbon and the
/// broker add `btime`, which each reads at a different moment. A wall-clock
/// step between those readings shifts `btime`; tolerate a small difference.
fn start_time_matches(expected: u64, actual: u64) -> bool {
	expected.abs_diff(actual) <= START_TIME_TOLERANCE
}

fn clock_ticks_per_second() -> u64 {
	// SAFETY: sysconf only reads a system configuration value.
	let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
	u64::try_from(ticks)
		.ok()
		.filter(|ticks| *ticks > 0)
		.unwrap_or(DEFAULT_PROC_CLOCK_TICKS_PER_SECOND)
}

fn is_studio_command_line(command_line: &[u8], executable_name: &str) -> bool {
	let program = command_line.split(|byte| *byte == 0).next().unwrap_or_default();
	program
		.rsplit(|byte| matches!(byte, b'/' | b'\\'))
		.next()
		.is_some_and(|name| name.eq_ignore_ascii_case(executable_name.as_bytes()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessIdentity {
	state: char,
	creation_filetime: u64,
	studio: bool,
}

impl ProcessIdentity {
	fn exited(&self) -> bool {
		matches!(self.state, 'Z' | 'X')
	}

	fn matches(&self, creation_filetime: u64) -> bool {
		self.studio && start_time_matches(creation_filetime, self.creation_filetime)
	}
}

struct ProcFs {
	root: PathBuf,
	executable_name: &'static str,
	clock_ticks_per_second: u64,
	boot_time: OnceLock<u64>,
}

impl ProcFs {
	fn system() -> &'static Self {
		static SYSTEM: LazyLock<ProcFs> = LazyLock::new(|| ProcFs::new("/proc", STUDIO_EXECUTABLE_NAME));
		&SYSTEM
	}

	fn new(root: impl Into<PathBuf>, executable_name: &'static str) -> Self {
		Self {
			root: root.into(),
			executable_name,
			clock_ticks_per_second: clock_ticks_per_second(),
			boot_time: OnceLock::new(),
		}
	}

	fn boot_time(&self) -> Result<u64> {
		if let Some(boot_time) = self.boot_time.get() {
			return Ok(*boot_time);
		}
		let path = self.root.join("stat");
		let contents = fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
		let boot_time = parse_boot_time(&contents).with_context(|| format!("invalid {}", path.display()))?;
		Ok(*self.boot_time.get_or_init(|| boot_time))
	}

	fn read_process_file(&self, process_id: u32, name: &str) -> Result<Option<Vec<u8>>> {
		let path = self.root.join(process_id.to_string()).join(name);
		match fs::read(&path) {
			Ok(contents) => Ok(Some(contents)),
			Err(error)
				if matches!(error.kind(), std::io::ErrorKind::NotFound)
					|| error.raw_os_error() == Some(libc::ESRCH) =>
			{
				Ok(None)
			}
			Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
		}
	}

	fn identity(&self, process_id: u32) -> Result<Option<ProcessIdentity>> {
		let Some(stat) = self.read_process_file(process_id, "stat")? else {
			return Ok(None);
		};
		let stat = parse_proc_stat(&String::from_utf8_lossy(&stat))
			.with_context(|| format!("invalid process stat for PID {process_id}"))?;
		let studio = self
			.read_process_file(process_id, "cmdline")?
			.is_some_and(|command_line| is_studio_command_line(&command_line, self.executable_name));
		Ok(Some(ProcessIdentity {
			state: stat.state,
			creation_filetime: start_filetime(self.boot_time()?, stat.start_ticks, self.clock_ticks_per_second),
			studio,
		}))
	}

	fn studio_processes(&self) -> Result<Vec<u32>> {
		let entries = fs::read_dir(&self.root).with_context(|| format!("failed to read {}", self.root.display()))?;
		let mut processes = Vec::new();
		for entry in entries.flatten() {
			let Some(process_id) = entry.file_name().to_str().and_then(|name| name.parse().ok()) else {
				continue;
			};
			if self
				.identity(process_id)
				.ok()
				.flatten()
				.is_some_and(|identity| identity.studio && !identity.exited())
			{
				processes.push(process_id);
			}
		}
		Ok(processes)
	}
}

/// A pidfd pins one process, so signals cannot reach a recycled PID.
struct PidFd(OwnedFd);

impl PidFd {
	fn open(process_id: u32) -> Result<Option<Self>> {
		let pid = libc::pid_t::try_from(process_id).context("process ID is out of range")?;
		// SAFETY: pidfd_open takes a PID and flags and returns a new descriptor or -1.
		let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
		if fd < 0 {
			let error = std::io::Error::last_os_error();
			match error.raw_os_error() {
				Some(libc::ESRCH) => return Ok(None),
				Some(libc::ENOSYS) => {
					bail!("controlling Roblox Studio on a Linux Wine host requires Linux 5.3 or newer (pidfd_open)")
				}
				_ => return Err(error).with_context(|| format!("failed to open PID {process_id}")),
			}
		}
		// SAFETY: the kernel returned a new descriptor that this value now owns.
		Ok(Some(Self(unsafe { OwnedFd::from_raw_fd(fd as i32) })))
	}

	/// Returns false when the process has already exited.
	fn signal(&self, signal: libc::c_int) -> Result<bool> {
		// SAFETY: the descriptor is a live pidfd; a null siginfo is permitted.
		let result = unsafe {
			libc::syscall(
				libc::SYS_pidfd_send_signal,
				self.0.as_raw_fd(),
				signal,
				std::ptr::null::<libc::siginfo_t>(),
				0,
			)
		};
		if result == 0 {
			return Ok(true);
		}
		let error = std::io::Error::last_os_error();
		if error.raw_os_error() == Some(libc::ESRCH) {
			return Ok(false);
		}
		Err(error).context("failed to signal Roblox Studio")
	}

	/// Wait until the process exits; `None` waits indefinitely.
	fn wait(&self, timeout: Option<Duration>) -> Result<bool> {
		let deadline = timeout.map(|timeout| Instant::now() + timeout);
		loop {
			let remaining = match deadline {
				Some(deadline) => {
					let remaining = deadline.saturating_duration_since(Instant::now());
					if remaining.is_zero() {
						return Ok(false);
					}
					i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX)
				}
				None => -1,
			};
			let mut poll = libc::pollfd {
				fd: self.0.as_raw_fd(),
				events: libc::POLLIN,
				revents: 0,
			};
			// SAFETY: one valid pollfd is passed for the duration of the call.
			let ready = unsafe { libc::poll(&mut poll, 1, remaining) };
			if ready > 0 {
				return Ok(true);
			}
			if ready < 0 {
				let error = std::io::Error::last_os_error();
				if error.kind() != std::io::ErrorKind::Interrupted {
					return Err(error).context("failed to wait for Roblox Studio");
				}
			}
		}
	}
}

/// A Wine Studio this Carbon process launched and still owns as its child.
#[derive(Debug)]
pub(crate) struct OwnedStudio {
	child: Child,
	process_id: u32,
	creation_filetime: u64,
}

impl OwnedStudio {
	/// Whether Studio has exited; reaps it when it has.
	pub(crate) fn exited(&mut self) -> Result<bool> {
		Ok(self.child.try_wait()?.is_some())
	}

	/// Stop exactly this Studio, then reap it.
	pub(crate) fn stop(self) -> Result<()> {
		self.stop_with(ProcFs::system())
	}

	fn stop_with(mut self, proc: &ProcFs) -> Result<()> {
		// A reaped PID may already belong to another process, so never signal it.
		if self.exited()? {
			return Ok(());
		}
		// The unreaped child cannot be recycled, so the fallback kill is exact.
		let stopped = terminate_with(proc, self.process_id, self.creation_filetime);
		if stopped.is_err() {
			let _ = self.child.kill();
		}
		self.child.wait().context("failed to reap Roblox Studio")?;
		stopped
	}
}

fn launch(proc: &ProcFs, command: &mut Command, exec_timeout: Duration) -> Result<OwnedStudio> {
	let mut child = command
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.process_group(0)
		.spawn()
		.with_context(|| format!("failed to run the Wine Studio launcher from {LAUNCHER_ENV}"))?;
	match await_studio_exec(proc, &mut child, exec_timeout) {
		Ok((process_id, creation_filetime)) => Ok(OwnedStudio {
			child,
			process_id,
			creation_filetime,
		}),
		Err(error) => {
			let _ = child.kill();
			let _ = child.wait();
			Err(error)
		}
	}
}

/// Wait until the launcher has exec'd Studio in place and return its identity.
fn await_studio_exec(proc: &ProcFs, child: &mut Child, exec_timeout: Duration) -> Result<(u32, u64)> {
	let process_id = child.id();
	// The unreaped child keeps its PID, so this identity is exact.
	let identity = proc
		.identity(process_id)?
		.context("the Wine Studio launcher exited immediately")?;
	let deadline = Instant::now() + exec_timeout;
	loop {
		if let Some(status) = child.try_wait()? {
			bail!("the Wine Studio launcher exited ({status}) before Roblox Studio started");
		}
		if proc
			.identity(process_id)?
			.is_some_and(|current| current.studio && current.creation_filetime == identity.creation_filetime)
		{
			return Ok((process_id, identity.creation_filetime));
		}
		ensure!(
			Instant::now() < deadline,
			"the Wine Studio launcher did not exec {} within {} seconds; it must exec Wine in place so Studio keeps its PID",
			proc.executable_name,
			exec_timeout.as_secs()
		);
		thread::sleep(POLL_INTERVAL);
	}
}

pub(crate) fn terminate(process_id: u32, creation_filetime: u64) -> Result<()> {
	terminate_with(ProcFs::system(), process_id, creation_filetime)
}

fn terminate_with(proc: &ProcFs, process_id: u32, creation_filetime: u64) -> Result<()> {
	let Some(pidfd) = PidFd::open(process_id)? else {
		return Ok(());
	};
	let Some(identity) = proc.identity(process_id)? else {
		return Ok(());
	};
	if identity.exited() {
		return Ok(());
	}
	ensure!(
		identity.matches(creation_filetime),
		"PID {process_id} is no longer the managed Roblox Studio process"
	);
	if !pidfd.signal(libc::SIGTERM)? || pidfd.wait(Some(STUDIO_STOP_GRACE))? {
		return Ok(());
	}
	if !pidfd.signal(libc::SIGKILL)? {
		return Ok(());
	}
	ensure!(
		pidfd.wait(Some(STUDIO_KILL_TIMEOUT))?,
		"managed Roblox Studio process {process_id} did not exit after SIGKILL"
	);
	Ok(())
}

pub(crate) fn wait_for_exit(process_id: u32) -> Result<()> {
	if let Some(pidfd) = PidFd::open(process_id)? {
		pidfd.wait(None)?;
	}
	Ok(())
}

pub(crate) fn is_running() -> Result<bool> {
	Ok(!ProcFs::system().studio_processes()?.is_empty())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::os::unix::fs::symlink;

	fn executable(path: &Path, mode: u32) {
		fs::write(path, "#!/bin/sh\n").unwrap();
		fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
	}

	#[test]
	fn wine_host_requires_native_linux_and_an_executable_launcher() {
		let directory = tempfile::tempdir().unwrap();
		let launcher = directory.path().join("studio-wine");
		executable(&launcher, 0o755);
		let plain = directory.path().join("plain");
		executable(&plain, 0o644);

		assert_eq!(
			WineHost::detect(false, Some(launcher.clone().into())).unwrap(),
			Some(WineHost {
				launcher: launcher.clone()
			})
		);
		assert_eq!(WineHost::detect(true, Some(launcher.into())).unwrap(), None);
		assert_eq!(WineHost::detect(false, None).unwrap(), None);
		assert_eq!(WineHost::detect(false, Some(OsString::new())).unwrap(), None);
	}

	#[test]
	fn wine_host_rejects_a_launcher_that_is_not_an_executable_file() {
		let directory = tempfile::tempdir().unwrap();
		let plain = directory.path().join("plain");
		executable(&plain, 0o644);

		for launcher in [plain, directory.path().to_path_buf(), directory.path().join("missing")] {
			let error = WineHost::detect(false, Some(launcher.clone().into())).unwrap_err();
			let message = format!("{error:#}");
			assert!(message.contains(LAUNCHER_ENV), "{message}");
			assert!(message.contains(&launcher.display().to_string()), "{message}");
		}
		assert_eq!(
			WineHost::detect(true, Some(directory.path().join("missing").into())).unwrap(),
			None
		);
	}

	fn version_resource(file_version: &str, fixed_ms: u32, fixed_ls: u32) -> Vec<u8> {
		version_resource_with_value_length(file_version, fixed_ms, fixed_ls, 1)
	}

	/// `value_length_unit` is 1 for the documented word count, 2 for toolchains
	/// that write a byte count.
	fn version_resource_with_value_length(
		file_version: &str,
		fixed_ms: u32,
		fixed_ls: u32,
		value_length_unit: usize,
	) -> Vec<u8> {
		fn pad(bytes: &mut Vec<u8>) {
			while !bytes.len().is_multiple_of(4) {
				bytes.push(0);
			}
		}
		let mut string = Vec::new();
		let value = utf16_key(file_version);
		string.extend([0, 0]);
		string.extend(
			u16::try_from(value.len() / 2 * value_length_unit)
				.unwrap()
				.to_le_bytes(),
		);
		string.extend(1u16.to_le_bytes());
		string.extend(utf16_key("FileVersion"));
		pad(&mut string);
		string.extend(value);
		pad(&mut string);
		let length = u16::try_from(string.len()).unwrap().to_le_bytes();
		string[..2].copy_from_slice(&length);

		let mut block = Vec::new();
		block.extend([0, 0, 52, 0, 0, 0]);
		block.extend(utf16_key("VS_VERSION_INFO"));
		pad(&mut block);
		block.extend(FIXED_FILE_INFO_SIGNATURE);
		block.extend(fixed_ms.to_le_bytes());
		block.extend(fixed_ls.to_le_bytes());
		block.extend([0; 52 - 16]);
		block.extend(utf16_key("StringFileInfo"));
		pad(&mut block);
		block.extend(string);
		let length = u16::try_from(block.len()).unwrap().to_le_bytes();
		block[..2].copy_from_slice(&length);

		let mut resources = vec![0xAA; 20];
		resources.extend(block);
		resources
	}

	fn pe_image(resources: &[u8]) -> Vec<u8> {
		const PE_OFFSET: usize = 0x40;
		const OPTIONAL_SIZE: usize = 240;
		const RAW_OFFSET: usize = 0x200;
		const RESOURCE_RVA: u32 = 0x3000;
		let mut image = vec![0; RAW_OFFSET];
		image[..2].copy_from_slice(b"MZ");
		image[0x3C..0x40].copy_from_slice(&(PE_OFFSET as u32).to_le_bytes());
		image[PE_OFFSET..PE_OFFSET + 4].copy_from_slice(b"PE\0\0");
		image[PE_OFFSET + 6..PE_OFFSET + 8].copy_from_slice(&2u16.to_le_bytes());
		image[PE_OFFSET + 20..PE_OFFSET + 22].copy_from_slice(&(OPTIONAL_SIZE as u16).to_le_bytes());
		let optional = PE_OFFSET + 24;
		image[optional..optional + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
		image[optional + 108..optional + 112].copy_from_slice(&16u32.to_le_bytes());
		image[optional + 128..optional + 132].copy_from_slice(&(RESOURCE_RVA + 8).to_le_bytes());
		image[optional + 132..optional + 136].copy_from_slice(&(resources.len() as u32).to_le_bytes());
		let mut section = |index: usize, name: &[u8], rva: u32, raw: usize| {
			let header = optional + OPTIONAL_SIZE + index * 40;
			image[header..header + name.len()].copy_from_slice(name);
			image[header + 8..header + 12].copy_from_slice(&0x1000u32.to_le_bytes());
			image[header + 12..header + 16].copy_from_slice(&rva.to_le_bytes());
			image[header + 16..header + 20].copy_from_slice(&0x1000u32.to_le_bytes());
			image[header + 20..header + 24].copy_from_slice(&(raw as u32).to_le_bytes());
		};
		section(0, b".text", 0x1000, RAW_OFFSET);
		section(1, b".rsrc", RESOURCE_RVA, RAW_OFFSET + 0x1000);
		image.resize(RAW_OFFSET + 0x2000, 0);
		let start = RAW_OFFSET + 0x1000 + 8;
		image[start..start + resources.len()].copy_from_slice(resources);
		image
	}

	#[test]
	fn studio_file_version_reads_the_version_string_from_pe_resources() {
		let resources = version_resource("0, 740, 19, 7400003", 740, (19 << 16) | (7_400_003 & 0xFFFF));
		assert_eq!(version_info_file_version(&resources).unwrap(), "0, 740, 19, 7400003");

		let directory = tempfile::tempdir().unwrap();
		let executable = directory.path().join("RobloxStudioBeta.exe");
		fs::write(&executable, pe_image(&resources)).unwrap();
		assert_eq!(studio_file_version(&executable).unwrap(), "0, 740, 19, 7400003");
	}

	#[test]
	fn studio_file_version_rejects_missing_or_inconsistent_version_resources() {
		let mismatched = version_resource("0, 740, 19, 7400003", 741, 19 << 16);
		assert!(format!("{:#}", version_info_file_version(&mismatched).unwrap_err()).contains("disagrees"));
		assert!(version_info_file_version(&[0; 128]).is_err());

		let directory = tempfile::tempdir().unwrap();
		let executable = directory.path().join("RobloxStudioBeta.exe");
		fs::write(&executable, b"#!/bin/sh\n").unwrap();
		assert!(studio_file_version(&executable).is_err());
	}

	#[test]
	fn studio_file_version_accepts_a_byte_counted_value_length() {
		let resources =
			version_resource_with_value_length("0, 740, 19, 7400003", 740, (19 << 16) | (7_400_003 & 0xFFFF), 2);
		assert_eq!(version_info_file_version(&resources).unwrap(), "0, 740, 19, 7400003");
	}

	fn studio_version(versions: &Path, version: &str, modified: std::time::SystemTime) -> PathBuf {
		let directory = versions.join(version);
		fs::create_dir_all(&directory).unwrap();
		let executable = directory.join(STUDIO_EXECUTABLE_NAME);
		File::create(&executable).unwrap().set_modified(modified).unwrap();
		executable
	}

	#[test]
	fn installed_studio_is_the_most_recently_written_version() {
		let versions = tempfile::tempdir().unwrap();
		let epoch = std::time::SystemTime::UNIX_EPOCH;
		studio_version(
			versions.path(),
			"version-old",
			epoch + Duration::from_secs(1_700_000_000),
		);
		let newest = studio_version(
			versions.path(),
			"version-new",
			epoch + Duration::from_secs(1_800_000_000),
		);
		fs::create_dir_all(versions.path().join("version-empty")).unwrap();
		fs::write(versions.path().join("stray.txt"), "").unwrap();

		assert_eq!(newest_studio_executable(versions.path()).unwrap(), newest);
	}

	#[test]
	fn installed_studio_requires_a_studio_version() {
		let versions = tempfile::tempdir().unwrap();
		fs::create_dir_all(versions.path().join("version-empty")).unwrap();
		let error = newest_studio_executable(versions.path()).unwrap_err();
		assert!(format!("{error:#}").contains("ROBLOX_STUDIO_EXE"));
		assert!(newest_studio_executable(&versions.path().join("missing")).is_err());
	}

	fn profile(prefix: &Path, user: &str) -> PathBuf {
		let local = prefix.join("drive_c/users").join(user).join("AppData/Local");
		fs::create_dir_all(&local).unwrap();
		local
	}

	#[test]
	fn local_app_data_uses_the_single_non_public_wine_profile() {
		let prefix = tempfile::tempdir().unwrap();
		profile(prefix.path(), "Public");
		let steamuser = profile(prefix.path(), "steamuser");
		fs::create_dir_all(prefix.path().join("drive_c/users/empty")).unwrap();

		assert_eq!(
			local_app_data(prefix.path(), "MCP_PLUGINS_DIR").unwrap(),
			fs::canonicalize(steamuser).unwrap()
		);
	}

	#[test]
	fn local_app_data_deduplicates_the_proton_user_symlink() {
		let prefix = tempfile::tempdir().unwrap();
		profile(prefix.path(), "Public");
		let steamuser = profile(prefix.path(), "steamuser");
		symlink("steamuser", prefix.path().join("drive_c/users/carbon")).unwrap();

		assert_eq!(
			local_app_data(prefix.path(), "MCP_PLUGINS_DIR").unwrap(),
			fs::canonicalize(steamuser).unwrap()
		);
	}

	#[test]
	fn local_app_data_rejects_ambiguous_or_missing_profiles() {
		let prefix = tempfile::tempdir().unwrap();
		profile(prefix.path(), "Public");
		let missing = local_app_data(prefix.path(), "CARBON_STUDIO_AUTOSAVES_DIR").unwrap_err();
		assert!(format!("{missing:#}").contains("CARBON_STUDIO_AUTOSAVES_DIR"));

		profile(prefix.path(), "steamuser");
		profile(prefix.path(), "carbon");
		let ambiguous = local_app_data(prefix.path(), "MCP_PLUGINS_DIR").unwrap_err();
		let message = format!("{ambiguous:#}");
		assert!(message.contains("several Wine user profiles"));
		assert!(message.contains("MCP_PLUGINS_DIR"));

		assert!(local_app_data(&prefix.path().join("missing"), "MCP_PLUGINS_DIR").is_err());
		assert!(prefix_is_required());
	}

	fn prefix_is_required() -> bool {
		prefix(None).is_err() && prefix(Some("relative/prefix".into())).is_err()
	}

	#[test]
	fn wine_paths_map_unix_paths_through_drive_z() {
		assert_eq!(
			wine_path(Path::new("/home/carbon/place file.rbxl")).unwrap(),
			r"Z:\home\carbon\place file.rbxl"
		);
		assert!(wine_path(Path::new("relative.rbxl")).is_err());
	}

	fn proc_root(boot_time: u64) -> tempfile::TempDir {
		let root = tempfile::tempdir().unwrap();
		fs::write(
			root.path().join("stat"),
			format!("cpu  1 2 3 4\nintr 0\nbtime {boot_time}\nprocesses 9\n"),
		)
		.unwrap();
		root
	}

	fn proc_process(root: &Path, process_id: u32, comm: &str, state: char, start_ticks: u64, cmdline: &[u8]) {
		let directory = root.join(process_id.to_string());
		fs::create_dir_all(&directory).unwrap();
		let mut fields = vec!["0".to_owned(); 50];
		fields[0] = state.to_string();
		fields[19] = start_ticks.to_string();
		fs::write(
			directory.join("stat"),
			format!("{process_id} ({comm}) {}\n", fields.join(" ")),
		)
		.unwrap();
		fs::write(directory.join("cmdline"), cmdline).unwrap();
	}

	#[test]
	fn proc_identity_uses_pid_start_filetime_and_the_studio_command_line() {
		let root = proc_root(1_700_000_000);
		proc_process(
			root.path(),
			4242,
			"Roblox) T (x",
			'S',
			12_345,
			b"C:\\Program Files\\Roblox\\Versions\\v1\\robloxstudiobeta.EXE\0--task\0EditFile\0",
		);
		proc_process(
			root.path(),
			4243,
			"wine",
			'R',
			12_346,
			b"/usr/bin/wine\0RobloxStudioBeta.exe\0",
		);
		proc_process(
			root.path(),
			4244,
			"RobloxStudioBet",
			'Z',
			12_347,
			b"/opt/Roblox/RobloxStudioBeta.exe\0",
		);
		fs::create_dir_all(root.path().join("self")).unwrap();
		let proc = ProcFs::new(root.path(), STUDIO_EXECUTABLE_NAME);

		let expected = FILETIME_UNIX_EPOCH + 1_700_000_000 * 10_000_000 + 12_345 * 100_000;
		let identity = proc.identity(4242).unwrap().unwrap();
		assert_eq!(
			identity,
			ProcessIdentity {
				state: 'S',
				creation_filetime: expected,
				studio: true,
			}
		);
		assert!(identity.matches(expected));
		assert!(identity.matches(expected + START_TIME_TOLERANCE));
		assert!(identity.matches(expected - START_TIME_TOLERANCE));
		assert!(!identity.matches(expected + START_TIME_TOLERANCE + 1));
		assert!(!proc.identity(4243).unwrap().unwrap().matches(expected + 100_000));
		assert!(proc.identity(4244).unwrap().unwrap().exited());
		assert_eq!(proc.identity(4245).unwrap(), None);
		assert_eq!(proc.studio_processes().unwrap(), vec![4242]);
	}

	#[test]
	fn proc_boot_time_is_read_once_per_process() {
		let root = proc_root(1_700_000_000);
		proc_process(root.path(), 7, "studio", 'S', 0, b"RobloxStudioBeta.exe\0");
		let proc = ProcFs::new(root.path(), STUDIO_EXECUTABLE_NAME);
		let before = proc.identity(7).unwrap().unwrap().creation_filetime;
		fs::write(root.path().join("stat"), "btime 1700000009\n").unwrap();
		assert_eq!(proc.identity(7).unwrap().unwrap().creation_filetime, before);
	}

	#[test]
	fn studio_command_line_matches_only_the_program_basename() {
		assert!(is_studio_command_line(
			b"Z:\\home\\carbon\\RobloxStudioBeta.exe\0",
			STUDIO_EXECUTABLE_NAME
		));
		assert!(is_studio_command_line(
			b"/prefix/drive_c/RobloxStudioBeta.exe",
			STUDIO_EXECUTABLE_NAME
		));
		assert!(!is_studio_command_line(
			b"/bin/sh\0/launcher\0RobloxStudioBeta.exe\0",
			STUDIO_EXECUTABLE_NAME
		));
		assert!(!is_studio_command_line(
			b"RobloxStudioBeta.exe.so\0",
			STUDIO_EXECUTABLE_NAME
		));
		assert!(!is_studio_command_line(b"", STUDIO_EXECUTABLE_NAME));
	}

	const TEST_STUDIO: &str = "carbon-wine-test-studio";

	/// Spawn a harmless `sleep` whose argv[0] matches the test Studio name.
	/// Exec publishes argv shortly after `spawn` returns, so wait for it.
	fn test_studio(proc: &ProcFs) -> (std::process::Child, ProcessIdentity) {
		let mut child = Command::new("sleep")
			.arg0(format!("/carbon/{TEST_STUDIO}"))
			.arg("30")
			.spawn()
			.unwrap();
		let deadline = Instant::now() + Duration::from_secs(5);
		while Instant::now() < deadline {
			let identity = proc.identity(child.id()).unwrap().unwrap();
			if identity.studio {
				return (child, identity);
			}
			thread::sleep(Duration::from_millis(10));
		}
		child.kill().unwrap();
		child.wait().unwrap();
		panic!("test Studio never published its argv");
	}

	#[test]
	fn terminate_stops_only_the_exact_process_identity() {
		let proc = ProcFs::new("/proc", TEST_STUDIO);
		let (mut child, identity) = test_studio(&proc);
		let process_id = child.id();

		let error = terminate_with(&proc, process_id, identity.creation_filetime + 3 * 10_000_000).unwrap_err();
		assert!(format!("{error:#}").contains("no longer the managed Roblox Studio"));
		let other = ProcFs::new("/proc", STUDIO_EXECUTABLE_NAME);
		assert!(terminate_with(&other, process_id, identity.creation_filetime).is_err());
		assert_eq!(child.try_wait().unwrap(), None);

		terminate_with(&proc, process_id, identity.creation_filetime + 10_000_000).unwrap();
		assert!(child.wait().unwrap().code().is_none());
		terminate_with(&proc, process_id, identity.creation_filetime).unwrap();
	}

	#[test]
	fn wait_for_exit_returns_after_the_process_exits() {
		let mut child = Command::new("sleep").arg("0.2").spawn().unwrap();
		wait_for_exit(child.id()).unwrap();
		assert!(child.wait().unwrap().success());
	}

	/// Freshly written executables race other tests' forks (ETXTBSY), so the
	/// launcher is `/bin/bash` and the "Studio executable" is a script it reads.
	fn bash_launch(directory: &Path, body: &str, exec_timeout: Duration) -> Result<OwnedStudio> {
		let host = WineHost {
			launcher: PathBuf::from("/bin/bash"),
		};
		let script = directory.join("studio.sh");
		fs::write(&script, body).unwrap();
		launch(
			&ProcFs::new("/proc", TEST_STUDIO),
			host.command(&script).arg("--flag"),
			exec_timeout,
		)
	}

	#[test]
	fn launch_returns_the_launcher_pid_once_it_execs_studio() {
		let directory = tempfile::tempdir().unwrap();
		let studio = bash_launch(
			directory.path(),
			&format!("[ \"$1\" = --flag ] && exec -a /studio/{TEST_STUDIO} sleep 30"),
			Duration::from_secs(10),
		)
		.unwrap();
		let proc = ProcFs::new("/proc", TEST_STUDIO);
		let identity = proc.identity(studio.process_id).unwrap().unwrap();
		assert!(identity.matches(studio.creation_filetime));
		terminate_with(&proc, studio.process_id, studio.creation_filetime).unwrap();
	}

	#[test]
	fn owned_studio_stops_and_is_reaped() {
		let directory = tempfile::tempdir().unwrap();
		let proc = ProcFs::new("/proc", TEST_STUDIO);
		let mut studio = bash_launch(
			directory.path(),
			&format!("exec -a /studio/{TEST_STUDIO} sleep 30"),
			Duration::from_secs(10),
		)
		.unwrap();
		let process_id = studio.process_id;
		assert!(!studio.exited().unwrap());
		studio.stop_with(&proc).unwrap();
		assert_eq!(proc.identity(process_id).unwrap(), None);

		let mut finished = bash_launch(
			directory.path(),
			&format!("exec -a /studio/{TEST_STUDIO} sleep 1"),
			Duration::from_secs(10),
		)
		.unwrap();
		let process_id = finished.process_id;
		let deadline = Instant::now() + Duration::from_secs(5);
		while !finished.exited().unwrap() {
			assert!(Instant::now() < deadline, "test Studio never exited");
			thread::sleep(Duration::from_millis(20));
		}
		finished.stop_with(&proc).unwrap();
		assert_eq!(proc.identity(process_id).unwrap(), None);
	}

	#[test]
	fn launch_fails_closed_when_the_launcher_does_not_exec_studio() {
		let directory = tempfile::tempdir().unwrap();
		let error = bash_launch(directory.path(), "exit 0", Duration::from_secs(5)).unwrap_err();
		assert!(format!("{error:#}").contains("launcher exited"));

		let started = Instant::now();
		let error = bash_launch(directory.path(), "exec sleep 30", Duration::from_millis(500)).unwrap_err();
		assert!(format!("{error:#}").contains("did not exec"));
		assert!(started.elapsed() < Duration::from_secs(5));
	}
}
