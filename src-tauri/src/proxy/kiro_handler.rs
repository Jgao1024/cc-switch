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
use super::ProxyError;
use crate::provider::Provider;

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
) -> Result<axum::response::Response, ProxyError> {
    let model = request_model(body);
    let cw_body = transform_kiro::anthropic_to_cw_request(body);
    let resp = kiro_send(state, &provider, cw_body).await?;

    if is_stream {
        let sse = transform_kiro::create_anthropic_sse_stream_from_kiro(resp.bytes_stream(), model);
        return Ok((sse_response_headers(), axum::body::Body::from_stream(sse)).into_response());
    }

    let events = kiro_collect_events(resp).await?;
    let message = transform_kiro::cw_events_to_anthropic_message(&events, model.as_deref());
    Ok((StatusCode::OK, Json(message)).into_response())
}

/// Kiro 供应商：OpenAI Chat Completions (`/v1/chat/completions`) → CodeWhisperer。
pub async fn handle_kiro_openai_chat(
    state: &ProxyState,
    body: &Value,
    provider: Provider,
    is_stream: bool,
) -> Result<axum::response::Response, ProxyError> {
    let model = request_model(body);
    let cw_body = transform_kiro::openai_chat_to_cw_request(body);
    let resp = kiro_send(state, &provider, cw_body).await?;

    if is_stream {
        let sse = transform_kiro::create_openai_sse_stream_from_kiro(resp.bytes_stream(), model);
        return Ok((sse_response_headers(), axum::body::Body::from_stream(sse)).into_response());
    }

    let events = kiro_collect_events(resp).await?;
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
) -> Result<axum::response::Response, ProxyError> {
    let model = request_model(body);
    // Responses 请求 → Chat Completions 请求 → CW 请求
    let chat_body = transform_codex_chat::responses_to_chat_completions(body.clone())?;
    let cw_body = transform_kiro::openai_chat_to_cw_request(&chat_body);
    let resp = kiro_send(state, &provider, cw_body).await?;

    // CW 字节流 → Chat SSE → Responses SSE
    let chat_sse = transform_kiro::create_openai_sse_stream_from_kiro(resp.bytes_stream(), model);
    let responses_sse =
        create_responses_sse_stream_from_chat_with_context(chat_sse, codex_tool_context);
    Ok((
        sse_response_headers(),
        axum::body::Body::from_stream(responses_sse),
    )
        .into_response())
}
