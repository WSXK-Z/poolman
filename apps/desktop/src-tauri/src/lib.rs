mod apikey;
mod config;
mod converter;
mod extension;
mod openai;
mod session;

use std::sync::Arc;
use tauri::Manager;
use tracing::info;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// 数据目录（数据库/日志）：环境变量 AI_GATEWAY_DATA 覆盖；
/// 开发模式(debug)用项目 data 目录(../data，保持现有数据不丢)，
/// 打包发布(release)用可执行文件所在目录（安装后的软件目录）下的 data/。
fn data_dir() -> String {
    if let Ok(d) = std::env::var("AI_GATEWAY_DATA") {
        if !d.trim().is_empty() {
            return d.trim().to_string();
        }
    }
    if cfg!(debug_assertions) {
        // 开发模式：保持项目 data 目录（apps/desktop/data），避免 dev 换库丢数据
        "../data".to_string()
    } else {
        // 打包发布：数据库/日志放可执行文件所在目录（安装后=软件目录）下的 data/
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let d = dir.to_string_lossy().to_string();
                if !d.is_empty() {
                    return format!("{}/data", d);
                }
            }
        }
        "../data".to_string()
    }
}

/// 日志初始化：同时输出到 stderr 与文件（写入 data 目录，避免 tauri dev 监听 src-tauri 触发重启）
fn init_logging() {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = tracing_appender::rolling::never(&dir, "gateway.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file);
    // 保活 non_blocking guard，避免日志丢失
    std::mem::forget(guard);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file_writer)
                .with_ansi(false),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(tracing_subscriber::filter::LevelFilter::INFO)
        .init();
}

use apikey::ApiKeyStore;
use config::ServerConfig;
use extension::credentials::CredentialStore;
use extension::websocket::WebSocketBridge;
use openai::chat_router::ChatRouter;
use extension::adapters::deepseek::DeepSeekService;
use session::manager::SessionManager;

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

/// 定位浏览器插件构建产物目录，供「加载已解压扩展程序」一键引导使用。
/// 查找顺序：打包后的资源目录(resource_dir/chrome-mv3，兼容旧版 _up_ 转义路径) → 环境变量 → 工作目录/exe 目录推算(开发模式)。
#[tauri::command]
fn plugin_extension_dir(app: tauri::AppHandle) -> Result<String, String> {
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

/// 创建 API Key（绑定平台，决定用哪个转换器）
#[tauri::command]
async fn create_api_key(
    state: tauri::State<'_, Arc<ApiKeyStore>>,
    platform: String,
    note: Option<String>,
) -> Result<apikey::ApiKey, String> {
    state.create(&platform, note).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_api_keys(
    state: tauri::State<'_, Arc<ApiKeyStore>>,
) -> Result<Vec<apikey::ApiKey>, String> {
    state.list().await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn revoke_api_key(
    state: tauri::State<'_, Arc<ApiKeyStore>>,
    key: String,
) -> Result<bool, String> {
    state.revoke(&key).await.map_err(|e| e.to_string())
}

/// 列出会话（可按 API Key 过滤）
#[tauri::command]
async fn list_sessions(
    state: tauri::State<'_, Arc<SessionManager>>,
    api_key: Option<String>,
) -> Result<Vec<session::manager::SessionInfo>, String> {
    state.list_sessions(api_key.as_deref()).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_session(
    state: tauri::State<'_, Arc<SessionManager>>,
    id: String,
) -> Result<Option<session::manager::SessionDetail>, String> {
    state.get_session(&id).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn delete_session(
    state: tauri::State<'_, Arc<SessionManager>>,
    id: String,
) -> Result<bool, String> {
    state.delete_session(&id).await.map_err(|e| e.to_string())
}

/// 启动内置网关服务端：连接扩展 WebSocket → 组装 DeepSeek provider → 起 HTTP API
async fn start_server(
    api_keys: Arc<ApiKeyStore>,
    sessions: Arc<SessionManager>,
) -> anyhow::Result<()> {
    init_logging();
    info!("正在启动 Rust AI 网关服务端（Tauri 集成）...");

    let config = ServerConfig::from_env();
    info!(
        "配置已加载: host={}, port={}, ws_port={}",
        config.host, config.port, config.ws_port
    );

    // 凭证存储（扩展通过 CREDENTIAL_SYNC 上报）
    let credentials = Arc::new(CredentialStore::new());

    // WebSocket 桥接（与浏览器扩展通信，捕获凭证 + 执行 fetch）
    let bridge = Arc::new(WebSocketBridge::new(credentials.clone()));

    // 监听扩展 WebSocket（服务端模式，后台任务）：扩展作为客户端连接 ws://host:ws_port
    let ws_addr = format!("{}:{}", config.host, config.ws_port);
    let ws_addr_log = ws_addr.clone();
    {
        let bridge_ws = bridge.clone();
        tokio::spawn(async move {
            if let Err(e) = bridge_ws.listen(&ws_addr).await {
                info!("WebSocket 服务端错误: {}", e);
            }
        });
    }
    info!("WebSocket 服务端正在监听 ws://{}", ws_addr_log);

    // DeepSeek 网页版服务端直管（请求组装 / PoW / SSE / 会话 / 工具）
    let deepseek = Arc::new(DeepSeekService::new(
        bridge.clone(),
        credentials.clone(),
        config.request_timeout_ms,
        Some(sessions.clone()),
    ));
    let chat_router = Arc::new(ChatRouter::new(
        bridge.clone(),
        Some(deepseek),
        config.request_timeout_ms,
    ));

    // HTTP API（OpenAI 兼容）
    let app = openai::api::create_app(
        chat_router,
        bridge.clone(),
        credentials.clone(),
        config.clone(),
        api_keys,
    );

    let addr = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("HTTP 服务端正在监听 http://{}", addr);
    info!("API 端点: /health, /v1/models, /v1/chat/completions");

    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 初始化数据库（sea-orm + SQLite 持久化；路径可用 AI_GATEWAY_DB 覆盖；
    // 默认放 data 目录：开发在项目 data/，打包后随安装的软件目录）
    let data_dir = data_dir();
    let _ = std::fs::create_dir_all(&data_dir);
    let db_path = std::env::var("AI_GATEWAY_DB")
        .unwrap_or_else(|_| format!("{}/gateway.db", data_dir));
    let init_rt = tokio::runtime::Runtime::new().expect("创建 tokio 运行时失败");
    let api_keys = Arc::new(
        init_rt
            .block_on(ApiKeyStore::init(&db_path))
            .expect("初始化数据库失败"),
    );
    let sessions = Arc::new(
        init_rt
            .block_on(SessionManager::new(api_keys.db()))
            .expect("初始化会话数据库失败"),
    );
    drop(init_rt);
    let api_keys_server = api_keys.clone();
    let sessions_server = sessions.clone();

    // 在独立线程中启动 HTTP 服务器（避免阻塞 Tauri UI）
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("创建 tokio 运行时失败");
        if let Err(e) = rt.block_on(start_server(api_keys_server, sessions_server)) {
            eprintln!("HTTP 服务端错误: {}", e);
        }
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(api_keys)
        .manage(sessions)
        .invoke_handler(tauri::generate_handler![
            greet,
            plugin_extension_dir,
            create_api_key,
            list_api_keys,
            revoke_api_key,
            list_sessions,
            get_session,
            delete_session
        ])
        .run(tauri::generate_context!())
        .expect("运行 tauri 应用出错");
}
