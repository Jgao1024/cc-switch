# Kiro CLI 认证机制与接口协议分析

> 分析方法：
> - 二进制符号分析：`strings "/Applications/Kiro CLI.app/Contents/MacOS/kiro-cli"`
> - SQLite 数据库：`~/Library/Application Support/kiro-cli/data.sqlite3`
> - 源码位置（二进制中内嵌的 Rust panic 路径）：`crates/fig_auth/src/portal.rs`、`crates/q_cli/src/cli/user.rs`
> - 开源参考：https://github.com/aws/amazon-q-developer-cli（kiro-cli 的前身 amazon-q-developer-cli）
>
> 目的：为 kiro-cli 接口转接提供完整的认证和协议参考。

---

## 一、认证方式总览

Kiro CLI 支持四种认证方式，核心都基于 **AWS SSO OIDC** 协议（`aws-sdk-ssooidc`）：

| 方式 | SQLite key（auth_kv 表） | 说明 |
|------|----------------------|------|
| Builder ID / PKCE | `kirocli:odic:token` | 默认，扫码或 PKCE 浏览器登录 |
| Social（Google/GitHub）| `kirocli:social:token` | 社交登录 |
| External IdP | `kirocli:external-idp:token` | 企业 Entra ID (Azure AD) |
| OAuth 客户端注册 | `kirocli:odic:device-registration` | client_id/client_secret，每 region 注册一次 |

**来源**：
- SQLite key 名：`strings kiro-cli | grep "kirocli:"` → 原始输出中出现 `kirocli:odic:token`、`kirocli:odic:device-registration`、`kirocli:external-idp:token`、`kirocli:social:token`
- 源码路径（二进制内嵌）：`crates/fig_auth/src/portal.rs:115/135/255/277/297/304/314/425`
- 开源对应文件：`crates/fig_auth/src/builder_id.rs`（推断）

---

## 二、登录流程（PKCE + 浏览器回调）

登录时用户看到的浏览器跳转是标准 **OAuth 2.0 PKCE** 流程，入口是 `https://app.kiro.dev`。

```
1. CLI 向 start URL 发起认证
   start_url = "https://d-9567af5588.awsapps.com/start"   ← 账号专属

2. 打开浏览器
   https://app.kiro.dev/signin?
     state=<random>
     &code_challenge=<PKCE>
     &code_challenge_method=S256
     &redirect_uri=<callback>
     &redirect_from=kirocli

3. 用户选择登录方式（Builder ID / Google / GitHub / 企业 IdP）
   浏览器回调 → http://127.0.0.1:55496/oauth/callback

4. CLI 用 code 换取 token
   POST https://d-9567af5588.awsapps.com/start/oauth/token
   Body: { grant_type, code, code_verifier, redirect_uri }
   返回: { accessToken, refreshToken, expiresIn, profileArn }
```

**来源**：
- `/signin?` URL 模板：`strings kiro-cli | grep "signin"` → `/signin?state=&code_challenge=&code_challenge_method=S256&redirect_uri=&redirect_from=kirocli`
- `/oauth/callback` 路径：`strings kiro-cli | grep "/oauth"` → `/oauth/callback/%`
- `https://app.kiro.dev`：`strings kiro-cli | grep "app.kiro.dev"` → `https://app.kiro.dev`
- 成功/失败回调路径：`/signin?auth_status=success&redirect_from=kirocli`、`/signin?auth_status=error&...`
- 源码路径：`crates/fig_auth/src/portal.rs:135`（redirect_base）、`:277`（callback 处理）、`:297-314`（state 校验）
- redirect_uri 端口 55496：来自 `kirocli:odic:device-registration` 中 `enabledGrants.AUTH_CODE.redirectUris[0]` = `http://127.0.0.1:55496/oauth/callback`

### 备选：Device Code Flow（远程环境）

```
POST /oauth/device/authorization    → 获取 device_code + verification_url
轮询 /oauth/device/poll             → 等待用户在浏览器授权
POST /oauth/token                   → 换取 access_token
```

**来源**：
- 路径字面量：`strings kiro-cli | grep "/oauth/device"` → `/oauth/device/authorization`、`/oauth/device/poll`、`/oauth/token`
- 源码路径：`crates/q_cli/src/cli/user.rs:531/583`（login 命令）、`crates/fig_auth/src/portal.rs:425`（device poll）
- 日志字符串：`Device authorization request failed`、`Device authorization initiated`

---

## 三、Token 存储结构

**来源**：直接查询 `data.sqlite3`：
```bash
sqlite3 ~/Library/Application\ Support/kiro-cli/data.sqlite3 \
  "SELECT key FROM auth_kv ORDER BY key;"
# → kirocli:odic:device-registration
# → kirocli:odic:token

sqlite3 ... "SELECT value FROM auth_kv WHERE key='kirocli:odic:token';" \
  | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); print(list(d.keys()))"
# → ['access_token', 'expires_at', 'refresh_token', 'region', 'start_url', 'oauth_flow', 'scopes']
```

### `kirocli:odic:token`（主 token）

```json
{
  "access_token": "<Bearer token>",
  "expires_at": "2026-06-27T12:48:46.723389Z",
  "refresh_token": "<token>",
  "region": "ap-northeast-1",
  "start_url": "https://d-9567af5588.awsapps.com/start",
  "oauth_flow": "PKCE",
  "scopes": [
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations"
  ]
}
```

二进制中对应的 Rust struct 定义（符号提取）：
```
struct BuilderIdToken with 7 elements:
  access_token, expires_at, refresh_token, region, start_url, oauth_flow, scopes
```
**来源**：`strings kiro-cli | grep "struct BuilderIdToken"` → `struct BuilderIdToken with 7 elements`

### `kirocli:odic:device-registration`（客户端注册）

字段：`client_id`、`client_secret`（JWT）、`client_secret_expires_at`、`region`、`oauth_flow`、`scopes`

**注意**：`client_id` 是 per-region 的动态值（`QFoOoqSXXzHfkjA65wLApGFwLW5vcnRoZWFzdC0x`），有效期约 90 天（`client_secret_expires_at: 2026-09-23`）。

二进制中对应 struct：
```
struct Profile with 2 elements     ← region + start_url 的组合
```
**来源**：`strings kiro-cli | grep "struct Profile"` → `struct Profile with 2 elements`

---

## 四、Token 刷新

**来源**：
- 刷新逻辑日志字符串：`strings kiro-cli | grep "Refresh"` → `Refreshing access token`、`Refreshed access token, new token:`、`Failed to refresh builder id access token`
- 错误字符串：`invalid_grant but peer token in store; returning peer's token`
- Social 刷新路径：`/refreshToken`（URL 后缀）→ `refreshing social access token for provider: /refreshToken`
- External IdP 刷新：`Content-Type: application/x-www-form-urlencoded`、字段 `grant_type`、`client_id`、`scope`
- 源码路径：`crates/fig_auth/src/builder_id.rs`（推断）、`crates/q_cli/src/cli/user.rs:719`

access_token 过期（约 8 小时）后，使用 refresh_token 静默刷新：

```
POST <start_url>/oauth/token
Content-Type: application/x-www-form-urlencoded
Body: grant_type=refresh_token&client_id=...&scope=...&refresh_token=...
```

---

## 五、Profile ARN（关键概念）

登录成功后会获得一个 `profile_arn`，格式为：

```
arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK
```

**来源**：
- 字面量：`strings kiro-cli | grep "arn:aws:codewhisperer"` → `arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK`（出现两次，第一次附带 `/oauth/token` 后缀，说明是 token 端点 URL 的前缀片段）
- struct 中的字段：`strings kiro-cli | grep "profile_arn"` → `profile_arn` 出现在 `SocialToken`、`GenerateCompletionsInput`、`SendTelemetryEventInput`、`GetUsageLimitsInput` 等结构体定义中
- 二进制符号：`struct SocialToken with 5 elements: access_token, refresh_token, profile_arn, ...`

---

## 六、API 协议

Kiro CLI 使用 **AWS CodeWhisperer API（Smithy/gRPC over HTTPS）**。

**来源**：
- SDK 名称：`strings kiro-cli | grep "aws-sdk"` → `aws-sdk-ssooidc`、`aws-sdk-cognitoidentity`
- 服务 ID：`strings kiro-cli | grep "ssooidc"` → `AWSSSOOIDCServiceServiceRuntimePlugin`
- Smithy RPC 协议：`strings kiro-cli | grep "rpc\."` → `rpc.service`、`rpc.method`、`rpc.system`
- 操作名称：`strings kiro-cli | grep -E "^[A-Z][a-z]+Input$"` → `SendTelemetryEventInput`、`GenerateCompletionsInput`、`GetUsageLimitsInput`、`RegisterClientInput`

### 请求认证头

```
x-amz-sso_bearer_token: <access_token>
```

**来源**：`strings kiro-cli | grep "x-amz-sso"` → `x-amz-sso_bearer_token`

### 主要 API 操作

| 操作 | struct 名 | 来源字符串 |
|------|----------|-----------|
| 发送对话消息 | `SendMessage` | `"SendMessage"` 字面量 |
| 发送遥测数据 | `SendTelemetryEventInput` | struct 名 + `profile_arn`、`telemetry_event`、`user_context` |
| 代码补全 | `GenerateCompletionsInput` | struct 名 + `file_context`、`editor_state`、`max_results` |
| 获取使用限制 | `GetUsageLimitsInput` | struct 名 + `profile_arn`、`resource_type`、`is_email_required` |
| 注册 OAuth 客户端 | `RegisterClientInput` | struct 名 + `client_name`、`client_type`、`scopes`、`issuer_url` |
| 设备流 token 创建 | `auth_builder_id_poll_create_token` | 函数名字面量 |
| 获取 Profile | `GetProfile` | changelog 描述中提到（`v1.14.1` 更新日志：`MCP admin-level configuration with GetProfile`） |

**来源**：全部来自 `strings "/Applications/Kiro CLI.app/Contents/MacOS/kiro-cli" | grep -E "Input$|Response$"` 以及相关上下文

### API 端点

```
# OIDC token 端点
https://oidc.<region>.amazonaws.com/token          ← 标准 AWS OIDC（SDK 自动拼接）
https://oidc-fips.<region>.amazonaws.com/...       ← FIPS 合规

# 账号专属 token 端点（Social/PKCE 流程）
https://d-9567af5588.awsapps.com/start/oauth/token

# CodeWhisperer API（endpoint resolver 动态解析）
codewhisperer.<region>.amazonaws.com              ← 推断，sdk endpoint resolver 拼接
```

**来源**：
- `https://oidc.` 前缀：`strings kiro-cli | grep "oidc\."` → `https://oidc.`（endpoint builder 片段）
- `https://oidc-fips.`：直接字面量出现
- `https://view.awsapps.com/start`：`strings kiro-cli | grep "awsapps"` → `https://view.awsapps.com/start`（加载 device registration 时的 fallback）
- 账号专属 start_url `https://d-9567af5588.awsapps.com/start`：来自 `kirocli:odic:token` SQLite 记录中的 `start_url` 字段

---

## 七、对接口转接的关键结论

### 无法通过标准 Bearer Token 直接替换

Kiro CLI 的 access_token 是 AWS SSO OIDC token，**不是** OpenAI 格式的 API key：
- 认证头是 `x-amz-sso_bearer_token`，不是 `Authorization: Bearer`
- 请求体需要 `profile_arn`
- API 是 AWS Smithy 协议，不是 OpenAI REST API

### 接口转接的两种路径

**路径 A：读取本地 token 直调 CodeWhisperer API**

```python
import sqlite3, json
db = sqlite3.connect(
    os.path.expanduser("~/Library/Application Support/kiro-cli/data.sqlite3")
)
row = db.execute(
    "SELECT value FROM auth_kv WHERE key='kirocli:odic:token'"
).fetchone()
token = json.loads(row[0])
access_token = token["access_token"]
profile_arn = token.get("profile_arn")   # 注意：部分流程下 profile_arn 在 social token 里
# 注意检查 token["expires_at"]，过期需用 refresh_token 刷新
```

**路径 B：代理层包装为 OpenAI 接口**

1. 接收 OpenAI 格式请求（`POST /v1/chat/completions`）
2. 从 SQLite 读取 kiro-cli 的 `access_token` + `profile_arn`（自动刷新）
3. 转换为 CodeWhisperer `SendMessage` 请求格式
4. 设置 `x-amz-sso_bearer_token` 头，body 携带 `profile_arn`
5. 返回转换后的 OpenAI 格式响应

### 注意事项

| 注意点 | 来源 |
|--------|------|
| access_token 约 8 小时过期 | `expires_at` 字段（SQLite 实测） |
| client_id 是 per-region、有效期 90 天 | `client_secret_expires_at` 字段（SQLite 实测） |
| Social 登录（Google/GitHub）token 在 `kirocli:social:token` | `strings kiro-cli \| grep "kirocli:social"` |
| Social refresh 走专属端点 `/refreshToken`，不是标准 `/oauth/token` | `strings kiro-cli \| grep "refreshToken"` → `refreshing social access token for provider: /refreshToken` |
| profile_arn 是 API 调用必须项 | `profile_arn` 字段出现在所有 Input struct 中 |

---

## 八、本地文件路径

```
# 主数据库（包含所有 token）
~/Library/Application Support/kiro-cli/data.sqlite3
  表结构来源：sqlite3 .schema 命令

# 应用二进制（分析来源）
/Applications/Kiro CLI.app/Contents/MacOS/kiro-cli          ← Rust 主程序，strings 分析主体
/Applications/Kiro CLI.app/Contents/MacOS/kiro-cli-chat     ← chat 子命令入口
/Applications/Kiro CLI.app/Contents/MacOS/kiro-cli-term     ← terminal 集成

# TUI 脚本（bun + React，minified，难以分析）
~/Library/Application Support/kiro-cli/tui.js

# 配置目录
~/.kiro/
  settings/      ← 用户设置
  sessions/      ← 会话记录
  steering/      ← steering 文件（CLAUDE.md 等）
  skills/        ← 技能扩展

# 版本信息
~/Library/Application Support/kiro-cli/feed.json   ← 版本更新记录（版本 2.9.0，2026-06-22）
/Applications/Kiro CLI.app/Contents/Resources/manifest.json
  → {"version": "2.9.0", "packaged_by": "amazon", "variant": "full"}
```

---

## 九、Rust 源码路径（二进制内嵌 panic 信息提取）

以下路径来自 `strings kiro-cli | grep "crates/"` 提取的 Rust 源码位置，可对照 https://github.com/aws/amazon-q-developer-cli 查看逻辑：

| 功能 | 源码路径 |
|------|---------|
| 认证 Portal（PKCE/登录流程主体） | `crates/fig_auth/src/portal.rs` |
| login 命令入口 | `crates/q_cli/src/cli/user.rs` |
| HTTP 客户端 | `crates/fig_request/src/reqwest_client.rs` |
| IPC multiplexer | `crates/q_cli/src/cli/internal/multiplexer.rs` |
| 内部命令 | `crates/q_cli/src/cli/internal/mod.rs` |
| inline 补全 | `crates/q_cli/src/cli/inline.rs` |
