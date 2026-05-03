use super::WindowBackend;
use crate::{
    backend::{
        BACKENDS, Backends,
        window::{CursorState, ImeState, WindowProcData, cursor::load_cursor},
    },
    event_sink::OverlayEventSink,
    util::get_client_size,
};
use asdf_overlay_event::{
    OverlayEvent, WindowEvent,
    input::{
        ConversionMode, CursorAction, CursorEvent, CursorInput, CursorInputState, Ime,
        ImeCandidateList, InputEvent, InputPosition, KeyboardInput, ScrollAxis,
    },
};
use core::{alloc::Layout, mem, slice};
use parking_lot::MutexGuard;
use scopeguard::defer;
use std::alloc;
use tracing::trace;
use utf16string::{LittleEndian, WStr, WString};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        Globalization::LCIDToLocaleName,
        System::SystemServices::{LOCALE_NAME_MAX_LENGTH, SORT_DEFAULT},
        UI::{
            Controls::{self, HOVER_DEFAULT},
            Input::{
                Ime::{
                    self as ime, CANDIDATELIST, HIMC, IME_COMPOSITION_STRING, IME_CONVERSION_MODE,
                    ImmGetCandidateListW, ImmGetCompositionStringW, ImmGetContext,
                    ImmGetConversionStatus, ImmReleaseContext,
                },
                KeyboardAndMouse::{
                    GetCapture, GetDoubleClickTime, GetKeyboardLayout, ReleaseCapture, SetCapture,
                    TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
                },
            },
            WindowsAndMessaging::{
                self as msg, CallWindowProcA, DefWindowProcA, GetMessageTime, SetCursor,
                WM_NCDESTROY, XBUTTON1,
            },
        },
    },
    core::BOOL,
};

// `GetCursorPos` and `ScreenToClient` are linked manually so we don't
// have to enable additional `windows` crate features. Only the bare
// signatures are needed; the macro generates a thin extern wrapper.
windows::core::link!("user32.dll" "system" fn GetCursorPos(lppoint: *mut POINT) -> BOOL);
windows::core::link!("user32.dll" "system" fn ScreenToClient(hwnd: HWND, lppoint: *mut POINT) -> BOOL);

/// Decide whether a cursor event at (x, y) in client coords should be
/// consumed (not passed to the game's original wndproc).
///
/// * Always-on `BlockInput` → consume (legacy behavior).
/// * Position-filtered `block_cursor_in_overlay`:
///   * If the game window currently has mouse capture, we're mid-drag that
///     started inside the overlay -- keep consuming so the drag completes
///     cleanly even if the cursor wanders outside.
///   * Otherwise, consume iff the cursor is inside the overlay rect.
/// * Otherwise → pass through.
#[inline]
fn should_consume_cursor(proc: &WindowProcData, hwnd_id: u32, x: i16, y: i16) -> bool {
    let blocking = proc.input_blocking();
    let bcio = proc.block_cursor_in_overlay;
    let cap = unsafe { GetCapture() }.0 as u32;
    let (ox, oy) = proc.position;
    let (sw, sh) = proc.surface_size;
    let in_overlay = proc.cursor_in_overlay(x, y);
    let result = if blocking {
        true
    } else if !bcio {
        false
    } else if cap == hwnd_id {
        true
    } else {
        in_overlay
    };
    crate::proc_diag::log(format_args!(
        "consume_cursor hwnd={hwnd_id} xy=({x},{y}) blocking={blocking} bcio={bcio} cap={cap} pos=({ox},{oy}) size=({sw},{sh}) in_overlay={in_overlay} -> {result}"
    ));
    result
}

/// Same decision as [`should_consume_cursor`] but for `WM_MOUSEWHEEL` /
/// `WM_MOUSEHWHEEL`, whose lparam carries screen coords (not client) and so
/// isn't useful for hit-testing. We fall back to the last-known client
/// position tracked in `cursor_state`.
#[inline]
fn wheel_should_consume(proc: &WindowProcData, hwnd_id: u32) -> bool {
    if proc.input_blocking() {
        return true;
    }
    if !proc.block_cursor_in_overlay {
        return false;
    }
    if unsafe { GetCapture() }.0 as u32 == hwnd_id {
        return true;
    }
    match proc.last_cursor_client_pos() {
        Some((x, y)) => proc.cursor_in_overlay(x, y),
        None => false,
    }
}

#[inline]
fn process_wnd_proc(
    backend: &WindowBackend,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    match msg {
        msg::WM_WINDOWPOSCHANGED => {
            let new_size = get_client_size(HWND(backend.id as _)).unwrap();
            let mut render = backend.render.lock();
            if render.window_size != new_size {
                render.window_size = new_size;

                OverlayEventSink::emit(OverlayEvent::Window {
                    id: backend.id,
                    event: WindowEvent::Resized {
                        width: new_size.0,
                        height: new_size.1,
                    },
                });
            }
            drop(render);
            backend.invalidate_layout();
        }

        // set cursor in client area
        msg::WM_SETCURSOR
            if {
                let [area, _] = bytemuck::cast::<_, [u16; 2]>(lparam.0 as u32);
                // check if cursor is on client
                area == 1
            } =>
        {
            // Resolve `GetCursorPos` *before* taking `backend.proc.lock()`.
            // Our hook `hooked_get_cursor_pos` calls
            // `foreground_hwnd_input_blocked()`, which itself takes
            // `backend.proc.lock()`. parking_lot's mutex is non-reentrant,
            // so calling the detoured `GetCursorPos` from inside our own
            // proc lock on the message-pump thread re-enters the same lock
            // and permanently deadlocks the host's message loop -- League
            // freezes (black screen + Windows "not responding"). Same bug
            // we fixed previously in `should_filter_message`.
            //
            // `ScreenToClient` is NOT detoured, so it's safe inside the
            // lock.
            let mut cursor_screen = POINT::default();
            let cursor_ok = unsafe { GetCursorPos(&mut cursor_screen) }.as_bool();

            let proc = backend.proc.lock();
            if proc.input_blocking() {
                unsafe { SetCursor(proc.blocking_cursor.and_then(load_cursor)) };
                return Some(LRESULT(1));
            }
            // Hover mode: when the cursor is over the overlay rect,
            // override the game's cursor with our own. League sets a
            // custom cursor in WM_SETCURSOR every frame and Windows
            // draws it using the hardware cursor on top of any DComp
            // visual, including our composed overlay surface. By
            // claiming WM_SETCURSOR here we replace it with the overlay
            // cursor (currently `proc.blocking_cursor`, defaulting to
            // IDC_ARROW; later this will reflect WebView2's hovered
            // cursor type via `SetBlockingCursor`).
            //
            // Hit-test on live `GetCursorPos` rather than the cached
            // `cursor_state`, same as `should_filter_message` and
            // `any_backend_in_hover_block`, because the cache is
            // unreliable when the cursor sits over a child window.
            if proc.block_cursor_in_overlay && cursor_ok {
                let mut p = cursor_screen;
                if unsafe { ScreenToClient(HWND(backend.id as _), &mut p) }.as_bool()
                    && proc.cursor_in_overlay(p.x as i16, p.y as i16)
                {
                    unsafe { SetCursor(proc.blocking_cursor.and_then(load_cursor)) };
                    return Some(LRESULT(1));
                }
            }
        }

        // stop input capture when user request to
        msg::WM_CLOSE => {
            let input_blocking = backend.proc.lock().input_blocking();
            if input_blocking {
                backend.block_input(false);
                return Some(LRESULT(0));
            }
        }

        msg::WM_LBUTTONDOWN | msg::WM_LBUTTONDBLCLK => {
            let mut proc = backend.proc.lock();
            let state = CursorInputState::Pressed {
                double_click: check_double_click(&mut proc),
            };
            return cursor_event::<0>(backend.id, proc, CursorAction::Left, state, lparam);
        }

        msg::WM_MBUTTONDOWN | msg::WM_MBUTTONDBLCLK => {
            let mut proc = backend.proc.lock();
            let state = CursorInputState::Pressed {
                double_click: check_double_click(&mut proc),
            };
            return cursor_event::<0>(backend.id, proc, CursorAction::Middle, state, lparam);
        }

        msg::WM_RBUTTONDOWN | msg::WM_RBUTTONDBLCLK => {
            let mut proc = backend.proc.lock();
            let state = CursorInputState::Pressed {
                double_click: check_double_click(&mut proc),
            };
            return cursor_event::<0>(backend.id, proc, CursorAction::Right, state, lparam);
        }

        msg::WM_XBUTTONDOWN | msg::WM_XBUTTONDBLCLK => {
            let [_, button] = bytemuck::cast::<_, [u16; 2]>(lparam.0 as u32);
            let mut proc = backend.proc.lock();
            let state = CursorInputState::Pressed {
                double_click: check_double_click(&mut proc),
            };
            return cursor_event::<1>(
                backend.id,
                proc,
                if button == XBUTTON1 {
                    CursorAction::Back
                } else {
                    CursorAction::Forward
                },
                state,
                lparam,
            );
        }

        msg::WM_LBUTTONUP => {
            return cursor_event::<0>(
                backend.id,
                backend.proc.lock(),
                CursorAction::Left,
                CursorInputState::Released,
                lparam,
            );
        }
        msg::WM_MBUTTONUP => {
            return cursor_event::<0>(
                backend.id,
                backend.proc.lock(),
                CursorAction::Middle,
                CursorInputState::Released,
                lparam,
            );
        }
        msg::WM_RBUTTONUP => {
            return cursor_event::<0>(
                backend.id,
                backend.proc.lock(),
                CursorAction::Right,
                CursorInputState::Released,
                lparam,
            );
        }
        msg::WM_XBUTTONUP => {
            let [_, button] = bytemuck::cast::<_, [u16; 2]>(lparam.0 as u32);
            return cursor_event::<1>(
                backend.id,
                backend.proc.lock(),
                if button == XBUTTON1 {
                    CursorAction::Back
                } else {
                    CursorAction::Forward
                },
                CursorInputState::Released,
                lparam,
            );
        }

        Controls::WM_MOUSELEAVE => {
            let mut proc = backend.proc.lock();
            // Always reset `cursor_state` so position-filtered hover gates
            // (`wheel_should_consume`, hover-mode polling-API gate, ...)
            // see Outside as soon as the cursor leaves the window. Event
            // emission is still gated on `listening_cursor()`.
            let was_listening = proc.listening_cursor();
            proc.cursor_state = CursorState::Outside;

            if was_listening {
                OverlayEventSink::emit(cursor_input(
                    backend.id,
                    proc.position,
                    lparam,
                    CursorEvent::Leave,
                ));

                if proc.input_blocking() {
                    return Some(LRESULT(0));
                }
            }
        }

        msg::WM_MOUSEMOVE => {
            let mut proc = backend.proc.lock();
            let [x, y] = bytemuck::cast::<_, [i16; 2]>(lparam.0 as u32);

            // Always keep `cursor_state` in sync. Without this the
            // `Outside → Inside(x, y)` transition only fires when the
            // client explicitly subscribed via `ListenInput` (or legacy
            // `BlockInput` is on), so position-filtered consumers
            // (`wheel_should_consume`, hover-mode polling-API gate)
            // observe a stale `Outside` state in pure hover mode and
            // fail to fire. Event emission is still gated on
            // `listening_cursor()` so we don't change observable IPC.
            let was_outside = matches!(proc.cursor_state, CursorState::Outside);
            match proc.cursor_state {
                CursorState::Inside(ref mut old_x, ref mut old_y) => {
                    *old_x = x;
                    *old_y = y;
                }
                CursorState::Outside => {
                    proc.cursor_state = CursorState::Inside(x, y);
                    crate::proc_diag::log(format_args!(
                        "wm_mousemove Outside->Inside id={} x={} y={} (first transition this run will be visible)",
                        backend.id, x, y
                    ));
                }
            }

            // `TrackMouseEvent` must be (re-)armed every Outside →
            // Inside transition so we get the matching `WM_MOUSELEAVE`
            // and reset state cleanly. Cheap and idempotent.
            if was_outside {
                _ = unsafe {
                    TrackMouseEvent(&mut TRACKMOUSEEVENT {
                        cbSize: mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: HWND(backend.id as _),
                        dwHoverTime: HOVER_DEFAULT,
                    })
                };
            }

            if proc.listening_cursor() {
                if was_outside {
                    OverlayEventSink::emit(cursor_input(
                        backend.id,
                        proc.position,
                        lparam,
                        CursorEvent::Enter,
                    ));
                }

                OverlayEventSink::emit(cursor_input(
                    backend.id,
                    proc.position,
                    lparam,
                    CursorEvent::Move,
                ));
            }

            // Consume move events while the cursor is over the overlay (or
            // while dragging after a press started inside it). Upstream
            // didn't consume WM_MOUSEMOVE at all -- keep that behavior for
            // the legacy BlockInput path, but apply the new rule when
            // `block_cursor_in_overlay` is active.
            if proc.block_cursor_in_overlay
                && (unsafe { GetCapture() }.0 as u32 == backend.id || proc.cursor_in_overlay(x, y))
            {
                return Some(LRESULT(0));
            }
        }

        msg::WM_MOUSEWHEEL => {
            let proc = backend.proc.lock();
            if !proc.listening_cursor() {
                return None;
            }

            let [_, delta] = bytemuck::cast::<_, [i16; 2]>(wparam.0 as u32);
            OverlayEventSink::emit(cursor_input(
                backend.id,
                proc.position,
                lparam,
                CursorEvent::Scroll {
                    axis: ScrollAxis::Y,
                    delta,
                },
            ));

            if wheel_should_consume(&proc, backend.id) {
                return Some(LRESULT(0));
            }
        }

        msg::WM_MOUSEHWHEEL => {
            let proc = backend.proc.lock();
            if !proc.listening_cursor() {
                return None;
            }

            let [_, delta] = bytemuck::cast::<_, [i16; 2]>(wparam.0 as u32);
            OverlayEventSink::emit(cursor_input(
                backend.id,
                proc.position,
                lparam,
                CursorEvent::Scroll {
                    axis: ScrollAxis::X,
                    delta,
                },
            ));

            if wheel_should_consume(&proc, backend.id) {
                return Some(LRESULT(0));
            }
        }

        msg::WM_APPCOMMAND => {
            let input_blocking = backend.proc.lock().input_blocking();
            if input_blocking {
                return Some(unsafe { DefWindowProcA(HWND(backend.id as _), msg, wparam, lparam) });
            }
        }

        // Block WM_POINTER* by forwarding to DefWindowProc, which converts them
        // to legacy WM_LBUTTON*/WM_MOUSEMOVE/WM_MOUSEWHEEL messages.
        // Those legacy messages then re-enter this WndProc where they are
        // emitted to the overlay UI and blocked from the game.
        msg::WM_POINTERUPDATE
        | msg::WM_POINTERDOWN
        | msg::WM_POINTERUP
        | msg::WM_POINTERENTER
        | msg::WM_POINTERLEAVE
        | msg::WM_POINTERACTIVATE
        | msg::WM_POINTERCAPTURECHANGED
        | msg::WM_POINTERWHEEL
        | msg::WM_POINTERHWHEEL => {
            let proc = backend.proc.lock();
            if proc.input_blocking() {
                return Some(unsafe { DefWindowProcA(HWND(backend.id as _), msg, wparam, lparam) });
            }
        }

        // WM_INPUT (Raw Input) delivery. Modern games read mouse clicks via
        // `RegisterRawInputDevices` + `GetRawInputData`, which bypasses the
        // usual legacy `WM_?BUTTON*` messages. The game is notified about new
        // raw-input records by `WM_INPUT` posted to its focused window. If
        // we never chain that message into the original wndproc, the game's
        // wndproc is never notified and never calls `GetRawInputData` for
        // that sequence, so the event is effectively dropped for that app
        // (the kernel's per-process input queue will age it out).
        //
        // We apply the same hit-test as the legacy cursor consume:
        //   * Legacy full-block → consume unconditionally.
        //   * `block_cursor_in_overlay` → consume while over the overlay rect,
        //     or while the game window has mouse capture (mid-drag that
        //     started over the overlay).
        //   * Otherwise → pass through, so outside-overlay clicks behave
        //     exactly as they do without the overlay attached.
        msg::WM_INPUT => {
            let proc = backend.proc.lock();
            let blocking = proc.input_blocking();
            let bcio = proc.block_cursor_in_overlay;
            let (cap_ours, in_overlay) = if bcio && !blocking {
                let cap_ours = unsafe { GetCapture() }.0 as u32 == backend.id;
                let in_overlay = match proc.last_cursor_client_pos() {
                    Some((x, y)) => proc.cursor_in_overlay(x, y),
                    None => false,
                };
                (cap_ours, in_overlay)
            } else {
                (false, false)
            };
            let consume = blocking || (bcio && (cap_ours || in_overlay));
            crate::proc_diag::log(format_args!(
                "wm_input hwnd={} blocking={} bcio={} cap_ours={} in_overlay={} -> consume={}",
                backend.id, blocking, bcio, cap_ours, in_overlay, consume
            ));
            if consume {
                return Some(LRESULT(0));
            }
        }

        // block other keyboard, mouse event
        msg::WM_CAPTURECHANGED
        | msg::WM_ACTIVATE
        | msg::WM_ACTIVATEAPP
        | msg::WM_SETFOCUS
        | msg::WM_KILLFOCUS
        | msg::WM_DEADCHAR
        | msg::WM_HOTKEY
        | msg::WM_SYSDEADCHAR
        | msg::WM_UNICHAR
        | msg::WM_IME_REQUEST => {
            let proc = backend.proc.lock();
            if proc.input_blocking() {
                return Some(LRESULT(0));
            }
        }

        msg::WM_INPUTLANGCHANGEREQUEST => {
            let input_blocking = backend.proc.lock().input_blocking();
            if input_blocking {
                return Some(unsafe { DefWindowProcA(HWND(backend.id as _), msg, wparam, lparam) });
            }
        }

        msg::WM_IME_NOTIFY => {
            let proc = backend.proc.lock();
            if !proc.listening_keyboard() {
                return None;
            }

            handle_ime_notify(backend.id, wparam.0 as _);
            if proc.input_blocking() {
                drop(proc);
                return Some(LRESULT(0));
            }
        }

        msg::WM_INPUTLANGCHANGE => {
            let proc = backend.proc.lock();
            if !proc.listening_keyboard() {
                return None;
            }

            if let Some(lang) = get_lang_id_locale(lparam.0 as u16) {
                OverlayEventSink::emit(keyboard_input(
                    backend.id,
                    KeyboardInput::Ime(Ime::Changed(lang)),
                ));
            }

            if proc.input_blocking() {
                return Some(LRESULT(0));
            }
        }

        msg::WM_IME_SETCONTEXT => {
            let proc = backend.proc.lock();
            if !proc.listening_keyboard() {
                return None;
            }

            let lang_id = unsafe { GetKeyboardLayout(0) }.0 as u16;
            OverlayEventSink::emit(keyboard_input(
                backend.id,
                KeyboardInput::Ime(if wparam.0 != 0 {
                    Ime::Enabled {
                        lang: get_lang_id_locale(lang_id).unwrap_or_else(|| "en".to_string()),
                        conversion: with_himc(backend.id, ime_conversion_mode),
                    }
                } else {
                    Ime::Disabled
                }),
            ));

            if proc.input_blocking() {
                drop(proc);
                return Some(unsafe {
                    DefWindowProcA(
                        HWND(backend.id as _),
                        msg,
                        wparam,
                        // Disable composition, candinate window
                        LPARAM(0),
                    )
                });
            }
        }

        msg::WM_IME_STARTCOMPOSITION => {
            let mut proc = backend.proc.lock();
            proc.ime = ImeState::Enabled;
            if proc.input_blocking() {
                return Some(LRESULT(0));
            }
        }

        msg::WM_IME_COMPOSITION => {
            let mut proc = backend.proc.lock();
            if !proc.listening_keyboard() {
                return None;
            }

            if proc.ime != ImeState::Disabled {
                with_himc(backend.id, |himc| {
                    let comp = IME_COMPOSITION_STRING(lparam.0 as _);

                    // cancelled
                    if comp == IME_COMPOSITION_STRING(0) {
                        OverlayEventSink::emit(keyboard_input(
                            backend.id,
                            KeyboardInput::Ime(Ime::Commit(String::new())),
                        ));
                    }

                    if comp.contains(ime::GCS_RESULTSTR)
                        && let Some(text) = get_ime_string(himc, ime::GCS_RESULTSTR)
                    {
                        proc.ime = ImeState::Enabled;
                        OverlayEventSink::emit(keyboard_input(
                            backend.id,
                            KeyboardInput::Ime(Ime::Commit(text.to_utf8())),
                        ));
                    }

                    if comp.0 & (ime::GCS_COMPSTR | ime::GCS_COMPATTR | ime::GCS_CURSORPOS).0 != 0 {
                        let caret = if !comp.contains(IME_COMPOSITION_STRING(ime::CS_NOMOVECARET))
                            && comp.contains(ime::GCS_CURSORPOS)
                        {
                            unsafe {
                                ImmGetCompositionStringW(himc, ime::GCS_CURSORPOS, None, 0) as usize
                            }
                        } else {
                            0
                        };

                        if let Some(text) = get_ime_string(himc, ime::GCS_COMPSTR) {
                            proc.ime = ImeState::Compose;

                            OverlayEventSink::emit(keyboard_input(
                                backend.id,
                                KeyboardInput::Ime(Ime::Compose {
                                    text: text.to_utf8(),
                                    caret,
                                }),
                            ));
                        }
                    }
                });
            }

            if proc.input_blocking() {
                return Some(LRESULT(0));
            }
        }

        msg::WM_IME_ENDCOMPOSITION => {
            let mut proc = backend.proc.lock();
            let ime = proc.ime;
            proc.ime = ImeState::Disabled;

            if ime == ImeState::Compose {
                let hwnd = HWND(backend.id as _);
                let himc = unsafe { ImmGetContext(hwnd) };
                defer!(unsafe {
                    _ = ImmReleaseContext(hwnd, himc);
                });
                if let Some(text) = get_ime_string(himc, ime::GCS_RESULTSTR) {
                    OverlayEventSink::emit(keyboard_input(
                        backend.id,
                        KeyboardInput::Ime(Ime::Commit(text.to_utf8())),
                    ));
                }
            }

            if proc.input_blocking() {
                return Some(LRESULT(0));
            }
        }

        _ => {}
    }
    None
}

fn handle_ime_notify(hwnd: u32, command: u32) {
    match command {
        ime::IMN_SETCONVERSIONMODE => OverlayEventSink::emit(keyboard_input(
            hwnd,
            KeyboardInput::Ime(Ime::ConversionChanged(with_himc(hwnd, ime_conversion_mode))),
        )),

        ime::IMN_OPENCANDIDATE | ime::IMN_CHANGECANDIDATE => {
            with_himc(hwnd, |himc| {
                if let Some(candidate_list) = get_ime_candidate_list(himc, 0) {
                    OverlayEventSink::emit(keyboard_input(
                        hwnd,
                        KeyboardInput::Ime(Ime::CandidateChanged(candidate_list)),
                    ));
                }
            });
        }

        ime::IMN_CLOSECANDIDATE => OverlayEventSink::emit(keyboard_input(
            hwnd,
            KeyboardInput::Ime(Ime::CandidateClosed),
        )),

        _ => {}
    }
}

#[tracing::instrument]
pub(crate) unsafe extern "system" fn hooked_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    trace!("WndProc called");

    defer!({
        // cleanup backend
        if msg == WM_NCDESTROY {
            trace!("cleanup hwnd: {:?}", hwnd);
            Backends::remove_backend(hwnd);
        }
    });

    let backend = BACKENDS.map.get(&(hwnd.0 as u32)).unwrap();
    if let Some(ret) = process_wnd_proc(&backend, msg, wparam, lparam) {
        return ret;
    }
    let original_proc = backend.original_proc;
    drop(backend);
    unsafe { CallWindowProcA(original_proc, hwnd, msg, wparam, lparam) }
}

#[inline]
fn cursor_event<const BLOCK_RESULT: isize>(
    hwnd: u32,
    proc: MutexGuard<WindowProcData>,
    action: CursorAction,
    state: CursorInputState,
    lparam: LPARAM,
) -> Option<LRESULT> {
    if !proc.listening_cursor() {
        return None;
    }

    OverlayEventSink::emit(cursor_input(
        hwnd,
        proc.position,
        lparam,
        CursorEvent::Action { action, state },
    ));

    // lparam low/high words encode (x, y) in client coords for all
    // WM_?BUTTON* messages. Use them for the position-based consume
    // decision -- for the legacy `input_blocking` path the values don't
    // matter since it consumes unconditionally.
    let [x, y] = bytemuck::cast::<_, [i16; 2]>(lparam.0 as u32);
    if should_consume_cursor(&proc, hwnd, x, y) {
        // prevent deadlock
        drop(proc);
        match state {
            CursorInputState::Pressed { .. } => unsafe {
                SetCapture(HWND(hwnd as _));
            },
            CursorInputState::Released => unsafe {
                _ = ReleaseCapture();
            },
        }

        Some(LRESULT(BLOCK_RESULT))
    } else {
        None
    }
}

#[inline]
fn check_double_click(proc: &mut WindowProcData) -> bool {
    proc.update_click_time(unsafe { GetMessageTime() }) <= unsafe { GetDoubleClickTime() }
}

#[inline]
fn cursor_input(id: u32, position: (i32, i32), lparam: LPARAM, event: CursorEvent) -> OverlayEvent {
    let [x, y] = bytemuck::cast::<_, [i16; 2]>(lparam.0 as u32);

    let window = InputPosition {
        x: x as _,
        y: y as _,
    };
    let surface = InputPosition {
        x: window.x - position.0,
        y: window.y - position.1,
    };
    OverlayEvent::Window {
        id,
        event: WindowEvent::Input(InputEvent::Cursor(CursorInput {
            event,
            client: surface,
            window,
        })),
    }
}

#[inline(always)]
fn keyboard_input(id: u32, input: KeyboardInput) -> OverlayEvent {
    OverlayEvent::Window {
        id,
        event: WindowEvent::Input(InputEvent::Keyboard(input)),
    }
}

#[inline]
fn with_himc<R>(hwnd: u32, f: impl FnOnce(HIMC) -> R) -> R {
    let hwnd = HWND(hwnd as _);
    let himc = unsafe { ImmGetContext(hwnd) };
    defer!(unsafe {
        _ = ImmReleaseContext(hwnd, himc);
    });

    f(himc)
}

fn get_ime_string(himc: HIMC, comp: IME_COMPOSITION_STRING) -> Option<WString<LittleEndian>> {
    let byte_size = unsafe { ImmGetCompositionStringW(himc, comp, None, 0) };
    if byte_size >= 0 {
        let mut buf = vec![0_u8; byte_size as usize];

        unsafe {
            ImmGetCompositionStringW(himc, comp, Some(buf.as_mut_ptr().cast()), buf.len() as _)
        };

        WString::from_utf16le(buf).ok()
    } else {
        None
    }
}

fn get_ime_candidate_list(himc: HIMC, index: u32) -> Option<ImeCandidateList> {
    let byte_size = unsafe { ImmGetCandidateListW(himc, index, None, 0) };
    if byte_size == 0 {
        return None;
    }

    let layout = Layout::from_size_align(byte_size as _, mem::align_of::<CANDIDATELIST>()).ok()?;
    let mut candidate_list_ptr = scopeguard::guard(
        unsafe { alloc::alloc(layout) }.cast::<CANDIDATELIST>(),
        |ptr| unsafe {
            alloc::dealloc(ptr as _, layout);
        },
    );

    let res = unsafe { ImmGetCandidateListW(himc, index, Some(*candidate_list_ptr), byte_size) };
    if res == 0 {
        return None;
    }

    let CANDIDATELIST {
        dwCount: count,
        dwSelection: selected_index,
        dwPageStart: page_start_index,
        dwPageSize: page_size,
        ..
    } = unsafe { **candidate_list_ptr };
    let candidates = {
        let mut list = Vec::with_capacity(count as _);
        let base = unsafe { &raw mut (**candidate_list_ptr).dwOffset }.cast::<u32>();
        for i in 0..count {
            let candidate_offset = unsafe { *base.add(i as _) };
            let candidate_start = unsafe {
                candidate_list_ptr
                    .byte_add(candidate_offset as _)
                    .cast::<u16>()
            };
            let size = {
                let mut len = 0;
                while (unsafe { *candidate_start.add(len) }) != 0 {
                    len += 1;
                }
                len * 2
            };

            list.push(
                unsafe {
                    WStr::from_utf16le_unchecked(slice::from_raw_parts(
                        candidate_start.cast::<u8>(),
                        size,
                    ))
                }
                .to_utf8(),
            );
        }
        list
    };

    Some(ImeCandidateList {
        page_start_index,
        page_size,
        selected_index,
        candidates,
    })
}

fn get_lang_id_locale(lang_id: u16) -> Option<String> {
    let lcid = const { SORT_DEFAULT << 16 } | lang_id as u32;

    let mut buf = [0_u16; LOCALE_NAME_MAX_LENGTH as usize];
    let size = unsafe { LCIDToLocaleName(lcid, Some(&mut buf), 0) };
    if size > 0 {
        Some(
            WStr::from_utf16le(bytemuck::cast_slice::<_, u8>(&buf[..(size - 1) as usize]))
                .ok()?
                .to_utf8(),
        )
    } else {
        None
    }
}

fn ime_conversion_mode(himc: HIMC) -> ConversionMode {
    let mut raw_mode = IME_CONVERSION_MODE(0);
    _ = unsafe { ImmGetConversionStatus(himc, Some(&mut raw_mode), None) };

    let mut mode = ConversionMode::empty();
    if raw_mode.contains(ime::IME_CMODE_NATIVE) {
        mode |= ConversionMode::NATIVE;
    }
    if raw_mode.contains(ime::IME_CMODE_FULLSHAPE) {
        mode |= ConversionMode::FULLSHAPE;
    }
    if raw_mode.contains(ime::IME_CMODE_NOCONVERSION) {
        mode |= ConversionMode::NO_CONVERSION;
    }
    if raw_mode.contains(ime::IME_CMODE_HANJACONVERT) {
        mode |= ConversionMode::HANJA_CONVERT;
    }
    if raw_mode.contains(ime::IME_CMODE_KATAKANA) {
        mode |= ConversionMode::KATAKANA;
    }
    mode
}
