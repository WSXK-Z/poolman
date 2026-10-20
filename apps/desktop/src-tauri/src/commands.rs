//! Tauri 前端命令面：桌面界面调用的全部 `#[tauri::command]`。
//!
//! 命令按业务域分文件，各自用 `command_set!` 建立自己的命令组（见 [`crate::handlers`]），
//! 这里用 `chain_commands!` 汇总成一个处理器交给 `run()` 注册。
//! 命令只做参数搬运与「先落库、后刷新运行期状态」的编排，业务实现都在各模块内。

mod api_keys;
mod plugins;
mod providers;
mod sessions;
mod settings;

use tauri::ipc::Invoke;

use crate::handlers::{chain_commands, command_set};

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
pub fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

/// 汇总全部命令的调用处理器：函数名即前端 `invoke` 使用的命令名。
pub fn handler() -> impl Fn(Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    chain_commands![
        command_set![greet],
        api_keys::handler(),
        sessions::handler(),
        plugins::handler(),
        providers::handler(),
        settings::handler(),
    ]
}
