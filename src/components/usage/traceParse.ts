/**
 * 请求体语义解析：把 Anthropic / OpenAI Chat / OpenAI Responses 三种请求体
 * 解析成统一的「系统提示词 + 工具 + 消息 + 参数」结构，供详情查看器卡片化展示。
 *
 * 设计原则：
 * - 纯函数、无副作用，便于单测。
 * - 极度防御：任何字段缺失/类型不符都不抛错，识别不了就返回 format="unknown"，
 *   由调用方回退到原始 JSON 展示。
 */

export type TraceFormat =
  | "anthropic"
  | "openai-chat"
  | "openai-responses"
  | "unknown";

export type TraceBlock =
  | { kind: "text"; text: string }
  | { kind: "tool_use"; name: string; id?: string; input: unknown }
  | {
      kind: "tool_result";
      toolUseId?: string;
      content: string;
      isError?: boolean;
    }
  | { kind: "image"; note: string }
  | { kind: "unknown"; raw: unknown };

export interface TraceMessage {
  role: string;
  blocks: TraceBlock[];
}

export interface TraceTool {
  name: string;
  description?: string;
  schema?: unknown;
}

export interface ParsedRequest {
  format: TraceFormat;
  /** 合并后的系统提示词文本（可能为空） */
  system?: string;
  tools: TraceTool[];
  messages: TraceMessage[];
  /** 模型参数（temperature / max_tokens / stream 等基础类型字段） */
  params: Record<string, unknown>;
}

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** 把 anthropic/responses 风格的「文本块数组或字符串」合并为纯文本。 */
function joinText(value: unknown): string {
  if (value == null) return "";
  if (typeof value === "string") return value;
  if (Array.isArray(value)) {
    return value
      .map((item) => {
        if (typeof item === "string") return item;
        if (isObject(item) && typeof item.text === "string") return item.text;
        return "";
      })
      .filter(Boolean)
      .join("\n");
  }
  if (isObject(value) && typeof value.text === "string") return value.text;
  return "";
}

function stringify(value: unknown): string {
  if (value == null) return "";
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

/** 收集顶层基础参数（排除 system/messages/tools/input/instructions）。 */
function collectParams(obj: Record<string, unknown>): Record<string, unknown> {
  const skip = new Set([
    "system",
    "messages",
    "tools",
    "input",
    "instructions",
  ]);
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(obj)) {
    if (skip.has(k)) continue;
    // 仅保留基础类型与小对象，避免把大块内容塞进参数区
    if (
      v == null ||
      typeof v === "string" ||
      typeof v === "number" ||
      typeof v === "boolean"
    ) {
      out[k] = v;
    } else if (isObject(v) || Array.isArray(v)) {
      // 小对象/数组（如 tool_choice、reasoning、metadata）保留
      const s = stringify(v);
      if (s.length <= 500) out[k] = v;
    }
  }
  return out;
}

// ---------- Anthropic ----------

function parseAnthropicContent(content: unknown): TraceBlock[] {
  if (typeof content === "string") return [{ kind: "text", text: content }];
  if (!Array.isArray(content))
    return [{ kind: "text", text: stringify(content) }];
  return content.map((block): TraceBlock => {
    if (!isObject(block)) return { kind: "unknown", raw: block };
    switch (block.type) {
      case "text":
        return { kind: "text", text: String(block.text ?? "") };
      case "tool_use":
        return {
          kind: "tool_use",
          name: String(block.name ?? "tool"),
          id: typeof block.id === "string" ? block.id : undefined,
          input: block.input,
        };
      case "tool_result":
        return {
          kind: "tool_result",
          toolUseId:
            typeof block.tool_use_id === "string"
              ? block.tool_use_id
              : undefined,
          content: joinText(block.content) || stringify(block.content),
          isError: block.is_error === true,
        };
      case "image":
        return { kind: "image", note: "[image]" };
      default:
        return { kind: "unknown", raw: block };
    }
  });
}

function parseAnthropic(obj: Record<string, unknown>): ParsedRequest {
  const tools: TraceTool[] = Array.isArray(obj.tools)
    ? obj.tools.filter(isObject).map((t) => ({
        name: String(t.name ?? "tool"),
        description:
          typeof t.description === "string" ? t.description : undefined,
        schema: t.input_schema,
      }))
    : [];
  const messages: TraceMessage[] = Array.isArray(obj.messages)
    ? obj.messages.filter(isObject).map((m) => ({
        role: String(m.role ?? "user"),
        blocks: parseAnthropicContent(m.content),
      }))
    : [];
  return {
    format: "anthropic",
    system: joinText(obj.system) || undefined,
    tools,
    messages,
    params: collectParams(obj),
  };
}

// ---------- OpenAI Chat Completions ----------

function parseOpenAIChat(obj: Record<string, unknown>): ParsedRequest {
  const tools: TraceTool[] = Array.isArray(obj.tools)
    ? obj.tools.filter(isObject).map((t) => {
        const fn = isObject(t.function) ? t.function : undefined;
        return {
          name: String(fn?.name ?? t.name ?? "tool"),
          description:
            typeof fn?.description === "string" ? fn.description : undefined,
          schema: fn?.parameters,
        };
      })
    : [];

  const systemParts: string[] = [];
  const messages: TraceMessage[] = [];
  if (Array.isArray(obj.messages)) {
    for (const m of obj.messages) {
      if (!isObject(m)) continue;
      const role = String(m.role ?? "user");
      if (role === "system" || role === "developer") {
        systemParts.push(joinText(m.content));
        continue;
      }
      const blocks: TraceBlock[] = [];
      const text = joinText(m.content);
      if (text) blocks.push({ kind: "text", text });
      // assistant 工具调用
      if (Array.isArray(m.tool_calls)) {
        for (const call of m.tool_calls) {
          if (!isObject(call)) continue;
          const fn = isObject(call.function) ? call.function : undefined;
          let input: unknown = fn?.arguments;
          if (typeof input === "string") {
            try {
              input = JSON.parse(input);
            } catch {
              /* 保留原始字符串 */
            }
          }
          blocks.push({
            kind: "tool_use",
            name: String(fn?.name ?? "tool"),
            id: typeof call.id === "string" ? call.id : undefined,
            input,
          });
        }
      }
      // tool 角色 = 工具结果
      if (role === "tool") {
        blocks.length = 0;
        blocks.push({
          kind: "tool_result",
          toolUseId:
            typeof m.tool_call_id === "string" ? m.tool_call_id : undefined,
          content: joinText(m.content) || stringify(m.content),
        });
      }
      if (blocks.length === 0) blocks.push({ kind: "text", text: "" });
      messages.push({ role, blocks });
    }
  }
  return {
    format: "openai-chat",
    system: systemParts.filter(Boolean).join("\n") || undefined,
    tools,
    messages,
    params: collectParams(obj),
  };
}

// ---------- OpenAI Responses ----------

function parseResponsesItem(item: unknown): TraceMessage | null {
  if (!isObject(item)) return null;
  const type = item.type;
  // function_call / function_call_output 是独立 item
  if (type === "function_call") {
    let input: unknown = item.arguments;
    if (typeof input === "string") {
      try {
        input = JSON.parse(input);
      } catch {
        /* keep */
      }
    }
    return {
      role: "assistant",
      blocks: [
        {
          kind: "tool_use",
          name: String(item.name ?? "tool"),
          id: typeof item.call_id === "string" ? item.call_id : undefined,
          input,
        },
      ],
    };
  }
  if (type === "function_call_output") {
    return {
      role: "tool",
      blocks: [
        {
          kind: "tool_result",
          toolUseId:
            typeof item.call_id === "string" ? item.call_id : undefined,
          content: joinText(item.output) || stringify(item.output),
        },
      ],
    };
  }
  // message item: { role, content: string | [{type:'input_text'|'output_text', text}] }
  const role = String(item.role ?? "user");
  const text = joinText(item.content);
  return { role, blocks: [{ kind: "text", text }] };
}

function parseOpenAIResponses(obj: Record<string, unknown>): ParsedRequest {
  const tools: TraceTool[] = Array.isArray(obj.tools)
    ? obj.tools.filter(isObject).map((t) => {
        const fn = isObject(t.function) ? t.function : undefined;
        return {
          name: String(fn?.name ?? t.name ?? "tool"),
          description:
            typeof (fn?.description ?? t.description) === "string"
              ? String(fn?.description ?? t.description)
              : undefined,
          schema: fn?.parameters ?? t.parameters,
        };
      })
    : [];
  const messages: TraceMessage[] = [];
  if (Array.isArray(obj.input)) {
    for (const item of obj.input) {
      const parsed = parseResponsesItem(item);
      if (parsed) messages.push(parsed);
    }
  } else if (typeof obj.input === "string") {
    messages.push({
      role: "user",
      blocks: [{ kind: "text", text: obj.input }],
    });
  }
  return {
    format: "openai-responses",
    system: joinText(obj.instructions) || undefined,
    tools,
    messages,
    params: collectParams(obj),
  };
}

// ---------- 格式检测 + 入口 ----------

function detectFormat(obj: Record<string, unknown>): TraceFormat {
  // Responses：有 input（数组或字符串）且无 messages
  if (obj.input !== undefined && obj.messages === undefined) {
    return "openai-responses";
  }
  if (Array.isArray(obj.messages)) {
    // 工具结构区分 anthropic / openai
    if (Array.isArray(obj.tools) && obj.tools.length > 0) {
      const first = obj.tools.find(isObject);
      if (first) {
        if ("input_schema" in first) return "anthropic";
        if ("function" in first || first.type === "function")
          return "openai-chat";
      }
    }
    // 无工具时看 system / max_tokens（anthropic 必填 max_tokens）
    if (obj.system !== undefined || obj.max_tokens !== undefined) {
      // openai chat 没有顶层 system 字段，system 是消息角色
      return "anthropic";
    }
    return "openai-chat";
  }
  return "unknown";
}

/**
 * 解析请求体 JSON 字符串为统一结构。
 * @returns 解析结果；JSON 不合法或无法识别时 format="unknown"。
 */
export function parseRequestBody(body: string | undefined): ParsedRequest {
  const empty: ParsedRequest = {
    format: "unknown",
    tools: [],
    messages: [],
    params: {},
  };
  if (!body) return empty;
  let obj: unknown;
  try {
    obj = JSON.parse(body);
  } catch {
    return empty;
  }
  if (!isObject(obj)) return empty;

  switch (detectFormat(obj)) {
    case "anthropic":
      return parseAnthropic(obj);
    case "openai-chat":
      return parseOpenAIChat(obj);
    case "openai-responses":
      return parseOpenAIResponses(obj);
    default:
      return { ...empty, params: collectParams(obj) };
  }
}
