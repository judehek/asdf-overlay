//! Injector module for injecting overlay DLL into target process.
//!
//! Two injection strategies are available:
//!
//! * [`inject`]: the classic create-remote-thread + `LoadLibraryW` path.
//!   Requires opening the target process with write / thread-creation rights
//!   and is blocked by most kernel-level anti-cheats (Vanguard, EAC kernel).
//! * [`safe_inject`]: `SetWindowsHookEx(WH_GETMESSAGE, ...)` against the
//!   target's main UI thread. The OS itself loads the DLL into the target as
//!   a side effect of message pumping, which bypasses the NT syscall hooks
//!   anti-cheats install on `NtOpenProcess` / `NtCreateThreadEx`. The target
//!   DLL must be code signed by a trusted CA and export
//!   `asdf_overlay_hook_proc` (provided by this project's DLL crate).

use core::{mem, time::Duration};
use std::{
    ffi::{CString, OsStr},
    fs,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    thread::sleep,
};

use anyhow::{Context, bail};
use goblin::pe::PE;
use ntapi::{
    ntapi_base::CLIENT_ID,
    ntmmapi::{NtAllocateVirtualMemory, NtFreeVirtualMemory, NtWriteVirtualMemory},
    ntpsapi::NtOpenProcess,
    ntrtl::{PUSER_THREAD_START_ROUTINE, RtlCreateUserThread},
};
use scopeguard::defer;
use windows::{
    Wdk::Foundation::OBJECT_ATTRIBUTES,
    Win32::{
        Foundation::{
            CloseHandle, HANDLE, HMODULE, HWND, LPARAM, MAX_PATH, NTSTATUS, WAIT_TIMEOUT, WPARAM,
        },
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            LibraryLoader::{DONT_RESOLVE_DLL_REFERENCES, GetProcAddress, LoadLibraryExW},
            Memory::{MEM_COMMIT, MEM_RELEASE, PAGE_EXECUTE_READWRITE},
            ProcessStatus::{EnumProcessModulesEx, GetModuleBaseNameA, LIST_MODULES_ALL},
            SystemInformation::{
                GetSystemWow64DirectoryA, IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_AMD64,
                IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_UNKNOWN,
            },
            Threading::{
                GetCurrentProcess, GetExitCodeThread, IsWow64Process2, OpenProcess,
                PROCESS_CREATE_THREAD, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_OPERATION,
                PROCESS_VM_READ, PROCESS_VM_WRITE, WaitForSingleObject,
            },
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GetWindowThreadProcessId, HHOOK, HOOKPROC, PostThreadMessageW,
            SetWindowsHookExA, UnhookWindowsHookEx, WH_GETMESSAGE, WM_NULL,
        },
    },
    core::{BOOL, PCSTR, PCWSTR},
};

windows::core::link!(
    "kernel32.dll" "system" fn LoadLibraryW(lplibfilename: PCSTR) -> HMODULE
);

/// Name of the exported hook procedure in `asdf-overlay-dll`. See the DLL
/// crate's `asdf_overlay_hook_proc` for details.
const HOOK_PROC_NAME: &str = "asdf_overlay_hook_proc";

use crate::OverlayDll;

/// RAII guard over an installed `WH_GETMESSAGE` hook. Dropping the guard
/// calls `UnhookWindowsHookEx`, which in turn causes the OS to unload the
/// hook DLL from every process it was dispatched into. Callers must keep the
/// guard alive for as long as the injected DLL should remain loaded in the
/// target.
pub struct HookGuard {
    hook: HHOOK,
}

impl HookGuard {
    fn new(hook: HHOOK) -> Self {
        Self { hook }
    }
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.hook);
        }
    }
}

// HHOOK is a raw pointer to a Windows kernel object and is not marked Send by
// default. In practice, hook handles are process-global and safe to move
// across threads for the purposes of dropping/unhooking.
unsafe impl Send for HookGuard {}

/// Inject overlay DLL into target process and returns the module handle of the injected DLL.
///
/// Note that returned module handle is truncated to u32 and may not point to the actual module handle.
pub fn inject(pid: u32, dll: OverlayDll, timeout: Option<Duration>) -> anyhow::Result<u32> {
    let mut handle = HANDLE(0 as _);
    unsafe {
        let mut attr = OBJECT_ATTRIBUTES {
            Length: mem::size_of::<OBJECT_ATTRIBUTES>() as _,
            ..Default::default()
        };

        // NtOpenProcess is more permissive
        NTSTATUS(NtOpenProcess(
            &mut handle as *mut _ as _,
            (PROCESS_QUERY_LIMITED_INFORMATION
                | PROCESS_CREATE_THREAD
                | PROCESS_VM_OPERATION
                | PROCESS_VM_READ
                | PROCESS_VM_WRITE)
                .0,
            &mut attr as *mut _ as _,
            &mut CLIENT_ID {
                UniqueProcess: pid as _,
                UniqueThread: 0 as _,
            },
        ))
        .ok()
        .context("cannot open process")?;
    };
    defer!(unsafe {
        _ = CloseHandle(handle);
    });

    let target_arch = get_process_arch(handle);
    let current_arch = get_process_arch(unsafe { GetCurrentProcess() });

    let path = match target_arch {
        IMAGE_FILE_MACHINE_AMD64 => dll.x64.context("x64 dll path is not provided")?,
        IMAGE_FILE_MACHINE_I386 => dll.x86.context("x86 dll path is not provided")?,
        IMAGE_FILE_MACHINE_ARM64 => dll.arm64.context("arm64 dll path is not provided")?,
        arch => bail!("Unsupported arch: {}", arch.0),
    };

    execute_remote_fn(
        handle,
        load_library_w_for(handle, target_arch, current_arch)
            .context("cannot find LoadLibraryW")?,
        path.as_os_str(),
        timeout,
    )
}

/// Get the architecture of the target process handle.
fn get_process_arch(handle: HANDLE) -> IMAGE_FILE_MACHINE {
    let mut native_output = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut wow64_output = IMAGE_FILE_MACHINE_UNKNOWN;
    unsafe {
        _ = IsWow64Process2(handle, &mut wow64_output, Some(&mut native_output));
    }

    if wow64_output != IMAGE_FILE_MACHINE_UNKNOWN {
        wow64_output
    } else {
        native_output
    }
}

/// Get the address of LoadLibraryW in the target process.
fn load_library_w_for(
    process: HANDLE,
    target_arch: IMAGE_FILE_MACHINE,
    process_arch: IMAGE_FILE_MACHINE,
) -> anyhow::Result<usize> {
    if target_arch == process_arch {
        Ok(LoadLibraryW as *const () as usize)
    } else {
        match (process_arch, target_arch) {
            (IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_AMD64) => {
                bail!("cannot inject to x64 process from x86 process")
            }

            // wow64 x86
            (_, IMAGE_FILE_MACHINE_I386) => {
                let mut kernel32_path = unsafe {
                    let size = GetSystemWow64DirectoryA(None);
                    let mut buf = vec![0u8; size as _];
                    GetSystemWow64DirectoryA(Some(&mut buf));
                    // pop nul
                    buf.pop();
                    PathBuf::from(str::from_utf8(&buf)?)
                };
                kernel32_path.push("kernel32.dll");

                let data = fs::read(&kernel32_path)?;
                let pe = PE::parse(&data)?;
                let ex = pe
                    .exports
                    .iter()
                    .find(|ex| matches!(ex.name, Some("LoadLibraryW")))
                    .context("cannot find LoadLibraryW exports")?;

                let mut mod_list = vec![HMODULE::default(); 1024];
                let mut cb_size = 0;
                unsafe {
                    EnumProcessModulesEx(
                        process,
                        mod_list.as_mut_ptr(),
                        (mod_list.len() * mem::size_of::<HMODULE>()) as u32,
                        &mut cb_size,
                        LIST_MODULES_ALL,
                    )?;
                };
                mod_list.truncate(cb_size as usize / mem::size_of::<HMODULE>());

                let target_kernel32_base = {
                    let mut buf = [0_u8; MAX_PATH as usize + 1];

                    mod_list
                        .into_iter()
                        .find({
                            |module| unsafe {
                                let len = GetModuleBaseNameA(process, Some(*module), &mut buf);
                                str::from_utf8(&buf[..len as usize])
                                    .map(|path| path.eq_ignore_ascii_case("kernel32.dll"))
                                    .unwrap_or(false)
                            }
                        })
                        .context("cannot find kernel32.dll in target process")?
                };

                Ok(ex.rva + target_kernel32_base.0 as usize)
            }

            // x64 on arm64
            (IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_AMD64) => {
                Ok(LoadLibraryW as *const () as usize)
            }

            (current_arch, target_arch) => {
                bail!(
                    "Unsupported target arch: {}, current arch: {}",
                    target_arch.0,
                    current_arch.0
                );
            }
        }
    }
}

/// Execute a function to the target process by creating a remote thread.
fn execute_remote_fn(
    process: HANDLE,
    f: usize,
    param: &OsStr,
    timeout: Option<Duration>,
) -> anyhow::Result<u32> {
    let param_encoded = param.encode_wide().collect::<Vec<u16>>();
    let param_encoded = bytemuck::cast_slice::<_, u8>(&param_encoded);

    unsafe {
        let mut base_addr = 0_usize;
        let mut region_size = param_encoded.len();
        // allocate rw page
        NTSTATUS(NtAllocateVirtualMemory(
            process.0 as _,
            &raw mut base_addr as _,
            0,
            &mut region_size,
            MEM_COMMIT.0,
            PAGE_EXECUTE_READWRITE.0,
        ))
        .ok()?;
        // free memory on exit
        defer!({
            let mut base_addr = base_addr;
            _ = NtFreeVirtualMemory(
                process.0 as _,
                &raw mut base_addr as _,
                &mut 0_usize as *mut _,
                MEM_RELEASE.0,
            );
        });

        // write dll path
        NTSTATUS(NtWriteVirtualMemory(
            process.0 as _,
            base_addr as _,
            param_encoded.as_ptr() as _,
            param_encoded.len(),
            0 as _,
        ))
        .ok()?;

        let mut thread_handle: HANDLE = HANDLE::default();
        // create a user thread in the process and execute LoadLibraryW
        NTSTATUS(RtlCreateUserThread(
            process.0 as _,
            0 as _,
            0,
            0,
            0,
            0,
            mem::transmute::<usize, PUSER_THREAD_START_ROUTINE>(f),
            base_addr as _,
            &mut thread_handle as *mut _ as _,
            0 as _,
        ))
        .ok()?;
        // cleanup thread handle
        defer!({
            _ = CloseHandle(thread_handle);
        });

        // wait for overlay dll to start
        let res = WaitForSingleObject(
            thread_handle,
            timeout
                .map(|duration| duration.as_millis() as u32)
                .unwrap_or(u32::MAX),
        );
        if res == WAIT_TIMEOUT {
            bail!("remote thread wait timeout");
        }

        let mut module_handle = 0_u32;
        // Get loaded module handle
        GetExitCodeThread(thread_handle, &mut module_handle)?;
        if module_handle == 0 {
            bail!("failed to load overlay DLL");
        }

        Ok(module_handle)
    }
}

/// Inject overlay DLL into target process using `SetWindowsHookEx(WH_GETMESSAGE, ...)`.
///
/// This path does not call `OpenProcess` with write / thread-creation rights or
/// `CreateRemoteThread`, which means it passes through the NT syscall hooks
/// kernel anti-cheats (e.g. Vanguard) install. The cost of admission is that
/// the DLL must be signed by a trusted code-signing authority and must export
/// [`HOOK_PROC_NAME`] (see `asdf_overlay_hook_proc` in the DLL crate).
///
/// Returns a best-effort module handle (truncated to `u32`; this is the
/// injector's local `HMODULE`, not the target's, because kernel anti-cheats
/// deny `PROCESS_VM_READ` so we can't enumerate the target's modules) and a
/// [`HookGuard`]. The caller MUST keep the guard alive for as long as the
/// injected DLL should remain loaded in the target: dropping the guard
/// unhooks, which causes the OS to unload the DLL from every process it was
/// dispatched into.
///
/// # Notes
/// * Target process architecture must match the current process's. Unlike
///   [`inject`], cross-arch injection isn't supported here because we need to
///   `LoadLibraryEx` the DLL locally to get a valid `HINSTANCE` for
///   `SetWindowsHookEx`.
/// * The DLL's `DllMain` does all real initialization as a side effect of the
///   OS-initiated load. This function returns after the OS has had a chance
///   to dispatch the hook, but does not verify the DLL actually landed —
///   anti-cheat-protected targets will deny any such verification call.
pub fn safe_inject(
    pid: u32,
    dll: OverlayDll,
    _timeout: Option<Duration>,
) -> anyhow::Result<(u32, HookGuard)> {
    let current_arch = get_process_arch(unsafe { GetCurrentProcess() });
    let dll_path: &Path = match current_arch {
        IMAGE_FILE_MACHINE_AMD64 => dll.x64.context("x64 dll path is not provided")?,
        IMAGE_FILE_MACHINE_I386 => dll.x86.context("x86 dll path is not provided")?,
        IMAGE_FILE_MACHINE_ARM64 => dll.arm64.context("arm64 dll path is not provided")?,
        arch => bail!("Unsupported current arch: {}", arch.0),
    };
    eprintln!("[safe_inject] current arch ok, dll={}", dll_path.display());

    let query_handle = unsafe {
        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .context("failed to open target process for arch query")?
    };
    let target_arch = get_process_arch(query_handle);
    unsafe {
        _ = CloseHandle(query_handle);
    }
    if target_arch != IMAGE_FILE_MACHINE_UNKNOWN && target_arch != current_arch {
        bail!(
            "safe_inject requires matching architectures (current: {}, target: {}). Use a same-arch injector binary.",
            current_arch.0,
            target_arch.0
        );
    }
    eprintln!("[safe_inject] target arch matches ({:#x})", target_arch.0);

    // Load the DLL into the current process so SetWindowsHookEx has a valid
    // HINSTANCE to hand to the OS. DONT_RESOLVE_DLL_REFERENCES avoids running
    // the overlay's DllMain locally.
    let dll_wide = wide_path(dll_path);
    let lib = unsafe {
        LoadLibraryExW(
            PCWSTR(dll_wide.as_ptr()),
            None,
            DONT_RESOLVE_DLL_REFERENCES,
        )
        .context("failed to LoadLibraryEx overlay DLL locally")?
    };
    // Note: we intentionally do NOT FreeLibrary on the way out. Keeping the
    // reference ensures the hook remains valid for the lifetime of the
    // returned HookGuard. FreeLibrary on the local handle wouldn't affect the
    // target's copy, but we've seen no benefit to releasing it early.
    eprintln!("[safe_inject] LoadLibraryEx ok, hmodule={:?}", lib.0);

    let hook_proc_name = CString::new(HOOK_PROC_NAME)?;
    let hook_proc_addr = unsafe { GetProcAddress(lib, PCSTR(hook_proc_name.as_ptr() as *const u8)) };
    let hook_proc: HOOKPROC = match hook_proc_addr {
        Some(addr) => Some(unsafe { mem::transmute(addr) }),
        None => bail!(
            "overlay DLL does not export `{}`. Rebuild the DLL with the exported hook proc.",
            HOOK_PROC_NAME
        ),
    };
    eprintln!("[safe_inject] hook proc resolved");

    // Find a UI thread in the target process to hook. Prefer one that owns a
    // top-level window (those have message pumps, which WH_GETMESSAGE rides
    // on). Fall back to the earliest-created thread in the process.
    let thread_id = find_ui_thread(pid)
        .or_else(|| find_oldest_thread(pid))
        .context("could not locate a thread to hook in target process")?;
    eprintln!("[safe_inject] target thread id = {}", thread_id);

    let hook = unsafe {
        SetWindowsHookExA(WH_GETMESSAGE, hook_proc, Some(lib.into()), thread_id)
            .context("SetWindowsHookExA failed")?
    };
    eprintln!("[safe_inject] SetWindowsHookEx ok");

    // Kick the target thread's message pump so Windows actually dispatches
    // our hook (and therefore loads the DLL into the target). A few kicks is
    // plenty; WM_NULL matches what DrNseven/goverlay-style injectors use.
    for _ in 0..3 {
        unsafe {
            _ = PostThreadMessageW(thread_id, WM_NULL as u32, WPARAM(0), LPARAM(0));
        }
    }

    // Give Windows a brief moment to dispatch the hook into the target.
    // We intentionally do not call EnumProcessModulesEx to verify: kernel
    // anti-cheats (Vanguard in particular) deny PROCESS_VM_READ on their
    // protected processes, so such a verification call would always appear
    // to fail even when injection succeeded.
    sleep(Duration::from_millis(200));
    eprintln!("[safe_inject] kicks posted, returning hook guard (unverified)");

    Ok((lib.0 as u32, HookGuard::new(hook)))
}

/// Encode a path as a NUL-terminated UTF-16 buffer suitable for `PCWSTR`.
fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Find a thread in the target process that owns a top-level window. Such
/// threads have message pumps and are the correct target for WH_GETMESSAGE.
fn find_ui_thread(pid: u32) -> Option<u32> {
    struct Search {
        target_pid: u32,
        found_tid: u32,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let mut owner_pid = 0u32;
        let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner_pid)) };
        if tid != 0 && owner_pid == search.target_pid {
            search.found_tid = tid;
            return BOOL(0);
        }
        BOOL(1)
    }

    let mut search = Search {
        target_pid: pid,
        found_tid: 0,
    };
    unsafe {
        _ = EnumWindows(Some(enum_proc), LPARAM(&mut search as *mut _ as isize));
    }

    (search.found_tid != 0).then_some(search.found_tid)
}

/// Find the oldest (usually main) thread of a process via toolhelp snapshot.
fn find_oldest_thread(pid: u32) -> Option<u32> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0).ok()? };
    defer!(unsafe {
        _ = CloseHandle(snap);
    });

    let mut entry = THREADENTRY32 {
        dwSize: mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };

    let mut first = 0u32;
    let mut ok = unsafe { Thread32First(snap, &mut entry) }.is_ok();
    while ok {
        if entry.th32OwnerProcessID == pid && first == 0 {
            first = entry.th32ThreadID;
        }
        ok = unsafe { Thread32Next(snap, &mut entry) }.is_ok();
    }
    (first != 0).then_some(first)
}

