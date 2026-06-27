//! Kiro (Amazon Q / CodeWhisperer) 专用代理 handler
//!
//! Kiro 供应商不走通用透传转发，而是使用专用的 CodeWhisperer 协议转换：
//! 请求 Anthropic / OpenAI → CodeWhisperer，响应 event-stream → SSE。
//!
//! 该模块从 `handlers.rs` 中抽离，使 Kiro 集成对既有 handler 文件的侵入
//! 仅剩三个一行 dispatch hook（见 `handlers.rs` 中的 `provider_is_kiro` 调用）。

use std::sync::Arc;

use axum::{
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::Value;

use super::providers::{
    kiro_auth::KiroAuthManager,
    streaming_codex_chat::create_responses_sse_stream_from_chat_with_context, transform_codex_chat,
    transform_kiro,
};
use super::server::ProxyState;
use super::usage::logger::UsageLogger;
use super::ProxyError;
use crate::provider::Provider;

/// kiro 请求日志所需的上下文（应用类型 / 会话 / 原始请求头与体）。
///
/// 由 `handlers.rs` 的 dispatch 钩子构造并传入，使 kiro 请求也能进入
/// 「使用统计」与「请求日志」（含原始与转接后请求体）。
pub struct KiroLogContext {
    pub app_type: &'static str,
    pub session_id: Option<String>,
    /// 脱敏后的原始请求头 JSON 字符串。
    pub request_headers: Option<String>,
    /// 原始客户端请求体 JSON 字符串。
    pub request_body: Option<String>,
}

impl KiroLogContext {
    /// 从原始请求头与请求体构造（自动脱敏敏感头、序列化 body）。
    pub fn new(
        app_type: &'static str,
        session_id: Option<String>,
        headers: &HeaderMap,
        body: &Value,
    ) -> Self {
        Self {
            app_type,
            session_id,
            request_headers: Some(redact_headers(headers)),
            request_body: serde_json::to_string(body).ok(),
        }
    }
}

/// 敏感请求头名单（脱敏为 "***"，避免把令牌写进日志库）。
fn is_sensitive_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "authorization"
        || n == "x-api-key"
        || n == "api-key"
        || n == "cookie"
        || n == "proxy-authorization"
        || n.starts_with("x-amz-")
}

/// 将请求头序列化为脱敏后的 JSON 对象字符串。
fn redact_headers(headers: &HeaderMap) -> String {
    let mut map = serde_json::Map::new();
    for (name, value) in headers.iter() {
        let key = name.as_str().to_string();
        let val = if is_sensitive_header(name.as_str()) {
            Value::String("***".to_string())
        } else {
            Value::String(value.to_str().unwrap_or("<non-utf8>").to_string())
        };
        map.insert(key, val);
    }
    Value::Object(map).to_string()
}

/// 估算请求输入 token（递归累计请求体内全部字符串文本，chars/4）。
fn estimate_input_tokens(body: &Value) -> u32 {
    transform_kiro::estimate_tokens_in_value(body).min(u32::MAX as u64) as u32
}

/// 异步记录一条 kiro 用量 + 请求明细日志（套餐：credits 计量，token 为估算值）。
#[allow(clippy::too_many_arguments)]
fn spawn_kiro_log(
    state: &ProxyState,
    provider_id: String,
    app_type: &'static str,
    model: String,
    input_tokens: u32,
    usage: transform_kiro::KiroUsage,
    latency_ms: u64,
    status_code: u16,
    session_id: Option<String>,
    is_streaming: bool,
    request_headers: Option<String>,
    request_body: Option<String>,
    upstream_request_body: Option<String>,
) {
    // 尊重「启用日志」开关
    if let Ok(config) = state.config.try_read() {
        if !config.enable_logging {
            return;
        }
    }
    let db = state.db.clone();
    let output_tokens = usage.output_tokens.min(u32::MAX as u64) as u32;
    let credits = usage.credits;
    tokio::spawn(async move {
        let logger = UsageLogger::new(&db);
        let request_id = format!("kiro_{}", uuid::Uuid::new_v4().simple());
        if let Err(e) = logger.log_kiro(
            request_id,
            provider_id,
            app_type.to_string(),
            model,
            input_tokens,
            output_tokens,
            credits,
            latency_ms,
            None,
            status_code,
            session_id,
            is_streaming,
            request_headers,
            request_body,
            upstream_request_body,
        ) {
            log::warn!("[USG-KIRO] 记录 kiro 用量失败: {e}");
        }
    });
}

/// 判断 provider 是否为 Kiro 类型（meta.provider_type == "kiro"）。
pub fn provider_is_kiro(provider: &Provider) -> bool {
    provider
        .meta
        .as_ref()
        .and_then(|m| m.provider_type.as_deref())
        == Some("kiro")
}

/// 从 Kiro provider 配置解析上游代理地址（如 clash `http://127.0.0.1:7897`）。
fn kiro_upstream_proxy(provider: &Provider) -> Option<String> {
    ["upstreamProxy", "proxyUrl", "upstream_proxy", "proxy_url"]
        .iter()
        .find_map(|key| {
            provider
                .settings_config
                .get(*key)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
}

/// 获取 Kiro 认证管理器并按 provider 配置设置上游代理。
/// 优先用 provider 配置中的代理；缺省时回退到 cc-switch 全局出站代理。
fn kiro_manager(
    state: &ProxyState,
    provider: &Provider,
) -> Result<Arc<KiroAuthManager>, ProxyError> {
    use tauri::Manager;
    let app_handle = state
        .app_handle
        .as_ref()
        .ok_or_else(|| ProxyError::Internal("Kiro 认证不可用（无 AppHandle）".to_string()))?;
    let manager = app_handle
        .state::<crate::commands::kiro::KiroAuthState>()
        .0
        .clone();
    let proxy =
        kiro_upstream_proxy(provider).or_else(|| state.db.get_global_proxy_url().ok().flatten());
    manager.set_proxy(proxy);
    Ok(manager)
}

/// 公共发送流程：取管理器 → 发送 CW 请求 → 校验状态，返回成功的上游响应。
async fn kiro_send(
    state: &ProxyState,
    provider: &Provider,
    cw_body: Value,
) -> Result<reqwest::Response, ProxyError> {
    let manager = kiro_manager(state, provider)?;
    let resp = manager
        .send_generate_assistant_response(cw_body)
        .await
        .map_err(|e| ProxyError::Internal(format!("Kiro 请求失败: {e}")))?;
    if !resp.status().is_success() {
        return Err(kiro_upstream_error(resp).await);
    }
    Ok(resp)
}

/// 提取请求体中的 model 字段。
fn request_model(body: &Value) -> Option<String> {
    body.get("model")
        .and_then(|m| m.as_str())
        .map(str::to_string)
}

/// 构造 text/event-stream 响应头。
fn sse_response_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
    headers
}

/// 将 Kiro 失败响应转为 ProxyError。
async fn kiro_upstream_error(resp: reqwest::Response) -> ProxyError {
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    ProxyError::UpstreamError {
        status,
        body: Some(text.chars().take(2000).collect()),
    }
}

/// 非流式：缓冲全部字节，解码为 CodeWhisperer 事件序列。
async fn kiro_collect_events(
    resp: reqwest::Response,
) -> Result<Vec<transform_kiro::DecodedEvent>, ProxyError> {
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ProxyError::Internal(format!("读取 Kiro 响应失败: {e}")))?;
    let mut decoder = transform_kiro::EventStreamDecoder::new();
    Ok(decoder.push(&bytes))
}

/// Kiro 供应商请求处理：Anthropic `/v1/messages` → CodeWhisperer，
/// 响应 event-stream → Anthropic SSE（或非流式聚合为 Anthropic JSON）。
pub async fn handle_kiro_messages(
    state: &ProxyState,
    body: &Value,
    provider: Provider,
    is_stream: bool,
    log_ctx: KiroLogContext,
) -> Result<axum::response::Response, ProxyError> {
    let start = std::time::Instant::now();
    let model = request_model(body);
    let model_str = model.clone().unwrap_or_else(|| "unknown".to_string());
    let input_tokens = estimate_input_tokens(body);
    let cw_body = transform_kiro::anthropic_to_cw_request(body);
    let upstream_body = serde_json::to_string(&cw_body).ok();
    let resp = kiro_send(state, &provider, cw_body).await?;

    if is_stream {
        let cb = build_kiro_log_callback(
            state,
            &provider,
            &log_ctx,
            model_str,
            input_tokens,
            start,
            upstream_body,
        );
        let sse =
            transform_kiro::create_anthropic_sse_stream_from_kiro(resp.bytes_stream(), model, cb);
        return Ok((sse_response_headers(), axum::body::Body::from_stream(sse)).into_response());
    }

    let events = kiro_collect_events(resp).await?;
    let usage = transform_kiro::extract_kiro_usage(&events);
    spawn_kiro_log(
        state,
        provider.id.clone(),
        log_ctx.app_type,
        model_str,
        input_tokens,
        usage,
        start.elapsed().as_millis() as u64,
        StatusCode::OK.as_u16(),
        log_ctx.session_id.clone(),
        false,
        log_ctx.request_headers.clone(),
        log_ctx.request_body.clone(),
        upstream_body,
    );
    let message = transform_kiro::cw_events_to_anthropic_message(&events, model.as_deref());
    Ok((StatusCode::OK, Json(message)).into_response())
}

/// Kiro 供应商：OpenAI Chat Completions (`/v1/chat/completions`) → CodeWhisperer。
pub async fn handle_kiro_openai_chat(
    state: &ProxyState,
    body: &Value,
    provider: Provider,
    is_stream: bool,
    log_ctx: KiroLogContext,
) -> Result<axum::response::Response, ProxyError> {
    let start = std::time::Instant::now();
    let model = request_model(body);
    let model_str = model.clone().unwrap_or_else(|| "unknown".to_string());
    let input_tokens = estimate_input_tokens(body);
    let cw_body = transform_kiro::openai_chat_to_cw_request(body);
    let upstream_body = serde_json::to_string(&cw_body).ok();
    let resp = kiro_send(state, &provider, cw_body).await?;

    if is_stream {
        let cb = build_kiro_log_callback(
            state,
            &provider,
            &log_ctx,
            model_str,
            input_tokens,
            start,
            upstream_body,
        );
        let sse =
            transform_kiro::create_openai_sse_stream_from_kiro(resp.bytes_stream(), model, cb);
        return Ok((sse_response_headers(), axum::body::Body::from_stream(sse)).into_response());
    }

    let events = kiro_collect_events(resp).await?;
    let usage = transform_kiro::extract_kiro_usage(&events);
    spawn_kiro_log(
        state,
        provider.id.clone(),
        log_ctx.app_type,
        model_str,
        input_tokens,
        usage,
        start.elapsed().as_millis() as u64,
        StatusCode::OK.as_u16(),
        log_ctx.session_id.clone(),
        false,
        log_ctx.request_headers.clone(),
        log_ctx.request_body.clone(),
        upstream_body,
    );
    let message = transform_kiro::cw_events_to_openai_message(&events, model.as_deref());
    Ok((StatusCode::OK, Json(message)).into_response())
}

/// Kiro 供应商：OpenAI Responses API (`/v1/responses`, Codex CLI) → CodeWhisperer。
///
/// 复用既有 Responses↔Chat 转换：Responses 请求→Chat 请求→CW；
/// CW 响应→Chat SSE→Responses SSE。Codex CLI 始终流式。
pub async fn handle_kiro_responses(
    state: &ProxyState,
    body: &Value,
    provider: Provider,
    codex_tool_context: transform_codex_chat::CodexToolContext,
    log_ctx: KiroLogContext,
) -> Result<axum::response::Response, ProxyError> {
    let start = std::time::Instant::now();
    let model = request_model(body);
    let model_str = model.clone().unwrap_or_else(|| "unknown".to_string());
    let input_tokens = estimate_input_tokens(body);
    // Responses 请求 → Chat Completions 请求 → CW 请求
    let chat_body = transform_codex_chat::responses_to_chat_completions(body.clone())?;
    let cw_body = transform_kiro::openai_chat_to_cw_request(&chat_body);
    let upstream_body = serde_json::to_string(&cw_body).ok();
    let resp = kiro_send(state, &provider, cw_body).await?;

    // CW 字节流 → Chat SSE → Responses SSE
    let cb = build_kiro_log_callback(
        state,
        &provider,
        &log_ctx,
        model_str,
        input_tokens,
        start,
        upstream_body,
    );
    let chat_sse =
        transform_kiro::create_openai_sse_stream_from_kiro(resp.bytes_stream(), model, cb);
    let responses_sse =
        create_responses_sse_stream_from_chat_with_context(chat_sse, codex_tool_context);
    Ok((
        sse_response_headers(),
        axum::body::Body::from_stream(responses_sse),
    )
        .into_response())
}

/// 构造流式完成回调：流结束（含客户端断开）时按累计用量落库一条 kiro 日志。
fn build_kiro_log_callback(
    state: &ProxyState,
    provider: &Provider,
    log_ctx: &KiroLogContext,
    model: String,
    input_tokens: u32,
    start: std::time::Instant,
    upstream_body: Option<String>,
) -> Box<dyn FnOnce(transform_kiro::KiroUsage) + Send> {
    let state = state.clone();
    let provider_id = provider.id.clone();
    let app_type = log_ctx.app_type;
    let session_id = log_ctx.session_id.clone();
    let request_headers = log_ctx.request_headers.clone();
    let request_body = log_ctx.request_body.clone();
    Box::new(move |usage| {
        spawn_kiro_log(
            &state,
            provider_id,
            app_type,
            model,
            input_tokens,
            usage,
            start.elapsed().as_millis() as u64,
            StatusCode::OK.as_u16(),
            session_id,
            true,
            request_headers,
            request_body,
            upstream_body,
        );
    })
}
