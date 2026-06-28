import { describe, expect, it } from "vitest";
import { parseRequestBody } from "@/components/usage/traceParse";

describe("parseRequestBody", () => {
  it("returns unknown for invalid / empty input", () => {
    expect(parseRequestBody(undefined).format).toBe("unknown");
    expect(parseRequestBody("").format).toBe("unknown");
    expect(parseRequestBody("not json").format).toBe("unknown");
    expect(parseRequestBody("[1,2,3]").format).toBe("unknown");
  });

  it("parses an Anthropic Messages request", () => {
    const body = JSON.stringify({
      model: "claude-sonnet",
      max_tokens: 1024,
      temperature: 0.7,
      system: [{ type: "text", text: "You are helpful." }],
      tools: [
        {
          name: "get_weather",
          description: "Get weather",
          input_schema: { type: "object", properties: {} },
        },
      ],
      messages: [
        { role: "user", content: "Hello" },
        {
          role: "assistant",
          content: [
            { type: "text", text: "Let me check" },
            {
              type: "tool_use",
              id: "t1",
              name: "get_weather",
              input: { city: "SF" },
            },
          ],
        },
        {
          role: "user",
          content: [
            {
              type: "tool_result",
              tool_use_id: "t1",
              content: [{ type: "text", text: "sunny" }],
            },
          ],
        },
      ],
    });
    const parsed = parseRequestBody(body);
    expect(parsed.format).toBe("anthropic");
    expect(parsed.system).toContain("You are helpful");
    expect(parsed.tools).toHaveLength(1);
    expect(parsed.tools[0].name).toBe("get_weather");
    expect(parsed.messages).toHaveLength(3);
    // assistant message has text + tool_use blocks
    const assistant = parsed.messages[1];
    expect(assistant.role).toBe("assistant");
    expect(assistant.blocks.map((b) => b.kind)).toEqual(["text", "tool_use"]);
    // tool_result content joined to text
    const toolResult = parsed.messages[2].blocks[0];
    expect(toolResult.kind).toBe("tool_result");
    if (toolResult.kind === "tool_result") {
      expect(toolResult.content).toBe("sunny");
      expect(toolResult.toolUseId).toBe("t1");
    }
    // params exclude system/messages/tools
    expect(parsed.params.max_tokens).toBe(1024);
    expect(parsed.params.system).toBeUndefined();
    expect(parsed.params.messages).toBeUndefined();
  });

  it("parses an OpenAI Chat Completions request with tools and tool result", () => {
    const body = JSON.stringify({
      model: "gpt-4o",
      messages: [
        { role: "system", content: "Be concise." },
        { role: "user", content: "Weather?" },
        {
          role: "assistant",
          content: null,
          tool_calls: [
            {
              id: "call_1",
              function: { name: "get_weather", arguments: '{"city":"SF"}' },
            },
          ],
        },
        { role: "tool", tool_call_id: "call_1", content: "sunny" },
      ],
      tools: [
        {
          type: "function",
          function: {
            name: "get_weather",
            description: "Get weather",
            parameters: { type: "object" },
          },
        },
      ],
    });
    const parsed = parseRequestBody(body);
    expect(parsed.format).toBe("openai-chat");
    expect(parsed.system).toBe("Be concise.");
    expect(parsed.tools[0].name).toBe("get_weather");
    // system extracted out, so 3 messages remain
    expect(parsed.messages).toHaveLength(3);
    const assistant = parsed.messages[1];
    const toolUse = assistant.blocks.find((b) => b.kind === "tool_use");
    expect(toolUse).toBeDefined();
    if (toolUse && toolUse.kind === "tool_use") {
      // arguments string parsed to object
      expect(toolUse.input).toEqual({ city: "SF" });
    }
    const toolMsg = parsed.messages[2];
    expect(toolMsg.role).toBe("tool");
    expect(toolMsg.blocks[0].kind).toBe("tool_result");
  });

  it("parses an OpenAI Responses request", () => {
    const body = JSON.stringify({
      model: "gpt-5-codex",
      instructions: "You are Codex.",
      input: [
        {
          type: "message",
          role: "user",
          content: [{ type: "input_text", text: "List files" }],
        },
        {
          type: "function_call",
          call_id: "c1",
          name: "bash",
          arguments: '{"cmd":"ls"}',
        },
        { type: "function_call_output", call_id: "c1", output: "a.txt" },
      ],
      tools: [{ type: "function", name: "bash", description: "run bash" }],
    });
    const parsed = parseRequestBody(body);
    expect(parsed.format).toBe("openai-responses");
    expect(parsed.system).toBe("You are Codex.");
    expect(parsed.tools[0].name).toBe("bash");
    expect(parsed.messages).toHaveLength(3);
    expect(parsed.messages[0].blocks[0].kind).toBe("text");
    expect(parsed.messages[1].blocks[0].kind).toBe("tool_use");
    expect(parsed.messages[2].blocks[0].kind).toBe("tool_result");
  });

  it("falls back to params-only for unrecognized object", () => {
    const body = JSON.stringify({ conversationState: { foo: "bar" } });
    const parsed = parseRequestBody(body);
    expect(parsed.format).toBe("unknown");
    expect(parsed.messages).toHaveLength(0);
  });
});
