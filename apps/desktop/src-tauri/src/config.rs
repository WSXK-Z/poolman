//! 本地网关运行配置。
//!
//! 配置统一从环境变量读取，并在缺少或格式无效时采用本机开发默认值；
//! HTTP 与 WebSocket 服务共享同一份配置，避免监听地址和超时设置分散。

use std::env;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub ws_port: u16,
    pub log_level: String,
    /// 非流式请求的总超时（毫秒）。
    pub request_timeout_ms: u64,
    /// 流式请求的静默容忍（毫秒）：这么久没有新数据才判定上游挂死。
    pub stream_idle_timeout_ms: u64,
    pub cors_origin: String,
    pub ws_auth_token: Option<String>,
}

impl ServerConfig {
    pub fn from_env() -> Self {
        Self {
            host: env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
            port: env::var("PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(13213),
            ws_port: env::var("WS_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3001),
            log_level: env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string()),
            request_timeout_ms: env::var("REQUEST_TIMEOUT_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(120_000),
            stream_idle_timeout_ms: env::var("STREAM_IDLE_TIMEOUT_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(180_000),
            cors_origin: env::var("CORS_ORIGIN").unwrap_or_else(|_| "*".to_string()),
            ws_auth_token: env::var("WS_AUTH_TOKEN").ok(),
        }
    }
}
