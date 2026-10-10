//! bridge64/bridge32 辅助进程的生命周期管理。
//!
//! CLI 每次运行按需启动 bridge：
//! - 已有实例正在服务命令（上次运行的残留、或其它 OpenSpeedy 进程启动的）→ 直接复用；
//! - 否则从可执行文件同目录启动，并加入 Job 对象 —— 本进程退出（含被强杀）时
//!   内核关闭 Job 句柄，bridge 随之被终止，不会留下孤儿进程。

use std::process::Child;
use std::sync::Mutex;

use crate::applog;
use crate::bridge_client;
use crate::process_enumerator;

/// 本进程启动的 bridge（退出时由 `shutdown` 杀掉；强杀由 Job 兜底）。
static BRIDGE_CHILDREN: Mutex<Vec<Child>> = Mutex::new(Vec::new());

/// 确保 bridge64 与 bridge32 都能服务命令。
pub fn ensure() {
    ensure_one("bridge64", bridge_client::bridge64_health);
    ensure_one("bridge32", bridge_client::bridge32_health);
}

fn ensure_one(name: &str, health: fn() -> bool) {
    // 已在运行 → 复用，不算进本进程的清理列表（不替它的所有者杀它）。
    if health() {
        applog::info(
            "bridge",
            format!("{name} already serving — reusing the running instance"),
        );
        return;
    }

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();
    let path = exe_dir.join(format!("{name}.exe"));
    if !path.exists() {
        applog::warn(
            "bridge",
            format!("{name}.exe not found at {} — skipped", path.display()),
        );
        return;
    }

    let log_path = exe_dir.join(format!("{name}.exe.log"));
    let stderr = std::fs::File::create(&log_path)
        .map(std::process::Stdio::from)
        .unwrap_or(std::process::Stdio::null());

    match std::process::Command::new(&path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr)
        .spawn()
    {
        Ok(mut child) => {
            applog::info("bridge", format!("spawned {name} as pid {}", child.id()));
            // 先做存活检查再加入 Job：启动即退出的 bridge 不可能变孤儿，
            // 对这种进程报告"未加入 Job"反而误导日志读者。
            match child.try_wait() {
                Ok(Some(status)) => {
                    let msg = format!(
                        "{name} (pid {}) exited immediately ({status}) — not added to the cleanup job",
                        child.id()
                    );
                    applog::warn("bridge", msg);
                }
                _ => {
                    crate::bridge_job::assign(&child, name);
                }
            }

            // 等管道服务器就绪。
            let start = std::time::Instant::now();
            let mut ok = health();
            while !ok && start.elapsed() < std::time::Duration::from_secs(2) {
                std::thread::sleep(std::time::Duration::from_millis(100));
                ok = health();
            }
            if ok {
                applog::info(
                    "bridge",
                    format!("{name} ready after {} ms", start.elapsed().as_millis()),
                );
            } else {
                applog::warn("bridge", format!("{name} did not become ready within 2 s"));
            }

            if let Ok(mut children) = BRIDGE_CHILDREN.lock() {
                children.push(child);
            }
        }
        Err(e) => applog::error("bridge", format!("failed to spawn {}: {e}", path.display())),
    }
}

/// 杀掉本进程启动的 bridge（正常退出路径走这里；强杀/崩溃由 Job 兜底）。
pub fn shutdown() {
    if let Ok(mut children) = BRIDGE_CHILDREN.lock() {
        for mut child in children.drain(..) {
            applog::info("bridge", format!("killing bridge pid {}", child.id()));
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 杀掉系统里所有 bridge64/bridge32 进程（`stop` 命令），返回 (名称, pid, 是否成功)。
pub fn kill_all() -> Vec<(String, u32, bool)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    let mut killed = Vec::new();
    for p in process_enumerator::enumerate_processes_fast() {
        let name = p.name.to_ascii_lowercase();
        if name != "bridge64.exe" && name != "bridge32.exe" {
            continue;
        }
        let ok = unsafe {
            match OpenProcess(PROCESS_TERMINATE, false, p.pid) {
                Ok(h) => {
                    let r = TerminateProcess(h, 0).is_ok();
                    let _ = CloseHandle(h);
                    r
                }
                Err(_) => false,
            }
        };
        killed.push((p.name.clone(), p.pid, ok));
    }
    killed
}
