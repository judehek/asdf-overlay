#![windows_subsystem = "windows"]

//! No-op stub DLL used to isolate what in the real `asdf-overlay-dll` triggers
//! kernel anti-cheat kills.
//!
//! This DLL exposes the same `asdf_overlay_hook_proc` entry point that the
//! real overlay DLL exports, so `asdf-overlay-client`'s `safe_inject`
//! (`SetWindowsHookEx(WH_GETMESSAGE, ...)`) accepts it unchanged. `DllMain`
//! does nothing but return `TRUE`: no tokio runtime, no IPC server, no
//! graphics API hooks. If this DLL can be loaded into a Vanguard-protected
//! process via `safe_inject` without crashing the target, the injection path
//! itself is fine and the problem is behavior inside the real DLL's startup.

use windows::Win32::{
    Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM},
    UI::WindowsAndMessaging::{CallNextHookEx, HHOOK},
};

/// Exported hook procedure for `SetWindowsHookEx(WH_GETMESSAGE, ...)`. Just
/// forwards to the next hook in the chain (per MSDN).
#[unsafe(no_mangle)]
pub extern "system" fn asdf_overlay_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { CallNextHookEx(Some(HHOOK(core::ptr::null_mut())), code, wparam, lparam) }
}

/// No-op `DllMain`. Returning `TRUE` for all reasons is the minimum a loader
/// will accept. We intentionally do not touch threads, runtimes, IPC, or
/// memory of other modules, so that anything the anti-cheat reacts to must
/// be coming from outside this DLL.
///
/// # Safety
/// Can be called by loader only. Must not be called manually.
#[unsafe(no_mangle)]
#[allow(non_snake_case, unused_variables)]
pub unsafe extern "system" fn DllMain(
    dll_module: HINSTANCE,
    fdw_reason: u32,
    _: *mut (),
) -> bool {
    true
}
