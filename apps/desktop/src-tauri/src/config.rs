//! 本地网关运行配置。
//!
//! 配置统一从环境变量读取，并在缺少或格式无效时采用本机开发默认值；
//! HTTP 与 WebSocket 服务共享同一份配置，避免监听地址和超时设置分散。

use std::env;

/// 默认请求体上限：32 MiB。
///
/// axum 的默认上限是 2 MiB，长对话（整段历史 + 工具结果）或多模态请求很容易超过，
/// 客户端看到的是 413 `Failed to buffer the request body: length limit exceeded`。
pub const DEFAULT_REQUEST_BODY_LIMIT_BYTES: u64 = 32 * 1024 * 1024;

/// 请求体上限的环境变量名（单位 MiB，`0` 表示不限制）。
const REQUEST_BODY_LIMIT_ENV: &str = "MAX_REQUEST_BODY_MB";

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
    /// HTTP 请求体大小上限（字节）；0 表示不限制。
    pub max_request_body_bytes: u64,
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
            max_request_body_bytes: request_body_limit_bytes(
                env::var(REQUEST_BODY_LIMIT_ENV).ok().as_deref(),
            ),
            cors_origin: env::var("CORS_ORIGIN").unwrap_or_else(|_| "*".to_string()),
            ws_auth_token: env::var("WS_AUTH_TOKEN").ok(),
        }
    }
}

/// 解析请求体上限：`MAX_REQUEST_BODY_MB` 以 MiB 为单位，上限 1024 MiB；
/// `0` 表示不限制，非法值用默认值。
fn request_body_limit_bytes(megabytes: Option<&str>) -> u64 {
    match megabytes.and_then(|value| value.trim().parse::<u64>().ok()) {
        Some(mb) => mb.min(1024) * 1024 * 1024,
        None => DEFAULT_REQUEST_BODY_LIMIT_BYTES,
    }
}

#[cfg(test)]
mod tests {
    use super::{request_body_limit_bytes, DEFAULT_REQUEST_BODY_LIMIT_BYTES};

    /// 请求体上限：按 MiB 读取，0 不限制，超过 1024 夹到上限，非法值回落默认。
    #[test]
    fn request_body_limit_reads_megabytes_with_bounds() {
        assert_eq!(
            request_body_limit_bytes(None),
            DEFAULT_REQUEST_BODY_LIMIT_BYTES
        );
        assert_eq!(request_body_limit_bytes(Some("64")), 64 * 1024 * 1024);
        assert_eq!(request_body_limit_bytes(Some(" 8 ")), 8 * 1024 * 1024);
        assert_eq!(request_body_limit_bytes(Some("0")), 0, "0 表示不限制");
        assert_eq!(
            request_body_limit_bytes(Some("99999")),
            1024 * 1024 * 1024
        );
        assert_eq!(
            request_body_limit_bytes(Some("huge")),
            DEFAULT_REQUEST_BODY_LIMIT_BYTES,
            "非法值回落默认"
        );
        assert_eq!(
            request_body_limit_bytes(Some("-1")),
            DEFAULT_REQUEST_BODY_LIMIT_BYTES,
            "负数按非法值处理"
        );
    }
}
