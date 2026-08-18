#![no_std]
#![no_main]

use core::{ffi::c_void, panic::PanicInfo, ptr};

const HCBT_ACTIVATE: i32 = 5;

#[link(name = "user32", kind = "raw-dylib")]
unsafe extern "system" {
	fn CallNextHookEx(hook: *mut c_void, code: i32, w_param: usize, l_param: isize) -> isize;
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn CarbonWindowGuardHook(code: i32, w_param: usize, l_param: isize) -> isize {
	if code == HCBT_ACTIVATE {
		return 1;
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
