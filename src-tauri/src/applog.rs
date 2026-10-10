//! Crash/startup diagnostics log.
//!
//! Release builds run with `windows_subsystem = "windows"`, so the process has
//! no console and every `eprintln!` in the app is discarded on a user's
//! machine. This module writes those messages — plus panics, startup
//! diagnostics and frontend errors — to a log file next to the executable so a
//! user hitting a crash can send the file back.
//!
//! Layout — a `logs` folder next to `OpenSpeedy.exe`, one file per day:
//! ```text
//! logs/openspeedy.20260912.log   today (appended to on every run)
//! logs/openspeedy.20260911.log   yesterday
//! ```
//!
//! Files older than `KEEP_DAYS` are deleted at startup, so the folder stays
//! bounded and can be left enabled in every release.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use windows::Win32::System::SystemInformation::GetLocalTime;

/// Log folder next to the executable.
const LOG_DIR_NAME: &str = "logs";
/// Log files kept; older days are deleted at startup.
const KEEP_DAYS: usize = 14;
/// Start a fresh file once a day's log passes this size, so a session that
/// logs without pause cannot fill the disk.
const MAX_BYTES: u64 = 16 * 1024 * 1024;

static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();
/// Current file and the day it belongs to — both change together at midnight.
static WRITER: Mutex<Option<File>> = Mutex::new(None);
static CURRENT_DATE: Mutex<String> = Mutex::new(String::new());
static PID: AtomicU32 = AtomicU32::new(0);

/// Collapse whitespace and control characters so one record stays on one line.
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `YYYY-MM-DD HH:MM:SS.mmm` in local time.
fn timestamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

fn thread_name() -> String {
    std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_string()
}

/// Close the file handle so the log can be copied while the app is still
/// running (and so a panic does not leave it locked). The next write reopens it
/// and appends.
fn release_file() {
    if let Ok(mut guard) = WRITER.lock() {
        *guard = None;
    }
}

/// Local date as `YYYYMMDD`, used both for the file name and to decide when a
/// new day needs a new file.
fn date_stamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:04}{:02}{:02}", t.wYear, t.wMonth, t.wDay)
}

fn file_name_for(date: &str) -> String {
    format!("openspeedy.{date}.log")
}

/// The install directory — next to `OpenSpeedy.exe`.
fn install_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(std::env::temp_dir)
}

/// Open (or reopen) today's file for appending, and remember which day it is.
/// Must be called with the writer lock held.
fn open_for_today(guard: &mut MutexGuard<Option<File>>, date: &str) {
    let dir = LOG_DIR.get().cloned().unwrap_or_else(install_dir);
    let path = dir.join(file_name_for(date));

    **guard = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .or_else(|_| {
            // Unreachable in practice — `init` proved the directory is writable.
            File::create(std::env::temp_dir().join(file_name_for(date)))
        })
        .ok();

    if let Ok(mut current) = CURRENT_DATE.lock() {
        current.clear();
        current.push_str(date);
    }
}

/// The log file the app is writing to right now.
pub fn log_path() -> PathBuf {
    let dir = LOG_DIR.get().cloned().unwrap_or_else(install_dir);
    dir.join(file_name_for(&date_stamp()))
}

/// Write one record, switching to a new file when the day changes — the app can
/// sit in the tray overnight. Silently does nothing before `init`; never
/// panics, so it is safe to call from a panic hook.
fn write_line(level: &str, phase: &str, msg: &str) {
    if LOG_DIR.get().is_none() {
        return;
    }

    let line = format!(
        "{} [{}] [{}] [pid {}] [{}] {}\n",
        timestamp(),
        level,
        phase,
        PID.load(Ordering::Relaxed),
        thread_name(),
        one_line(msg)
    );

    let today = date_stamp();
    let Ok(mut guard) = WRITER.lock() else { return };

    let stale_day = CURRENT_DATE.lock().map(|d| *d != today).unwrap_or(true);
    if guard.is_none() || stale_day {
        open_for_today(&mut guard, &today);
    }
    let Some(file) = guard.as_mut() else { return };

    let _ = file.write_all(line.as_bytes());
    let _ = file.flush();

    // A single day of logs should not grow without bound either.
    if file
        .metadata()
        .map(|m| m.len() > MAX_BYTES)
        .unwrap_or(false)
    {
        let path = log_path();
        *guard = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .ok();
    }
}

/// Delete log files older than `KEEP_DAYS` so the folder cannot grow forever.
fn prune_old_files(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    // Stamp embedded in the file name, e.g. `openspeedy.20260912.log` → 20260912.
    let mut stamps: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter_map(|name| {
            let rest = name.strip_prefix("openspeedy.")?;
            let stamp = rest.strip_suffix(".log")?;
            (stamp.len() == 8 && stamp.bytes().all(|b| b.is_ascii_digit()))
                .then(|| stamp.to_string())
        })
        .collect();
    if stamps.is_empty() {
        return;
    }

    stamps.sort();
    let keep_from = stamps.len().saturating_sub(KEEP_DAYS);
    for stamp in &stamps[..keep_from] {
        let _ = std::fs::remove_file(dir.join(file_name_for(stamp)));
    }
}

/// Install the panic hook. Runs before any thread is spawned so panics from
/// every thread end up in the log.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Nothing here may panic: a panicking panic hook aborts the process.
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_string());
        let last_error = unsafe { windows::Win32::Foundation::GetLastError().0 };

        write_line(
            "PANIC",
            if std::thread::current().name() == Some("main") {
                "main-thread"
            } else {
                "thread"
            },
            &format!("{payload} | at {location} | GetLastError={last_error}"),
        );
        // Keep the log readable while the app is still crashed-but-alive.
        release_file();

        previous(info);
    }));
}

fn is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// OS name for the startup banner — the condensed form, without the build
/// number.
fn os_name_for_log() -> String {
    os_version()
        .split(' ')
        .next()
        .unwrap_or("Windows")
        .to_string()
}

fn os_version() -> String {
    use windows::Win32::System::SystemInformation::{GetVersionExW, OSVERSIONINFOW};

    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            // RtlGetVersion always returns the real OS version (GetVersionExW can lie)
            let ntdll = unsafe {
                windows::Win32::System::LibraryLoader::GetModuleHandleW(
                    windows::core::PCWSTR::from_raw(to_wide("ntdll.dll").as_ptr()),
                )
            };
            let rtl_get_version: Option<unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32> =
                ntdll
                    .ok()
                    .and_then(|h| unsafe {
                        windows::Win32::System::LibraryLoader::GetProcAddress(
                            h,
                            windows::core::s!("RtlGetVersion"),
                        )
                    })
                    .map(|p| unsafe { std::mem::transmute(p) });

            let mut vi = OSVERSIONINFOW {
                dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
                ..Default::default()
            };

            let ok = if let Some(f) = rtl_get_version {
                unsafe { f(&mut vi) == 0 } // STATUS_SUCCESS
            } else {
                unsafe { GetVersionExW(&mut vi) }.is_ok()
            };

            if ok {
                let name = match (vi.dwMajorVersion, vi.dwMinorVersion, vi.dwBuildNumber) {
                    (10, 0, b) if b >= 22000 => "Windows 11",
                    (10, 0, _) => "Windows 10",
                    (6, 3, _) => "Windows 8.1",
                    (6, 2, _) => "Windows 8",
                    (6, 1, _) => "Windows 7",
                    _ => "Windows",
                };
                format!("{name} ({})", vi.dwBuildNumber)
            } else {
                String::from("Windows")
            }
        })
        .clone()
}

fn to_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Absolute path of the log file, for the "reveal in Explorer" action.
pub fn log_path_string() -> String {
    log_path().display().to_string()
}

pub fn info(phase: &str, msg: impl AsRef<str>) {
    write_line("INFO", phase, msg.as_ref());
}

pub fn warn(phase: &str, msg: impl AsRef<str>) {
    write_line("WARN", phase, msg.as_ref());
}

pub fn error(phase: &str, msg: impl AsRef<str>) {
    write_line("ERROR", phase, msg.as_ref());
}

/// A non-fatal event worth keeping, with a value attached so the line greps
/// cleanly (`context=value`).
pub fn info_context(phase: &str, context: &str, detail: impl std::fmt::Display) {
    info(phase, format!("{context}={detail}"));
}

/// Pick the log folder: `logs` next to the executable (keeps the logs with the
/// tool), falling back to `%LOCALAPPDATA%\OpenSpeedy\logs` when that directory
/// is not writable — an install under `Program Files` denies writes to a
/// standard user.
fn resolve_log_dir() -> PathBuf {
    let beside_exe = install_dir().join(LOG_DIR_NAME);
    if std::fs::create_dir_all(&beside_exe).is_ok()
        && OpenOptions::new()
            .create(true)
            .append(true)
            .open(beside_exe.join(file_name_for(&date_stamp())))
            .is_ok()
    {
        return beside_exe;
    }

    let fallback = std::env::var("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("OpenSpeedy").join(LOG_DIR_NAME))
        .unwrap_or_else(|_| std::env::temp_dir().join("OpenSpeedy").join(LOG_DIR_NAME));
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

/// Called once, before anything else in `main`. Idempotent.
pub fn init() {
    if LOG_DIR.get().is_some() {
        return;
    }

    PID.store(std::process::id(), Ordering::Relaxed);

    let dir = resolve_log_dir();
    prune_old_files(&dir);
    let _ = LOG_DIR.set(dir);

    install_panic_hook();

    info(
        "app",
        format!("--- OpenSpeedy {} starting ---", env!("CARGO_PKG_VERSION")),
    );
    info(
        "app",
        format!(
            "exe={}",
            std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ),
    );
    info(
        "app",
        format!(
            "os={} arch={} family={} elevated={}",
            os_name_for_log(),
            std::env::consts::ARCH,
            std::env::consts::FAMILY,
            is_elevated()
        ),
    );
    info("app", format!("log={}", log_path_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_keeps_a_record_on_a_single_line() {
        assert_eq!(one_line("first\nsecond\tthird"), "first second third");
        assert_eq!(one_line("  padded  "), "padded");
        // Backslashes in Windows paths survive — only whitespace is collapsed.
        assert_eq!(
            one_line(r"C:\Program Files\OpenSpeedy"),
            r"C:\Program Files\OpenSpeedy"
        );
    }

    #[test]
    fn timestamp_has_fixed_width_fields() {
        let ts = timestamp();
        assert_eq!(ts.len(), 23, "unexpected timestamp format: {ts}");
        assert_eq!(ts.as_bytes()[4], b'-');
        assert_eq!(ts.as_bytes()[10], b' ');
    }

    /// The log must land in `logs/openspeedy.YYYYMMDD.log`, and every run that
    /// day must append to it — a restart must never wipe the crash that is
    /// being investigated.
    #[test]
    fn log_file_is_written_and_survives_reinit() {
        init();
        let marker = format!("marker-{}", std::process::id());
        error("test", &marker);
        release_file();

        let path = log_path();
        assert_eq!(
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str()),
            Some(LOG_DIR_NAME)
        );
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some(file_name_for(&date_stamp()).as_str()),
            "log file must be named for today"
        );

        let first = std::fs::read_to_string(&path).expect("log file readable");
        assert!(first.contains(&marker), "record missing from log: {first}");
        assert!(first.contains("--- OpenSpeedy "), "startup banner missing");

        init();
        error("test", "second-write");
        release_file();

        let second = std::fs::read_to_string(&path).expect("log file readable");
        assert!(
            second.contains(&marker),
            "re-init truncated an existing log"
        );
        assert!(second.contains("second-write"));
    }

    #[test]
    fn prune_keeps_the_most_recent_days() {
        let dir = std::env::temp_dir().join(format!("openspeedy-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp log dir");

        // One more file than is kept, plus names that must be left alone.
        let day = |d: u32| format!("202609{d:02}");
        for d in 1..=(KEEP_DAYS as u32 + 1) {
            std::fs::write(dir.join(file_name_for(&day(d))), b"x").expect("write log");
        }
        std::fs::write(dir.join("openspeedy.log"), b"x").expect("write unrelated log");
        std::fs::write(dir.join("bridge64.exe.log"), b"x").expect("write bridge log");

        prune_old_files(&dir);

        assert!(
            !dir.join(file_name_for(&day(1))).exists(),
            "oldest day should be deleted"
        );
        assert!(
            dir.join(file_name_for(&day(2))).exists(),
            "remaining days should survive"
        );
        assert!(
            dir.join(file_name_for(&day(KEEP_DAYS as u32 + 1))).exists(),
            "newest day should survive"
        );
        assert!(
            dir.join("openspeedy.log").exists(),
            "unrelated files must be left alone"
        );
        assert!(
            dir.join("bridge64.exe.log").exists(),
            "bridge logs must be left alone"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
