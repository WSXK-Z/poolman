//! 运行设置命令：对话请求频率的读取与修改。
//!
//! 频率值由 [`ChatFrequency`] 统一持有：保存后对之后的对话请求立即生效，
//! 同时写入数据目录的 `settings.json`，重启后沿用。

use std::sync::Arc;

use crate::handlers::{command_set, CommandSet};
use crate::throttle::{ChatFrequency, DEFAULT_CHAT_INTERVAL_MS, MAX_CHAT_INTERVAL_MS};
use crate::title_engine::{TitleSettings, TitleSource, DEFAULT_MODEL_DIR};

/// 本组命令的处理器。
pub(super) fn handler() -> CommandSet<tauri::Wry> {
    command_set![
        get_chat_settings,
        set_chat_interval,
        get_title_settings,
        set_title_settings
    ]
}

/// 当前对话请求频率与允许范围（界面据此显示输入框）。
#[tauri::command]
pub fn get_chat_settings(frequency: tauri::State<'_, Arc<ChatFrequency>>) -> serde_json::Value {
    serde_json::json!({
        "chat_interval_ms": frequency.interval_ms(),
        "default_chat_interval_ms": DEFAULT_CHAT_INTERVAL_MS,
        "max_chat_interval_ms": MAX_CHAT_INTERVAL_MS,
    })
}

/// 设置对话请求频率（秒），返回实际生效的毫秒值；0 表示不限制，越界值夹到允许范围。
#[tauri::command]
pub fn set_chat_interval(
    frequency: tauri::State<'_, Arc<ChatFrequency>>,
    interval_seconds: f64,
) -> Result<u64, String> {
    if !interval_seconds.is_finite() || interval_seconds < 0.0 {
        return Err("对话请求频率需为不小于 0 的秒数（0 表示不限制）".to_string());
    }
    // 秒 → 毫秒：界面按秒输入，服务端按毫秒排队，四舍五入避免浮点尾数。
    let interval_ms = (interval_seconds * 1000.0).round();
    if interval_ms > u64::MAX as f64 {
        return Err(format!(
            "对话请求频率不能超过 {} 秒",
            MAX_CHAT_INTERVAL_MS / 1000
        ));
    }
    frequency.set_interval_ms(interval_ms as u64)
}

/// 当前会话标题设置：来源策略、本地模型目录与默认目录。
#[tauri::command]
pub fn get_title_settings(
    title: tauri::State<'_, Arc<TitleSettings>>,
) -> serde_json::Value {
    serde_json::json!({
        "source": title.source().as_str(),
        "model_dir": title.model_dir(),
        "default_model_dir": DEFAULT_MODEL_DIR,
        "has_local_model": title.has_local_model(),
        "model_dir_resolved": title
            .model_path()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default(),
    })
}

/// 保存会话标题设置：`source` 为 heuristic / local / web。
#[tauri::command]
pub fn set_title_settings(
    title: tauri::State<'_, Arc<TitleSettings>>,
    source: String,
    model_dir: String,
) -> Result<serde_json::Value, String> {
    let parsed = TitleSource::parse(&source);
    title.save(parsed, &model_dir)?;
    Ok(get_title_settings(title))
}
