use anyhow::Result;

#[cfg(target_os = "windows")]
use windows_sys::Win32::{
	Foundation::{
		CloseHandle, BOOL, FALSE, FILETIME, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, RECT, STILL_ACTIVE, TRUE,
	},
	Storage::FileSystem::GetFullPathNameW,
	System::Threading::{
		AttachThreadInput, GetCurrentThreadId, GetExitCodeProcess, GetProcessTimes, OpenProcess,
		QueryFullProcessImageNameW, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
	},
	UI::{
		Input::KeyboardAndMouse::SetFocus,
		WindowsAndMessaging::{
			BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindow, GetWindowLongPtrW, GetWindowRect,
			GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, SetForegroundWindow, ShowWindowAsync,
			GWL_EXSTYLE, GWL_STYLE, GW_OWNER, SW_RESTORE, WS_CHILD, WS_EX_TOOLWINDOW,
		},
	},
};

#[cfg(target_os = "windows")]
struct SafeHandle(HANDLE);

#[cfg(target_os = "windows")]
impl SafeHandle {
	fn new(handle: HANDLE) -> Self {
		Self(handle)
	}

	fn is_valid(&self) -> bool {
		!self.0.is_null() && self.0 != INVALID_HANDLE_VALUE
	}

	fn get(&self) -> HANDLE {
		self.0
	}
}

#[cfg(target_os = "windows")]
impl Drop for SafeHandle {
	fn drop(&mut self) {
		if self.is_valid() {
			unsafe {
				CloseHandle(self.0);
			}
			self.0 = std::ptr::null_mut();
		}
	}
}

#[cfg(target_os = "windows")]
struct ThreadInputAttachment {
	from_thread: u32,
	to_thread: u32,
	attached: bool,
}

#[cfg(target_os = "windows")]
impl ThreadInputAttachment {
	fn attach(from_thread: u32, to_thread: u32) -> Self {
		let attached = unsafe { AttachThreadInput(from_thread, to_thread, TRUE) != 0 };
		Self {
			from_thread,
			to_thread,
			attached,
		}
	}

	fn none() -> Self {
		Self {
			from_thread: 0,
			to_thread: 0,
			attached: false,
		}
	}
}

#[cfg(target_os = "windows")]
impl Drop for ThreadInputAttachment {
	fn drop(&mut self) {
		if self.attached {
			unsafe {
				AttachThreadInput(self.from_thread, self.to_thread, FALSE);
			}
			self.attached = false;
		}
	}
}

#[cfg(target_os = "windows")]
fn normalize_path(path: &str) -> String {
	let mut s = path.replace('/', "\\");
	if s.starts_with(r"\\?\UNC\") {
		s = format!(r"\\{}", &s[8..]);
	} else if s.starts_with(r"\\?\") {
		s = s[4..].to_string();
	}

	let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
	let mut buf = [0u16; 32768];
	let len = unsafe { GetFullPathNameW(wide.as_ptr(), buf.len() as u32, buf.as_mut_ptr(), std::ptr::null_mut()) };
	if len > 0 && (len as usize) < buf.len() {
		s = String::from_utf16_lossy(&buf[..len as usize]);
	}

	while s.len() > 3 && s.ends_with('\\') {
		s.pop();
	}

	s
}

#[cfg(target_os = "windows")]
fn paths_equal(p1: &str, p2: &str) -> bool {
	let n1 = normalize_path(p1);
	let n2 = normalize_path(p2);
	n1.to_lowercase() == n2.to_lowercase()
}

#[cfg(target_os = "windows")]
fn validate_process_identity(pid: u32, expected_filetime: u64, studio_executable: &str) -> Result<SafeHandle> {
	let h_process_raw = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION, FALSE, pid) };
	let h_process_raw = if h_process_raw.is_null() {
		unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) }
	} else {
		h_process_raw
	};

	let h_process = SafeHandle::new(h_process_raw);
	if !h_process.is_valid() {
		anyhow::bail!("OpenProcess failed: {}", std::io::Error::last_os_error());
	}

	let mut exit_code = 0u32;
	if unsafe { GetExitCodeProcess(h_process.get(), &mut exit_code) } == 0 {
		anyhow::bail!("GetExitCodeProcess failed: {}", std::io::Error::last_os_error());
	}
	if exit_code != STILL_ACTIVE as u32 {
		anyhow::bail!("Process is not running");
	}

	let mut ft_create = FILETIME {
		dwLowDateTime: 0,
		dwHighDateTime: 0,
	};
	let mut ft_exit = FILETIME {
		dwLowDateTime: 0,
		dwHighDateTime: 0,
	};
	let mut ft_kernel = FILETIME {
		dwLowDateTime: 0,
		dwHighDateTime: 0,
	};
	let mut ft_user = FILETIME {
		dwLowDateTime: 0,
		dwHighDateTime: 0,
	};

	if unsafe {
		GetProcessTimes(
			h_process.get(),
			&mut ft_create,
			&mut ft_exit,
			&mut ft_kernel,
			&mut ft_user,
		)
	} == 0
	{
		anyhow::bail!("GetProcessTimes failed: {}", std::io::Error::last_os_error());
	}

	let pid_filetime = ((ft_create.dwHighDateTime as u64) << 32) | (ft_create.dwLowDateTime as u64);
	if pid_filetime != expected_filetime {
		anyhow::bail!("Process creation time mismatch");
	}

	let mut exe_path_buf = [0u16; 32768];
	let mut size = exe_path_buf.len() as u32;
	if unsafe { QueryFullProcessImageNameW(h_process.get(), 0, exe_path_buf.as_mut_ptr(), &mut size) } == 0 {
		anyhow::bail!("QueryFullProcessImageNameW failed: {}", std::io::Error::last_os_error());
	}

	let exe_path = String::from_utf16_lossy(&exe_path_buf[..size as usize]);
	if !paths_equal(&exe_path, studio_executable) {
		anyhow::bail!("Process image path mismatch");
	}

	Ok(h_process)
}

#[cfg(target_os = "windows")]
struct FocusWindowCandidate {
	hwnd: HWND,
	area: i32,
}

#[cfg(target_os = "windows")]
struct FocusEnumContext {
	target_pid: u32,
	allow_tool_window: bool,
	candidates: Vec<FocusWindowCandidate>,
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn enum_windows_callback(hwnd: HWND, l_param: LPARAM) -> BOOL {
	let ctx = &mut *(l_param as *mut FocusEnumContext);

	let mut window_pid = 0u32;
	GetWindowThreadProcessId(hwnd, &mut window_pid);
	if window_pid != ctx.target_pid {
		return TRUE;
	}

	if IsWindowVisible(hwnd) == 0 {
		return TRUE;
	}

	if GetWindow(hwnd, GW_OWNER) != std::ptr::null_mut() {
		return TRUE;
	}

	let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
	if (style as u32 & WS_CHILD) != 0 {
		return TRUE;
	}

	if !ctx.allow_tool_window {
		let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
		if (ex_style as u32 & WS_EX_TOOLWINDOW) != 0 {
			return TRUE;
		}
	}

	let mut r = RECT {
		left: 0,
		top: 0,
		right: 0,
		bottom: 0,
	};
	let mut area = 0i32;
	if GetWindowRect(hwnd, &mut r) != 0 {
		let width = r.right - r.left;
		let height = r.bottom - r.top;
		if width > 0 && height > 0 {
			area = width.saturating_mul(height);
		}
	}

	ctx.candidates.push(FocusWindowCandidate { hwnd, area });
	TRUE
}

#[cfg(target_os = "windows")]
fn activate_window(target_hwnd: HWND) -> bool {
	unsafe {
		SetForegroundWindow(target_hwnd);
	}

	if unsafe { GetForegroundWindow() } != target_hwnd {
		let current_thread = unsafe { GetCurrentThreadId() };
		let target_thread = unsafe { GetWindowThreadProcessId(target_hwnd, std::ptr::null_mut()) };
		let foreground_hwnd = unsafe { GetForegroundWindow() };
		let foreground_thread = if !foreground_hwnd.is_null() {
			unsafe { GetWindowThreadProcessId(foreground_hwnd, std::ptr::null_mut()) }
		} else {
			0
		};

		let _attached_foreground = if foreground_thread != 0 && foreground_thread != current_thread {
			ThreadInputAttachment::attach(current_thread, foreground_thread)
		} else {
			ThreadInputAttachment::none()
		};
		let _attached_target =
			if target_thread != 0 && target_thread != current_thread && target_thread != foreground_thread {
				ThreadInputAttachment::attach(current_thread, target_thread)
			} else {
				ThreadInputAttachment::none()
			};

		unsafe {
			BringWindowToTop(target_hwnd);
			SetForegroundWindow(target_hwnd);
			SetFocus(target_hwnd);
		}
	}

	for _ in 0..10 {
		if unsafe { GetForegroundWindow() } == target_hwnd {
			return true;
		}
		std::thread::sleep(std::time::Duration::from_millis(10));
	}
	(unsafe { GetForegroundWindow() }) == target_hwnd
}

#[cfg(target_os = "windows")]
pub fn focus_process(process_id: u32, creation_filetime: u64, studio_executable: &str) -> Result<()> {
	let _h_process = validate_process_identity(process_id, creation_filetime, studio_executable)?;

	let mut ctx = FocusEnumContext {
		target_pid: process_id,
		allow_tool_window: false,
		candidates: Vec::new(),
	};

	unsafe {
		EnumWindows(Some(enum_windows_callback), &mut ctx as *mut FocusEnumContext as LPARAM);
	}

	if ctx.candidates.is_empty() {
		ctx.allow_tool_window = true;
		unsafe {
			EnumWindows(Some(enum_windows_callback), &mut ctx as *mut FocusEnumContext as LPARAM);
		}
	}

	if ctx.candidates.is_empty() {
		anyhow::bail!("Roblox Studio process {} has no main window", process_id);
	}

	ctx.candidates.sort_by(|a, b| b.area.cmp(&a.area));

	let target_hwnd = ctx.candidates[0].hwnd;
	let previous_hwnd = unsafe { GetForegroundWindow() };

	unsafe {
		if IsIconic(target_hwnd) != 0 {
			ShowWindowAsync(target_hwnd, SW_RESTORE);
		}
	}
	if !activate_window(target_hwnd) {
		anyhow::bail!(
			"Windows denied foreground activation for Roblox Studio process {}",
			process_id
		);
	}

	if !previous_hwnd.is_null()
		&& previous_hwnd != target_hwnd
		&& unsafe { IsWindow(previous_hwnd) } != 0
		&& !activate_window(previous_hwnd)
	{
		anyhow::bail!("Windows denied restoration of the previously focused window");
	}

	Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn focus_process(_process_id: u32, _creation_filetime: u64, _studio_executable: &str) -> Result<()> {
	anyhow::bail!("studio_windows::focus_process is only supported on Windows")
}
