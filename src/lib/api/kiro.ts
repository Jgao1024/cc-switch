/**
 * Kiro (Amazon Q / CodeWhisperer) API
 *
 * Kiro 供应商复用本机 kiro-cli 的登录凭证（AWS SSO OIDC），经 CodeWhisperer
 * 后端提供 Claude / Codex 能力。聊天转发在本地代理中完成；这里仅暴露凭证
 * 检测与 profileArn 预取（用于"配置时检测登录态"）。
 *
 * 后端命令见 `src-tauri/src/commands/kiro.rs`。
 */

import { invoke } from "@tauri-apps/api/core";

/**
 * 本地是否存在可直接继承的 kiro-cli 登录凭证。
 *
 * 用于配置 Kiro 供应商时检测：
 * - true：已安装并登录过 kiro-cli，可直接继承，无需再登录
 * - false：需要引导用户登录（kiro-cli 登录或后续在 cc-switch 内 OAuth）
 */
export async function kiroHasCliCredentials(): Promise<boolean> {
  return invoke<boolean>("kiro_has_cli_credentials");
}

/**
 * 当前是否已认证（内存凭证或本地 kiro-cli 任一存在）。
 */
export async function kiroIsAuthenticated(): Promise<boolean> {
  return invoke<boolean>("kiro_is_authenticated");
}

/**
 * 预取 profileArn —— 同时验证 token 与上游代理链路是否可用。
 *
 * @param proxyUrl 上游代理（如 clash `http://127.0.0.1:7897`）
 * @returns CodeWhisperer profileArn（成功即表示链路打通）
 */
export async function kiroPrefetchProfile(proxyUrl?: string): Promise<string> {
  return invoke<string>("kiro_prefetch_profile", { proxyUrl });
}
