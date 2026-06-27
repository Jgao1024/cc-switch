import { useTranslation } from "react-i18next";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { ScrollArea } from "@/components/ui/scroll-area";
import { useRequestDetail } from "@/lib/query/usage";

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

function Section({ title, content }: { title: string; content: string }) {
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
  const { data: detail, isLoading } = useRequestDetail(requestId ?? "");

  return (
    <Dialog
      open={!!requestId}
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      <DialogContent className="max-w-2xl" zIndex="nested">
        <DialogHeader>
          <DialogTitle>
            {t("usage.requestDetail", { defaultValue: "请求详情" })}
          </DialogTitle>
        </DialogHeader>
        <ScrollArea className="flex-1 px-6 py-4">
          {isLoading || !detail ? (
            <div className="h-32 animate-pulse rounded bg-muted/40" />
          ) : (
            <div className="space-y-4">
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
                <div className="text-muted-foreground">{t("usage.status")}</div>
                <div className="text-right font-mono">{detail.statusCode}</div>
                <div className="text-muted-foreground">
                  {t("usage.inputTokens")} / {t("usage.outputTokens")}
                </div>
                <div className="text-right font-mono">
                  {detail.inputTokens} / {detail.outputTokens}
                </div>
                {detail.credits != null &&
                  Number.parseFloat(detail.credits) > 0 && (
                    <>
                      <div className="text-muted-foreground">
                        {t("usage.credits", { defaultValue: "Credits" })}
                      </div>
                      <div className="text-right font-mono">
                        {detail.credits}
                      </div>
                    </>
                  )}
                <div className="text-muted-foreground">{t("usage.time")}</div>
                <div className="text-right font-mono">
                  {new Date(detail.createdAt * 1000).toLocaleString()}
                </div>
              </div>

              {detail.errorMessage && (
                <Section
                  title={t("usage.errorMessage", { defaultValue: "错误信息" })}
                  content={detail.errorMessage}
                />
              )}

              <Section
                title={t("usage.requestHeaders", {
                  defaultValue: "原始请求头",
                })}
                content={prettyJson(detail.requestHeaders)}
              />
              <Section
                title={t("usage.requestBody", { defaultValue: "原始请求体" })}
                content={prettyJson(detail.requestBody)}
              />
              <Section
                title={t("usage.upstreamRequestBody", {
                  defaultValue: "转接后请求体（发往上游）",
                })}
                content={prettyJson(detail.upstreamRequestBody)}
              />

              {!detail.requestHeaders &&
                !detail.requestBody &&
                !detail.upstreamRequestBody && (
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
