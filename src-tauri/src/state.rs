//! CLI 速度倍率的持久化。
//!
//! `speed` 命令把倍率写进 `%APPDATA%\OpenSpeedy\cli-state.json`；之后每个需要
//! bridge 的命令启动 bridge 后会先恢复该值，再执行操作。
//!
//! 为什么需要恢复：速度值存在 speedpatch DLL 的共享数据段里，由映射该 DLL 的
//! 进程（bridge、以及被注入的游戏）共同持有。bridge 是本 CLI 每次运行临时拉起
//! 的，进程退出后共享段的最后映射者也随之消失，值会回到默认 1.0。持久化 +
//! 恢复保证「先 speed、后 inject」的顺序也能让游戏直接以设定倍率运行。

use std::path::PathBuf;

const APP_DIR: &str = "OpenSpeedy";
const FILE_NAME: &str = "cli-state.json";

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct State {
    speed: Option<f64>,
}

fn state_path() -> PathBuf {
    // 优先 %APPDATA%（程序目录可能在 Program Files 下不可写）。
    if let Ok(appdata) = std::env::var("APPDATA") {
        if !appdata.is_empty() {
            return PathBuf::from(appdata).join(APP_DIR).join(FILE_NAME);
        }
    }
    // 退化：可执行文件同目录。
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(std::env::temp_dir)
        .join(FILE_NAME)
}

/// 读取已保存的速度倍率；文件不存在或损坏时返回 None。
pub fn load_speed() -> Option<f64> {
    let data = std::fs::read_to_string(state_path()).ok()?;
    let state: State = serde_json::from_str(&data).ok()?;
    state.speed.filter(|f| f.is_finite() && *f > 0.0)
}

/// 保存速度倍率（尽力而为，失败只记日志）。
pub fn save_speed(factor: f64) {
    let path = state_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let state = State {
        speed: Some(factor),
    };
    match serde_json::to_string_pretty(&state) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                crate::applog::warn("state", format!("无法保存 {}：{e}", path.display()));
            }
        }
        Err(e) => crate::applog::warn("state", format!("序列化状态失败：{e}")),
    }
}
