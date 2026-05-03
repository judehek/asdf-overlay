use core::ffi::c_void;

use asdf_overlay_hook::DetourHook;
use once_cell::sync::OnceCell;
use tracing::debug;
use windows::{
    Win32::{
        Foundation::{HWND, POINT, RECT},
        UI::{
            Input::{
                HRAWINPUT,
                KeyboardAndMouse::GetActiveWindow,
                RAW_INPUT_DATA_COMMAND_FLAGS, RAWINPUT, RAWINPUTHEADER, RID_HEADER, RID_INPUT,
            },
            WindowsAndMessaging::GetForegroundWindow,
        },
    },
    core::BOOL,
};

use crate::{
    backend::{Backends, window::InputBlockData},
    hook::proc::message_reading,
};

windows::core::link!("user32.dll" "system" fn ClipCursor(lprect: *const RECT) -> BOOL);
windows::core::link!("user32.dll" "system" fn SetCursorPos(x: i32, y: i32) -> BOOL);

windows::core::link!("user32.dll" "system" fn GetClipCursor(lprect: *mut RECT) -> BOOL);
windows::core::link!("user32.dll" "system" fn GetCursorPos(lppoint: *mut POINT) -> BOOL);
windows::core::link!("user32.dll" "system" fn ScreenToClient(hwnd: HWND, lppoint: *mut POINT) -> BOOL);
windows::core::link!("user32.dll" "system" fn GetPhysicalCursorPos(lppoint: *mut POINT) -> BOOL);
windows::core::link!("user32.dll" "system" fn GetKeyboardState(buf: *mut u8) -> BOOL);
windows::core::link!("user32.dll" "system" fn GetKeyState(vkey: i32) -> i16);
windows::core::link!("user32.dll" "system" fn GetAsyncKeyState(vkey: i32) -> i16);
windows::core::link!(
    "user32.dll" "system"
    fn GetRawInputData(
        hrawinput: HRAWINPUT,
        uicommand: RAW_INPUT_DATA_COMMAND_FLAGS,
        pdata: *mut c_void,
        pcbsize: *mut u32,
        cbsizeheader: u32,
    ) -> u32
);
windows::core::link!("user32.dll" "system" fn GetRawInputBuffer(pdata: *mut RAWINPUT, pcbsize: *mut u32, cbsizeheader: u32) -> u32);

struct Hook {
    clip_cursor: DetourHook<ClipCursorFn>,
    set_cursor_pos: DetourHook<SetCursorFn>,

    get_clip_cursor: DetourHook<GetClipCursorFn>,
    get_cursor_pos: DetourHook<GetCursorPos>,
    get_physical_cursor_pos: DetourHook<GetPhysicalCursorPos>,
    get_async_key_state: DetourHook<GetAsyncKeyStateFn>,
    get_key_state: DetourHook<GetKeyStateFn>,
    get_keyboard_state: DetourHook<GetKeyboardStateFn>,
    get_raw_input_data: DetourHook<GetRawInputDataFn>,
    get_raw_input_buffer: DetourHook<GetRawInputBufferFn>,
}
static HOOK: OnceCell<Hook> = OnceCell::new();

type ClipCursorFn = unsafe extern "system" fn(*const RECT) -> BOOL;
type SetCursorFn = unsafe extern "system" fn(i32, i32) -> BOOL;

type GetClipCursorFn = unsafe extern "system" fn(*mut RECT) -> BOOL;
type GetCursorPos = unsafe extern "system" fn(*mut POINT) -> BOOL;
type GetPhysicalCursorPos = unsafe extern "system" fn(*mut POINT) -> BOOL;
type GetAsyncKeyStateFn = unsafe extern "system" fn(i32) -> i16;
type GetKeyStateFn = unsafe extern "system" fn(i32) -> i16;
type GetKeyboardStateFn = unsafe extern "system" fn(*mut u8) -> BOOL;
type GetRawInputDataFn = unsafe extern "system" fn(
    HRAWINPUT,
    RAW_INPUT_DATA_COMMAND_FLAGS,
    *mut c_void,
    *mut u32,
    u32,
) -> u32;
type GetRawInputBufferFn = unsafe extern "system" fn(*mut RAWINPUT, *mut u32, u32) -> u32;

pub fn hook() -> anyhow::Result<()> {
    HOOK.get_or_try_init(|| unsafe {
        debug!("hooking ClipCursor");
        let clip_cursor = DetourHook::attach(ClipCursor as _, hooked_clip_cursor as _)?;

        debug!("hooking SetCursorPos");
        let set_cursor_pos = DetourHook::attach(SetCursorPos as _, hooked_set_cursor_pos as _)?;

        debug!("hooking GetClipCursor");
        let get_clip_cursor = DetourHook::attach(GetClipCursor as _, hooked_get_clip_cursor as _)?;

        debug!("hooking GetCursorPos");
        let get_cursor_pos = DetourHook::attach(GetCursorPos as _, hooked_get_cursor_pos as _)?;

        debug!("hooking GetPhysicalCursorPos");
        let get_physical_cursor_pos = DetourHook::attach(
            GetPhysicalCursorPos as _,
            hooked_get_physical_cursor_pos as _,
        )?;

        debug!("hooking GetAsyncKeyState");
        let get_async_key_state =
            DetourHook::attach(GetAsyncKeyState as _, hooked_get_async_key_state as _)?;

        debug!("hooking GetKeyState");
        let get_key_state = DetourHook::attach(GetKeyState as _, hooked_get_key_state as _)?;

        debug!("hooking GetKeyboardState");
        let get_keyboard_state =
            DetourHook::attach(GetKeyboardState as _, hooked_get_keyboard_state as _)?;

        debug!("hooking GetRawInputData");
        let get_raw_input_data =
            DetourHook::attach(GetRawInputData as _, hooked_get_raw_input_data as _)?;

        debug!("hooking GetRawInputBuffer");
        let get_raw_input_buffer =
            DetourHook::attach(GetRawInputBuffer as _, hooked_get_raw_input_buffer as _)?;

        Ok::<_, anyhow::Error>(Hook {
            clip_cursor,
            set_cursor_pos,

            get_clip_cursor,
            get_cursor_pos,
            get_physical_cursor_pos,
            get_async_key_state,
            get_key_state,
            get_keyboard_state,
            get_raw_input_data,
            get_raw_input_buffer,
        })
    })?;

    Ok(())
}

#[inline]
fn active_hwnd_can_block() -> Option<HWND> {
    let hwnd = unsafe { GetActiveWindow() };

    if !hwnd.is_invalid() && !message_reading() {
        Some(hwnd)
    } else {
        None
    }
}

#[inline]
fn active_hwnd_with<R>(f: impl FnOnce(&mut InputBlockData) -> R) -> Option<R> {
    Backends::with_backend(active_hwnd_can_block()?.0 as _, |backend| {
        let mut proc = backend.proc.lock();
        Some(f(proc.blocking_state.as_mut()?))
    })
    .flatten()
}

#[inline]
fn foreground_hwnd_input_blocked() -> bool {
    let hwnd = unsafe { GetForegroundWindow() };

    !hwnd.is_invalid()
        && Backends::with_backend(hwnd.0 as _, |backend| backend.proc.lock().input_blocking())
            .unwrap_or(false)
}

#[inline]
fn active_hwnd_input_blocked() -> bool {
    active_hwnd_with(|_| ()).is_some()
}

/// Hover-mode mouse-button gate.
///
/// Hover mode (`block_cursor_in_overlay`) only swallows wndproc messages.
/// Games that poll `GetAsyncKeyState(VK_LBUTTON)` (League of Legends does)
/// would still see clicks, even though the wndproc never delivered them. To
/// close that hole we also lie to the polling APIs whenever the cursor is
/// over the overlay rect.
///
/// Returns true if **any** registered backend currently has
/// `block_cursor_in_overlay` enabled AND the live cursor position falls
/// inside its overlay rect.
///
/// Implementation note: we resolve the cursor position via `GetCursorPos`
/// + `ScreenToClient` on each backend's HWND rather than reading the
/// `cursor_state` cached by the wndproc subclass. League of Legends
/// renders into child windows, so `WM_MOUSEMOVE` is only delivered to the
/// root window's wndproc when the cursor happens to be over the root's
/// own non-child client area; the rest of the time the root receives
/// `WM_MOUSELEAVE` and `cursor_state` is wedged at `Outside`. Querying
/// `GetCursorPos` directly is authoritative regardless of who is
/// receiving mouse-move messages.
///
/// We don't key off `GetForegroundWindow` / `GetActiveWindow` either:
/// `GetActiveWindow` is per-thread and returns NULL on background threads
/// that poll input, and `GetForegroundWindow` returned an HWND for which
/// `Backends::with_backend` resolved to `None` in League (probably a
/// child render window). In practice this iterates a 1-element map.
///
/// Important: we deliberately do **not** use a `GetCapture() == backend.id`
/// shortcut here. The backend id IS the game's own top-level HWND, so the
/// game's normal use of `SetCapture` on its own window (Valorant does this
/// for click-and-hold UI buttons, drags, etc.) makes that predicate true
/// even though the overlay is uninvolved. Returning true in that case
/// caused our raw-input hooks to zero out the buffer Valorant polls, which
/// made the entire game unresponsive to mouse input -- even when the
/// cursor was nowhere near the overlay. Cursor-position alone is the
/// correct hit-test.
#[inline]
fn any_backend_in_hover_block() -> bool {
    let mut screen_pt = POINT::default();
    let cursor_ok = unsafe { GetCursorPos(&mut screen_pt) }.as_bool();

    let mut saw_any = false;
    for backend in Backends::iter() {
        saw_any = true;
        let proc = backend.proc.lock();
        if !proc.block_cursor_in_overlay {
            if once_log::check_and_set(&once_log::GATE_BCIO_FALSE) {
                crate::proc_diag::log(format_args!(
                    "gate: backend id={} bcio=false (one-shot)",
                    backend.id
                ));
            }
            continue;
        }
        if once_log::check_and_set(&once_log::GATE_BCIO_TRUE) {
            crate::proc_diag::log(format_args!(
                "gate: backend id={} bcio=true cursor_ok={cursor_ok} (one-shot)",
                backend.id
            ));
        }

        if !cursor_ok {
            if once_log::check_and_set(&once_log::GATE_LAST_POS_NONE) {
                crate::proc_diag::log(format_args!(
                    "gate: id={} GetCursorPos failed (one-shot)",
                    backend.id
                ));
            }
            continue;
        }

        let mut client_pt = screen_pt;
        let hwnd = HWND(backend.id as _);
        let mapped = unsafe { ScreenToClient(hwnd, &mut client_pt) }.as_bool();
        if !mapped {
            continue;
        }

        let x = client_pt.x as i16;
        let y = client_pt.y as i16;
        if proc.cursor_in_overlay(x, y) {
            if once_log::check_and_set(&once_log::GATE_LAST_POS_IN) {
                crate::proc_diag::log(format_args!(
                    "gate: id={} screen=({},{}) client=({x},{y}) in_overlay=true (one-shot)",
                    backend.id, screen_pt.x, screen_pt.y
                ));
            }
            return true;
        }
        if once_log::check_and_set(&once_log::GATE_LAST_POS_OUT) {
            crate::proc_diag::log(format_args!(
                "gate: id={} screen=({},{}) client=({x},{y}) in_overlay=false pos=({},{}) size=({},{}) (one-shot)",
                backend.id, screen_pt.x, screen_pt.y,
                proc.position.0,
                proc.position.1,
                proc.surface_size.0,
                proc.surface_size.1
            ));
        }
    }
    if !saw_any && once_log::check_and_set(&once_log::GATE_NO_BACKENDS) {
        crate::proc_diag::log(format_args!("gate: no backends registered (one-shot)"));
    }
    false
}

/// VK codes for the five mouse buttons. Hardcoded because the windows-rs
/// `VK_LBUTTON` etc. constants are `VIRTUAL_KEY` newtypes and matching them
/// in a `matches!` arm is more friction than it's worth here.
///
/// `VK_LBUTTON=0x01, VK_RBUTTON=0x02, VK_MBUTTON=0x04, VK_XBUTTON1=0x05,
/// VK_XBUTTON2=0x06`.
#[inline]
fn is_mouse_button_vk(vkey: i32) -> bool {
    matches!(vkey, 0x01 | 0x02 | 0x04 | 0x05 | 0x06)
}

/// One-shot diagnostic flags so we can verify each hooked polling API is
/// actually being called by the host process. Logs to `proc_diag` exactly
/// once per API per process. Idle CPU cost is a relaxed atomic load.
mod once_log {
    use core::sync::atomic::{AtomicBool, Ordering};

    pub static GET_ASYNC_KEY_STATE: AtomicBool = AtomicBool::new(false);
    pub static GET_KEY_STATE: AtomicBool = AtomicBool::new(false);
    pub static GET_KEYBOARD_STATE: AtomicBool = AtomicBool::new(false);
    pub static GET_RAW_INPUT_DATA: AtomicBool = AtomicBool::new(false);
    pub static GET_RAW_INPUT_BUFFER: AtomicBool = AtomicBool::new(false);
    pub static BLOCK_RAW_INPUT_DATA: AtomicBool = AtomicBool::new(false);
    pub static BLOCK_RAW_INPUT_BUFFER: AtomicBool = AtomicBool::new(false);

    // One-shot trace points inside `any_backend_in_hover_block`, so we can
    // see which decision path the gate is taking inside League without
    // flooding the proc_diag log.
    pub static GATE_NO_BACKENDS: AtomicBool = AtomicBool::new(false);
    pub static GATE_BCIO_FALSE: AtomicBool = AtomicBool::new(false);
    pub static GATE_BCIO_TRUE: AtomicBool = AtomicBool::new(false);
    pub static GATE_LAST_POS_NONE: AtomicBool = AtomicBool::new(false);
    pub static GATE_LAST_POS_OUT: AtomicBool = AtomicBool::new(false);
    pub static GATE_LAST_POS_IN: AtomicBool = AtomicBool::new(false);

    #[inline]
    pub fn check_and_set(flag: &AtomicBool) -> bool {
        // `Ordering::Relaxed` is fine: this is a debug fast path, the
        // payload is just "we want exactly one log line".
        !flag.swap(true, Ordering::Relaxed)
    }
}

#[tracing::instrument]
extern "system" fn hooked_clip_cursor(lprect: *const RECT) -> BOOL {
    if active_hwnd_with(|data| {
        data.clip_cursor = unsafe { lprect.as_ref() }.copied();
    })
    .is_some()
    {
        return BOOL(1);
    }

    unsafe { HOOK.wait().clip_cursor.original_fn()(lprect) }
}

#[tracing::instrument]
extern "system" fn hooked_set_cursor_pos(x: i32, y: i32) -> BOOL {
    if foreground_hwnd_input_blocked() {
        return BOOL(1);
    }

    unsafe { HOOK.wait().set_cursor_pos.original_fn()(x, y) }
}

#[tracing::instrument]
extern "system" fn hooked_get_clip_cursor(lprect: *mut RECT) -> BOOL {
    match active_hwnd_with(|data| data.clip_cursor).flatten() {
        Some(rect) => {
            unsafe { lprect.write(rect) };
            BOOL(1)
        }
        None => unsafe { HOOK.wait().get_clip_cursor.original_fn()(lprect) },
    }
}

#[tracing::instrument]
extern "system" fn hooked_get_cursor_pos(lppoint: *mut POINT) -> BOOL {
    if foreground_hwnd_input_blocked() {
        // Return a fixed position instead of the real cursor position to prevent games from tracking mouse movement
        if !lppoint.is_null() {
            unsafe {
                lppoint.write(POINT { x: 0, y: 0 });
            }
        }
        return BOOL(1);
    }

    unsafe { HOOK.wait().get_cursor_pos.original_fn()(lppoint) }
}

#[tracing::instrument]
extern "system" fn hooked_get_physical_cursor_pos(lppoint: *mut POINT) -> BOOL {
    if foreground_hwnd_input_blocked() {
        // Return a fixed position instead of the real cursor position to prevent games from tracking mouse movement
        if !lppoint.is_null() {
            unsafe {
                lppoint.write(POINT { x: 0, y: 0 });
            }
        }
        return BOOL(1);
    }

    unsafe { HOOK.wait().get_physical_cursor_pos.original_fn()(lppoint) }
}

#[tracing::instrument]
extern "system" fn hooked_get_async_key_state(vkey: i32) -> i16 {
    if once_log::check_and_set(&once_log::GET_ASYNC_KEY_STATE) {
        crate::proc_diag::log(format_args!(
            "hook fired: GetAsyncKeyState first call vkey={vkey:#x}"
        ));
    }
    if foreground_hwnd_input_blocked() {
        return 0;
    }
    // Hover-mode polling fix: pretend mouse buttons are up while the cursor
    // is over the overlay surface. The wndproc subclass already swallows
    // the corresponding `WM_LBUTTONDOWN` / `WM_INPUT`, but games like
    // League of Legends poll `GetAsyncKeyState(VK_LBUTTON)` directly to
    // detect clicks, which bypasses the wndproc path entirely.
    if is_mouse_button_vk(vkey) && any_backend_in_hover_block() {
        crate::proc_diag::log(format_args!(
            "block GetAsyncKeyState vkey={vkey:#x} (hover mode + cursor in overlay)"
        ));
        return 0;
    }

    unsafe { HOOK.wait().get_async_key_state.original_fn()(vkey) }
}

#[tracing::instrument]
extern "system" fn hooked_get_key_state(vkey: i32) -> i16 {
    if once_log::check_and_set(&once_log::GET_KEY_STATE) {
        crate::proc_diag::log(format_args!(
            "hook fired: GetKeyState first call vkey={vkey:#x}"
        ));
    }
    if active_hwnd_input_blocked() {
        return 0;
    }
    // Same rationale as `hooked_get_async_key_state` -- only the mouse
    // buttons are masked in hover mode so keyboard polling continues to
    // reach the game.
    if is_mouse_button_vk(vkey) && any_backend_in_hover_block() {
        crate::proc_diag::log(format_args!(
            "block GetKeyState vkey={vkey:#x} (hover mode + cursor in overlay)"
        ));
        return 0;
    }

    unsafe { HOOK.wait().get_key_state.original_fn()(vkey) }
}

#[tracing::instrument]
extern "system" fn hooked_get_keyboard_state(buf: *mut u8) -> BOOL {
    if once_log::check_and_set(&once_log::GET_KEYBOARD_STATE) {
        crate::proc_diag::log(format_args!("hook fired: GetKeyboardState first call"));
    }
    if active_hwnd_input_blocked() {
        // buf is 256 bytes array according to doc.
        unsafe {
            buf.write_bytes(0u8, 256);
        };
        return BOOL(1);
    }

    let original = unsafe { HOOK.wait().get_keyboard_state.original_fn()(buf) };
    // In hover mode we mask only the mouse-button slots so the game's
    // polling-based click detection lines up with the wndproc swallow.
    if original.as_bool() && !buf.is_null() && any_backend_in_hover_block() {
        crate::proc_diag::log(format_args!(
            "mask GetKeyboardState mouse slots (hover mode + cursor in overlay)"
        ));
        // VK_LBUTTON=0x01, VK_RBUTTON=0x02, VK_MBUTTON=0x04, VK_XBUTTON1=0x05,
        // VK_XBUTTON2=0x06. Slot 0x00 is reserved/unused.
        unsafe {
            for slot in [0x01u8, 0x02, 0x04, 0x05, 0x06] {
                buf.add(slot as usize).write(0);
            }
        }
    }
    original
}

#[tracing::instrument]
extern "system" fn hooked_get_raw_input_data(
    hrawinput: HRAWINPUT,
    uicommand: RAW_INPUT_DATA_COMMAND_FLAGS,
    pdata: *mut c_void,
    pcbsize: *mut u32,
    cbsizeheader: u32,
) -> u32 {
    if once_log::check_and_set(&once_log::GET_RAW_INPUT_DATA) {
        crate::proc_diag::log(format_args!(
            "hook fired: GetRawInputData first call uicommand={:#x}",
            uicommand.0
        ));
    }
    // Hover-mode + legacy block-mode share the same masking strategy
    // here: zero out the data structure so the game sees an empty raw
    // input event. This complements the queue filter that swallows
    // `WM_INPUT` messages -- if the game still finds a way to call
    // `GetRawInputData` (e.g. from a polling thread that owns the
    // HRAWINPUT) the data it gets back is harmless.
    if foreground_hwnd_input_blocked() || any_backend_in_hover_block() {
        if once_log::check_and_set(&once_log::BLOCK_RAW_INPUT_DATA) {
            crate::proc_diag::log(format_args!(
                "block GetRawInputData (hover/block mode + cursor in overlay)"
            ));
        }
        if !pdata.is_null() {
            match uicommand {
                RID_HEADER => {
                    unsafe {
                        pdata
                            .cast::<RAWINPUTHEADER>()
                            .write(RAWINPUTHEADER::default());
                    };
                }

                RID_INPUT => unsafe {
                    pdata.cast::<RAWINPUT>().write(RAWINPUT::default());
                },

                _ => {}
            }
        }

        return 0;
    }

    unsafe {
        HOOK.wait().get_raw_input_data.original_fn()(
            hrawinput,
            uicommand,
            pdata,
            pcbsize,
            cbsizeheader,
        )
    }
}

#[tracing::instrument]
extern "system" fn hooked_get_raw_input_buffer(
    pdata: *mut RAWINPUT,
    pcbsize: *mut u32,
    cbsizeheader: u32,
) -> u32 {
    if once_log::check_and_set(&once_log::GET_RAW_INPUT_BUFFER) {
        crate::proc_diag::log(format_args!("hook fired: GetRawInputBuffer first call"));
    }
    if foreground_hwnd_input_blocked() || any_backend_in_hover_block() {
        if once_log::check_and_set(&once_log::BLOCK_RAW_INPUT_BUFFER) {
            crate::proc_diag::log(format_args!(
                "block GetRawInputBuffer (hover/block mode + cursor in overlay)"
            ));
        }
        if !pcbsize.is_null() {
            unsafe { *pcbsize = 0 };
        }
        return 0;
    }

    unsafe { HOOK.wait().get_raw_input_buffer.original_fn()(pdata, pcbsize, cbsizeheader) }
}
