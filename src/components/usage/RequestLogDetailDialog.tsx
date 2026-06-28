import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  ArrowLeft,
  Check,
  ChevronRight,
  Copy,
  CornerDownLeft,
  Wrench,
} from "lucide-react";
import { toast } from "sonner";
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  isLinux,
  isWindows,
  DRAG_REGION_ATTR,
  DRAG_REGION_STYLE,
} from "@/lib/platform";
import { useRequestDetail, useRequestLogDetails } from "@/lib/query/usage";
import type { RequestLogPayload } from "@/types/usage";
import {
  parseRequestBody,
  type ParsedRequest,
  type TraceBlock,
  type TraceMessage,
} from "./traceParse";

interface RequestLogDetailDialogProps {
  /** 选中的请求 ID；为 null 时对话框关闭 */
  requestId: string | null;
  onClose: () => void;
}

const KIND_ORDER = [
  "request_original",
  "request_upstream",
  "response_original",
  "response_upstream",
];

// 与 FullScreenPanel（设置页）保持一致：macOS 顶部预留 28px 拖拽占位，
// 头部高度 64px，使返回按钮与窗口控制按钮错开、垂直居中对齐。
const DRAG_BAR_HEIGHT = isWindows() || isLinux() ? 0 : 28;
const HEADER_HEIGHT = 64;

function prettyJson(value?: string): string {
  if (!value) return "";
  try {
    return JSON.stringify(JSON.parse(value), null, 2);
  } catch {
    return value;
  }
}

function prettyValue(value: unknown): string {
  if (value == null) return "";
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

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
function CopyButton({ text, label }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false);
  const { t } = useTranslation();
  const onCopy = async (e: React.MouseEvent) => {
    e.stopPropagation();
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
      className="inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[11px] text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
      title={label ?? t("common.copy", { defaultValue: "复制" })}
    >
      {copied ? (
        <Check className="h-3 w-3 text-green-500" />
      ) : (
        <Copy className="h-3 w-3" />
      )}
      {label ?? t("common.copy", { defaultValue: "复制" })}
    </button>
  );
}

/** 可折叠分区（默认展开）。 */
function Collapsible({
  title,
  count,
  defaultOpen = true,
  copyText,
  children,
}: {
  title: string;
  count?: number;
  defaultOpen?: boolean;
  copyText?: string;
  children: React.ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <div className="rounded-lg border border-border/60 bg-card/40">
      <div className="flex items-center justify-between px-3 py-2">
        <button
          type="button"
          onClick={() => setOpen((o) => !o)}
          className="flex items-center gap-1.5 text-sm font-semibold"
        >
          <ChevronRight
            className={`h-4 w-4 transition-transform ${open ? "rotate-90" : ""}`}
          />
          {title}
          {count != null && (
            <span className="rounded-full bg-muted px-1.5 text-xs text-muted-foreground">
              {count}
            </span>
          )}
        </button>
        {copyText != null && <CopyButton text={copyText} />}
      </div>
      {open && <div className="border-t border-border/60 p-3">{children}</div>}
    </div>
  );
}

const ROLE_STYLES: Record<string, string> = {
  user: "bg-blue-500/10 text-blue-600 dark:text-blue-400 border-blue-500/30",
  assistant:
    "bg-green-500/10 text-green-600 dark:text-green-400 border-green-500/30",
  system: "bg-muted text-muted-foreground border-border",
  developer: "bg-muted text-muted-foreground border-border",
  tool: "bg-amber-500/10 text-amber-600 dark:text-amber-400 border-amber-500/30",
};

function Pre({ children }: { children: string }) {
  return (
    <pre className="overflow-x-auto whitespace-pre-wrap break-words rounded-md bg-muted/40 p-2 font-mono text-[11px] leading-relaxed">
      {children}
    </pre>
  );
}

function BlockView({ block }: { block: TraceBlock }) {
  const { t } = useTranslation();
  switch (block.kind) {
    case "text":
      return block.text ? (
        <div className="whitespace-pre-wrap break-words text-[13px] leading-relaxed">
          {block.text}
        </div>
      ) : (
        <div className="text-xs italic text-muted-foreground">
          {t("usage.emptyContent", { defaultValue: "（空）" })}
        </div>
      );
    case "tool_use":
      return (
        <div className="rounded-md border border-amber-500/30 bg-amber-500/5 p-2">
          <div className="mb-1 flex items-center gap-1 text-xs font-medium text-amber-600 dark:text-amber-400">
            <Wrench className="h-3 w-3" />
            {t("usage.toolCall", { defaultValue: "调用工具" })}: {block.name}
          </div>
          <Pre>{prettyValue(block.input)}</Pre>
        </div>
      );
    case "tool_result":
      return (
        <div
          className={`rounded-md border p-2 ${
            block.isError
              ? "border-red-500/40 bg-red-500/5"
              : "border-border/60 bg-muted/20"
          }`}
        >
          <div className="mb-1 flex items-center gap-1 text-xs font-medium text-muted-foreground">
            <CornerDownLeft className="h-3 w-3" />
            {t("usage.toolResult", { defaultValue: "工具结果" })}
            {block.isError && (
              <span className="text-red-500">
                {" "}
                ({t("usage.error", { defaultValue: "错误" })})
              </span>
            )}
          </div>
          <Pre>{block.content}</Pre>
        </div>
      );
    case "image":
      return (
        <div className="text-xs italic text-muted-foreground">{block.note}</div>
      );
    default:
      return <Pre>{prettyValue(block.raw)}</Pre>;
  }
}

function MessageCard({ message }: { message: TraceMessage }) {
  const roleClass = ROLE_STYLES[message.role] ?? ROLE_STYLES.system;
  return (
    <div className="rounded-lg border border-border/60 bg-card/40 p-3">
      <div
        className={`mb-2 inline-block rounded border px-1.5 py-0.5 text-[11px] font-medium uppercase ${roleClass}`}
      >
        {message.role}
      </div>
      <div className="space-y-2">
        {message.blocks.map((b, i) => (
          <BlockView key={i} block={b} />
        ))}
      </div>
    </div>
  );
}

/** 语义视图：系统提示词 / 参数 / 工具 / 消息。 */
function SemanticView({
  parsed,
  rawBody,
}: {
  parsed: ParsedRequest;
  rawBody: string;
}) {
  const { t } = useTranslation();
  const paramEntries = Object.entries(parsed.params);
  return (
    <div className="space-y-3">
      {paramEntries.length > 0 && (
        <div className="flex flex-wrap gap-1.5">
          {paramEntries.map(([k, v]) => (
            <span
              key={k}
              className="rounded border border-border/60 bg-muted/30 px-1.5 py-0.5 font-mono text-[11px]"
              title={prettyValue(v)}
            >
              <span className="text-muted-foreground">{k}</span>
              {": "}
              <span className="break-all">
                {typeof v === "object"
                  ? Array.isArray(v)
                    ? `[${(v as unknown[]).length}]`
                    : "{…}"
                  : String(v)}
              </span>
            </span>
          ))}
        </div>
      )}

      {parsed.system && (
        <Collapsible
          title={t("usage.systemPrompt", { defaultValue: "系统提示词" })}
          copyText={parsed.system}
        >
          <div className="whitespace-pre-wrap break-words text-[13px] leading-relaxed">
            {parsed.system}
          </div>
        </Collapsible>
      )}

      {parsed.tools.length > 0 && (
        <Collapsible
          title={t("usage.tools", { defaultValue: "工具" })}
          count={parsed.tools.length}
          defaultOpen={false}
        >
          <div className="space-y-2">
            {parsed.tools.map((tool, i) => (
              <div
                key={`${tool.name}-${i}`}
                className="rounded-md border border-border/50 bg-muted/20 p-2"
              >
                <div className="flex items-center gap-1 text-xs font-semibold">
                  <Wrench className="h-3 w-3 text-muted-foreground" />
                  {tool.name}
                </div>
                {tool.description && (
                  <div className="mt-1 text-xs text-muted-foreground">
                    {tool.description}
                  </div>
                )}
                {tool.schema != null && (
                  <details className="mt-1">
                    <summary className="cursor-pointer text-[11px] text-muted-foreground">
                      {t("usage.schema", { defaultValue: "参数schema" })}
                    </summary>
                    <Pre>{prettyValue(tool.schema)}</Pre>
                  </details>
                )}
              </div>
            ))}
          </div>
        </Collapsible>
      )}

      <Collapsible
        title={t("usage.messages", { defaultValue: "消息" })}
        count={parsed.messages.length}
        copyText={rawBody}
      >
        {parsed.messages.length > 0 ? (
          <div className="space-y-2">
            {parsed.messages.map((m, i) => (
              <MessageCard key={i} message={m} />
            ))}
          </div>
        ) : (
          <div className="text-xs italic text-muted-foreground">
            {t("usage.emptyContent", { defaultValue: "（空）" })}
          </div>
        )}
      </Collapsible>
    </div>
  );
}

/** 单个 payload（原始/转接后）的内容区。 */
function PayloadView({
  payload,
  rawMode,
}: {
  payload: RequestLogPayload;
  rawMode: boolean;
}) {
  const { t } = useTranslation();
  const prettyBody = useMemo(() => prettyJson(payload.body), [payload.body]);
  const parsed = useMemo(() => parseRequestBody(payload.body), [payload.body]);
  const canSemantic = parsed.format !== "unknown";

  return (
    <div className="space-y-3">
      {payload.headers && (
        <Collapsible
          title={t("usage.headers", { defaultValue: "请求头" })}
          defaultOpen={false}
          copyText={prettyJson(payload.headers)}
        >
          <Pre>{prettyJson(payload.headers)}</Pre>
        </Collapsible>
      )}

      {!payload.body ? (
        <div className="py-6 text-center text-xs text-muted-foreground">
          {t("usage.noRequestDetail", {
            defaultValue: "该请求未记录详细请求数据",
          })}
        </div>
      ) : rawMode || !canSemantic ? (
        <Collapsible
          title={t("usage.body", { defaultValue: "请求体" })}
          copyText={prettyBody}
        >
          <Pre>{prettyBody}</Pre>
        </Collapsible>
      ) : (
        <SemanticView parsed={parsed} rawBody={prettyBody} />
      )}
    </div>
  );
}

export function RequestLogDetailDialog({
  requestId,
  onClose,
}: RequestLogDetailDialogProps) {
  const { t } = useTranslation();
  const [rawMode, setRawMode] = useState(false);
  const { data: detail } = useRequestDetail(requestId ?? "");
  const { data: payloads, isLoading } = useRequestLogDetails(requestId ?? "");

  const sortedPayloads = useMemo<RequestLogPayload[]>(() => {
    const list = [...(payloads ?? [])];
    list.sort(
      (a, b) =>
        (KIND_ORDER.indexOf(a.kind) < 0 ? 99 : KIND_ORDER.indexOf(a.kind)) -
        (KIND_ORDER.indexOf(b.kind) < 0 ? 99 : KIND_ORDER.indexOf(b.kind)),
    );
    return list;
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
        variant="fullscreen"
        zIndex="top"
        className="p-0 sm:rounded-none"
        onInteractOutside={() => {}}
      >
        <div className="flex h-full flex-col">
          {/* 顶部拖拽占位：避开 macOS 窗口控制按钮（与设置页 FullScreenPanel 一致） */}
          {DRAG_BAR_HEIGHT > 0 && (
            <div
              data-tauri-drag-region
              style={
                {
                  WebkitAppRegion: "drag",
                  height: DRAG_BAR_HEIGHT,
                } as React.CSSProperties
              }
            />
          )}
          {/* 顶部栏：返回 + 标题（高度/对齐与设置页一致） */}
          <div
            className="flex flex-shrink-0 items-center"
            {...DRAG_REGION_ATTR}
            style={
              {
                ...DRAG_REGION_STYLE,
                height: HEADER_HEIGHT,
              } as React.CSSProperties
            }
          >
            <div
              className="flex w-full items-center gap-4 px-6"
              {...DRAG_REGION_ATTR}
              style={{ ...DRAG_REGION_STYLE } as React.CSSProperties}
            >
              <DialogClose asChild>
                <Button
                  type="button"
                  variant="outline"
                  size="icon"
                  className="select-none rounded-lg"
                  style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
                  aria-label={t("common.back", { defaultValue: "返回" })}
                >
                  <ArrowLeft className="h-4 w-4" />
                </Button>
              </DialogClose>
              <DialogTitle className="select-none text-lg font-semibold text-foreground">
                {t("usage.requestDetail", { defaultValue: "请求详情" })}
              </DialogTitle>
            </div>
          </div>
          {/* 元信息条 */}
          {detail && (
            <div className="flex flex-shrink-0 flex-wrap gap-x-5 gap-y-1 border-b border-border-default bg-muted/30 px-6 py-2 text-xs">
              <Meta label={t("usage.provider")}>
                {detail.providerName || detail.providerId}
              </Meta>
              <Meta label={t("usage.billingModel")}>
                {detail.requestModel && detail.requestModel !== detail.model
                  ? `${detail.requestModel} → ${detail.model}`
                  : detail.model}
              </Meta>
              <Meta label={t("usage.status")}>{detail.statusCode}</Meta>
              <Meta
                label={`${t("usage.inputTokens")}/${t("usage.outputTokens")}`}
              >
                {detail.inputTokens}/{detail.outputTokens}
              </Meta>
              <Meta label={t("usage.latency", { defaultValue: "耗时" })}>
                {(detail.latencyMs / 1000).toFixed(1)}s
              </Meta>
              <Meta label={t("usage.time")}>
                {new Date(detail.createdAt * 1000).toLocaleString()}
              </Meta>
            </div>
          )}

          {/* 主体 */}
          {isLoading ? (
            <div className="flex-1 p-6">
              <div className="h-40 animate-pulse rounded bg-muted/40" />
            </div>
          ) : hasPayloads ? (
            <Tabs
              defaultValue={defaultTab}
              className="flex min-h-0 flex-1 flex-col"
            >
              <div className="flex flex-shrink-0 items-center justify-between border-b border-border/60 px-6 py-2">
                <TabsList>
                  {sortedPayloads.map((p) => (
                    <TabsTrigger
                      key={p.kind}
                      value={p.kind}
                      className="min-w-0"
                    >
                      {kindLabel(p.kind, t)}
                    </TabsTrigger>
                  ))}
                </TabsList>
                <button
                  type="button"
                  onClick={() => setRawMode((r) => !r)}
                  className="rounded-md border border-border/60 px-2 py-1 text-xs text-muted-foreground hover:bg-muted hover:text-foreground"
                >
                  {rawMode
                    ? t("usage.semanticView", { defaultValue: "语义视图" })
                    : t("usage.rawJson", { defaultValue: "原始 JSON" })}
                </button>
              </div>
              {sortedPayloads.map((p) => (
                <TabsContent
                  key={p.kind}
                  value={p.kind}
                  className="mt-0 min-h-0 flex-1 overflow-y-auto px-6 py-4"
                >
                  <div className="mx-auto max-w-4xl">
                    <PayloadView payload={p} rawMode={rawMode} />
                  </div>
                </TabsContent>
              ))}
            </Tabs>
          ) : (
            <div className="flex-1 py-12 text-center text-sm text-muted-foreground">
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
    <span className="inline-flex items-center gap-1">
      <span className="text-muted-foreground">{label}:</span>
      <span className="break-all font-mono">{children}</span>
    </span>
  );
}
