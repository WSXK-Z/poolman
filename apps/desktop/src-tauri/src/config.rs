use std::env;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub ws_port: u16,
    pub log_level: String,
    pub request_timeout_ms: u64,
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
            cors_origin: env::var("CORS_ORIGIN").unwrap_or_else(|_| "*".to_string()),
            ws_auth_token: env::var("WS_AUTH_TOKEN").ok(),
        }
    }
}
