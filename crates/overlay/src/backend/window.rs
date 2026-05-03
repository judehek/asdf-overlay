pub(crate) mod cursor;
pub(crate) mod proc;

use super::WindowBackend;
use asdf_overlay_common::cursor::Cursor;
use windows::Win32::Foundation::RECT;

pub(crate) struct WindowProcData {
    pub position: (i32, i32),
    /// Cached copy of the overlay surface's size in physical pixels, so the
    /// `hooked_wnd_proc` can do in-bounds hit-tests without taking the
    /// (different) `render` lock (which would invert the lock order used by
    /// `WindowBackend::invalidate_layout`).
    pub surface_size: (u32, u32),

    pub listen_input: ListenInputFlags,
    pub blocking_state: Option<InputBlockData>,
    pub blocking_cursor: Option<Cursor>,
    /// If true, cursor events whose position falls inside the overlay rect
    /// are consumed (not passed to the game's original wndproc). Events
    /// outside pass through unchanged. Orthogonal to `blocking_state`.
    pub block_cursor_in_overlay: bool,

    pub(crate) cursor_state: CursorState,
    ime: ImeState,
    last_click_time: i32,
}

impl WindowProcData {
    pub fn new() -> Self {
        Self {
            position: (0, 0),
            surface_size: (0, 0),

            listen_input: ListenInputFlags::empty(),
            blocking_state: None,
            blocking_cursor: Some(Cursor::Default),
            block_cursor_in_overlay: false,

            cursor_state: CursorState::Outside,
            ime: ImeState::Disabled,
            last_click_time: 0,
        }
    }

    pub fn reset(&mut self) {
        self.position = (0, 0);
        self.surface_size = (0, 0);
        self.listen_input = ListenInputFlags::empty();
        self.blocking_cursor = Some(Cursor::Default);
        self.block_cursor_in_overlay = false;
    }

    #[inline]
    pub fn listening_cursor(&self) -> bool {
        self.listen_input.contains(ListenInputFlags::CURSOR) || self.blocking_state.is_some()
    }

    #[inline]
    pub fn listening_keyboard(&self) -> bool {
        self.listen_input.contains(ListenInputFlags::KEYBOARD) || self.blocking_state.is_some()
    }

    #[inline]
    pub fn input_blocking(&self) -> bool {
        self.blocking_state.is_some()
    }

    /// Is a cursor position (x, y) in window client coords inside the
    /// overlay surface's currently-laid-out rect? Returns false when the
    /// surface has zero size (no overlay texture bound yet).
    #[inline]
    pub fn cursor_in_overlay(&self, x: i16, y: i16) -> bool {
        let (w, h) = self.surface_size;
        if w == 0 || h == 0 {
            return false;
        }
        let (ox, oy) = self.position;
        let x = x as i32;
        let y = y as i32;
        x >= ox && x < ox + (w as i32) && y >= oy && y < oy + (h as i32)
    }

    /// Last-known cursor position in window client coords, if the cursor
    /// has been seen inside this window since the last leave.
    #[inline]
    pub fn last_cursor_client_pos(&self) -> Option<(i16, i16)> {
        match self.cursor_state {
            CursorState::Inside(x, y) => Some((x, y)),
            CursorState::Outside => None,
        }
    }

    pub fn update_click_time(&mut self, new_time: i32) -> u32 {
        let delta = (new_time as u32).wrapping_sub(self.last_click_time as _);
        self.last_click_time = new_time;
        delta
    }
}

#[derive(Clone, Copy)]
pub(crate) struct InputBlockData {
    pub clip_cursor: Option<RECT>,
    pub old_ime_cx: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorState {
    Inside(i16, i16),
    Outside,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImeState {
    Enabled,
    Compose,
    Disabled,
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    /// Flags for listening to input events.
    pub struct ListenInputFlags: u8 {
        /// Listen for cursor events.
        const CURSOR = 0b00000001;
        /// Listen for keyboard events.
        const KEYBOARD = 0b00000010;
    }
}
