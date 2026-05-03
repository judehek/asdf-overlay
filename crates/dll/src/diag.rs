//! Minimal always-on diagnostic logger for the DLL.
//!
//! Writes timestamped lines to `%TEMP%\asdf_overlay_diag.log`. This is
//! intentionally simple -- no tracing subscriber, no async I/O, no crate
//! deps -- so it works unchanged in release builds and can't itself
//! introduce new failure modes. Logging is best-effort: a failed write is
//! silently dropped rather than panicking inside the game process.
//!
//! This file is a temporary debugging aid while we track down the IPC
//! disconnect that happens when the client sends `BlockCursorInOverlay`.
//! Remove once the root cause is fixed.

use std::{
    fs::OpenOptions,
    io::Write,
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

fn log_path() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::env::var("TEMP")
            .or_else(|_| std::env::var("TMP"))
            .unwrap_or_else(|_| "C:\\".to_string());
        let pid = unsafe {
            windows::Win32::System::Threading::GetCurrentProcessId()
        };
        format!("{dir}\\asdf_overlay_diag_{pid}.log")
    })
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Append a single diagnostic line. Best-effort; swallows all errors.
pub fn log(msg: impl AsRef<str>) {
    let _ = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path())?;
        writeln!(f, "{} {}", now_ms(), msg.as_ref())
    })();
}
