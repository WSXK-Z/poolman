//! 请求落盘工具（开发期链路分段排查用，可插入到任意环节）。
//!
//! 插在推理入口最外层时：把客户端对话请求的全部参数（方法 / 路径 / 查询 / 请求头 / 原始正文）
//! 落成 `<目录>/chat-request-<时间戳>-<随机>.json`（一请求一文件），随后按请求的接口格式与
//! `stream` 参数返回一份形状正常的模型输出（正文固定为「开发中」），其后的环节
//! （鉴权、路由、模型、插件）都不执行。
//!
//! 开关：环境变量 `AI_GATEWAY_DUMP_REQUESTS`，`1` / `true` / `yes` / `on` 启用，其余值关闭（默认关闭）。
//! 响应阶段的落盘（[`DumpConfig::write_response`] / [`DumpConfig::write_tool_calls`]，由模型在解析完
//! 网页返回后调用）开关为 `AI_GATEWAY_RESPONSE_DUMP`，未设置时跟随请求阶段。
//! 两个阶段关闭时都不写任何文件。
//! 目录：环境变量 `AI_GATEWAY_DUMP_DIR`，默认应用数据目录。
//!
//! 落盘内容含请求头（其中可能有 API Key）、提示词与模型返回，因此产物不入库（见 `.gitignore`）。

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{request::Parts, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use serde::Serialize;
use serde_json::Value;
use tracing::{info, warn};

/// 单次请求正文上限：超过则拒绝落盘，避免把超大正文读进内存。
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// 落盘配置：开关 + 目标目录。
///
/// 两个阶段各有开关，互不影响：
/// - 请求阶段（推理入口）：`AI_GATEWAY_DUMP_REQUESTS`，落盘后直接回占位回答
/// - 响应阶段（模型解析完网页返回后）：`AI_GATEWAY_RESPONSE_DUMP`，落盘后链路继续
#[derive(Debug)]
pub struct DumpConfig {
    enabled: bool,
    response_enabled: bool,
    dir: PathBuf,
}

impl DumpConfig {
    pub fn new(enabled: bool, dir: impl Into<PathBuf>) -> Self {
        Self {
            enabled,
            response_enabled: enabled,
            dir: dir.into(),
        }
    }

    /// 从环境变量读取开关与目录；目录默认取应用数据目录。
    pub fn from_env(data_dir: &str) -> Self {
        let enabled = env_flag("AI_GATEWAY_DUMP_REQUESTS", false);
        // 响应阶段的开关独立；未设置时跟随请求阶段（保持「插入即生效」的手感）
        let response_enabled = env_flag("AI_GATEWAY_RESPONSE_DUMP", enabled);
        let dir = env::var("AI_GATEWAY_DUMP_DIR")
            .ok()
            .filter(|dir| !dir.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(data_dir));
        Self {
            enabled,
            response_enabled,
            dir,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 响应阶段是否落盘。
    pub fn response_enabled(&self) -> bool {
        self.response_enabled
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 把一个请求快照写成 `chat-request-<时间戳>-<随机>.json`，返回落盘路径。
    ///
    /// 请求阶段关闭时不写任何文件。
    pub fn write(&self, dump: &RequestDump) -> anyhow::Result<PathBuf> {
        if !self.enabled {
            anyhow::bail!("请求落盘已关闭（AI_GATEWAY_DUMP_REQUESTS）");
        }
        self.write_json("chat-request", dump)
    }

    /// 把一份响应阶段快照写成 `response-<时间戳>-<随机>.json`，返回落盘路径。
    ///
    /// 响应阶段关闭时不写任何文件。
    pub fn write_response<T: Serialize>(&self, payload: &T) -> anyhow::Result<PathBuf> {
        if !self.response_enabled {
            anyhow::bail!("响应落盘已关闭（AI_GATEWAY_RESPONSE_DUMP）");
        }
        self.write_json("response", payload)
    }

    /// 把一份工具调用快照写成 `tool-call-<时间戳>-<随机>.json`，返回落盘路径。
    ///
    /// 与响应快照同属响应阶段（同一开关、同一目录），只是单独成文件：
    /// 工具调用一有就落，便于直接对照「模型吐的原文」与「解析出的调用」。
    /// 响应阶段关闭时不写任何文件。
    pub fn write_tool_calls<T: Serialize>(&self, payload: &T) -> anyhow::Result<PathBuf> {
        if !self.response_enabled {
            anyhow::bail!("工具调用落盘已关闭（AI_GATEWAY_RESPONSE_DUMP）");
        }
        self.write_json("tool-call", payload)
    }

    /// 一快照一文件：`<前缀>-<时间戳>-<随机>.json`。
    fn write_json<T: Serialize>(&self, prefix: &str, payload: &T) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!(
            "{}-{}-{}.json",
            prefix,
            chrono::Utc::now().timestamp_millis(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&path, serde_json::to_vec_pretty(payload)?)?;
        Ok(path)
    }
}

/// 读一个布尔型环境变量开关：`0` / `false` / `off` / `no` 关闭，其余值启用，未设置时用 `default`。
fn env_flag(name: &str, default: bool) -> bool {
    env::var(name)
        .map(|value| {
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        })
        .unwrap_or(default)
}

/// 一次请求的完整快照。
#[derive(Debug, Serialize)]
pub struct RequestDump {
    /// 到达时间（RFC 3339）。
    pub received_at: String,
    pub method: String,
    /// 完整请求目标（含查询串）。
    pub uri: String,
    pub path: String,
    pub query: Option<String>,
    /// 请求头：名小写，同名多个以 `, ` 合并。
    pub headers: BTreeMap<String, String>,
    /// 正文按 JSON 解析的结果（不是 JSON 时为 null）。
    pub body: Option<Value>,
    /// 正文原文（非 UTF-8 字节按替换字符处理）。
    pub body_raw: String,
}

impl RequestDump {
    /// 从请求分片与已读出的正文字节构造快照。
    pub fn capture(parts: &Parts, body: &[u8]) -> Self {
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        for (name, value) in parts.headers.iter() {
            let value = value.to_str().unwrap_or_default().to_string();
            headers
                .entry(name.as_str().to_string())
                .and_modify(|existing| {
                    existing.push_str(", ");
                    existing.push_str(&value);
                })
                .or_insert(value);
        }
        Self {
            received_at: chrono::Utc::now().to_rfc3339(),
            method: parts.method.to_string(),
            uri: parts.uri.to_string(),
            path: parts.uri.path().to_string(),
            query: parts.uri.query().map(str::to_string),
            headers,
            body: serde_json::from_slice(body).ok(),
            body_raw: String::from_utf8_lossy(body).to_string(),
        }
    }

    /// 请求是否要求流式输出（`body.stream == true`）。
    pub fn wants_stream(&self) -> bool {
        self.body
            .as_ref()
            .and_then(|body| body.get("stream"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// 请求的模型名；缺失时回落为 `poolman`（占位回答原样回带，供客户端校验）。
    pub fn model_name(&self) -> &str {
        self.body
            .as_ref()
            .and_then(|body| body.get("model"))
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .unwrap_or("poolman")
    }
}

/// 占位回答的正文（落盘后固定回这句话，链路后续环节不执行）。
const STUB_TEXT: &str = "开发中";

/// 中间件：启用时落盘请求并直接返回占位回答；未启用则原样放行。
pub async fn require_dump(
    State(config): State<Arc<DumpConfig>>,
    request: Request,
    next: Next,
) -> Response {
    if !config.enabled() {
        return next.run(request).await;
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return body_too_large(),
    };

    let dump = RequestDump::capture(&parts, &bytes);
    let mut response = stub_response(&dump);
    match config.write(&dump) {
        Ok(path) => {
            info!("对话请求已落盘：{}", path.display());
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                if let Ok(value) = HeaderValue::from_str(name) {
                    response
                        .headers_mut()
                        .insert("x-poolman-request-dump", value);
                }
            }
        }
        Err(error) => warn!("对话请求落盘失败：{}", error),
    }
    response
}

/// 占位回答：按请求路径选接口格式、按 `stream` 参数选流式或非流式，
/// 返回一份形状正常的模型输出（正文为 [`STUB_TEXT`]）。
fn stub_response(dump: &RequestDump) -> Response {
    let model = dump.model_name();
    let stream = dump.wants_stream();
    match dump.path.as_str() {
        "/v1/completions" => completion_stub(model, stream),
        "/v1/responses" => responses_stub(model, stream),
        // `/v1/chat/completions` 以及未知路径：都按对话补全形状返回
        _ => chat_stub(model, stream),
    }
}

/// 对话补全（`/v1/chat/completions`）占位回答。
fn chat_stub(model: &str, stream: bool) -> Response {
    let id = format!("chatcmpl-{}", chrono::Utc::now().timestamp_millis());
    let created = chrono::Utc::now().timestamp();
    if stream {
        let frames = [
            serde_json::json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{ "index": 0, "delta": { "content": STUB_TEXT }, "finish_reason": Value::Null }],
            }),
            serde_json::json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            }),
        ];
        return sse_frames(&frames.iter().map(data_frame).collect::<Vec<_>>());
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "id": id,
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": STUB_TEXT },
                "finish_reason": "stop",
            }],
            "usage": stub_usage(),
        })),
    )
        .into_response()
}

/// 文本补全（`/v1/completions`）占位回答。
fn completion_stub(model: &str, stream: bool) -> Response {
    let id = format!("cmpl-{}", chrono::Utc::now().timestamp_millis());
    let created = chrono::Utc::now().timestamp();
    if stream {
        let frames = [
            serde_json::json!({
                "id": id,
                "object": "text_completion",
                "created": created,
                "model": model,
                "choices": [{ "text": STUB_TEXT, "index": 0, "logprobs": Value::Null, "finish_reason": Value::Null }],
            }),
            serde_json::json!({
                "id": id,
                "object": "text_completion",
                "created": created,
                "model": model,
                "choices": [{ "text": "", "index": 0, "logprobs": Value::Null, "finish_reason": "stop" }],
            }),
        ];
        return sse_frames(&frames.iter().map(data_frame).collect::<Vec<_>>());
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "id": id,
            "object": "text_completion",
            "created": created,
            "model": model,
            "choices": [{
                "text": STUB_TEXT,
                "index": 0,
                "logprobs": Value::Null,
                "finish_reason": "stop",
            }],
            "usage": stub_usage(),
        })),
    )
        .into_response()
}

/// 响应接口（`/v1/responses`）占位回答：流式输出命名事件，与真实编码器同形。
fn responses_stub(model: &str, stream: bool) -> Response {
    if !stream {
        return (StatusCode::OK, Json(response_object(model, STUB_TEXT))).into_response();
    }
    let frames = [
        named_frame(
            "response.created",
            serde_json::json!({
                "type": "response.created",
                "response": response_object(model, ""),
            }),
        ),
        named_frame(
            "response.output_text.delta",
            serde_json::json!({
                "type": "response.output_text.delta",
                "delta": STUB_TEXT,
            }),
        ),
        named_frame(
            "response.completed",
            serde_json::json!({
                "type": "response.completed",
                "response": response_object(model, STUB_TEXT),
            }),
        ),
    ];
    sse_response(&frames)
}

/// Response 对象（与 `apis::openai::responses` 的非流式输出同形）。
fn response_object(model: &str, text: &str) -> Value {
    serde_json::json!({
        "id": format!("resp_{}", uuid::Uuid::new_v4().simple()),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": "completed",
        "model": model,
        "output": [{
            "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }],
        }],
        "usage": {
            "input_tokens": 0,
            "output_tokens": STUB_TEXT.chars().count() as u64,
            "total_tokens": STUB_TEXT.chars().count() as u64,
        },
    })
}

/// 用量对象：占位回答不做真实统计，输出量按正文长度算。
fn stub_usage() -> Value {
    let output = STUB_TEXT.chars().count() as u64;
    serde_json::json!({
        "prompt_tokens": 0,
        "completion_tokens": output,
        "total_tokens": output,
    })
}

/// 一行 SSE 数据帧。
fn data_frame(payload: &Value) -> String {
    format!("data: {}\n\n", payload)
}

/// 一条带事件名的 SSE 帧。
fn named_frame(event: &str, payload: Value) -> String {
    format!("event: {}\ndata: {}\n\n", event, payload)
}

/// 组装 SSE 响应（帧内容固定，无需流式生成）。
fn sse_response(frames: &[String]) -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        frames.concat(),
    )
        .into_response()
}

/// 组装 SSE 数据帧序列，末尾补 `[DONE]`（与两个非 Responses 格式的真实编码器一致）。
fn sse_frames(frames: &[String]) -> Response {
    let mut body: String = frames.concat();
    body.push_str("data: [DONE]\n\n");
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

/// 正文超过上限，无法落盘。
fn body_too_large() -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(serde_json::json!({
            "error": {
                "message": format!("请求体超过 {} 字节，无法落盘", MAX_BODY_BYTES),
                "type": "invalid_request_error"
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::{stub_response, DumpConfig, RequestDump};
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use serde_json::Value;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("poolman-dump-{}", uuid::Uuid::new_v4()))
    }

    /// 环境变量是进程级共享状态，涉及它的用例必须串行，否则互相污染。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 造一个指定路径与正文的快照。
    fn dump_for(path: &str, body: &str) -> RequestDump {
        let request = HttpRequest::builder()
            .method("POST")
            .uri(path)
            .body(Body::empty())
            .unwrap();
        let (parts, _) = request.into_parts();
        RequestDump::capture(&parts, body.as_bytes())
    }

    /// 取响应正文并按 JSON 解析。
    fn body_json(response: axum::response::Response) -> Value {
        serde_json::from_str(&body_text(response)).unwrap()
    }

    /// 取响应正文文本。
    fn body_text(response: axum::response::Response) -> String {
        let bytes =
            block_on(axum::body::to_bytes(response.into_body(), usize::MAX)).expect("读取响应正文");
        String::from_utf8(bytes.to_vec()).expect("响应正文应为 UTF-8")
    }

    /// 在同步测试里驱动一个已完成的 future。
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("建运行时")
            .block_on(future)
    }

    /// 快照保留方法与查询串；正文同时给出解析结果与原文。
    #[test]
    fn snapshot_keeps_all_request_parameters() {
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/v1/chat/completions?trace=1")
            .header("authorization", "Bearer sk-test")
            .header("content-type", "application/json")
            .body(Body::from(""))
            .unwrap();
        let (parts, _) = request.into_parts();

        let dump = RequestDump::capture(&parts, br#"{"model":"deepseek-web","stream":true}"#);
        assert_eq!(dump.method, "POST");
        assert_eq!(dump.path, "/v1/chat/completions");
        assert_eq!(dump.query.as_deref(), Some("trace=1"));
        assert_eq!(dump.uri, "/v1/chat/completions?trace=1");
        assert_eq!(dump.headers.get("authorization").unwrap(), "Bearer sk-test");
        assert_eq!(dump.body.as_ref().unwrap()["model"], "deepseek-web");
        assert_eq!(dump.body_raw, r#"{"model":"deepseek-web","stream":true}"#);

        let json = serde_json::to_value(&dump).unwrap();
        assert!(json["received_at"].is_string(), "应带到达时间");
        assert!(json["body"].is_object(), "应带解析后的正文");
        assert!(json["body_raw"].is_string(), "应带正文原文");
    }

    /// 真实载荷（多模态 + base64 图片）：解析结果与原文都在，字段顺序/空白也与原文一致。
    #[test]
    fn multimodal_payload_keeps_parsed_and_raw() {
        let raw = r#"{"model":"deepseek-web","max_tokens":16000,"stream":true,"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}},{"type":"text","text":"图片背景是什么颜色"}]}]}"#;
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .body(Body::from(""))
            .unwrap();
        let (parts, _) = request.into_parts();
        let dump = RequestDump::capture(&parts, raw.as_bytes());

        let body = dump.body.as_ref().expect("应能解析为 JSON");
        assert_eq!(body["messages"][0]["content"][0]["type"], "image_url");
        assert_eq!(
            body["messages"][0]["content"][1]["text"],
            "图片背景是什么颜色"
        );
        assert_eq!(dump.body_raw, raw, "原文应按到达时的字节原样保留");
    }

    /// 非 JSON 正文：解析结果为 null，原文照存。
    #[test]
    fn non_json_body_keeps_raw_only() {
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/v1/completions")
            .body(Body::empty())
            .unwrap();
        let (parts, _) = request.into_parts();
        let dump = RequestDump::capture(&parts, b"not json");
        assert!(dump.body.is_none());
        assert_eq!(dump.body_raw, "not json");
        assert!(dump.query.is_none());
    }

    /// 空正文（如 GET）：解析结果为 null，原文为空串，两个字段仍照常出现。
    #[test]
    fn empty_body_keeps_both_fields() {
        let request = HttpRequest::builder()
            .method("GET")
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap();
        let (parts, _) = request.into_parts();
        let dump = RequestDump::capture(&parts, b"");

        assert!(dump.body.is_none());
        assert_eq!(dump.body_raw, "");
        let json = serde_json::to_value(&dump).unwrap();
        assert!(json["body"].is_null());
        assert_eq!(json["body_raw"], "");
    }

    /// 响应阶段落盘：文件名前缀为 `response-`，内容与请求阶段互不影响。
    #[test]
    fn write_response_uses_response_prefix() {
        let dir = temp_dir();
        let config = DumpConfig::new(true, dir.to_string_lossy().to_string());
        let payload = serde_json::json!({ "stage": "response", "text": "你好" });

        let path = config.write_response(&payload).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("response-") && name.ends_with(".json"),
            "{}",
            name
        );
        let restored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(restored["stage"], "response");
        assert_eq!(restored["text"], "你好");

        // 请求阶段仍用 chat-request- 前缀，两者共存不冲突
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .body(Body::empty())
            .unwrap();
        let (parts, _) = request.into_parts();
        let request_path = config
            .write(&RequestDump::capture(&parts, br#"{"a":1}"#))
            .unwrap();
        assert!(request_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("chat-request-"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 响应阶段开关独立：未设置时跟随请求阶段，设置时可单独开关。
    #[test]
    fn response_flag_follows_request_flag_by_default() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = temp_dir().to_string_lossy().to_string();
        std::env::remove_var("AI_GATEWAY_RESPONSE_DUMP");

        std::env::set_var("AI_GATEWAY_DUMP_REQUESTS", "0");
        assert!(
            !DumpConfig::from_env(&dir).response_enabled(),
            "跟随请求阶段关闭"
        );

        std::env::set_var("AI_GATEWAY_DUMP_REQUESTS", "1");
        assert!(
            DumpConfig::from_env(&dir).response_enabled(),
            "跟随请求阶段启用"
        );

        std::env::set_var("AI_GATEWAY_RESPONSE_DUMP", "0");
        assert!(
            !DumpConfig::from_env(&dir).response_enabled(),
            "请求阶段开着也能单独关掉响应阶段"
        );

        std::env::remove_var("AI_GATEWAY_DUMP_REQUESTS");
        std::env::remove_var("AI_GATEWAY_RESPONSE_DUMP");
    }

    /// 关闭时不写任何文件：请求 / 响应 / 工具调用三条落盘路径都被拦下，连目录都不创建。
    #[test]
    fn disabled_dump_writes_nothing() {
        let dir = temp_dir();
        let config = DumpConfig::new(false, dir.to_string_lossy().to_string());
        assert!(!config.enabled());
        assert!(!config.response_enabled());

        let request = HttpRequest::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .body(Body::from("{}"))
            .unwrap();
        let (parts, _) = request.into_parts();
        let dump = RequestDump::capture(&parts, b"{}");

        assert!(config.write(&dump).is_err(), "请求阶段关闭时不应落盘");
        assert!(
            config
                .write_response(&serde_json::json!({ "text": "答" }))
                .is_err(),
            "响应阶段关闭时不应落盘"
        );
        assert!(
            config
                .write_tool_calls(&serde_json::json!({ "calls": [] }))
                .is_err(),
            "工具调用属响应阶段，关闭时不应落盘"
        );
        assert!(!dir.exists(), "关闭时连落盘目录都不该创建");
    }

    /// 落盘：一请求一文件，目录自动创建，内容为可解析的 JSON。
    #[test]
    fn write_creates_one_json_file_per_request() {
        let dir = temp_dir();
        let config = DumpConfig::new(true, dir.to_string_lossy().to_string());
        assert!(config.enabled());
        assert!(!dir.exists(), "目录应由落盘时创建");

        let request = HttpRequest::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .body(Body::from("{}"))
            .unwrap();
        let (parts, _) = request.into_parts();
        let dump = RequestDump::capture(&parts, b"{}");

        let first = config.write(&dump).unwrap();
        let second = config.write(&dump).unwrap();
        assert_ne!(first, second, "每个请求应落成不同文件");
        assert!(first.exists() && second.exists());
        let name = first.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("chat-request-") && name.ends_with(".json"));

        let restored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&first).unwrap()).unwrap();
        assert_eq!(restored["path"], "/v1/chat/completions");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 对话补全：非流式返回 `chat.completion`，正文为「开发中」、模型名原样回带。
    #[test]
    fn chat_stub_returns_normal_completion() {
        let dump = dump_for("/v1/chat/completions", r#"{"model":"deepseek-web"}"#);
        let response = stub_response(&dump);
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("json"));

        let json = body_json(response);
        assert_eq!(json["object"], "chat.completion");
        assert_eq!(json["model"], "deepseek-web");
        assert_eq!(json["choices"][0]["message"]["role"], "assistant");
        assert_eq!(json["choices"][0]["message"]["content"], "开发中");
        assert_eq!(json["choices"][0]["finish_reason"], "stop");
        assert!(json["usage"]["total_tokens"].is_number());
    }

    /// 对话补全流式：SSE 两条数据帧 + `[DONE]`，事件形状与真实编码器一致。
    #[test]
    fn chat_stub_streams_sse_frames() {
        let dump = dump_for(
            "/v1/chat/completions",
            r#"{"model":"deepseek-web","stream":true}"#,
        );
        let response = stub_response(&dump);
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("text/event-stream"));

        let text = body_text(response);
        let frames: Vec<&str> = text.split("\n\n").filter(|f| !f.is_empty()).collect();
        assert_eq!(frames.len(), 3, "内容帧 + 结束帧 + [DONE]：{}", text);

        let first: Value = serde_json::from_str(frames[0].strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(first["object"], "chat.completion.chunk");
        assert_eq!(first["choices"][0]["delta"]["content"], "开发中");
        assert!(first["choices"][0]["finish_reason"].is_null());

        let second: Value =
            serde_json::from_str(frames[1].strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(second["choices"][0]["finish_reason"], "stop");
        assert_eq!(frames[2], "data: [DONE]");
    }

    /// 文本补全：对象名是 `text_completion`，正文放在 `choices[0].text`。
    #[test]
    fn completion_stub_uses_text_completion_shape() {
        let plain = body_json(stub_response(&dump_for(
            "/v1/completions",
            r#"{"model":"m"}"#,
        )));
        assert_eq!(plain["object"], "text_completion");
        assert_eq!(plain["choices"][0]["text"], "开发中");
        assert_eq!(plain["choices"][0]["finish_reason"], "stop");

        let stream = body_text(stub_response(&dump_for(
            "/v1/completions",
            r#"{"model":"m","stream":true}"#,
        )));
        assert!(stream.contains(r#""object":"text_completion""#));
        assert!(
            stream.trim_end().ends_with("data: [DONE]"),
            "应正常收尾：{}",
            stream
        );
    }

    /// 响应接口：非流式是 `response` 对象，流式是命名事件（与真实编码器同形）。
    #[test]
    fn responses_stub_uses_response_events() {
        let plain = body_json(stub_response(&dump_for(
            "/v1/responses",
            r#"{"model":"m"}"#,
        )));
        assert_eq!(plain["object"], "response");
        assert_eq!(plain["status"], "completed");
        assert_eq!(plain["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(plain["output"][0]["content"][0]["text"], "开发中");

        let stream = body_text(stub_response(&dump_for(
            "/v1/responses",
            r#"{"model":"m","stream":true}"#,
        )));
        for event in [
            "event: response.created",
            "event: response.output_text.delta",
            "event: response.completed",
        ] {
            assert!(stream.contains(event), "应含事件 {}：{}", event, stream);
        }
        assert!(stream.contains(r#""delta":"开发中""#));
    }

    /// 未知路径按对话补全形状兜底；缺 model 时回落为 poolman。
    #[test]
    fn unknown_path_falls_back_and_model_defaults() {
        let json = body_json(stub_response(&dump_for("/v2/whatever", "{}")));
        assert_eq!(json["object"], "chat.completion");
        assert_eq!(json["model"], "poolman");
    }

    /// 开关解析：显式启用值启用、其余值关闭、未设置时默认关闭；目录可用环境变量覆盖。
    #[test]
    fn env_switch_and_dir_resolution() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let data_dir = temp_dir();
        let data_dir = data_dir.to_string_lossy().to_string();
        std::env::remove_var("AI_GATEWAY_DUMP_REQUESTS");
        std::env::remove_var("AI_GATEWAY_DUMP_DIR");

        for off in ["0", "false", "off", "no", "FALSE"] {
            std::env::set_var("AI_GATEWAY_DUMP_REQUESTS", off);
            assert!(!DumpConfig::from_env(&data_dir).enabled(), "{} 应关闭", off);
        }
        for on in ["1", "true", "yes", "on"] {
            std::env::set_var("AI_GATEWAY_DUMP_REQUESTS", on);
            assert!(DumpConfig::from_env(&data_dir).enabled(), "{} 应启用", on);
        }

        std::env::remove_var("AI_GATEWAY_DUMP_REQUESTS");
        let config = DumpConfig::from_env(&data_dir);
        assert!(!config.enabled(), "未设置时默认关闭");
        assert!(!config.response_enabled(), "响应阶段跟随关闭");
        assert_eq!(
            config.dir().to_string_lossy(),
            data_dir,
            "未设置目录时落到数据目录"
        );

        std::env::set_var("AI_GATEWAY_DUMP_DIR", "/tmp/poolman-dump");
        assert_eq!(
            DumpConfig::from_env(&data_dir).dir().to_string_lossy(),
            "/tmp/poolman-dump"
        );
        std::env::remove_var("AI_GATEWAY_DUMP_DIR");
    }
}
