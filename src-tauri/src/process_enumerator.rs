use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use windows::core::PWSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{
    EnumProcessModulesEx, GetModuleFileNameExW, GetModuleInformation, GetProcessMemoryInfo,
    LIST_MODULES_32BIT, LIST_MODULES_64BIT, MODULEINFO, PROCESS_MEMORY_COUNTERS,
};
use windows::Win32::System::Threading::{
    IsWow64Process, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};

// ---------------------------------------------------------------------------
// Cache: stores heavy-to-fetch per-process data, keyed by PID
// ---------------------------------------------------------------------------
struct CachedInfo {
    arch: String,
    window_title: Option<String>,
    memory_kb: u64,
    exe_path: Option<String>,
    admin: bool,
}

fn cache() -> &'static Mutex<HashMap<u32, CachedInfo>> {
    static CACHE: OnceLock<Mutex<HashMap<u32, CachedInfo>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_get(pid: u32) -> Option<CachedInfo> {
    cache().lock().ok()?.get(&pid).map(|c| CachedInfo {
        arch: c.arch.clone(),
        window_title: c.window_title.clone(),
        memory_kb: c.memory_kb,
        exe_path: c.exe_path.clone(),
        admin: c.admin,
    })
}

fn cache_set(
    pid: u32,
    arch: String,
    window_title: Option<String>,
    memory_kb: u64,
    exe_path: Option<String>,
    admin: bool,
) {
    if let Ok(mut cache) = cache().lock() {
        cache.insert(
            pid,
            CachedInfo {
                arch,
                window_title,
                memory_kb,
                exe_path,
                admin,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub arch: String,
    pub window_title: Option<String>,
    pub memory_kb: u64,
    pub exe_path: Option<String>,
    pub admin: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModuleInfo {
    pub name: String,
    pub path: String,
    pub base_address: u64,
    pub size: u32,
}

fn to_wide(s: &str) -> Vec<u16> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

// ---------------------------------------------------------------------------
// Module enumeration for a single process
// ---------------------------------------------------------------------------
pub fn enumerate_modules(pid: u32) -> Vec<ModuleInfo> {
    let mut modules = Vec::new();

    // Use EnumProcessModulesEx — handles cross-architecture (WOW64) correctly,
    // unlike CreateToolhelp32Snapshot which can only see modules matching the
    // caller's bitness.
    let h_proc =
        match unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) } {
            Ok(h) => h,
            Err(_) => return modules,
        };

    // Determine module filter flag: 32-bit or 64-bit modules
    let filter = {
        let mut wow64 = windows::Win32::Foundation::BOOL::default();
        let kernel32_wide = to_wide("kernel32.dll");
        let k32 = unsafe {
            windows::Win32::System::LibraryLoader::GetModuleHandleW(
                windows::core::PCWSTR::from_raw(kernel32_wide.as_ptr()),
            )
            .unwrap_or_default()
        };
        let is_wow64_proc: Option<
            unsafe extern "system" fn(
                windows::Win32::Foundation::HANDLE,
                *mut windows::Win32::Foundation::BOOL,
            ) -> windows::Win32::Foundation::BOOL,
        > = unsafe {
            std::mem::transmute(windows::Win32::System::LibraryLoader::GetProcAddress(
                k32,
                windows::core::s!("IsWow64Process"),
            ))
        };
        let is_wow64 = if let Some(f) = is_wow64_proc {
            unsafe {
                let _ = f(h_proc, &mut wow64);
            }
            wow64.as_bool()
        } else {
            false
        };
        if is_wow64 {
            LIST_MODULES_32BIT
        } else {
            LIST_MODULES_64BIT
        }
    };

    let mut h_mods: Vec<HMODULE> = vec![HMODULE::default(); 1024];
    let mut needed: u32 = 0;
    let h_mods_size = (h_mods.len() * std::mem::size_of::<HMODULE>()) as u32;

    if unsafe {
        EnumProcessModulesEx(
            h_proc,
            h_mods.as_mut_ptr(),
            h_mods_size,
            &mut needed,
            filter,
        )
    }
    .is_err()
    {
        unsafe {
            let _ = CloseHandle(h_proc);
        }
        return modules;
    }

    let count = (needed as usize) / std::mem::size_of::<HMODULE>();
    h_mods.truncate(count);

    for &h_mod in &h_mods {
        let mut name_buf = [0u16; 260];
        let len = unsafe { GetModuleFileNameExW(h_proc, h_mod, &mut name_buf) };
        if len == 0 {
            continue;
        }
        let name = String::from_utf16_lossy(&name_buf[..len as usize]);
        let base_name = std::path::Path::new(&name)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();

        let mut info = MODULEINFO::default();
        if unsafe {
            GetModuleInformation(
                h_proc,
                h_mod,
                &mut info,
                std::mem::size_of::<MODULEINFO>() as u32,
            )
        }
        .is_ok()
        {
            modules.push(ModuleInfo {
                name: base_name,
                path: name,
                base_address: info.lpBaseOfDll as u64,
                size: info.SizeOfImage,
            });
        }
    }

    unsafe {
        let _ = CloseHandle(h_proc);
    }
    modules
}

// ---------------------------------------------------------------------------
// Fast enumeration — snapshot-only, merges cached heavy data
// ---------------------------------------------------------------------------
pub fn enumerate_processes_fast() -> Vec<ProcessInfo> {
    let mut processes = Vec::new();

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_err() {
        return processes;
    }
    let snapshot = snapshot.unwrap();

    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
        loop {
            let pid = entry.th32ProcessID;
            let name = String::from_utf16_lossy(&entry.szExeFile)
                .trim_end_matches('\0')
                .to_string();

            if !name.is_empty() {
                // Merge from cache
                if let Some(cached) = cache_get(pid) {
                    processes.push(ProcessInfo {
                        pid,
                        name,
                        arch: cached.arch,
                        window_title: cached.window_title,
                        memory_kb: cached.memory_kb,
                        exe_path: cached.exe_path,
                        admin: cached.admin,
                    });
                } else {
                    // New process — eagerly fetch arch + exe path + admin (cheapest), defer memory+title
                    let arch = detect_arch(pid);
                    let exe_path = get_exe_path(pid);
                    let admin = is_admin(pid);
                    cache_set(pid, arch.clone(), None, 0, exe_path.clone(), admin);
                    processes.push(ProcessInfo {
                        pid,
                        name,
                        arch,
                        window_title: find_window_title(pid),
                        memory_kb: get_memory_kb(pid),
                        exe_path,
                        admin,
                    });
                }
            }

            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }

    let _ = unsafe { CloseHandle(snapshot) };
    processes
}

// ---------------------------------------------------------------------------
// Full enumeration — opens handles, reads memory, updates cache
// ---------------------------------------------------------------------------
pub fn enumerate_processes_full() -> Vec<ProcessInfo> {
    let mut processes = Vec::new();

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_err() {
        return processes;
    }
    let snapshot = snapshot.unwrap();

    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
        loop {
            let pid = entry.th32ProcessID;
            let name = String::from_utf16_lossy(&entry.szExeFile)
                .trim_end_matches('\0')
                .to_string();

            if !name.is_empty() {
                let arch = detect_arch(pid);
                let window_title = find_window_title(pid);
                let memory_kb = get_memory_kb(pid);
                let exe_path = get_exe_path(pid);
                let admin = is_admin(pid);

                cache_set(
                    pid,
                    arch.clone(),
                    window_title.clone(),
                    memory_kb,
                    exe_path.clone(),
                    admin,
                );

                processes.push(ProcessInfo {
                    pid,
                    name,
                    arch,
                    window_title,
                    memory_kb,
                    exe_path,
                    admin,
                });
            }

            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }

    let _ = unsafe { CloseHandle(snapshot) };
    // Clean up stale pids from cache (processes that no longer exist)
    let live_pids: std::collections::HashSet<u32> = processes.iter().map(|p| p.pid).collect();
    if let Ok(mut cache) = cache().lock() {
        cache.retain(|pid, _| live_pids.contains(pid));
    }

    processes
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
fn get_exe_path(pid: u32) -> Option<String> {
    let handle = open_process(pid)?;

    let mut buf = vec![0u16; 260];
    let mut len = buf.len() as u32;

    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR::from_raw(buf.as_mut_ptr()),
            &mut len,
        )
    };

    let _ = unsafe { CloseHandle(handle) };

    if result.is_ok() {
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    } else {
        None
    }
}

fn is_admin(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }

    // Try hardest first, fall back to limited (works on protected processes)
    let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION, false, pid) }
        .or_else(|_| unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) });
    let handle = match handle {
        Ok(h) => h,
        Err(_) => return false,
    };

    let mut token = Default::default();
    if unsafe { OpenProcessToken(handle, TOKEN_QUERY, &mut token) }.is_err() {
        let _ = unsafe { CloseHandle(handle) };
        return false;
    }

    let mut elevation = TOKEN_ELEVATION::default();
    let mut ret_len = 0u32;
    let result = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut std::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
    };

    let _ = unsafe { CloseHandle(token) };
    let _ = unsafe { CloseHandle(handle) };

    result.is_ok() && elevation.TokenIsElevated != 0
}

fn detect_arch(pid: u32) -> String {
    let handle = open_process(pid);
    if handle.is_none() {
        return "x64".to_string();
    }
    let handle = handle.unwrap();

    let mut is_wow64 = windows::Win32::Foundation::BOOL::default();
    let result = unsafe { IsWow64Process(handle, &mut is_wow64) };

    let _ = unsafe { CloseHandle(handle) };

    if result.is_ok() && is_wow64.as_bool() {
        "x86".to_string()
    } else {
        "x64".to_string()
    }
}

struct FindWindowCtx {
    pid: u32,
    title: Option<String>,
}

fn find_window_title(pid: u32) -> Option<String> {
    let mut ctx = FindWindowCtx { pid, title: None };
    let lparam = LPARAM(&mut ctx as *mut FindWindowCtx as isize);
    let _ = unsafe { EnumWindows(Some(enum_window_callback), lparam) };
    ctx.title
}

unsafe extern "system" fn enum_window_callback(
    hwnd: HWND,
    lparam: LPARAM,
) -> windows::Win32::Foundation::BOOL {
    let ctx = unsafe { &mut *(lparam.0 as *mut FindWindowCtx) };

    let mut window_pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut window_pid)) };

    if window_pid != ctx.pid {
        return windows::Win32::Foundation::BOOL::from(true);
    }

    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return windows::Win32::Foundation::BOOL::from(true);
    }

    let text_len = unsafe { GetWindowTextLengthW(hwnd) };
    if text_len == 0 {
        return windows::Win32::Foundation::BOOL::from(true);
    }

    let mut buf = vec![0u16; (text_len + 1) as usize];
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if len > 0 {
        ctx.title = Some(String::from_utf16_lossy(&buf[..len as usize]));
    }

    windows::Win32::Foundation::BOOL::from(true)
}

fn get_memory_kb(pid: u32) -> u64 {
    let handle = open_process(pid);
    if handle.is_none() {
        return 0;
    }
    let handle = handle.unwrap();

    let mut pmc = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };

    let result = unsafe {
        GetProcessMemoryInfo(
            handle,
            &mut pmc,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };

    let _ = unsafe { CloseHandle(handle) };

    if result.is_ok() {
        pmc.WorkingSetSize as u64 / 1024
    } else {
        0
    }
}

fn open_process(pid: u32) -> Option<HANDLE> {
    if pid == 0 {
        return None;
    }
    unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) }.ok()
}
