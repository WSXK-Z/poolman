//! 插件命令：插件实例的启停、渠道启停与渠道操作，以及插件构建目录定位。

use std::sync::Arc;

use tauri::Manager;

use crate::extension::websocket::WebSocketBridge;
use crate::handlers::{command_set, CommandSet};

/// 本组命令的处理器。
pub(super) fn handler() -> CommandSet<tauri::Wry> {
    command_set![
        plugin_extension_dir,
        list_plugins,
        set_plugin_enabled,
        set_plugin_channel_enabled,
        control_plugin_provider,
    ]
}

/// 定位浏览器插件构建产物目录，供「加载已解压扩展程序」一键引导使用。
/// 查找顺序：打包后的资源目录(resource_dir/chrome-mv3，兼容旧版 _up_ 转义路径) → 环境变量 → 工作目录/exe 目录推算(开发模式)。
#[tauri::command]
pub fn plugin_extension_dir(app: tauri::AppHandle) -> Result<String, String> {
    let mut candidates: Vec<String> = Vec::new();
    // 1) 打包后的资源目录（tauri 会把 bundle.resources 复制到这里，插件随应用分发）
    if let Ok(dir) = app.path().resource_dir() {
        // 新版 map 形式资源：../../extension/.output/chrome-mv3 → chrome-mv3（浅路径）
        candidates.push(dir.join("chrome-mv3").to_string_lossy().to_string());
        // 兼容旧版列表形式资源：../ 会被 tauri 转义为 _up_
        candidates.push(
            dir.join("_up_")
                .join("_up_")
                .join("extension/.output/chrome-mv3")
                .to_string_lossy()
                .to_string(),
        );
    }
    // 2) 环境变量覆盖
    if let Ok(d) = std::env::var("AI_GATEWAY_EXTENSION_DIR") {
        if !d.trim().is_empty() {
            candidates.push(d.trim().to_string());
        }
    }
    // 3) 工作目录 / exe 目录推算（开发模式）
    if let Ok(cwd) = std::env::current_dir() {
        let c = cwd.to_string_lossy().to_string();
        candidates.push(format!("{}/../extension/.output/chrome-mv3", c));
        candidates.push(format!("{}/extension/.output/chrome-mv3", c));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let d = dir.to_string_lossy().to_string();
            candidates.push(format!("{}/../extension/.output/chrome-mv3", d));
            candidates.push(format!("{}/extension/.output/chrome-mv3", d));
        }
    }
    for p in &candidates {
        if std::path::Path::new(p).is_dir() {
            return Ok(p.clone());
        }
    }
    Err(format!(
        "未找到插件构建目录，请先构建扩展。已检查: {}",
        candidates.join(" | ")
    ))
}

#[tauri::command]
pub async fn list_plugins(
    state: tauri::State<'_, Arc<WebSocketBridge>>,
) -> Result<Vec<serde_json::Value>, String> {
    Ok(state.list_plugins().await)
}

#[tauri::command]
pub async fn set_plugin_enabled(
    state: tauri::State<'_, Arc<WebSocketBridge>>,
    plugin_id: String,
    enabled: bool,
) -> Result<(), String> {
    state
        .set_plugin_enabled(&plugin_id, enabled)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_plugin_channel_enabled(
    state: tauri::State<'_, Arc<WebSocketBridge>>,
    plugin_id: String,
    channel: String,
    enabled: bool,
) -> Result<(), String> {
    if enabled {
        let authed = state
            .manager
            .status_of(&plugin_id)
            .and_then(|status| {
                status
                    .get(&channel)
                    .and_then(|channel_status| channel_status.get("authed"))
                    .and_then(serde_json::Value::as_bool)
            })
            .unwrap_or(false);
        if !authed {
            return Err(format!("插件中的渠道 {} 尚未登录，不能启用", channel));
        }
    }
    state
        .set_plugin_channel_enabled(&plugin_id, &channel, enabled)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn control_plugin_provider(
    state: tauri::State<'_, Arc<WebSocketBridge>>,
    plugin_id: String,
    provider: String,
    action: String,
) -> Result<(), String> {
    if !matches!(action.as_str(), "connect" | "disconnect" | "open") {
        return Err("不支持的供应商操作".to_string());
    }
    state
        .send_provider_control(&plugin_id, &provider, &action)
        .await
        .map_err(|e| e.to_string())
}
