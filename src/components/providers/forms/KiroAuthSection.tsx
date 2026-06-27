import { useEffect, useState } from "react";
import { kiroHasCliCredentials } from "@/lib/api/kiro";

/**
 * Kiro (Amazon Q) 认证状态区块。
 *
 * Kiro 复用本机 kiro-cli 的登录凭证，无需在 cc-switch 内输入 API Key。
 * 这里检测本地是否已存在 kiro-cli 登录态并给出提示。
 */
export function KiroAuthSection() {
  const [status, setStatus] = useState<"checking" | "found" | "missing">(
    "checking",
  );

  useEffect(() => {
    let alive = true;
    kiroHasCliCredentials()
      .then((has) => {
        if (alive) setStatus(has ? "found" : "missing");
      })
      .catch(() => {
        if (alive) setStatus("missing");
      });
    return () => {
      alive = false;
    };
  }, []);

  return (
    <div className="rounded-lg border border-violet-200 bg-violet-50 p-3 text-sm dark:border-violet-900/50 dark:bg-violet-950/30">
      <div className="font-medium text-violet-900 dark:text-violet-200">
        Kiro (Amazon Q / CodeWhisperer)
      </div>
      <p className="mt-1 text-violet-700 dark:text-violet-300">
        复用本机 <code>kiro-cli</code> 的登录凭证，无需填写 API Key。请求经本地代理转发到 CodeWhisperer（默认模型 claude-sonnet-4.5）。
      </p>
      <div className="mt-2">
        {status === "checking" && (
          <span className="text-violet-600 dark:text-violet-400">
            正在检测本机 kiro-cli 登录态…
          </span>
        )}
        {status === "found" && (
          <span className="text-emerald-600 dark:text-emerald-400">
            ✓ 已检测到 kiro-cli 登录凭证，将直接继承使用。
          </span>
        )}
        {status === "missing" && (
          <span className="text-amber-600 dark:text-amber-400">
            未检测到 kiro-cli 登录。请先安装并运行{" "}
            <code>kiro-cli</code> 完成登录（浏览器授权），再回到这里保存。
          </span>
        )}
      </div>
    </div>
  );
}
