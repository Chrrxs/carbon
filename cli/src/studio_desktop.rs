//! Focus and park managed Studios through one Windows PowerShell process.
//!
//! Each step used to start its own PowerShell, and most recompiled C# interop
//! with `Add-Type`, so `carbon focus` took seconds. The helper assembly in
//! `studio_desktop.cs` is compiled once per source digest into
//! `%LOCALAPPDATA%\Carbon\studio-desktop` and then only loaded. One process runs
//! the guardian policy changes, virtual desktop routing, activation, and
//! attention clearing for a whole plan.

use anyhow::{Context, Result};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde::Deserialize;

use crate::studio::{StudioDesktopPlacement, StudioProcessIdentity};

#[cfg(any(target_os = "linux", target_os = "windows"))]
const HELPER_SOURCE: &str = include_str!("studio_desktop.cs");

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub(crate) struct FocusReport {
	pub(crate) parked: usize,
	pub(crate) parked_process_ids: Vec<u32>,
	pub(crate) attention_windows: usize,
	pub(crate) post_focus_attention_windows: usize,
	pub(crate) target_audio_sessions: usize,
	pub(crate) target_audio_changes: usize,
	pub(crate) peer_audio_sessions: usize,
	pub(crate) peer_audio_changes: usize,
	pub(crate) peer_guarded_threads: usize,
	pub(crate) warnings: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub(crate) struct ParkReport {
	pub(crate) attention_windows: usize,
	pub(crate) audio_sessions: usize,
	pub(crate) audio_changes: usize,
	pub(crate) guarded_threads: usize,
}

/// What `carbon focus` should do besides activating the target Studio.
pub(crate) enum FocusRouting<'a> {
	/// Activate the Studio where it is.
	None,
	/// Bring the parked Studio to the active desktop with its audio and
	/// activation restored, and park these sibling Studios.
	Desktops { peers: &'a [StudioDesktopPlacement] },
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn encode(value: &str) -> String {
	BASE64_STANDARD.encode(value.as_bytes())
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn identity_fields(process: &StudioProcessIdentity) -> String {
	format!(
		"{} {} {}",
		process.process_id,
		process.creation_filetime,
		encode(&process.studio_executable)
	)
}

/// Line-oriented plan; every free-form value is base64 so a path or desktop
/// name can never change the plan's structure.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn plan_text(mode: &str, target: Option<&StudioProcessIdentity>, extra: &[String]) -> String {
	let mut lines = vec![format!("mode {mode}")];
	if let Some(target) = target {
		lines.push(format!("target {}", identity_fields(target)));
	}
	lines.extend(extra.iter().cloned());
	lines.join("\n")
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn guard_lines() -> Result<Vec<String>> {
	let audio = crate::studio::install_studio_audio_guard_script()?;
	let (window, hook) = crate::studio::install_studio_window_guard_assets()?;
	Ok(vec![
		format!(
			"audio-guard {}",
			encode(&crate::studio::native_windows_helper_path(&audio)?)
		),
		format!(
			"window-guard {}",
			encode(&crate::studio::native_windows_helper_path(&window)?)
		),
		format!(
			"window-hook {}",
			encode(&crate::studio::native_windows_helper_path(&hook)?)
		),
	])
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn focus_plan(
	target: &StudioProcessIdentity,
	routing: &FocusRouting<'_>,
	restore: bool,
	guards: Vec<String>,
) -> String {
	let mut extra = vec![
		format!("restore {}", u8::from(restore)),
		format!("route {}", u8::from(matches!(routing, FocusRouting::Desktops { .. }))),
	];
	if let FocusRouting::Desktops { peers } = routing {
		for peer in peers.iter() {
			extra.push(format!(
				"peer {} {}",
				identity_fields(&peer.process),
				encode(&peer.desktop_name)
			));
		}
		extra.extend(guards);
	}
	plan_text("focus", Some(target), &extra)
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn park_plan(placement: &StudioDesktopPlacement, guards: Vec<String>) -> String {
	let mut extra = vec![format!("parking-desktop {}", encode(&placement.desktop_name))];
	extra.extend(guards);
	plan_text("park", Some(&placement.process), &extra)
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn helper_digest() -> String {
	blake3::hash(HELPER_SOURCE.as_bytes()).to_hex()[..16].to_owned()
}

/// Compile the helper once into the user's local profile, then only load it:
/// `Add-Type` costs far more than the work it enables. Assemblies must load
/// from a local disk, so the source may live on the WSL share but the cache
/// cannot.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn bootstrap_script(source_path: &str, digest: &str, plan: &str) -> String {
	r#"
$ErrorActionPreference = 'Stop'
try {
    $directory = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'Carbon\studio-desktop'
    $assembly = Join-Path $directory 'carbon-studio-desktop-__DIGEST__.dll'
    if (-not [IO.File]::Exists($assembly)) {
        [void][IO.Directory]::CreateDirectory($directory)
        $temporary = Join-Path $directory ('carbon-studio-desktop-__DIGEST__-' + [Guid]::NewGuid().ToString('N') + '.dll')
        $source = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__SOURCE__'))
        try {
            Add-Type -TypeDefinition ([IO.File]::ReadAllText($source)) -OutputAssembly $temporary -OutputType Library
            try { [IO.File]::Move($temporary, $assembly) } catch { if (-not [IO.File]::Exists($assembly)) { throw } }
        } finally {
            if ([IO.File]::Exists($temporary)) { [IO.File]::Delete($temporary) }
        }
    }
    [void][Reflection.Assembly]::LoadFrom($assembly)
    [Console]::Out.Write([CarbonStudioDesktop.Helper]::Run('__PLAN__'))
} catch {
    [Console]::Error.Write($_.Exception.GetBaseException().Message)
    exit 1
}
"#
	.replace("__DIGEST__", digest)
	.replace("__SOURCE__", &encode(source_path))
	.replace("__PLAN__", &encode(plan))
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn run(plan: &str) -> Result<String> {
	let source = crate::studio::install_windows_helper("studio-desktop", "cs", HELPER_SOURCE.as_bytes())?;
	let source = crate::studio::native_windows_helper_path(&source)?;
	let script = bootstrap_script(&source, &helper_digest(), plan);
	let output = crate::studio::powershell_command()?
		.args(["-Sta", "-NoProfile", "-NonInteractive", "-Command", &script])
		.output()
		.context("failed to start the Carbon Studio desktop helper")?;
	anyhow::ensure!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr).trim()
	);
	String::from_utf8(output.stdout).context("the Carbon Studio desktop helper returned non-UTF-8 output")
}

/// Activate the exact Studio, routing desktops first when requested.
pub(crate) fn focus(target: &StudioProcessIdentity, routing: FocusRouting<'_>, restore: bool) -> Result<FocusReport> {
	#[cfg(target_os = "linux")]
	anyhow::ensure!(
		crate::studio::wine_host()?.is_none(),
		"focusing a Roblox Studio window is unsupported on a Linux Wine host"
	);
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	{
		let guards = match routing {
			FocusRouting::Desktops { .. } => guard_lines()?,
			FocusRouting::None => Vec::new(),
		};
		let report = run(&focus_plan(target, &routing, restore, guards))?;
		serde_json::from_str(report.trim()).context("the Carbon Studio desktop helper returned an invalid focus report")
	}
	#[cfg(not(any(target_os = "linux", target_os = "windows")))]
	{
		let _ = (target, routing, restore);
		anyhow::bail!("focusing a managed Roblox Studio is supported only on Windows and WSL")
	}
}

/// Mute, guard, and move the exact Studio to its parking desktop.
pub(crate) fn park(placement: &StudioDesktopPlacement) -> Result<ParkReport> {
	#[cfg(any(target_os = "linux", target_os = "windows"))]
	{
		let report = run(&park_plan(placement, guard_lines()?))?;
		serde_json::from_str(report.trim()).context("the Carbon Studio desktop helper returned an invalid park report")
	}
	#[cfg(not(any(target_os = "linux", target_os = "windows")))]
	{
		let _ = placement;
		anyhow::bail!("Studio parking is supported only when Carbon runs on Windows or WSL")
	}
}

#[cfg(all(test, any(target_os = "linux", target_os = "windows")))]
mod tests {
	use super::*;

	fn identity(process_id: u32) -> StudioProcessIdentity {
		StudioProcessIdentity {
			process_id,
			studio_executable: r"C:\Program Files\Roblox's Studio\RobloxStudioBeta.exe".to_owned(),
			creation_filetime: 133_700_000_000 + u64::from(process_id),
		}
	}

	#[test]
	fn focus_helper_plan_encodes_every_free_form_value() {
		let desktop = "Studios'); Stop-Process -Name RobloxStudioBeta; ('";
		let peers = vec![StudioDesktopPlacement {
			process: identity(102),
			desktop_name: desktop.to_owned(),
		}];
		let plan = focus_plan(
			&identity(101),
			&FocusRouting::Desktops { peers: &peers },
			true,
			vec!["audio-guard YQ==".to_owned()],
		);

		assert!(plan.contains("mode focus"));
		assert!(plan.contains("restore 1"));
		assert!(plan.contains("route 1"));
		assert!(plan.contains("target 101 133700000101 "));
		assert!(plan.contains(&format!(
			"peer 102 133700000102 {} {}",
			encode(&identity(102).studio_executable),
			encode(desktop)
		)));
		assert!(!plan.contains("Roblox's Studio"));
		assert!(!plan.contains(desktop));

		let unrouted = focus_plan(&identity(101), &FocusRouting::None, false, Vec::new());
		assert!(unrouted.contains("route 0"));
		assert!(!unrouted.contains("peer "));
		assert!(!unrouted.contains("guard"));
	}

	#[test]
	fn focus_helper_compiles_once_and_then_only_loads_its_assembly() {
		let script = bootstrap_script(
			r"\\wsl.localhost\Ubuntu\home\o'neil\studio-desktop.cs",
			"0123abcd",
			"mode probe",
		);

		assert!(script.contains("carbon-studio-desktop-0123abcd.dll"));
		assert!(script.contains("if (-not [IO.File]::Exists($assembly))"));
		assert!(script.contains("-OutputAssembly $temporary"));
		assert!(script.contains("[Reflection.Assembly]::LoadFrom($assembly)"));
		assert!(script.contains("GetBaseException().Message"));
		assert!(!script.contains("o'neil"));
		assert!(script.contains(&encode("mode probe")));
	}

	#[test]
	fn focus_helper_speaks_the_guardians_pipe_protocols() {
		for pipe in ["carbon-studio-audio-v4-", "carbon-studio-window-v1-"] {
			assert!(HELPER_SOURCE.contains(pipe), "{pipe}");
		}
		assert!(include_str!("studio_audio_guard.ps1").contains("carbon-studio-audio-v4-"));
		assert!(include_str!("studio_window_guard.ps1").contains("carbon-studio-window-v1-"));
		// Windows PowerShell's Add-Type compiles C# 5.
		assert!(!HELPER_SOURCE.contains("$\""));
		assert!(!HELPER_SOURCE.contains("?."));
		assert!(!HELPER_SOURCE.contains("nameof("));
	}

	#[test]
	fn focus_activation_accepts_any_window_of_the_studio_process() {
		assert!(HELPER_SOURCE.contains("Studio.WindowProcess(Native.GetForegroundWindow()) == processId"));
		assert!(HELPER_SOURCE.contains("SwitchToThisWindow"));
		assert!(HELPER_SOURCE.contains("IsHungAppWindow"));
		assert!(!HELPER_SOURCE.contains("keybd_event"));
		assert!(!HELPER_SOURCE.contains("SendInput"));
	}

	#[test]
	fn focus_helper_compiles_and_loads_on_windows() {
		#[cfg(target_os = "linux")]
		if std::env::var_os("WSL_DISTRO_NAME").is_none() {
			return;
		}
		let report = run(&plan_text("probe", None, &[])).unwrap();
		assert_eq!(report.trim(), r#"{"protocol":1}"#);
	}
}
