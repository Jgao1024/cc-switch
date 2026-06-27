# CC Switch 代理层 —— Codex 接口路由技术文档

> 目的：为 Kiro CLI 接口转接提供实现参考，避免重复阅读无关代码。
>
> 所有代码引用格式：`文件路径:函数名/结构名`

---

## 一、整体架构

```
Kiro CLI / Codex CLI
       |
       | POST /v1/responses  (OpenAI Responses API)
       | POST /v1/chat/completions  (OpenAI Chat API)
       | GET  /v1/models     (可达性探测)
       |
[本地代理服务器 127.0.0.1:15721]
       |
  handlers.rs
  ├── handle_responses()        ← Responses API 入口
  ├── handle_chat_completions() ← Chat Completions API 入口
  └── handle_models()           ← /v1/models 可达性探测
       |
  RequestContext::new()
  ├── 读 AppProxyConfig（超时/重试/熔断参数）
  ├── 读 RectifierConfig / OptimizerConfig
  └── ProviderRouter::select_providers("codex") → Vec<Provider>
       |
  RequestForwarder::forward_with_retry()
  ├── 依次尝试 providers（熔断器过滤）
  ├── 格式转换：Responses API → Chat Completions（如需）
  ├── 认证注入（Bearer / API Key）
  └── HTTP 发往上游 base_url
```

---

## 二、路由端点注册

**文件**：`src-tauri/src/proxy/server.rs` → `ProxyServer::build_router()`

代理服务器默认监听 `127.0.0.1:15721`，Codex 相关路由：

| 路由 | 处理函数 | 用途 |
|------|----------|------|
| `POST /v1/responses` | `handle_responses` | Codex 新版 Responses API |
| `POST /v1/responses/compact` | `handle_responses_compact` | Responses Compact（远程压缩） |
| `POST /v1/chat/completions` | `handle_chat_completions` | 旧版 Chat Completions |
| `GET /v1/models` | `handle_models` | Codex 启动时可达性探测 |

所有路由同时注册了 `/codex/v1/`、`/v1/v1/` 等前缀变体，兼容不同 base_url 配置。

---

## 三、请求入口处理

**文件**：`src-tauri/src/proxy/handlers.rs`

### handle_responses() / handle_chat_completions()

两者结构相同，核心步骤：

1. 读取并解压请求体（Codex Desktop 可能用 zstd 压缩）
   → `decode_codex_request_body()` — 同文件
2. 构建请求上下文
   → `RequestContext::new()` — `proxy/handler_context.rs`
3. 检查是否需要 Responses→Chat 格式转换
   → `providers::should_convert_codex_responses_to_chat()` — `proxy/providers/codex.rs`
4. 调用转发器
   → `RequestForwarder::forward_with_retry()` — `proxy/forwarder.rs`
5. 透传响应（或格式转换响应）
   → `process_response()` / `handle_codex_chat_to_responses_transform()` — `proxy/response_processor.rs`

### handle_models()

读取 `~/.codex/cc-switch-model-catalog.json`，只有 `config.toml` 里 `model_catalog_json` 指向该文件时才返回内容，否则返回 `{"models": []}`。

---

## 四、请求上下文（RequestContext）

**文件**：`src-tauri/src/proxy/handler_context.rs` → `RequestContext::new()`

初始化时从数据库读取：
- `AppProxyConfig`：超时/重试/熔断参数（per-app，key = `"codex"`）
- `RectifierConfig`：整流器开关（thinking signature / budget / media fallback）
- `OptimizerConfig`：Bedrock 优化器（默认关闭）
- 当前 provider ID：`settings::get_current_provider(&AppType::Codex)`

调用 `ProviderRouter::select_providers("codex")` 获取 provider 列表（见第五节）。

---

## 五、Provider 路由选择

**文件**：`src-tauri/src/proxy/provider_router.rs` → `ProviderRouter::select_providers()`

```
if auto_failover_enabled (来自 AppProxyConfig):
    → 按 failover_queue 顺序返回所有未熔断的 provider（P1 → P2 → ...）
else:
    → 仅返回 current_provider（单个，跳过熔断器检查）
```

熔断器状态（Closed / HalfOpen / Open）由 `CircuitBreaker` 管理：
- **文件**：`src-tauri/src/proxy/circuit_breaker.rs`
- 配置来自 `AppProxyConfig`（`circuit_failure_threshold` / `circuit_timeout_seconds` / `circuit_success_threshold`）

---

## 六、Provider 格式判定

**文件**：`src-tauri/src/proxy/providers/codex.rs`

### codex_provider_uses_chat_completions(provider)

判断某 provider 的 upstream 是否走 Chat Completions（而非 Responses API）。优先级顺序：

1. `provider.meta.api_format`
2. `provider.settings_config["api_format"]` / `["apiFormat"]`
3. `provider.settings_config["config"]`（TOML 文本）中解析 `wire_api` 字段
4. `provider.settings_config["base_url"]` 是否以 `/chat/completions` 结尾

### should_convert_codex_responses_to_chat(provider, endpoint)

当客户端发 `/v1/responses` 但 upstream 只支持 Chat Completions 时返回 `true`，触发格式转换。

---

## 七、请求转发核心流程

**文件**：`src-tauri/src/proxy/forwarder.rs` → `RequestForwarder::forward_with_retry_inner()`

依次尝试每个 provider，单次 provider 的处理步骤（`forward()` 函数，同文件）：

1. **提取 base_url** → `CodexAdapter::extract_base_url()` — `proxy/providers/codex.rs`
2. **模型映射** → `model_mapper::apply_model_mapping()` — `proxy/model_mapper.rs`
   - 客户端请求中的 `model` 是展示别名，upstream 真实模型名来自 `settings_config.model`
3. **端点改写**（Responses→Chat）→ `rewrite_codex_responses_endpoint_to_chat()` — 同文件
4. **请求体转换**（如需）→ `transform_codex_chat::responses_to_chat_completions_with_reasoning()` — `proxy/providers/transform_codex_chat.rs`
   - 同时处理 `previous_response_id`（从 `CodexChatHistoryStore` 还原 tool call 历史）
5. **Media 降级** → `media_sanitizer::replace_images_for_text_only_model()` — `proxy/media_sanitizer.rs`
6. **私有字段过滤**（去掉 `_` 前缀字段）→ `filter_private_params_with_whitelist()` — `proxy/body_filter.rs`
7. **认证注入** → `CodexAdapter::get_auth_headers()` — `proxy/providers/codex.rs`
8. **HTTP 发送** → `hyper` 客户端，保留原始请求头大小写 — `proxy/hyper_client.rs`

失败分类：
- `Retryable`（5xx / 超时）→ 记录熔断器，继续下一个 provider
- `NonRetryable`（4xx 客户端错误）→ 直接返回，不污染熔断器

---

## 八、认证层

**文件**：`src-tauri/src/proxy/providers/auth.rs`，`src-tauri/src/proxy/providers/codex.rs`

`CodexAdapter::extract_auth(provider)` 提取顺序：
1. `provider.settings_config["api_key"]`
2. `provider.settings_config["env"]["OPENAI_API_KEY"]`

`AuthStrategy` 枚举（`src-tauri/src/proxy/providers/auth.rs`）：

| 值 | 说明 | 实现位置 |
|----|------|----------|
| `BearerToken` | 标准 API key，`Authorization: Bearer <key>` | 通用 |
| `CodexOAuth` | ChatGPT Plus/Pro OAuth，动态获取 access_token | `proxy/providers/codex_oauth_auth.rs` |
| `GitHubCopilot` | Copilot token，动态刷新 | `proxy/providers/copilot_auth.rs` |

---

## 九、Reasoning 参数适配（Chat Completions 模式）

**文件**：`src-tauri/src/proxy/providers/codex.rs` → `infer_codex_chat_reasoning_config()`

当 provider 走 Chat Completions 且模型支持 thinking 时，各平台参数差异：

| 平台 | `thinking_param` | `output_format` | `effort_param` |
|------|-----------------|-----------------|----------------|
| DeepSeek | `thinking` | `reasoning_content` | `reasoning_effort` |
| Kimi/Moonshot | `thinking` | `reasoning_content` | — |
| Qwen/DashScope | `enable_thinking` | `reasoning_content` | — |
| StepFun | `none` | `reasoning` | `reasoning_effort`（部分版本）|
| GLM/Zhipu | `thinking` | `reasoning_content` | — |

聚合平台（OpenRouter 等）由 `infer_aggregator_platform_config()` 单独处理，通过 `base_url` / provider name 识别（不依赖模型名）。

转换实现：`src-tauri/src/proxy/providers/transform_codex_chat.rs` → `responses_to_chat_completions_with_reasoning()`

---

## 十、数据结构快速参考

| 结构 | 文件 | 说明 |
|------|------|------|
| `Provider` | `src-tauri/src/provider.rs` | 核心 provider 数据（含 settings_config / meta） |
| `AppProxyConfig` | `src-tauri/src/proxy/types.rs` | per-app 代理配置（超时/重试/熔断） |
| `ProxyConfig` | `src-tauri/src/proxy/types.rs` | 全局代理配置（端口/地址） |
| `RectifierConfig` | `src-tauri/src/proxy/types.rs` | 整流器开关 |
| `ProviderType` | `src-tauri/src/proxy/providers/mod.rs` | provider 类型枚举 |
| `CodexChatReasoningConfig` | `src-tauri/src/provider.rs` | Chat 模式 reasoning 参数配置 |

---

## 十一、Kiro CLI 接入要点

1. **配置**：`base_url = "http://127.0.0.1:15721"` + 任意 API key，代理自动接管
2. **格式透传**：发 Responses API 或 Chat Completions 均可，代理按 provider 配置决定是否转换
3. **`/v1/models`**：需要响应这个端点（Codex CLI 启动探测用），返回格式见 `handle_models()` — `proxy/handlers.rs`
4. **`previous_response_id`**：Responses API 的 stateful 字段，代理通过 `CodexChatHistoryStore` 维护历史（`proxy/providers/codex_chat_history.rs`），Kiro CLI 若要支持多轮对话需要同样处理
5. **zstd 解压**：Codex Desktop 客户端可能压缩请求体，见 `decode_codex_request_body()` — `proxy/handlers.rs`
6. **upstream model 分离**：客户端 `model` 字段是别名，upstream 真实模型名从 `settings_config.model` 取，见 `codex_provider_upstream_model()` — `proxy/providers/codex.rs`
