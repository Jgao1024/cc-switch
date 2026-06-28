import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, Copy } from "lucide-react";
import { toast } from "sonner";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useRequestDetail, useRequestLogDetails } from "@/lib/query/usage";
import type { RequestLogPayload } from "@/types/usage";

interface RequestLogDetailDialogProps {
  /** 选中的请求 ID；为 null 时对话框关闭 */
  requestId: string | null;
  onClose: () => void;
}

/** 尝试把 JSON 字符串格式化为缩进文本；失败则原样返回。 */
function prettyJson(value?: string): string {
  if (!value) return "";
  try {
    return JSON.stringify(JSON.parse(value), null, 2);
  } catch {
    return value;
  }
}

/** kind → 标签页标题（带 i18n 兜底）。 */
function kindLabel(kind: string, t: (k: string, o?: any) => string): string {
  switch (kind) {
    case "request_original":
      return t("usage.requestOriginal", { defaultValue: "原始请求" });
    case "request_upstream":
      return t("usage.requestUpstream", { defaultValue: "转接后请求" });
    case "response_original":
      return t("usage.responseOriginal", { defaultValue: "上游响应" });
    case "response_upstream":
      return t("usage.responseUpstream", { defaultValue: "转换后响应" });
    default:
      return kind;
  }
}

/** 一键复制按钮（复制后短暂显示 ✓）。 */
function CopyButton({ text, label }: { text: string; label: string }) {
  const [copied, setCopied] = useState(false);
  const { t } = useTranslation();
  const onCopy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    } catch {
      toast.error(t("common.error", { defaultValue: "出错了" }));
    }
  };
  return (
    <button
      type="button"
      onClick={onCopy}
      className="inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[11px] text-muted-foreground hover:bg-muted hover:text-foreground transition-colors"
      title={label}
    >
      {copied ? (
        <Check className="h-3 w-3 text-green-500" />
      ) : (
        <Copy className="h-3 w-3" />
      )}
      {label}
    </button>
  );
}

/** 一段可复制的代码块（headers / body）。 */
function Section({ title, content }: { title: string; content: string }) {
  const { t } = useTranslation();
  if (!content) return null;
  return (
    <div className="space-y-1">
      <div className="flex items-center justify-between">
        <span className="text-xs font-medium text-muted-foreground">
          {title}
        </span>
        <CopyButton
          text={content}
          label={t("common.copy", { defaultValue: "复制" })}
        />
      </div>
      <pre className="rounded-md border border-border/50 bg-muted/30 p-2 text-[11px] leading-relaxed whitespace-pre-wrap break-all font-mono">
        {content}
      </pre>
    </div>
  );
}

export function RequestLogDetailDialog({
  requestId,
  onClose,
}: RequestLogDetailDialogProps) {
  const { t } = useTranslation();
  const { data: detail } = useRequestDetail(requestId ?? "");
  const { data: payloads, isLoading } = useRequestLogDetails(requestId ?? "");

  // 标签页顺序：原始 → 转接后 → 响应（按已知 kind 排序，未知 kind 排末尾）
  const order = [
    "request_original",
    "request_upstream",
    "response_original",
    "response_upstream",
  ];
  const sortedPayloads = useMemo<RequestLogPayload[]>(() => {
    const list = [...(payloads ?? [])];
    list.sort((a, b) => {
      const ia = order.indexOf(a.kind);
      const ib = order.indexOf(b.kind);
      return (ia < 0 ? 99 : ia) - (ib < 0 ? 99 : ib);
    });
    return list;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [payloads]);

  const hasPayloads = sortedPayloads.length > 0;
  const defaultTab = hasPayloads ? sortedPayloads[0].kind : undefined;

  return (
    <Dialog
      open={!!requestId}
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      <DialogContent
        className="max-w-3xl"
        zIndex="nested"
        // 允许点击遮罩/空白处关闭（覆盖基础组件默认阻止外部交互的行为）
        onInteractOutside={() => {}}
      >
        <DialogHeader>
          <DialogTitle>
            {t("usage.requestDetail", { defaultValue: "请求详情" })}
          </DialogTitle>
        </DialogHeader>

        {/* flex-1 + min-h-0 让内容区在 max-h-[90vh] 内可正常滚动 */}
        <div className="flex flex-1 min-h-0 flex-col gap-3 px-6 py-4">
          {/* 元信息：始终可见，不参与滚动 */}
          {detail && (
            <div className="grid flex-shrink-0 grid-cols-2 gap-x-6 gap-y-1 text-xs sm:grid-cols-4">
              <Meta label={t("usage.provider")}>
                {detail.providerName || detail.providerId}
              </Meta>
              <Meta label={t("usage.status")}>{detail.statusCode}</Meta>
              <Meta label={t("usage.billingModel")}>
                {detail.requestModel && detail.requestModel !== detail.model
                  ? `${detail.requestModel} → ${detail.model}`
                  : detail.model}
              </Meta>
              <Meta
                label={`${t("usage.inputTokens")}/${t("usage.outputTokens")}`}
              >
                {detail.inputTokens}/{detail.outputTokens}
              </Meta>
            </div>
          )}

          {isLoading ? (
            <div className="h-32 flex-1 animate-pulse rounded bg-muted/40" />
          ) : hasPayloads ? (
            <Tabs
              defaultValue={defaultTab}
              className="flex min-h-0 flex-1 flex-col"
            >
              <TabsList className="flex-shrink-0 self-start">
                {sortedPayloads.map((p) => (
                  <TabsTrigger key={p.kind} value={p.kind} className="min-w-0">
                    {kindLabel(p.kind, t)}
                  </TabsTrigger>
                ))}
              </TabsList>
              {sortedPayloads.map((p) => (
                <TabsContent
                  key={p.kind}
                  value={p.kind}
                  className="min-h-0 flex-1 space-y-3 overflow-auto pr-1"
                >
                  <Section
                    title={t("usage.headers", { defaultValue: "请求头" })}
                    content={prettyJson(p.headers)}
                  />
                  <Section
                    title={t("usage.body", { defaultValue: "请求体" })}
                    content={prettyJson(p.body)}
                  />
                  {!p.headers && !p.body && (
                    <div className="py-4 text-center text-xs text-muted-foreground">
                      {t("usage.noRequestDetail", {
                        defaultValue: "该请求未记录详细请求数据",
                      })}
                    </div>
                  )}
                </TabsContent>
              ))}
            </Tabs>
          ) : (
            <div className="flex-1 py-8 text-center text-xs text-muted-foreground">
              {t("usage.noRequestDetail", {
                defaultValue: "该请求未记录详细请求数据",
              })}
            </div>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}

function Meta({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-muted-foreground">{label}</span>
      <span className="break-all font-mono">{children}</span>
    </div>
  );
}
