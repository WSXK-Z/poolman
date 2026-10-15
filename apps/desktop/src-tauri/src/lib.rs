//! 桌面端库：模块声明与启动装配。
//!
//! 应用入口 `run()` 由二进制目标 `main.rs` 调用；启动流程为
//! 初始化数据库 → 装配插件供应商与渠道模型 → 组装模型路由 → 拉起内置网关服务端。

mod agent;
mod apikey;
mod apis;
mod chat_router;
mod commands;
mod config;
mod converters;
mod dump;
mod extension;
mod handlers;
mod models;
mod prompts;
mod providers;
mod session;
mod stream_processor;
mod utils;

use std::sync::Arc;

use tracing::info;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use apikey::ApiKeyStore;
use apis::base::AppState;
use apis::create_app;
use chat_router::ChatRouter;
use config::ServerConfig;
use extension::credentials::CredentialStore;
use extension::executor::PluginChannelExecutor;
use extension::manager::ExtensionManager;
use extension::provider_control::{read_enablement_file, ProviderControl};
use extension::websocket::WebSocketBridge;
use models::{DeepSeekWebModel, PluginModel};
use providers::{seed, ProviderManager};
use session::manager::SessionManager;

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

/// 日志过滤器：默认 INFO；数据库驱动会把每条 SQL 以 INFO 打到 `sqlx::query`，
/// 与排查无关，因此按 target 降到 WARN（真实的连接/语句告警仍然保留）。
fn log_filter() -> tracing_subscriber::filter::Targets {
    use tracing_subscriber::filter::{LevelFilter, Targets};
    Targets::new()
        .with_default(LevelFilter::INFO)
        .with_target("sqlx", LevelFilter::WARN)
        .with_target("sea_orm", LevelFilter::WARN)
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
        .with(log_filter())
        .init();
}

/// 启动内置网关服务端：连接扩展 WebSocket → 起 HTTP API（模型入口由装配阶段传入）
async fn start_server(
    api_keys: Arc<ApiKeyStore>,
    credentials: Arc<CredentialStore>,
    bridge: Arc<WebSocketBridge>,
    chat_router: Arc<ChatRouter>,
    dump: Arc<dump::DumpConfig>,
) -> anyhow::Result<()> {
    init_logging();
    info!("正在启动 Rust AI 网关服务端（Tauri 集成）...");

    let config = ServerConfig::from_env();
    info!(
        "配置已加载: host={}, port={}, ws_port={}",
        config.host, config.port, config.ws_port
    );
    if dump.enabled() {
        info!(
            "请求落盘工具已启用：对话请求只落盘到 {} 并返回「开发中」，链路后续环节不执行（AI_GATEWAY_DUMP_REQUESTS=0 可关闭）",
            dump.dir().display()
        );
    }

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

    // HTTP API：横切关注点与三种 OpenAI 接口格式由 apis 层装配。
    let state = Arc::new(AppState {
        chat_router,
        bridge: bridge.clone(),
        credentials: credentials.clone(),
        config: config.clone(),
        api_keys,
        dump,
    });
    let app = create_app(state);

    let addr = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("HTTP 服务端正在监听 http://{}", addr);
    info!("API 端点: /health, /v1/models, /v1/chat/completions, /v1/completions, /v1/responses");

    axum::serve(listener, app).await?;
    Ok(())
}

/// 按持久化的模型行构建运行期渠道模型。
///
/// 目前只有 DeepSeek 网页版渠道有桌面端模型实现；其余渠道记录在库中但暂不实例化。
fn build_plugin_models(
    rows: &[providers::manager::Model],
    bridge: Arc<WebSocketBridge>,
    credentials: Arc<CredentialStore>,
    timeout_ms: u64,
    dump: Arc<dump::DumpConfig>,
) -> Vec<Arc<dyn PluginModel>> {
    let mut models: Vec<Arc<dyn PluginModel>> = Vec::new();
    let mut channels: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in rows {
        if !channels.insert(row.channel.clone()) {
            continue;
        }
        // 同一渠道下任一行启用即实例化
        if !rows
            .iter()
            .any(|candidate| candidate.channel == row.channel && candidate.enabled)
        {
            continue;
        }
        match row.channel.as_str() {
            "deepseek-web" => {
                let executor = Arc::new(PluginChannelExecutor::new(
                    bridge.clone(),
                    credentials.clone(),
                    row.channel.clone(),
                    timeout_ms,
                ));
                models.push(
                    Arc::new(
                        DeepSeekWebModel::new(row.display_name.clone(), executor)
                            .with_dump(dump.clone()),
                    ) as Arc<dyn PluginModel>,
                );
            }
            other => info!("渠道 {} 暂无桌面端模型实现，跳过实例化", other),
        }
    }
    models
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 初始化数据库（sea-orm + SQLite 持久化；路径可用 AI_GATEWAY_DB 覆盖；
    // 默认放 data 目录：开发在项目 data/，打包后随安装的软件目录）
    let data_dir = data_dir();
    let _ = std::fs::create_dir_all(&data_dir);
    let db_path =
        std::env::var("AI_GATEWAY_DB").unwrap_or_else(|_| format!("{}/gateway.db", data_dir));
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
    // 供应商与模型的持久化管理器（唯一真相：定义、启停、状态、配置）
    let provider_manager = Arc::new(
        init_rt
            .block_on(ProviderManager::new(api_keys.db()))
            .expect("初始化供应商/模型表失败"),
    );
    // 启动装配：幂等补齐插件供应商与其网页模型
    let seeded = init_rt
        .block_on(seed::ensure_plugin_provider(&provider_manager))
        .expect("初始化插件供应商失败");
    info!("插件供应商装配完成，本次新增模型 {} 条", seeded);
    // 数据目录中的 providers.json 记录着按渠道的启停选择：写入模型行后删除该文件，
    // 之后启停状态的持久化真相只在模型行上。
    if let Some(enablement) = read_enablement_file(&data_dir) {
        for (channel, enabled) in &enablement {
            let _ = init_rt.block_on(provider_manager.set_channel_enabled(
                providers::plugin::PLUGIN_PROVIDER_ID,
                channel,
                *enabled,
            ));
        }
        let _ = std::fs::remove_file(std::path::Path::new(&data_dir).join("providers.json"));
    }
    // 读取已登记的模型行，供运行期实例化
    let model_rows = init_rt
        .block_on(provider_manager.list_models(Some(providers::plugin::PLUGIN_PROVIDER_ID)))
        .expect("读取模型失败");
    let channel_enablement = init_rt
        .block_on(provider_manager.channel_enablement(providers::plugin::PLUGIN_PROVIDER_ID))
        .expect("读取渠道启停状态失败");
    drop(init_rt);

    let provider_control = Arc::new(ProviderControl::new(channel_enablement));
    let credentials = Arc::new(CredentialStore::new());
    // 插件实例管理器是插件连接信息的唯一归属，连接信息持久化到数据目录
    let extensions = Arc::new(ExtensionManager::new(&data_dir));
    let bridge = Arc::new(WebSocketBridge::new(
        credentials.clone(),
        extensions.clone(),
        Some(sessions.clone()),
    ));
    let startup_config = ServerConfig::from_env();
    // 请求落盘工具：开发期链路排查用，可在任意环节插入（请求阶段插在推理入口、
    // 提示词阶段插在模型内转换完成后）。模型装配需要它，故先构造。
    let dump = Arc::new(dump::DumpConfig::from_env(&data_dir));
    // 按持久化的模型行构建运行期渠道模型，组装模型分派入口
    let plugin_models = build_plugin_models(
        &model_rows,
        bridge.clone(),
        credentials.clone(),
        startup_config.request_timeout_ms,
        dump.clone(),
    );
    info!("已实例化渠道模型 {} 个", plugin_models.len());
    let chat_router = Arc::new(ChatRouter::new(plugin_models, provider_control.clone()));

    let api_keys_server = api_keys.clone();
    let credentials_server = credentials.clone();
    let bridge_server = bridge.clone();
    let chat_router_server = chat_router.clone();
    let dump_server = dump.clone();

    // 在独立线程中启动 HTTP 服务器（避免阻塞 Tauri UI）
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("创建 tokio 运行时失败");
        if let Err(e) = rt.block_on(start_server(
            api_keys_server,
            credentials_server,
            bridge_server,
            chat_router_server,
            dump_server,
        )) {
            eprintln!("HTTP 服务端错误: {}", e);
        }
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(api_keys)
        .manage(sessions)
        .manage(provider_control)
        .manage(provider_manager)
        .manage(extensions)
        .manage(bridge)
        .manage(chat_router)
        .invoke_handler(commands::handler())
        .run(tauri::generate_context!())
        .expect("运行 tauri 应用出错");
}

#[cfg(test)]
mod log_filter_tests {
    use super::log_filter;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::{Layer, SubscriberExt};

    /// 记录经过过滤器的事件（`target:level`）。
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> Layer<S> for Recorder {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let metadata = event.metadata();
            self.0
                .lock()
                .unwrap()
                .push(format!("{}:{}", metadata.target(), metadata.level()));
        }
    }

    /// 数据库逐条 SQL（`sqlx::query` 的 INFO）被过滤，真实告警与应用日志照常输出。
    #[test]
    fn sqlx_query_info_is_filtered_but_warnings_and_app_logs_pass() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let subscriber = tracing_subscriber::registry()
            .with(log_filter())
            .with(Recorder(seen.clone()));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "sqlx::query", "summary=\"SELECT 1\" db.statement=\"SELECT 1\"");
            tracing::debug!(target: "sqlx::query", "详细日志");
            tracing::warn!(target: "sqlx::query", "连接池告警");
            tracing::info!(target: "sea_orm::driver", "驱动日志");
            tracing::info!(target: "desktop::providers", "应用日志");
        });

        let seen = seen.lock().unwrap().clone();
        assert!(
            !seen.iter().any(|entry| entry == "sqlx::query:INFO"),
            "sqlx 的 INFO 级 SQL 日志应被过滤：{:?}",
            seen
        );
        assert!(
            seen.iter().any(|entry| entry == "sqlx::query:WARN"),
            "sqlx 的真实告警不应被一起屏蔽：{:?}",
            seen
        );
        assert!(
            !seen.iter().any(|entry| entry.starts_with("sea_orm")),
            "sea_orm 的 INFO 日志同样应被过滤：{:?}",
            seen
        );
        assert!(
            seen.iter().any(|entry| entry == "desktop::providers:INFO"),
            "应用自身日志应照常输出：{:?}",
            seen
        );
    }
}