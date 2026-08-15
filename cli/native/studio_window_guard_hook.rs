#![no_std]
#![no_main]

use core::{ffi::c_void, panic::PanicInfo, ptr};

const HCBT_ACTIVATE: i32 = 5;
const KEY_DOWN: i16 = i16::MIN;
const VK_MENU: i32 = 0x12;
const VK_LWIN: i32 = 0x5B;
const VK_RWIN: i32 = 0x5C;

#[repr(C)]
struct CbtActivate {
	mouse: i32,
	active_window: *mut c_void,
}

#[link(name = "user32", kind = "raw-dylib")]
unsafe extern "system" {
	fn CallNextHookEx(hook: *mut c_void, code: i32, w_param: usize, l_param: isize) -> isize;
	fn GetAsyncKeyState(virtual_key: i32) -> i16;
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn CarbonWindowGuardHook(code: i32, w_param: usize, l_param: isize) -> isize {
	if code == HCBT_ACTIVATE {
		let activation = (l_param as *const CbtActivate).as_ref();
		let mouse_activation = activation.is_some_and(|activation| activation.mouse != 0);
		let keyboard_switch = [VK_MENU, VK_LWIN, VK_RWIN]
			.into_iter()
			.any(|key| GetAsyncKeyState(key) & KEY_DOWN != 0);
		if !mouse_activation && !keyboard_switch {
			return 1;
		}
	}

	CallNextHookEx(ptr::null_mut(), code, w_param, l_param)
}

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(_module: *mut c_void, _reason: u32, _reserved: *mut c_void) -> i32 {
	1
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
	loop {}
}
