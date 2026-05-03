//! Temporary always-on diagnostic logger for the overlay crate's input path.
//!
//! Writes to `%TEMP%\asdf_overlay_proc_<pid>.log`. Swallows all IO errors.
//! Intended to be removed once the hover-based input path is confirmed working.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn log_path() -> PathBuf {
    let pid = std::process::id();
    let mut p = std::env::temp_dir();
    p.push(format!("asdf_overlay_proc_{pid}.log"));
    p
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn log(args: std::fmt::Arguments<'_>) {
    let _ = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path())?;
        writeln!(f, "{} {}", now_ms(), args)
    })();
}
