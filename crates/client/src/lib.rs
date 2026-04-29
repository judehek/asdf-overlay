//! Library for attaching `asdf-overlay` to a process and initiating IPC channel.
//!
//! By utilizing this library, you can render overlay from any process and control it via IPC.
//! It's designed to give you maximum flexibility as you can keep most of the logic in this process.
//!
//! # Example
//! ```no_run
//! use std::path::Path;
//! use std::time::Duration;
//! use asdf_overlay_client::{inject, OverlayDll};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let dll = OverlayDll {
//!         x64: Some(Path::new("asdf-overlay-x64.dll")),
//!         x86: Some(Path::new("asdf-overlay-x86.dll")),
//!         x86: Some(Path::new("asdf-overlay-arm64.dll")),
//!     };
//!
//!    let (mut conn, mut events) = inject(
//!         1234, // target process pid
//!         dll, // overlay dll paths
//!         Some(Duration::from_secs(10)), // timeout for injection and ipc connection
//!    ).await?;
//!
//!   // Use `conn` to send requests to overlay, and `events` to receive events from the overlay.
//!
//!   Ok(())
//! }
//!

pub mod client;
mod injector;
#[cfg(feature = "surface")]
pub mod surface;
#[cfg(feature = "surface")]
pub mod ty;

pub use asdf_overlay_common as common;
pub use asdf_overlay_event as event;
pub use injector::HookGuard;

use core::time::Duration;
use std::path::Path;

use anyhow::{Context, bail};
use asdf_overlay_common::ipc::create_ipc_addr;
use tokio::{net::windows::named_pipe::ClientOptions, select, time::sleep};

use crate::client::{IpcClientConn, IpcClientEventStream};

/// Injection strategy to use when attaching the overlay DLL.
#[derive(Debug, Clone, Copy, Default)]
pub enum InjectStrategy {
    /// `CreateRemoteThread` + `LoadLibraryW`. Simple and reliable on games
    /// without kernel anti-cheat. Blocked by Vanguard / EAC kernel.
    #[default]
    RemoteThread,

    /// `SetWindowsHookEx(WH_GETMESSAGE, ...)`. Requires the DLL to be signed
    /// by a trusted CA, but passes through kernel anti-cheat hooks on
    /// `NtOpenProcess` / `NtCreateThreadEx`.
    WindowsHook,
}

/// Paths to overlay DLLs for different architectures.
#[derive(Debug, Clone, Copy, Default)]
pub struct OverlayDll<'a> {
    /// Path to DLL to be used for x64 applications.
    pub x64: Option<&'a Path>,

    /// Path to DLL to be used for x86 applications.
    pub x86: Option<&'a Path>,

    /// Path to DLL to be used for ARM64 applications.
    pub arm64: Option<&'a Path>,
}

/// Result of a successful injection.
///
/// * `module_handle` is a best-effort value, used to address the IPC pipe.
///   For `WindowsHook` this is the injector's local `HMODULE` (not the
///   target's), because kernel anti-cheats deny `PROCESS_VM_READ` so we
///   can't enumerate the target's modules.
/// * `hook` is `Some` only for `WindowsHook` strategy. The caller MUST keep
///   this guard alive for as long as the injected DLL should remain loaded
///   in the target: dropping it unhooks, which causes the OS to unload the
///   DLL from the target.
pub struct InjectionResult {
    pub module_handle: u32,
    pub hook: Option<HookGuard>,
}

/// Inject overlay DLL into target process and create IPC connection.
///
/// Uses the [`InjectStrategy::RemoteThread`] strategy. For games protected by
/// kernel anti-cheat, use [`inject_with`] with [`InjectStrategy::WindowsHook`]
/// instead.
///
/// * If you didn't supply DLL path for the target architecture, it will return an error.
/// * If injection or IPC connection fails, it will return an error.
/// * If timeout is `None`, it may wait indefinitely.
///
/// Returns an IPC conn / event stream pair and an optional [`HookGuard`]. For
/// the `WindowsHook` strategy the caller must keep the guard alive for the
/// lifetime of the overlay session.
pub async fn inject(
    pid: u32,
    dll: OverlayDll<'_>,
    timeout: Option<Duration>,
) -> anyhow::Result<(IpcClientConn, IpcClientEventStream, Option<HookGuard>)> {
    inject_with(pid, dll, InjectStrategy::RemoteThread, timeout).await
}

/// Inject overlay DLL into target process using the chosen [`InjectStrategy`]
/// and create an IPC connection.
pub async fn inject_with(
    pid: u32,
    dll: OverlayDll<'_>,
    strategy: InjectStrategy,
    timeout: Option<Duration>,
) -> anyhow::Result<(IpcClientConn, IpcClientEventStream, Option<HookGuard>)> {
    let InjectionResult {
        module_handle: _,
        hook,
    } = inject_only_with(pid, dll, strategy, timeout)?;
    let ipc_addr = create_ipc_addr(pid);

    let connect = IpcClientConn::new(ClientOptions::new().open(ipc_addr)?);
    let timeout_fut = sleep(timeout.unwrap_or(Duration::MAX));
    let (conn, events) = select! {
        res = connect => res?,
        _ = timeout_fut => bail!("ipc client wait timeout"),
    };

    Ok((conn, events, hook))
}

/// Inject overlay DLL into target process using the chosen [`InjectStrategy`]
/// and return an [`InjectionResult`]. Unlike [`inject_with`], this does not
/// attempt to open an IPC connection to the overlay. Useful for DLLs that
/// don't implement the IPC protocol (e.g. a stub DLL used to isolate whether
/// the injection path itself is being blocked by an anti-cheat).
pub fn inject_only_with(
    pid: u32,
    dll: OverlayDll<'_>,
    strategy: InjectStrategy,
    timeout: Option<Duration>,
) -> anyhow::Result<InjectionResult> {
    match strategy {
        InjectStrategy::RemoteThread => {
            let module_handle =
                injector::inject(pid, dll, timeout).context("failed to inject overlay DLL")?;
            Ok(InjectionResult {
                module_handle,
                hook: None,
            })
        }
        InjectStrategy::WindowsHook => {
            let (module_handle, guard) = injector::safe_inject(pid, dll, timeout)
                .context("failed to inject overlay DLL via WH_GETMESSAGE hook")?;
            Ok(InjectionResult {
                module_handle,
                hook: Some(guard),
            })
        }
    }
}
