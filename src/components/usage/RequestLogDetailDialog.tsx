import { useTranslation } from "react-i18next";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { ScrollArea } from "@/components/ui/scroll-area";
import { useRequestDetail, useRequestLogDetails } from "@/lib/query/usage";

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

/** kind → 展示标题（带 i18n 兜底）。 */
function kindLabel(kind: string, t: (k: string, o?: any) => string): string {
  switch (kind) {
    case "request_original":
      return t("usage.requestOriginal", { defaultValue: "原始请求" });
    case "request_upstream":
      return t("usage.requestUpstream", {
        defaultValue: "转接后请求（发往上游）",
      });
    case "response_original":
      return t("usage.responseOriginal", { defaultValue: "上游响应" });
    case "response_upstream":
      return t("usage.responseUpstream", { defaultValue: "转换后响应" });
    default:
      return kind;
  }
}

function Block({ title, content }: { title: string; content: string }) {
  if (!content) return null;
  return (
    <div className="space-y-1">
      <div className="text-xs font-medium text-muted-foreground">{title}</div>
      <pre className="max-h-64 overflow-auto rounded-md border border-border/50 bg-muted/30 p-2 text-[11px] leading-relaxed whitespace-pre-wrap break-all font-mono">
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

  const hasPayloads = (payloads?.length ?? 0) > 0;

  return (
    <Dialog
      open={!!requestId}
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      <DialogContent
        className="max-w-2xl"
        zIndex="nested"
        // 允许点击遮罩/空白处关闭（覆盖基础组件默认阻止外部交互的行为）
        onInteractOutside={() => {}}
      >
        <DialogHeader>
          <DialogTitle>
            {t("usage.requestDetail", { defaultValue: "请求详情" })}
          </DialogTitle>
        </DialogHeader>
        <ScrollArea className="flex-1 px-6 py-4">
          {isLoading ? (
            <div className="h-32 animate-pulse rounded bg-muted/40" />
          ) : (
            <div className="space-y-4">
              {detail && (
                <div className="grid grid-cols-2 gap-x-4 gap-y-1 text-xs">
                  <div className="text-muted-foreground">
                    {t("usage.provider")}
                  </div>
                  <div className="text-right font-mono">
                    {detail.providerName || detail.providerId}
                  </div>
                  <div className="text-muted-foreground">
                    {t("usage.billingModel")}
                  </div>
                  <div className="text-right font-mono break-all">
                    {detail.requestModel && detail.requestModel !== detail.model
                      ? `${detail.requestModel} → ${detail.model}`
                      : detail.model}
                  </div>
                  <div className="text-muted-foreground">
                    {t("usage.status")}
                  </div>
                  <div className="text-right font-mono">
                    {detail.statusCode}
                  </div>
                  <div className="text-muted-foreground">
                    {t("usage.inputTokens")} / {t("usage.outputTokens")}
                  </div>
                  <div className="text-right font-mono">
                    {detail.inputTokens} / {detail.outputTokens}
                  </div>
                  <div className="text-muted-foreground">{t("usage.time")}</div>
                  <div className="text-right font-mono">
                    {new Date(detail.createdAt * 1000).toLocaleString()}
                  </div>
                </div>
              )}

              {hasPayloads ? (
                payloads!.map((p) => (
                  <div key={p.kind} className="space-y-2">
                    <div className="text-xs font-semibold">
                      {kindLabel(p.kind, t)}
                    </div>
                    <Block
                      title={t("usage.headers", { defaultValue: "请求头" })}
                      content={prettyJson(p.headers)}
                    />
                    <Block
                      title={t("usage.body", { defaultValue: "请求体" })}
                      content={prettyJson(p.body)}
                    />
                  </div>
                ))
              ) : (
                <div className="text-center text-xs text-muted-foreground py-4">
                  {t("usage.noRequestDetail", {
                    defaultValue: "该请求未记录详细请求数据",
                  })}
                </div>
              )}
            </div>
          )}
        </ScrollArea>
      </DialogContent>
    </Dialog>
  );
}
