//! Kiro (Amazon Q Developer / CodeWhisperer) 认证与凭证管理
//!
//! Kiro CLI 的聊天后端是 AWS CodeWhisperer Streaming API，认证基于 AWS SSO OIDC
//! (PKCE OAuth)。本模块负责：
//! - 从本地 kiro-cli 的 SQLite (`data.sqlite3` / auth_kv 表) 继承现有登录凭证
//! - access_token 过期时通过 SSO OIDC `CreateToken` 自动刷新
//! - 缓存 `profileArn`（首次通过 `ListAvailableProfiles` 获取，CodeWhisperer 调用必需）
//! - 所有上游 HTTP 请求经可配置代理（如 clash 7897）转发
//!
//! 协议细节见 `docs/kiro-cli-auth-and-api.md`（含实测验证）。

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::RwLock;

/// CodeWhisperer / Kiro 协议固定常量（均来自实测验证）
pub mod protocol {
    /// CodeWhisperer API 端点（固定 us-east-1，与 SSO region 无关）
    pub const CW_ENDPOINT: &str = "https://codewhisperer.us-east-1.amazonaws.com/";

    /// 非流式服务前缀（ListAvailableProfiles / GetUsageLimits 等）
    pub const TARGET_LIST_PROFILES: &str =
        "AmazonCodeWhispererService.ListAvailableProfiles";
    /// 流式聊天操作
    pub const TARGET_GENERATE_ASSISTANT_RESPONSE: &str =
        "AmazonCodeWhispererStreamingService.GenerateAssistantResponse";

    /// x-amz-json 协议 Content-Type
    pub const CONTENT_TYPE_AMZ_JSON: &str = "application/x-amz-json-1.0";

    /// User-Agent 必须包含应用标识，否则后端 403
    /// (AccessDeniedException: Your subscription does not support this application)
    pub const USER_AGENT: &str =
        "aws-sdk-rust/1.8.11 os/macos lang/rust AmazonQ-For-CLI/2.9.0 kirocli/2.9.0";

    /// 数据不用于训练
    pub const HEADER_OPTOUT: &str = "x-amzn-codewhisperer-optout";

    /// 后端默认模型标识（响应中回传的 modelId）
    pub const DEFAULT_MODEL_ID: &str = "claude-sonnet-4.5";
}

/// kiro-cli 凭证（从其 SQLite auth_kv 表继承的字段子集）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroCredentials {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// RFC3339 过期时间，如 `2026-06-27T12:48:46.723389Z`
    #[serde(default)]
    pub expires_at: Option<String>,
    /// SSO region（用于 OIDC 刷新端点），如 `ap-northeast-1`
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub start_url: Option<String>,
    /// CodeWhisperer profileArn（本地 token 不含，需 ListAvailableProfiles 获取后缓存）
    #[serde(default)]
    pub profile_arn: Option<String>,
}

impl KiroCredentials {
    /// access_token 是否已过期（或将在 60s 内过期）
    pub fn is_expired(&self) -> bool {
        match self.expires_at.as_deref().and_then(parse_rfc3339) {
            Some(exp) => Utc::now() + chrono::Duration::seconds(60) >= exp,
            // 无过期时间时保守视为有效（由调用方按上游 401 处理）
            None => false,
        }
    }
}

/// kiro-cli 客户端注册信息（OIDC 刷新需要 client_id/client_secret）
#[derive(Debug, Clone, Deserialize)]
struct KiroDeviceRegistration {
    client_id: String,
    client_secret: String,
    #[serde(default)]
    #[allow(dead_code)]
    region: Option<String>,
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// kiro-cli 本地数据库路径（按平台）
pub fn kirocli_data_db_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    #[cfg(target_os = "macos")]
    let p = home
        .join("Library")
        .join("Application Support")
        .join("kiro-cli")
        .join("data.sqlite3");
    #[cfg(target_os = "linux")]
    let p = home
        .join(".local")
        .join("share")
        .join("kiro-cli")
        .join("data.sqlite3");
    #[cfg(target_os = "windows")]
    let p = home
        .join("AppData")
        .join("Local")
        .join("kiro-cli")
        .join("data.sqlite3");
    Some(p)
}

/// 读取 auth_kv 表中某个 key 的原始 JSON 值（只读打开，避免与 kiro-cli 争锁）
fn read_auth_kv(db_path: &PathBuf, key: &str) -> Option<String> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_URI
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.query_row(
        "SELECT value FROM auth_kv WHERE key = ?1",
        [key],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// 从本地 kiro-cli SQLite 继承现有登录凭证
pub fn read_kirocli_credentials() -> Option<KiroCredentials> {
    let db = kirocli_data_db_path()?;
    if !db.exists() {
        return None;
    }
    let raw = read_auth_kv(&db, "kirocli:odic:token")?;
    serde_json::from_str::<KiroCredentials>(&raw).ok()
}

fn read_kirocli_registration() -> Option<KiroDeviceRegistration> {
    let db = kirocli_data_db_path()?;
    let raw = read_auth_kv(&db, "kirocli:odic:device-registration")?;
    serde_json::from_str::<KiroDeviceRegistration>(&raw).ok()
}

/// 本地是否存在可继承的 kiro-cli 凭证
pub fn has_kirocli_credentials() -> bool {
    read_kirocli_credentials()
        .map(|c| !c.access_token.is_empty())
        .unwrap_or(false)
}

#[derive(Debug, thiserror::Error)]
pub enum KiroAuthError {
    #[error("no kiro credentials available (please login)")]
    NoCredentials,
    #[error("token expired and refresh failed: {0}")]
    RefreshFailed(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("profile fetch failed: {0}")]
    ProfileFetch(String),
}

/// Kiro 认证管理器：持有凭证缓存，提供自动刷新的 token 与 profileArn
pub struct KiroAuthManager {
    inner: RwLock<KiroState>,
    /// 上游代理（如 `http://127.0.0.1:7897`）；None 表示直连。
    /// 用 std RwLock 便于同步读取（构建 client 时无 await）。
    proxy_url: std::sync::RwLock<Option<String>>,
}

#[derive(Default)]
struct KiroState {
    creds: Option<KiroCredentials>,
}

impl KiroAuthManager {
    pub fn new(proxy_url: Option<String>) -> Self {
        Self {
            inner: RwLock::new(KiroState::default()),
            proxy_url: std::sync::RwLock::new(proxy_url),
        }
    }

    /// 更新上游代理地址（provider 配置变化时调用）
    pub fn set_proxy(&self, proxy_url: Option<String>) {
        if let Ok(mut p) = self.proxy_url.write() {
            *p = proxy_url;
        }
    }

    /// 构建带代理的 reqwest 客户端
    fn http_client(&self) -> Result<reqwest::Client, KiroAuthError> {
        let mut builder = reqwest::Client::builder()
            .user_agent(protocol::USER_AGENT)
            .timeout(std::time::Duration::from_secs(60));
        let proxy = self.proxy_url.read().ok().and_then(|p| p.clone());
        if let Some(proxy) = proxy {
            builder = builder.proxy(
                reqwest::Proxy::all(&proxy)
                    .map_err(|e| KiroAuthError::Network(e.to_string()))?,
            );
        }
        builder
            .build()
            .map_err(|e| KiroAuthError::Network(e.to_string()))
    }

    /// 确保内存中有凭证：优先用已加载的，否则从 kiro-cli SQLite 继承
    async fn ensure_loaded(&self) -> Result<(), KiroAuthError> {
        {
            let st = self.inner.read().await;
            if st.creds.is_some() {
                return Ok(());
            }
        }
        let inherited = read_kirocli_credentials().ok_or(KiroAuthError::NoCredentials)?;
        let mut st = self.inner.write().await;
        st.creds = Some(inherited);
        Ok(())
    }

    /// 显式设置凭证（用于 OAuth 登录后写入）
    pub async fn set_credentials(&self, creds: KiroCredentials) {
        let mut st = self.inner.write().await;
        st.creds = Some(creds);
    }

    /// 是否已认证（内存或本地 kiro-cli 任一存在凭证）
    pub async fn is_authenticated(&self) -> bool {
        if self.inner.read().await.creds.is_some() {
            return true;
        }
        has_kirocli_credentials()
    }

    /// 获取有效 access_token，必要时刷新
    pub async fn get_valid_token(&self) -> Result<String, KiroAuthError> {
        self.ensure_loaded().await?;

        let needs_refresh = {
            let st = self.inner.read().await;
            st.creds.as_ref().map(|c| c.is_expired()).unwrap_or(true)
        };

        if needs_refresh {
            self.refresh().await?;
        }

        let st = self.inner.read().await;
        st.creds
            .as_ref()
            .map(|c| c.access_token.clone())
            .ok_or(KiroAuthError::NoCredentials)
    }

    /// 通过 SSO OIDC CreateToken 刷新 access_token
    ///
    /// 端点: `https://oidc.<sso_region>.amazonaws.com/token`
    /// Body (x-amz-json-1.1): {clientId, clientSecret, grantType:"refresh_token", refreshToken}
    async fn refresh(&self) -> Result<(), KiroAuthError> {
        let (refresh_token, region) = {
            let st = self.inner.read().await;
            let c = st.creds.as_ref().ok_or(KiroAuthError::NoCredentials)?;
            (
                c.refresh_token.clone(),
                c.region.clone().unwrap_or_else(|| "us-east-1".to_string()),
            )
        };
        let refresh_token = refresh_token
            .ok_or_else(|| KiroAuthError::RefreshFailed("no refresh_token".into()))?;
        let reg = read_kirocli_registration().ok_or_else(|| {
            KiroAuthError::RefreshFailed("no device-registration for refresh".into())
        })?;

        let url = format!("https://oidc.{region}.amazonaws.com/token");
        let body = json!({
            "clientId": reg.client_id,
            "clientSecret": reg.client_secret,
            "grantType": "refresh_token",
            "refreshToken": refresh_token,
        });

        let client = self.http_client()?;
        let resp = client
            .post(&url)
            .header("Content-Type", "application/x-amz-json-1.1")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| KiroAuthError::RefreshFailed(e.to_string()))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| KiroAuthError::RefreshFailed(e.to_string()))?;
        if !status.is_success() {
            return Err(KiroAuthError::RefreshFailed(format!(
                "HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            )));
        }

        // OIDC CreateToken 返回 {accessToken, refreshToken?, expiresIn, ...}
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| KiroAuthError::RefreshFailed(e.to_string()))?;
        let new_access = v
            .get("accessToken")
            .and_then(|x| x.as_str())
            .ok_or_else(|| KiroAuthError::RefreshFailed("no accessToken in response".into()))?
            .to_string();
        let new_refresh = v
            .get("refreshToken")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let expires_in = v.get("expiresIn").and_then(|x| x.as_i64());

        let mut st = self.inner.write().await;
        if let Some(c) = st.creds.as_mut() {
            c.access_token = new_access;
            if let Some(r) = new_refresh {
                c.refresh_token = Some(r);
            }
            if let Some(secs) = expires_in {
                c.expires_at =
                    Some((Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339());
            }
        }
        Ok(())
    }

    /// 获取 profileArn（缓存）。首次调用 ListAvailableProfiles 获取。
    pub async fn get_profile_arn(&self) -> Result<String, KiroAuthError> {
        {
            let st = self.inner.read().await;
            if let Some(arn) = st.creds.as_ref().and_then(|c| c.profile_arn.clone()) {
                if !arn.is_empty() {
                    return Ok(arn);
                }
            }
        }
        let token = self.get_valid_token().await?;
        let client = self.http_client()?;
        let resp = client
            .post(protocol::CW_ENDPOINT)
            .header("Content-Type", protocol::CONTENT_TYPE_AMZ_JSON)
            .header("X-Amz-Target", protocol::TARGET_LIST_PROFILES)
            .header("Authorization", format!("Bearer {token}"))
            .header(protocol::HEADER_OPTOUT, "true")
            .body(json!({ "maxResults": 10 }).to_string())
            .send()
            .await
            .map_err(|e| KiroAuthError::ProfileFetch(e.to_string()))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| KiroAuthError::ProfileFetch(e.to_string()))?;
        if !status.is_success() {
            return Err(KiroAuthError::ProfileFetch(format!(
                "HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            )));
        }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| KiroAuthError::ProfileFetch(e.to_string()))?;
        let arn = v
            .get("profiles")
            .and_then(|p| p.as_array())
            .and_then(|arr| arr.first())
            .and_then(|p| p.get("arn"))
            .and_then(|a| a.as_str())
            .ok_or_else(|| KiroAuthError::ProfileFetch("no profile arn in response".into()))?
            .to_string();

        let mut st = self.inner.write().await;
        if let Some(c) = st.creds.as_mut() {
            c.profile_arn = Some(arn.clone());
        }
        Ok(arn)
    }

    /// 发送 `GenerateAssistantResponse` 流式聊天请求。
    ///
    /// `cw_body` 为 `transform_kiro::anthropic_to_cw_request` 等产出的请求体
    /// （含 `conversationState`，不含 `profileArn`）。本方法自动注入有效 token、
    /// profileArn 与必需头部，经配置的上游代理发送，返回流式 `reqwest::Response`。
    pub async fn send_generate_assistant_response(
        &self,
        mut cw_body: serde_json::Value,
    ) -> Result<reqwest::Response, KiroAuthError> {
        let token = self.get_valid_token().await?;
        let arn = self.get_profile_arn().await?;
        cw_body["profileArn"] = serde_json::Value::String(arn);

        let client = self.http_client()?;
        let resp = client
            .post(protocol::CW_ENDPOINT)
            .header("Content-Type", protocol::CONTENT_TYPE_AMZ_JSON)
            .header("X-Amz-Target", protocol::TARGET_GENERATE_ASSISTANT_RESPONSE)
            .header("Authorization", format!("Bearer {token}"))
            .header(protocol::HEADER_OPTOUT, "true")
            .body(cw_body.to_string())
            .send()
            .await
            .map_err(|e| KiroAuthError::Network(e.to_string()))?;
        Ok(resp)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_expired_past() {
        let c = KiroCredentials {
            access_token: "x".into(),
            refresh_token: None,
            expires_at: Some("2000-01-01T00:00:00Z".into()),
            region: None,
            start_url: None,
            profile_arn: None,
        };
        assert!(c.is_expired());
    }

    #[test]
    fn test_is_expired_future() {
        let c = KiroCredentials {
            access_token: "x".into(),
            refresh_token: None,
            expires_at: Some("2099-01-01T00:00:00Z".into()),
            region: None,
            start_url: None,
            profile_arn: None,
        };
        assert!(!c.is_expired());
    }

    #[test]
    fn test_is_expired_none() {
        let c = KiroCredentials {
            access_token: "x".into(),
            refresh_token: None,
            expires_at: None,
            region: None,
            start_url: None,
            profile_arn: None,
        };
        assert!(!c.is_expired());
    }

    #[test]
    fn test_parse_credentials_from_kirocli_json() {
        // 模拟 kiro-cli auth_kv 中 token 的 JSON（含本模块未用的多余字段）
        let raw = r#"{
            "access_token":"AT","refresh_token":"RT",
            "expires_at":"2026-06-27T12:48:46.723389Z",
            "region":"ap-northeast-1",
            "start_url":"https://d-xxx.awsapps.com/start",
            "oauth_flow":"PKCE",
            "scopes":["codewhisperer:completions"]
        }"#;
        let c: KiroCredentials = serde_json::from_str(raw).unwrap();
        assert_eq!(c.access_token, "AT");
        assert_eq!(c.refresh_token.as_deref(), Some("RT"));
        assert_eq!(c.region.as_deref(), Some("ap-northeast-1"));
        assert!(c.profile_arn.is_none());
    }

    #[test]
    fn test_db_path_resolves() {
        // 仅验证路径构造不 panic
        let _ = kirocli_data_db_path();
    }

    /// 真实后端集成测试：经 clash 7897 用继承的 kiro-cli 凭证获取 token + profileArn。
    /// 需要本地存在 kiro-cli 登录态与 clash 代理，故默认 ignore。
    /// 运行：`cargo test --lib proxy::providers::kiro_auth -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_get_token_and_profile_via_clash() {
        let mgr = KiroAuthManager::new(Some("http://127.0.0.1:7897".to_string()));
        assert!(mgr.is_authenticated().await, "未检测到 kiro-cli 凭证");
        let token = mgr.get_valid_token().await.expect("获取 token 失败");
        assert!(!token.is_empty());
        println!("[live] token len = {}", token.len());
        let arn = mgr.get_profile_arn().await.expect("获取 profileArn 失败");
        println!("[live] profileArn = {arn}");
        assert!(arn.starts_with("arn:aws:codewhisperer:"));
    }
}
