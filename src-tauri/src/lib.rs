//! OpenSpeedy CLI — 库根。
//!
//! Windows 专用、纯命令行的游戏变速控制，不依赖 GUI/WebView。模块分工：
//! - `cli`：命令行解析与各命令实现（入口）
//! - `bridges`：bridge64/bridge32 辅助进程的生命周期（启动/复用/清理）
//! - `bridge_client`：命名管道客户端（向 bridge 发命令）
//! - `process_enumerator`：进程枚举（PID/架构/内存/窗口标题/模块）
//! - `state`：速度倍率的持久化
//! - `applog`：诊断日志（写可执行文件同目录 logs/）

pub mod applog;
mod bridge_client;
mod bridge_job;
mod bridges;
pub mod cli;
mod process_enumerator;
mod state;
