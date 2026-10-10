//! 命令行界面：参数解析与各命令实现。
//!
//! 用法总览见 [`print_help`]。命令分四类：
//! - 查询：`list`（进程列表）、`status`（倍率/注入状态）
//! - 变速：`speed`（设置倍率）
//! - 注入：`inject` / `enable` / `disable`
//! - 维护：`stop`（清理 bridge 进程）

use crate::bridge_client;
use crate::bridges;
use crate::process_enumerator::{self, ProcessInfo};
use crate::state;

/// 命令入口：返回进程退出码（0 成功，1 运行失败，2 用法错误）。
pub fn run(args: Vec<String>) -> i32 {
    enable_utf8_console();

    let mut it = args.into_iter();
    let Some(cmd) = it.next() else {
        print_help();
        return 0;
    };
    let rest: Vec<String> = it.collect();

    let code = match cmd.as_str() {
        "list" | "ls" | "ps" => cmd_list(&rest),
        "inject" | "attach" => cmd_inject(&rest),
        "enable" => change_enabled(&rest, true),
        "disable" => change_enabled(&rest, false),
        "speed" => cmd_speed(&rest),
        "status" => cmd_status(&rest),
        "stop" => cmd_stop(),
        "help" | "--help" | "-h" => {
            print_help();
            0
        }
        "version" | "--version" | "-V" => {
            println!("openspeedy {} (CLI)", env!("CARGO_PKG_VERSION"));
            0
        }
        other => {
            eprintln!("未知命令：{other}");
            eprintln!("运行 `openspeedy help` 查看用法。");
            2
        }
    };

    // 本进程启动的 bridge 到此为止（被强杀 / 崩溃时由 Job 对象兜底清理）。
    bridges::shutdown();
    code
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn cmd_list(rest: &[String]) -> i32 {
    let mut show_all = false;
    let mut filter: Option<String> = None;
    for a in rest {
        match a.as_str() {
            "-a" | "--all" => show_all = true,
            _ => filter = Some(a.to_lowercase()),
        }
    }

    let mut procs = process_enumerator::enumerate_processes_full();
    if !show_all {
        // 内存为 0 = 句柄打不开（受保护的系统进程），与 GUI 一样默认隐藏。
        procs.retain(|p| p.memory_kb > 0);
    }
    if let Some(f) = &filter {
        procs.retain(|p| {
            p.name.to_lowercase().contains(f.as_str())
                || p.pid.to_string().contains(f.as_str())
                || p.window_title
                    .as_deref()
                    .map(|t| t.to_lowercase().contains(f.as_str()))
                    .unwrap_or(false)
        });
    }
    procs.sort_by_key(|p| p.pid);

    if procs.is_empty() {
        println!("没有匹配的进程。");
        return 0;
    }

    println!("{:<7} {:<5} {:>10}  {:<34} 窗口标题", "PID", "ARCH", "内存", "进程名");
    for p in &procs {
        let mem = if p.memory_kb > 0 {
            format!("{} MB", p.memory_kb / 1024)
        } else {
            "-".to_string()
        };
        let title = p.window_title.as_deref().unwrap_or("");
        println!(
            "{:<7} {:<5} {:>10}  {:<34} {}",
            p.pid,
            p.arch,
            mem,
            truncate_chars(&p.name, 34),
            title
        );
    }
    println!();
    println!("共 {} 个进程", procs.len());
    0
}

// ---------------------------------------------------------------------------
// inject / enable / disable
// ---------------------------------------------------------------------------

fn cmd_inject(rest: &[String]) -> i32 {
    let Some(target) = rest.first() else {
        eprintln!("用法：openspeedy inject <PID|进程名>");
        return 2;
    };
    let targets = match resolve_targets(target) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    bridges::ensure();
    restore_saved_speed();

    let mut failed = 0usize;
    for t in &targets {
        let inject = inject_pid(t);
        // 与原 GUI 行为一致：无论注入结果如何都尝试 enable（未注入成功的进程会静默无操作）。
        let _ = set_enabled(t, true);
        match inject {
            Ok(()) => println!(
                "[OK]   pid={:<7} {} ({}) 已注入并启用",
                t.pid, t.name, t.arch
            ),
            Err(e) => {
                failed += 1;
                eprintln!(
                    "[FAIL] pid={:<7} {} ({}) 注入失败：{e}",
                    t.pid, t.name, t.arch
                );
            }
        }
    }
    if failed > 0 {
        eprintln!();
        eprintln!("提示：失败详情见可执行文件同目录的 bridge64.exe.log / bridge32.exe.log；");
        eprintln!("      目标进程以管理员运行时，本工具也需以管理员身份运行。");
        return 1;
    }
    0
}

fn change_enabled(rest: &[String], enabled: bool) -> i32 {
    let Some(target) = rest.first() else {
        eprintln!(
            "用法：openspeedy {} <PID|进程名>",
            if enabled { "enable" } else { "disable" }
        );
        return 2;
    };
    let targets = match resolve_targets(target) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    bridges::ensure();
    restore_saved_speed();

    let verb = if enabled { "启用" } else { "暂停" };
    let mut failed = 0usize;
    for t in &targets {
        match set_enabled(t, enabled) {
            Ok(()) => println!("[OK]   pid={:<7} {} ({}) 已{verb}", t.pid, t.name, t.arch),
            Err(e) => {
                failed += 1;
                eprintln!(
                    "[FAIL] pid={:<7} {} ({}) {verb}失败：{e}",
                    t.pid, t.name, t.arch
                );
            }
        }
    }
    if failed > 0 {
        1
    } else {
        0
    }
}

fn inject_pid(p: &ProcessInfo) -> Result<(), String> {
    if p.arch == "x86" {
        bridge_client::bridge32_inject(p.pid)
    } else {
        bridge_client::bridge64_inject(p.pid)
    }
}

fn set_enabled(p: &ProcessInfo, enabled: bool) -> Result<(), String> {
    if p.arch == "x86" {
        if enabled {
            bridge_client::bridge32_enable(p.pid)
        } else {
            bridge_client::bridge32_disable(p.pid)
        }
    } else if enabled {
        bridge_client::bridge64_enable(p.pid)
    } else {
        bridge_client::bridge64_disable(p.pid)
    }
}

// ---------------------------------------------------------------------------
// speed
// ---------------------------------------------------------------------------

fn cmd_speed(rest: &[String]) -> i32 {
    let Some(arg) = rest.first() else {
        eprintln!("用法：openspeedy speed <倍率>    （如 speed 2、speed 1.5、speed 0.5）");
        return 2;
    };
    let Ok(factor) = arg.parse::<f64>() else {
        eprintln!("倍率必须是数字，例如：openspeedy speed 2.5");
        return 2;
    };
    if !(0.01..=1000.0).contains(&factor) {
        eprintln!("倍率超出范围（0.01 – 1000）：{arg}");
        return 2;
    }

    bridges::ensure();

    let x64 = bridge_client::bridge64_set_speed(factor);
    let x86 = bridge_client::bridge32_set_speed(factor);
    let note = |r: &Result<(), String>| match r {
        Ok(()) => "OK".to_string(),
        Err(e) => format!("不可用（{e}）"),
    };
    println!(
        "当前速度倍率已设为 {arg}   [x64: {} | x86: {}]",
        note(&x64),
        note(&x86)
    );

    if x64.is_ok() || x86.is_ok() {
        state::save_speed(factor);
        println!("提示：已注入的游戏立即生效；倍率已保存，后续命令会自动恢复。");
        0
    } else {
        eprintln!("两个 bridge 都不可用，速度未生效。请确认 bridge64.exe / bridge32.exe 与 openspeedy.exe 在同一目录。");
        1
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn cmd_status(rest: &[String]) -> i32 {
    bridges::ensure();
    restore_saved_speed();

    let s64 = bridge_client::bridge64_get_speed();
    let s32 = bridge_client::bridge32_get_speed();
    println!(
        "当前速度倍率：x64 = {}   x86 = {}",
        fmt_speed(s64),
        fmt_speed(s32)
    );

    if let Some(target) = rest.first() {
        let targets = match resolve_targets(target) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        };
        for t in &targets {
            let modules = process_enumerator::enumerate_modules(t.pid);
            let patched = modules.iter().find(|m| {
                let n = m.name.to_ascii_lowercase();
                n == "speedpatch64.dll" || n == "speedpatch32.dll"
            });
            match patched {
                Some(m) => println!(
                    "pid={:<7} {} ({})：已注入（{}）",
                    t.pid, t.name, t.arch, m.name
                ),
                None => println!("pid={:<7} {} ({})：未注入", t.pid, t.name, t.arch),
            }
        }
    }
    0
}

fn fmt_speed(v: Option<f64>) -> String {
    match v {
        Some(f) => format!("{f:.2}"),
        None => "不可用".to_string(),
    }
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

fn cmd_stop() -> i32 {
    let killed = bridges::kill_all();
    if killed.is_empty() {
        println!("没有正在运行的 bridge 进程。");
    } else {
        let mut failed = 0usize;
        for (name, pid, ok) in &killed {
            if *ok {
                println!("[OK]   已终止 {name} (pid {pid})");
            } else {
                failed += 1;
                eprintln!("[FAIL] 无法终止 {name} (pid {pid})（可能需要管理员权限）");
            }
        }
        if failed > 0 {
            return 1;
        }
    }
    // 本进程启动的 children 可能不在枚举结果里（已退出），统一清账。
    bridges::shutdown();
    0
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// 解析目标参数：纯数字按 PID 精确匹配；其余按进程名（不区分大小写，可带/不带 .exe）。
fn resolve_targets(target: &str) -> Result<Vec<ProcessInfo>, String> {
    let procs = process_enumerator::enumerate_processes_fast();

    if let Ok(pid) = target.parse::<u32>() {
        return procs
            .into_iter()
            .find(|p| p.pid == pid)
            .map(|p| vec![p])
            .ok_or_else(|| format!("未找到 PID 为 {pid} 的进程"));
    }

    let t = target.to_ascii_lowercase();
    let t_exe = if t.ends_with(".exe") {
        t.clone()
    } else {
        format!("{t}.exe")
    };
    let matched: Vec<ProcessInfo> = procs
        .into_iter()
        .filter(|p| {
            let n = p.name.to_ascii_lowercase();
            n == t || n == t_exe
        })
        .collect();

    if matched.is_empty() {
        Err(format!(
            "未找到名为 “{target}” 的进程（可用 `openspeedy list` 查看运行中的进程）"
        ))
    } else {
        Ok(matched)
    }
}

/// bridge 进程是 DLL 共享段速度值的持有者；bridge 重启后先恢复已保存的倍率，
/// 保证 `speed` 设置的倍率跨命令持续生效（先设速、后注入的游戏直接命中）。
fn restore_saved_speed() {
    if let Some(saved) = state::load_speed() {
        if (saved - 1.0).abs() > 1e-9 {
            let _ = bridge_client::bridge64_set_speed(saved);
            let _ = bridge_client::bridge32_set_speed(saved);
        }
    }
}

/// 让控制台使用 UTF-8 代码页，保证中文输出在 cmd/PowerShell 下不乱码。
fn enable_utf8_console() {
    use windows::Win32::System::Console::{SetConsoleCP, SetConsoleOutputCP};
    unsafe {
        let _ = SetConsoleOutputCP(65001);
        let _ = SetConsoleCP(65001);
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn print_help() {
    println!(
        "OpenSpeedy {version} — 命令行游戏变速器（Windows，无需 WebView）\n\
         \n\
         用法：\n\
           openspeedy <命令> [参数]\n\
         \n\
         命令：\n\
           list [过滤词] [-a]    列出进程（过滤词匹配 PID/名称/窗口标题；-a 含系统进程）\n\
           inject <PID|名称>     注入并启用变速（自动识别 32/64 位）\n\
           enable <PID|名称>     恢复变速（对已注入的进程）\n\
           disable <PID|名称>    暂停变速（保持注入，随时可再启用）\n\
           speed <倍率>          设置速度倍率（0.01 – 1000，如 2、1.5、0.5）\n\
           status [PID|名称]     显示当前倍率；带参数时显示该进程的注入状态\n\
           stop                  终止所有后台 bridge 进程\n\
           help                  显示本帮助\n\
           version               显示版本\n\
         \n\
         示例：\n\
           openspeedy list                # 查看进程\n\
           openspeedy list notepad        # 过滤：名称/标题含 notepad\n\
           openspeedy inject 1234         # 注入 PID 1234\n\
           openspeedy inject game.exe     # 按名称注入（同名进程全部处理）\n\
           openspeedy speed 2             # 2 倍速\n\
           openspeedy disable 1234        # 暂停 PID 1234\n\
           openspeedy status 1234         # 查看 PID 1234 的注入状态\n\
         \n\
         说明：\n\
           · 目标进程以管理员身份运行时，本工具也需要以管理员身份运行\n\
           · 倍率保存在 %APPDATA%\\OpenSpeedy\\cli-state.json，后续命令自动恢复\n\
           · 变速对已注入进程全局生效；新注入的进程直接继承当前倍率\n\
           · 日志（排障用）：可执行文件同目录的 logs\\ 文件夹与 bridge*.exe.log\n\
         \n\
         警告：部分网游有反作弊机制，使用变速可能被封号；请仅用于学习研究。",
        version = env!("CARGO_PKG_VERSION")
    );
}
