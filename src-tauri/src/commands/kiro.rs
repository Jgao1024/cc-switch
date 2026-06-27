//! Kiro (Amazon Q / CodeWhisperer) Tauri 命令与状态
//!
//! 提供凭证检测、认证状态查询与 profileArn 预取。Kiro 供应商的实际聊天转发
//! 在代理 handler 中完成（见 `proxy::handlers::handle_kiro_messages`）。

use std::sync::Arc;
use tauri::State;

use crate::proxy::providers::kiro_auth::{has_kirocli_credentials, KiroAuthManager};

/// Kiro 认证状态（注册为 Tauri 全局 State）
pub struct KiroAuthState(pub Arc<KiroAuthManager>);

/// 本地是否存在可直接继承的 kiro-cli 登录凭证
#[tauri::command]
pub async fn kiro_has_cli_credentials() -> Result<bool, String> {
    Ok(has_kirocli_credentials())
}

/// 当前是否已认证（内存凭证或本地 kiro-cli 任一存在）
#[tauri::command]
pub async fn kiro_is_authenticated(state: State<'_, KiroAuthState>) -> Result<bool, String> {
    Ok(state.0.is_authenticated().await)
}

/// 预取 profileArn（同时验证 token 与代理链路可用）。
/// `proxyUrl` 为上游代理（如 clash `http://127.0.0.1:7897`）。
#[tauri::command(rename_all = "camelCase")]
pub async fn kiro_prefetch_profile(
    proxy_url: Option<String>,
    state: State<'_, KiroAuthState>,
) -> Result<String, String> {
    if proxy_url.is_some() {
        state.0.set_proxy(proxy_url);
    }
    state
        .0
        .get_profile_arn()
        .await
        .map_err(|e| e.to_string())
}
