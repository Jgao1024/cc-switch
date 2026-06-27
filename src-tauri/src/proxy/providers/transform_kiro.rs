//! Kiro (CodeWhisperer) 格式转换
//!
//! 双向转换：
//! - 请求：Anthropic Messages API → CodeWhisperer `GenerateAssistantResponse`
//! - 响应：CodeWhisperer `application/vnd.amazon.eventstream` → Anthropic SSE
//!
//! 同时实现 AWS event-stream 二进制帧解码器（增量式，供流式响应使用）。
//! 协议细节与实测验证见 `docs/kiro-cli-auth-and-api.md`。

use serde_json::{json, Value};

use super::kiro_auth::protocol;

// ===================================================================
// Part A: AWS event-stream 二进制解码器
// ===================================================================

/// 解码出的单个事件
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedEvent {
    /// `:event-type` 头（如 assistantResponseEvent / toolUseEvent / messageMetadataEvent）
    pub event_type: String,
    /// `:message-type` 头（event / exception 等）
    pub message_type: Option<String>,
    /// payload 原始字节（通常为 JSON）
    pub payload: Vec<u8>,
}

impl DecodedEvent {
    /// 将 payload 解析为 JSON
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.payload).ok()
    }
}

/// 增量式 event-stream 解码器。
///
/// 帧格式（大端）：
/// `[total_len:4][headers_len:4][prelude_crc:4][headers][payload][msg_crc:4]`
#[derive(Default)]
pub struct EventStreamDecoder {
    buf: Vec<u8>,
}

impl EventStreamDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// 追加新到达的字节，返回所有此刻能完整解析出的事件。
    pub fn push(&mut self, bytes: &[u8]) -> Vec<DecodedEvent> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        loop {
            if self.buf.len() < 12 {
                break;
            }
            let total = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])
                as usize;
            if total < 16 || total > 64 * 1024 * 1024 {
                // 异常长度，丢弃缓冲避免死循环
                self.buf.clear();
                break;
            }
            if self.buf.len() < total {
                break;
            }
            let headers_len =
                u32::from_be_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]) as usize;
            let frame = self.buf[..total].to_vec();
            self.buf.drain(..total);
            if let Some(ev) = decode_frame(&frame, headers_len) {
                events.push(ev);
            }
        }
        events
    }
}

fn decode_frame(frame: &[u8], headers_len: usize) -> Option<DecodedEvent> {
    let headers_start = 12usize;
    let headers_end = headers_start.checked_add(headers_len)?;
    if headers_end + 4 > frame.len() {
        return None;
    }
    let headers = &frame[headers_start..headers_end];
    let payload = frame[headers_end..frame.len() - 4].to_vec();

    let mut event_type = String::new();
    let mut message_type = None;
    let mut o = 0usize;
    while o < headers.len() {
        let name_len = headers[o] as usize;
        o += 1;
        if o + name_len > headers.len() {
            break;
        }
        let name = String::from_utf8_lossy(&headers[o..o + name_len]).to_string();
        o += name_len;
        if o >= headers.len() {
            break;
        }
        let vtype = headers[o];
        o += 1;
        // 按类型推进，并在 string 类型时取值
        let mut str_val: Option<String> = None;
        match vtype {
            0 | 1 => {}        // bool true/false: 0 字节
            2 => o += 1,        // byte
            3 => o += 2,        // short
            4 => o += 4,        // int
            5 => o += 8,        // long
            6 | 7 => {
                // bytes / string: [u16 len][data]
                if o + 2 > headers.len() {
                    break;
                }
                let len = u16::from_be_bytes([headers[o], headers[o + 1]]) as usize;
                o += 2;
                if o + len > headers.len() {
                    break;
                }
                if vtype == 7 {
                    str_val = Some(String::from_utf8_lossy(&headers[o..o + len]).to_string());
                }
                o += len;
            }
            8 => o += 8,        // timestamp
            9 => o += 16,       // uuid
            _ => break,
        }
        match name.as_str() {
            ":event-type" => {
                if let Some(v) = str_val {
                    event_type = v;
                }
            }
            ":message-type" => {
                message_type = str_val;
            }
            ":exception-type" => {
                if let Some(v) = str_val {
                    event_type = v;
                    message_type = Some("exception".to_string());
                }
            }
            _ => {}
        }
    }
    Some(DecodedEvent {
        event_type,
        message_type,
        payload,
    })
}

// ===================================================================
// Part B: 请求转换 Anthropic Messages → CodeWhisperer
// ===================================================================

/// 将 Anthropic `/v1/messages` 请求体转换为 CodeWhisperer
/// `GenerateAssistantResponse` 请求体（不含 profileArn，由调用方注入）。
pub fn anthropic_to_cw_request(body: &Value) -> Value {
    let system_text = extract_system_text(body);
    let tools = body.get("tools").and_then(|t| t.as_array());
    let cw_tools = tools.map(|arr| {
        arr.iter()
            .filter_map(anthropic_tool_to_cw)
            .collect::<Vec<_>>()
    });

    let empty = vec![];
    let messages = body
        .get("messages")
        .and_then(|m| m.as_array())
        .unwrap_or(&empty);

    // 构建 CW history（除最后一条 user 外的所有消息）+ currentMessage（最后一条 user）
    let mut cw_msgs: Vec<Value> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        // 仅在第一条 user 消息前注入 system 文本
        let inject_system = i == 0 && role == "user";
        let sys = if inject_system { system_text.as_deref() } else { None };
        match role {
            "assistant" => cw_msgs.push(json!({
                "assistantResponseMessage": anthropic_assistant_to_cw(msg)
            })),
            _ => cw_msgs.push(json!({
                "userInputMessage": anthropic_user_to_cw(msg, sys, cw_tools.as_ref())
            })),
        }
    }

    // currentMessage 必须是 userInputMessage；取末尾的 user 消息
    let current = if matches!(
        cw_msgs.last().and_then(|m| m.get("userInputMessage")),
        Some(_)
    ) {
        cw_msgs.pop().unwrap()
    } else {
        // 末尾不是 user（异常情况）：补一个空的 user 触发续写
        json!({ "userInputMessage": { "content": "", "origin": "CLI" } })
    };

    let mut conversation_state = json!({
        "chatTriggerType": "MANUAL",
        "currentMessage": current,
    });
    if !cw_msgs.is_empty() {
        conversation_state["history"] = json!(cw_msgs);
    }

    json!({ "conversationState": conversation_state })
}

/// 提取 system 文本（支持字符串或 block 数组）
fn extract_system_text(body: &Value) -> Option<String> {
    match body.get("system") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Array(arr)) => {
            let joined = arr
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            if joined.is_empty() {
                None
            } else {
                Some(joined)
            }
        }
        _ => None,
    }
}

/// Anthropic tool 定义 → CW Tool
fn anthropic_tool_to_cw(tool: &Value) -> Option<Value> {
    let name = tool.get("name").and_then(|n| n.as_str())?;
    let schema = tool
        .get("input_schema")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object"}));
    let mut spec = json!({
        "name": name,
        "inputSchema": { "json": schema },
    });
    if let Some(desc) = tool.get("description").and_then(|d| d.as_str()) {
        spec["description"] = json!(desc);
    }
    Some(json!({ "toolSpecification": spec }))
}

/// Anthropic user 消息 → CW userInputMessage
fn anthropic_user_to_cw(msg: &Value, system: Option<&str>, tools: Option<&Vec<Value>>) -> Value {
    let mut text_parts: Vec<String> = Vec::new();
    if let Some(s) = system {
        text_parts.push(s.to_string());
    }
    let mut tool_results: Vec<Value> = Vec::new();

    match msg.get("content") {
        Some(Value::String(s)) => text_parts.push(s.clone()),
        Some(Value::Array(blocks)) => {
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            text_parts.push(t.to_string());
                        }
                    }
                    Some("tool_result") => {
                        tool_results.push(anthropic_tool_result_to_cw(b));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    let mut ctx = json!({});
    let mut has_ctx = false;
    if let Some(t) = tools {
        if !t.is_empty() {
            ctx["tools"] = json!(t);
            has_ctx = true;
        }
    }
    if !tool_results.is_empty() {
        ctx["toolResults"] = json!(tool_results);
        has_ctx = true;
    }

    let mut out = json!({
        "content": text_parts.join("\n"),
        "origin": "CLI",
    });
    if has_ctx {
        out["userInputMessageContext"] = ctx;
    }
    out
}

/// Anthropic tool_result block → CW ToolResult
fn anthropic_tool_result_to_cw(block: &Value) -> Value {
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(|i| i.as_str())
        .unwrap_or("");
    let is_error = block
        .get("is_error")
        .and_then(|e| e.as_bool())
        .unwrap_or(false);
    let content = match block.get("content") {
        Some(Value::String(s)) => json!([{ "text": s }]),
        Some(Value::Array(arr)) => {
            let mut out = Vec::new();
            for c in arr {
                match c.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = c.get("text").and_then(|t| t.as_str()) {
                            out.push(json!({ "text": t }));
                        }
                    }
                    _ => {
                        // 其它结构作为 json 传递
                        out.push(json!({ "json": c }));
                    }
                }
            }
            if out.is_empty() {
                json!([{ "text": "" }])
            } else {
                json!(out)
            }
        }
        Some(other) => json!([{ "json": other }]),
        None => json!([{ "text": "" }]),
    };
    json!({
        "toolUseId": tool_use_id,
        "content": content,
        "status": if is_error { "error" } else { "success" },
    })
}

/// Anthropic assistant 消息 → CW assistantResponseMessage
fn anthropic_assistant_to_cw(msg: &Value) -> Value {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_uses: Vec<Value> = Vec::new();

    match msg.get("content") {
        Some(Value::String(s)) => text_parts.push(s.clone()),
        Some(Value::Array(blocks)) => {
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            text_parts.push(t.to_string());
                        }
                    }
                    Some("tool_use") => {
                        tool_uses.push(json!({
                            "toolUseId": b.get("id").and_then(|i| i.as_str()).unwrap_or(""),
                            "name": b.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                            "input": b.get("input").cloned().unwrap_or_else(|| json!({})),
                        }));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    let mut out = json!({ "content": text_parts.join("\n") });
    if !tool_uses.is_empty() {
        out["toolUses"] = json!(tool_uses);
    }
    out
}

// ===================================================================
// Part C: 响应转换 CodeWhisperer events → Anthropic SSE
// ===================================================================

/// 当前打开的内容块类型
#[derive(Debug, PartialEq)]
enum OpenBlock {
    None,
    Text,
    ToolUse,
}

/// 有状态转换器：吃 CW 事件，吐 Anthropic SSE 文本块。
pub struct KiroToAnthropic {
    message_id: String,
    model: String,
    started: bool,
    block_index: i64,
    open: OpenBlock,
    used_tool: bool,
    finished: bool,
}

impl KiroToAnthropic {
    pub fn new(model: Option<&str>) -> Self {
        Self {
            message_id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
            model: model.unwrap_or(protocol::DEFAULT_MODEL_ID).to_string(),
            started: false,
            block_index: -1,
            open: OpenBlock::None,
            used_tool: false,
            finished: false,
        }
    }

    fn sse(event: &str, data: &Value) -> String {
        format!("event: {}\ndata: {}\n\n", event, data)
    }

    fn ensure_started(&mut self, out: &mut String) {
        if self.started {
            return;
        }
        self.started = true;
        let msg = json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "model": self.model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        });
        out.push_str(&Self::sse("message_start", &msg));
    }

    fn close_open_block(&mut self, out: &mut String) {
        if self.open != OpenBlock::None {
            out.push_str(&Self::sse(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": self.block_index}),
            ));
            self.open = OpenBlock::None;
        }
    }

    /// 处理一个 CW 事件，返回要写给客户端的 SSE 文本（可能为空）。
    pub fn handle(&mut self, ev: &DecodedEvent) -> String {
        let mut out = String::new();
        let json = ev.json();
        match ev.event_type.as_str() {
            "assistantResponseEvent" => {
                let Some(j) = json else { return out };
                let content = j.get("content").and_then(|c| c.as_str()).unwrap_or("");
                if content.is_empty() {
                    return out;
                }
                self.ensure_started(&mut out);
                if let Some(m) = j.get("modelId").and_then(|m| m.as_str()) {
                    self.model = m.to_string();
                }
                if self.open != OpenBlock::Text {
                    self.close_open_block(&mut out);
                    self.block_index += 1;
                    out.push_str(&Self::sse(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": self.block_index,
                            "content_block": {"type": "text", "text": ""}
                        }),
                    ));
                    self.open = OpenBlock::Text;
                }
                out.push_str(&Self::sse(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": self.block_index,
                        "delta": {"type": "text_delta", "text": content}
                    }),
                ));
            }
            "toolUseEvent" => {
                let Some(j) = json else { return out };
                self.ensure_started(&mut out);
                let stop = j.get("stop").and_then(|s| s.as_bool()).unwrap_or(false);
                // 新工具开始：有 name 且当前未打开 toolUse 块
                let name = j.get("name").and_then(|n| n.as_str());
                let tool_use_id = j.get("toolUseId").and_then(|i| i.as_str());
                if self.open != OpenBlock::ToolUse && name.is_some() && !stop {
                    self.close_open_block(&mut out);
                    self.block_index += 1;
                    self.used_tool = true;
                    out.push_str(&Self::sse(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": self.block_index,
                            "content_block": {
                                "type": "tool_use",
                                "id": tool_use_id.unwrap_or(""),
                                "name": name.unwrap_or(""),
                                "input": {}
                            }
                        }),
                    ));
                    self.open = OpenBlock::ToolUse;
                }
                // input 分片 → input_json_delta
                if let Some(input) = j.get("input").and_then(|i| i.as_str()) {
                    if !input.is_empty() {
                        out.push_str(&Self::sse(
                            "content_block_delta",
                            &json!({
                                "type": "content_block_delta",
                                "index": self.block_index,
                                "delta": {"type": "input_json_delta", "partial_json": input}
                            }),
                        ));
                    }
                }
                if stop {
                    self.close_open_block(&mut out);
                }
            }
            _ => {
                // messageMetadataEvent / initial-response 等：暂不映射到 SSE
            }
        }
        out
    }

    /// 流结束：补齐收尾事件（content_block_stop / message_delta / message_stop）。
    pub fn finish(&mut self) -> String {
        if self.finished {
            return String::new();
        }
        self.finished = true;
        let mut out = String::new();
        self.ensure_started(&mut out);
        self.close_open_block(&mut out);
        let stop_reason = if self.used_tool { "tool_use" } else { "end_turn" };
        out.push_str(&Self::sse(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": {"output_tokens": 0}
            }),
        ));
        out.push_str(&Self::sse("message_stop", &json!({"type": "message_stop"})));
        out
    }
}


/// 将一组 CW 事件聚合为单个 Anthropic Messages 响应 JSON（用于非流式客户端）。
pub fn cw_events_to_anthropic_message(events: &[DecodedEvent], model: Option<&str>) -> Value {
    let mut text = String::new();
    let mut resolved_model = model.unwrap_or(protocol::DEFAULT_MODEL_ID).to_string();
    // 按出现顺序累积工具调用
    let mut tool_order: Vec<String> = Vec::new();
    let mut tool_name: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut tool_input: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut used_tool = false;

    for ev in events {
        let Some(j) = ev.json() else { continue };
        match ev.event_type.as_str() {
            "assistantResponseEvent" => {
                if let Some(c) = j.get("content").and_then(|c| c.as_str()) {
                    text.push_str(c);
                }
                if let Some(m) = j.get("modelId").and_then(|m| m.as_str()) {
                    resolved_model = m.to_string();
                }
            }
            "toolUseEvent" => {
                used_tool = true;
                let id = j
                    .get("toolUseId")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                if id.is_empty() {
                    continue;
                }
                if !tool_order.contains(&id) {
                    tool_order.push(id.clone());
                }
                if let Some(n) = j.get("name").and_then(|n| n.as_str()) {
                    tool_name.entry(id.clone()).or_insert_with(|| n.to_string());
                }
                if let Some(inp) = j.get("input").and_then(|i| i.as_str()) {
                    tool_input.entry(id.clone()).or_default().push_str(inp);
                }
            }
            _ => {}
        }
    }

    let mut content: Vec<Value> = Vec::new();
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    for id in &tool_order {
        let input_str = tool_input.get(id).cloned().unwrap_or_default();
        let input_val: Value = if input_str.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&input_str).unwrap_or_else(|_| json!({}))
        };
        content.push(json!({
            "type": "tool_use",
            "id": id,
            "name": tool_name.get(id).cloned().unwrap_or_default(),
            "input": input_val,
        }));
    }

    json!({
        "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
        "type": "message",
        "role": "assistant",
        "model": resolved_model,
        "content": content,
        "stop_reason": if used_tool { "tool_use" } else { "end_turn" },
        "stop_sequence": null,
        "usage": {"input_tokens": 0, "output_tokens": 0}
    })
}

// ===================================================================
// Part D: 流式适配 —— reqwest 字节流 → Anthropic SSE 字节流
// ===================================================================

use bytes::Bytes;
use futures::Stream;

/// 将上游 CodeWhisperer event-stream 字节流转换为 Anthropic SSE 字节流。
///
/// 入参为 `reqwest::Response::bytes_stream()`，出参可直接喂给
/// `create_logged_passthrough_stream` / `axum::body::Body::from_stream`。
pub fn create_anthropic_sse_stream_from_kiro(
    upstream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    model: Option<String>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        let mut decoder = EventStreamDecoder::new();
        let mut conv = KiroToAnthropic::new(model.as_deref());
        futures::pin_mut!(upstream);
        use futures::StreamExt;
        while let Some(item) = upstream.next().await {
            match item {
                Ok(bytes) => {
                    for ev in decoder.push(&bytes) {
                        // 异常事件：转成 Anthropic error 事件后结束
                        if ev.message_type.as_deref() == Some("exception") {
                            let msg = ev
                                .json()
                                .and_then(|j| j.get("message").and_then(|m| m.as_str()).map(String::from))
                                .unwrap_or_else(|| ev.event_type.clone());
                            let err = format!(
                                "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":{}}}}}\n\n",
                                serde_json::Value::String(msg)
                            );
                            yield Ok(Bytes::from(err));
                            return;
                        }
                        let sse = conv.handle(&ev);
                        if !sse.is_empty() {
                            yield Ok(Bytes::from(sse));
                        }
                    }
                }
                Err(e) => {
                    yield Err(std::io::Error::new(std::io::ErrorKind::Other, e.to_string()));
                    return;
                }
            }
        }
        let fin = conv.finish();
        if !fin.is_empty() {
            yield Ok(Bytes::from(fin));
        }
    }
}

// ===================================================================
// Part E: OpenAI Chat Completions ↔ CodeWhisperer
// ===================================================================

/// OpenAI Chat Completions 请求体 → CodeWhisperer 请求体（不含 profileArn）。
pub fn openai_chat_to_cw_request(body: &Value) -> Value {
    let tools = body.get("tools").and_then(|t| t.as_array());
    let cw_tools = tools.map(|arr| {
        arr.iter()
            .filter_map(openai_tool_to_cw)
            .collect::<Vec<Value>>()
    });

    let empty = vec![];
    let messages = body
        .get("messages")
        .and_then(|m| m.as_array())
        .unwrap_or(&empty);

    // 先提取 system 文本
    let system_text: Option<String> = messages
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))
        .and_then(|m| openai_content_to_text(m.get("content")))
        .filter(|s| !s.is_empty());

    let mut cw_msgs: Vec<Value> = Vec::new();
    // 待并入下一条 userInputMessage 的 toolResults（来自 role:tool 消息）
    let mut pending_tool_results: Vec<Value> = Vec::new();
    let mut system_injected = false;

    let flush_pending =
        |cw_msgs: &mut Vec<Value>, pending: &mut Vec<Value>| {
            if !pending.is_empty() {
                cw_msgs.push(json!({
                    "userInputMessage": {
                        "content": "",
                        "origin": "CLI",
                        "userInputMessageContext": { "toolResults": std::mem::take(pending) }
                    }
                }));
            }
        };

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        match role {
            "system" => { /* 已并入首条 user */ }
            "tool" => {
                pending_tool_results.push(openai_tool_message_to_cw(msg));
            }
            "assistant" => {
                flush_pending(&mut cw_msgs, &mut pending_tool_results);
                cw_msgs.push(json!({
                    "assistantResponseMessage": openai_assistant_to_cw(msg)
                }));
            }
            _ => {
                // user
                flush_pending(&mut cw_msgs, &mut pending_tool_results);
                let mut text = openai_content_to_text(msg.get("content")).unwrap_or_default();
                if !system_injected {
                    if let Some(sys) = system_text.as_deref() {
                        text = if text.is_empty() {
                            sys.to_string()
                        } else {
                            format!("{sys}\n{text}")
                        };
                    }
                    system_injected = true;
                }
                cw_msgs.push(json!({
                    "userInputMessage": { "content": text, "origin": "CLI" }
                }));
            }
        }
    }
    // 末尾剩余 tool 结果 → 独立 user 消息
    flush_pending(&mut cw_msgs, &mut pending_tool_results);

    // currentMessage 取末尾 user 消息（不是 user 则补空）
    let mut current = if cw_msgs
        .last()
        .and_then(|m| m.get("userInputMessage"))
        .is_some()
    {
        cw_msgs.pop().unwrap()
    } else {
        json!({ "userInputMessage": { "content": "", "origin": "CLI" } })
    };

    // 工具定义挂到 currentMessage 上
    if let Some(t) = cw_tools.as_ref() {
        if !t.is_empty() {
            let uim = current
                .get_mut("userInputMessage")
                .and_then(|v| v.as_object_mut())
                .unwrap();
            let ctx = uim
                .entry("userInputMessageContext")
                .or_insert_with(|| json!({}));
            ctx["tools"] = json!(t);
        }
    }

    let mut conversation_state = json!({
        "chatTriggerType": "MANUAL",
        "currentMessage": current,
    });
    if !cw_msgs.is_empty() {
        conversation_state["history"] = json!(cw_msgs);
    }
    json!({ "conversationState": conversation_state })
}

/// OpenAI content（字符串或数组）→ 纯文本
fn openai_content_to_text(content: Option<&Value>) -> Option<String> {
    match content {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(arr)) => {
            let joined = arr
                .iter()
                .filter_map(|b| {
                    // {type:"text", text:".."} 或 {type:"input_text", text:".."}
                    b.get("text").and_then(|t| t.as_str())
                })
                .collect::<Vec<_>>()
                .join("");
            Some(joined)
        }
        _ => None,
    }
}

/// OpenAI tool 定义 → CW Tool
fn openai_tool_to_cw(tool: &Value) -> Option<Value> {
    let func = tool.get("function").unwrap_or(tool);
    let name = func.get("name").and_then(|n| n.as_str())?;
    let schema = func
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object"}));
    let mut spec = json!({ "name": name, "inputSchema": { "json": schema } });
    if let Some(d) = func.get("description").and_then(|d| d.as_str()) {
        spec["description"] = json!(d);
    }
    Some(json!({ "toolSpecification": spec }))
}

/// OpenAI assistant 消息 → CW assistantResponseMessage
fn openai_assistant_to_cw(msg: &Value) -> Value {
    let content = openai_content_to_text(msg.get("content")).unwrap_or_default();
    let mut out = json!({ "content": content });
    if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        let tool_uses: Vec<Value> = tool_calls
            .iter()
            .filter_map(|tc| {
                let func = tc.get("function")?;
                let name = func.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = func
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .unwrap_or("{}");
                let input: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
                Some(json!({
                    "toolUseId": tc.get("id").and_then(|i| i.as_str()).unwrap_or(""),
                    "name": name,
                    "input": input,
                }))
            })
            .collect();
        if !tool_uses.is_empty() {
            out["toolUses"] = json!(tool_uses);
        }
    }
    out
}

/// OpenAI role:tool 消息 → CW ToolResult
fn openai_tool_message_to_cw(msg: &Value) -> Value {
    let tool_use_id = msg
        .get("tool_call_id")
        .and_then(|i| i.as_str())
        .unwrap_or("");
    let text = openai_content_to_text(msg.get("content")).unwrap_or_default();
    json!({
        "toolUseId": tool_use_id,
        "content": [{ "text": text }],
        "status": "success",
    })
}

/// 有状态转换器：CW 事件 → OpenAI Chat Completions chunk SSE。
pub struct KiroToOpenAIChat {
    id: String,
    model: String,
    created: i64,
    role_sent: bool,
    tool_index: i64,
    open_tool: bool,
    used_tool: bool,
    finished: bool,
}

impl KiroToOpenAIChat {
    pub fn new(model: Option<&str>) -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            model: model.unwrap_or(protocol::DEFAULT_MODEL_ID).to_string(),
            created: chrono::Utc::now().timestamp(),
            role_sent: false,
            tool_index: -1,
            open_tool: false,
            used_tool: false,
            finished: false,
        }
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> String {
        let c = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }]
        });
        format!("data: {c}\n\n")
    }

    pub fn handle(&mut self, ev: &DecodedEvent) -> String {
        let mut out = String::new();
        let Some(j) = ev.json() else { return out };
        match ev.event_type.as_str() {
            "assistantResponseEvent" => {
                let content = j.get("content").and_then(|c| c.as_str()).unwrap_or("");
                if content.is_empty() {
                    return out;
                }
                if let Some(m) = j.get("modelId").and_then(|m| m.as_str()) {
                    self.model = m.to_string();
                }
                let delta = if !self.role_sent {
                    self.role_sent = true;
                    json!({"role": "assistant", "content": content})
                } else {
                    json!({"content": content})
                };
                out.push_str(&self.chunk(delta, None));
            }
            "toolUseEvent" => {
                let stop = j.get("stop").and_then(|s| s.as_bool()).unwrap_or(false);
                let name = j.get("name").and_then(|n| n.as_str());
                let id = j.get("toolUseId").and_then(|i| i.as_str());
                if !self.open_tool && name.is_some() && !stop {
                    self.tool_index += 1;
                    self.open_tool = true;
                    self.used_tool = true;
                    let mut delta = json!({
                        "tool_calls": [{
                            "index": self.tool_index,
                            "id": id.unwrap_or(""),
                            "type": "function",
                            "function": {"name": name.unwrap_or(""), "arguments": ""}
                        }]
                    });
                    if !self.role_sent {
                        self.role_sent = true;
                        delta["role"] = json!("assistant");
                    }
                    out.push_str(&self.chunk(delta, None));
                }
                if let Some(input) = j.get("input").and_then(|i| i.as_str()) {
                    if !input.is_empty() {
                        out.push_str(&self.chunk(
                            json!({
                                "tool_calls": [{
                                    "index": self.tool_index,
                                    "function": {"arguments": input}
                                }]
                            }),
                            None,
                        ));
                    }
                }
                if stop {
                    self.open_tool = false;
                }
            }
            _ => {}
        }
        out
    }

    pub fn finish(&mut self) -> String {
        if self.finished {
            return String::new();
        }
        self.finished = true;
        let mut out = String::new();
        if !self.role_sent {
            // 无任何内容时也要给出合法的首块
            out.push_str(&self.chunk(json!({"role": "assistant", "content": ""}), None));
            self.role_sent = true;
        }
        let reason = if self.used_tool { "tool_calls" } else { "stop" };
        out.push_str(&self.chunk(json!({}), Some(reason)));
        out.push_str("data: [DONE]\n\n");
        out
    }
}

/// CW 字节流 → OpenAI Chat Completions SSE 字节流。
pub fn create_openai_sse_stream_from_kiro(
    upstream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    model: Option<String>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        let mut decoder = EventStreamDecoder::new();
        let mut conv = KiroToOpenAIChat::new(model.as_deref());
        futures::pin_mut!(upstream);
        use futures::StreamExt;
        while let Some(item) = upstream.next().await {
            match item {
                Ok(bytes) => {
                    for ev in decoder.push(&bytes) {
                        if ev.message_type.as_deref() == Some("exception") {
                            let msg = ev
                                .json()
                                .and_then(|j| j.get("message").and_then(|m| m.as_str()).map(String::from))
                                .unwrap_or_else(|| ev.event_type.clone());
                            let err = json!({"error": {"message": msg, "type": "api_error"}});
                            yield Ok(Bytes::from(format!("data: {err}\n\n")));
                            return;
                        }
                        let sse = conv.handle(&ev);
                        if !sse.is_empty() {
                            yield Ok(Bytes::from(sse));
                        }
                    }
                }
                Err(e) => {
                    yield Err(std::io::Error::new(std::io::ErrorKind::Other, e.to_string()));
                    return;
                }
            }
        }
        let fin = conv.finish();
        if !fin.is_empty() {
            yield Ok(Bytes::from(fin));
        }
    }
}

/// CW 事件聚合为单个 OpenAI Chat Completions 响应 JSON（非流式客户端用）。
pub fn cw_events_to_openai_message(events: &[DecodedEvent], model: Option<&str>) -> Value {
    let mut text = String::new();
    let mut resolved_model = model.unwrap_or(protocol::DEFAULT_MODEL_ID).to_string();
    let mut tool_order: Vec<String> = Vec::new();
    let mut tool_name: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut tool_input: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut used_tool = false;

    for ev in events {
        let Some(j) = ev.json() else { continue };
        match ev.event_type.as_str() {
            "assistantResponseEvent" => {
                if let Some(c) = j.get("content").and_then(|c| c.as_str()) {
                    text.push_str(c);
                }
                if let Some(m) = j.get("modelId").and_then(|m| m.as_str()) {
                    resolved_model = m.to_string();
                }
            }
            "toolUseEvent" => {
                used_tool = true;
                let id = j.get("toolUseId").and_then(|i| i.as_str()).unwrap_or("").to_string();
                if id.is_empty() {
                    continue;
                }
                if !tool_order.contains(&id) {
                    tool_order.push(id.clone());
                }
                if let Some(n) = j.get("name").and_then(|n| n.as_str()) {
                    tool_name.entry(id.clone()).or_insert_with(|| n.to_string());
                }
                if let Some(inp) = j.get("input").and_then(|i| i.as_str()) {
                    tool_input.entry(id.clone()).or_default().push_str(inp);
                }
            }
            _ => {}
        }
    }

    let mut message = json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) } });
    if !tool_order.is_empty() {
        let tool_calls: Vec<Value> = tool_order
            .iter()
            .map(|id| {
                let args = tool_input.get(id).cloned().unwrap_or_default();
                let args = if args.trim().is_empty() { "{}".to_string() } else { args };
                json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": tool_name.get(id).cloned().unwrap_or_default(), "arguments": args}
                })
            })
            .collect();
        message["tool_calls"] = json!(tool_calls);
    }

    json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": resolved_model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": if used_tool { "tool_calls" } else { "stop" }
        }],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- event-stream 解码器 ----

    /// 构造一个 event-stream 帧（仅含 :event-type string 头 + payload）
    fn make_frame(event_type: &str, payload: &[u8]) -> Vec<u8> {
        let name = ":event-type";
        // header: [name_len:1][name][type:1=7][val_len:2][val]
        let mut headers = Vec::new();
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7u8);
        headers.extend_from_slice(&(event_type.len() as u16).to_be_bytes());
        headers.extend_from_slice(event_type.as_bytes());

        let total = 4 + 4 + 4 + headers.len() + payload.len() + 4;
        let mut frame = Vec::new();
        frame.extend_from_slice(&(total as u32).to_be_bytes());
        frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        frame.extend_from_slice(&0u32.to_be_bytes()); // prelude crc (ignored)
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&0u32.to_be_bytes()); // msg crc (ignored)
        frame
    }

    #[test]
    fn test_decode_single_event() {
        let frame = make_frame("assistantResponseEvent", br#"{"content":"hi"}"#);
        let mut dec = EventStreamDecoder::new();
        let events = dec.push(&frame);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "assistantResponseEvent");
        assert_eq!(events[0].json().unwrap()["content"], "hi");
    }

    #[test]
    fn test_decode_partial_then_complete() {
        let frame = make_frame("assistantResponseEvent", br#"{"content":"hello"}"#);
        let mut dec = EventStreamDecoder::new();
        // 先喂前半，无完整事件
        let half = frame.len() / 2;
        assert!(dec.push(&frame[..half]).is_empty());
        // 再喂后半，得到完整事件
        let events = dec.push(&frame[half..]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].json().unwrap()["content"], "hello");
    }

    #[test]
    fn test_decode_two_events_in_one_push() {
        let mut buf = make_frame("a", br#"{"x":1}"#);
        buf.extend(make_frame("b", br#"{"y":2}"#));
        let mut dec = EventStreamDecoder::new();
        let events = dec.push(&buf);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "a");
        assert_eq!(events[1].event_type, "b");
    }

    // ---- 请求转换 ----

    #[test]
    fn test_anthropic_simple_text_request() {
        let body = json!({
            "model": "claude-3-5-sonnet",
            "system": "You are helpful.",
            "messages": [{"role": "user", "content": "Hello"}]
        });
        let cw = anthropic_to_cw_request(&body);
        let cur = &cw["conversationState"]["currentMessage"]["userInputMessage"];
        assert_eq!(cur["origin"], "CLI");
        // system 注入到首条 user 文本
        assert!(cur["content"].as_str().unwrap().contains("You are helpful."));
        assert!(cur["content"].as_str().unwrap().contains("Hello"));
        assert_eq!(cw["conversationState"]["chatTriggerType"], "MANUAL");
        assert!(cw["conversationState"].get("history").is_none());
    }

    #[test]
    fn test_anthropic_multiturn_history() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello there"},
                {"role": "user", "content": "bye"}
            ]
        });
        let cw = anthropic_to_cw_request(&body);
        let hist = cw["conversationState"]["history"].as_array().unwrap();
        assert_eq!(hist.len(), 2);
        assert!(hist[0].get("userInputMessage").is_some());
        assert!(hist[1].get("assistantResponseMessage").is_some());
        assert_eq!(
            cw["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "bye"
        );
    }

    #[test]
    fn test_anthropic_tools_and_tool_result() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "id": "t1", "name": "get_weather", "input": {"city": "Tokyo"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "sunny 22C"}
                ]}
            ],
            "tools": [{"name": "get_weather", "description": "w", "input_schema": {"type": "object"}}]
        });
        let cw = anthropic_to_cw_request(&body);
        let hist = cw["conversationState"]["history"].as_array().unwrap();
        // assistant toolUses 映射
        let tu = &hist[1]["assistantResponseMessage"]["toolUses"][0];
        assert_eq!(tu["toolUseId"], "t1");
        assert_eq!(tu["name"], "get_weather");
        assert_eq!(tu["input"]["city"], "Tokyo");
        // currentMessage 带 toolResults
        let cur_ctx =
            &cw["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"];
        assert_eq!(cur_ctx["toolResults"][0]["toolUseId"], "t1");
        assert_eq!(cur_ctx["toolResults"][0]["status"], "success");
        assert_eq!(cur_ctx["toolResults"][0]["content"][0]["text"], "sunny 22C");
        // 工具定义挂在 currentMessage（最后一条 user）上
        assert!(cur_ctx.get("tools").is_some());
    }

    // ---- 响应转换 ----

    #[test]
    fn test_cw_text_to_anthropic_sse() {
        let mut conv = KiroToAnthropic::new(Some("claude-sonnet-4.5"));
        let ev = DecodedEvent {
            event_type: "assistantResponseEvent".into(),
            message_type: Some("event".into()),
            payload: br#"{"content":"Hello","modelId":"claude-sonnet-4.5"}"#.to_vec(),
        };
        let out = conv.handle(&ev);
        assert!(out.contains("event: message_start"));
        assert!(out.contains("event: content_block_start"));
        assert!(out.contains("text_delta"));
        assert!(out.contains("Hello"));
        let fin = conv.finish();
        assert!(fin.contains("content_block_stop"));
        assert!(fin.contains("\"stop_reason\":\"end_turn\""));
        assert!(fin.contains("event: message_stop"));
    }

    #[test]
    fn test_cw_tooluse_to_anthropic_sse() {
        let mut conv = KiroToAnthropic::new(None);
        // 文本先行
        conv.handle(&DecodedEvent {
            event_type: "assistantResponseEvent".into(),
            message_type: None,
            payload: br#"{"content":"let me check"}"#.to_vec(),
        });
        // toolUse 开始
        let start = conv.handle(&DecodedEvent {
            event_type: "toolUseEvent".into(),
            message_type: None,
            payload: br#"{"name":"get_weather","toolUseId":"tooluse_1"}"#.to_vec(),
        });
        assert!(start.contains("\"type\":\"tool_use\""));
        assert!(start.contains("tooluse_1"));
        assert!(start.contains("get_weather"));
        // input 分片
        let delta = conv.handle(&DecodedEvent {
            event_type: "toolUseEvent".into(),
            message_type: None,
            payload: br#"{"input":"{\"city\":","name":"get_weather","toolUseId":"tooluse_1"}"#
                .to_vec(),
        });
        assert!(delta.contains("input_json_delta"));
        // stop
        let stop = conv.handle(&DecodedEvent {
            event_type: "toolUseEvent".into(),
            message_type: None,
            payload: br#"{"name":"get_weather","stop":true,"toolUseId":"tooluse_1"}"#.to_vec(),
        });
        assert!(stop.contains("content_block_stop"));
        let fin = conv.finish();
        assert!(fin.contains("\"stop_reason\":\"tool_use\""));
    }

    #[test]
    fn test_openai_chat_request_with_tools_and_tool_msg() {
        let body = json!({
            "model": "gpt-4",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Tokyo\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny 22C"}
            ],
            "tools": [{"type": "function", "function": {"name": "get_weather", "description": "w", "parameters": {"type": "object"}}}]
        });
        let cw = openai_chat_to_cw_request(&body);
        let hist = cw["conversationState"]["history"].as_array().unwrap();
        // user(含system前缀) + assistant(toolUses)
        assert!(hist[0]["userInputMessage"]["content"]
            .as_str()
            .unwrap()
            .contains("be brief"));
        assert_eq!(hist[1]["assistantResponseMessage"]["toolUses"][0]["toolUseId"], "call_1");
        assert_eq!(hist[1]["assistantResponseMessage"]["toolUses"][0]["input"]["city"], "Tokyo");
        // 末尾 tool 结果 → currentMessage.toolResults
        let cur_ctx =
            &cw["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"];
        assert_eq!(cur_ctx["toolResults"][0]["toolUseId"], "call_1");
        assert_eq!(cur_ctx["toolResults"][0]["content"][0]["text"], "sunny 22C");
        assert!(cur_ctx.get("tools").is_some());
    }

    #[test]
    fn test_cw_to_openai_chunk_text() {
        let mut conv = KiroToOpenAIChat::new(Some("claude-sonnet-4.5"));
        let out = conv.handle(&DecodedEvent {
            event_type: "assistantResponseEvent".into(),
            message_type: None,
            payload: br#"{"content":"Hi"}"#.to_vec(),
        });
        assert!(out.contains("chat.completion.chunk"));
        assert!(out.contains("\"role\":\"assistant\""));
        assert!(out.contains("\"content\":\"Hi\""));
        let fin = conv.finish();
        assert!(fin.contains("\"finish_reason\":\"stop\""));
        assert!(fin.contains("data: [DONE]"));
    }

    #[test]
    fn test_cw_to_openai_chunk_tool() {
        let mut conv = KiroToOpenAIChat::new(None);
        let start = conv.handle(&DecodedEvent {
            event_type: "toolUseEvent".into(),
            message_type: None,
            payload: br#"{"name":"get_weather","toolUseId":"call_1"}"#.to_vec(),
        });
        assert!(start.contains("\"tool_calls\""));
        assert!(start.contains("call_1"));
        assert!(start.contains("get_weather"));
        let delta = conv.handle(&DecodedEvent {
            event_type: "toolUseEvent".into(),
            message_type: None,
            payload: br#"{"input":"{\"city\":","toolUseId":"call_1"}"#.to_vec(),
        });
        assert!(delta.contains("arguments"));
        let fin = conv.finish();
        assert!(fin.contains("\"finish_reason\":\"tool_calls\""));
    }

    #[test]
    fn test_cw_events_to_openai_message_aggregation() {
        let events = vec![
            DecodedEvent {
                event_type: "assistantResponseEvent".into(),
                message_type: None,
                payload: br#"{"content":"hello","modelId":"claude-sonnet-4.5"}"#.to_vec(),
            },
        ];
        let msg = cw_events_to_openai_message(&events, None);
        assert_eq!(msg["object"], "chat.completion");
        assert_eq!(msg["choices"][0]["message"]["content"], "hello");
        assert_eq!(msg["choices"][0]["finish_reason"], "stop");
    }

    /// 全链路实测：Anthropic 请求 → CW → clash 7897 → 解码 → Anthropic SSE。
    /// 需本地 kiro-cli 登录态 + clash 代理。运行：
    /// `cargo test --lib proxy::providers::transform_kiro -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_full_pipeline_via_clash() {
        use super::super::kiro_auth::KiroAuthManager;
        let mgr = KiroAuthManager::new(Some("http://127.0.0.1:7897".to_string()));

        let anthropic_req = json!({
            "model": "claude-3-5-sonnet",
            "max_tokens": 256,
            "messages": [{"role": "user", "content": "Reply with exactly: PIPELINE_OK"}]
        });
        let cw_body = anthropic_to_cw_request(&anthropic_req);

        let resp = mgr
            .send_generate_assistant_response(cw_body)
            .await
            .expect("send failed");
        assert_eq!(resp.status().as_u16(), 200, "status not 200");
        let bytes = resp.bytes().await.expect("read body failed");

        let mut dec = EventStreamDecoder::new();
        let events = dec.push(&bytes);
        assert!(!events.is_empty(), "no events decoded");

        let mut conv = KiroToAnthropic::new(None);
        let mut sse = String::new();
        for ev in &events {
            sse.push_str(&conv.handle(ev));
        }
        sse.push_str(&conv.finish());
        println!("--- decoded {} events ---", events.len());
        println!("{sse}");

        assert!(sse.contains("event: message_start"));
        assert!(sse.contains("text_delta"));
        assert!(sse.contains("event: message_stop"));
        // 收集文本验证模型确实回应
        let text: String = events
            .iter()
            .filter(|e| e.event_type == "assistantResponseEvent")
            .filter_map(|e| e.json())
            .filter_map(|j| j.get("content").and_then(|c| c.as_str()).map(String::from))
            .collect();
        println!("ASSISTANT TEXT: {text:?}");
        assert!(text.contains("PIPELINE_OK"), "unexpected text: {text}");
    }

    /// 全链路实测：OpenAI Chat 请求 → CW → clash → OpenAI chunk SSE。
    #[tokio::test]
    #[ignore]
    async fn live_openai_chat_pipeline_via_clash() {
        use super::super::kiro_auth::KiroAuthManager;
        let mgr = KiroAuthManager::new(Some("http://127.0.0.1:7897".to_string()));
        let req = json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Reply with exactly: CHAT_OK"}],
            "stream": true
        });
        let cw_body = openai_chat_to_cw_request(&req);
        let resp = mgr
            .send_generate_assistant_response(cw_body)
            .await
            .expect("send failed");
        assert_eq!(resp.status().as_u16(), 200);
        let bytes = resp.bytes().await.expect("read failed");
        let mut dec = EventStreamDecoder::new();
        let events = dec.push(&bytes);
        let mut conv = KiroToOpenAIChat::new(None);
        let mut sse = String::new();
        for ev in &events {
            sse.push_str(&conv.handle(ev));
        }
        sse.push_str(&conv.finish());
        println!("{sse}");
        assert!(sse.contains("chat.completion.chunk"));
        assert!(sse.contains("data: [DONE]"));
        assert!(sse.contains("CHAT_OK"));
    }
}
