import { afterEach, describe, expect, it, vi } from "vitest";

import { CentaurClient, type StreamEvent } from "../src/client";

async function collectEvents(events: AsyncIterable<StreamEvent>): Promise<StreamEvent[]> {
  const collected: StreamEvent[] = [];
  for await (const event of events) {
    collected.push(event);
  }
  return collected;
}

function sseResponse(body: string, init?: ResponseInit): Response {
  return new Response(
    new ReadableStream({
      start(controller) {
        controller.enqueue(new TextEncoder().encode(body));
        controller.close();
      },
    }),
    {
      status: 200,
      headers: { "Content-Type": "text/event-stream" },
      ...init,
    },
  );
}

describe("CentaurClient", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("parses SSE ids, events, JSON data, [DONE], and invalid JSON payloads", async () => {
    const fetchMock = vi.fn(async () => sseResponse([
      "id: 11",
      "event: amp_raw_event",
      'data: {"type":"assistant","message":{"content":"hello"}}',
      "",
      "id: 12",
      "event: done",
      "data: [DONE]",
      "",
      "id: 13",
      "data: not-json",
      "",
      "",
    ].join("\n")));
    vi.stubGlobal("fetch", fetchMock);

    const client = new CentaurClient({
      apiUrl: "http://api.local",
      apiKey: "test-key",
    });

    await expect(collectEvents(client.streamEvents({ threadKey: "thread-1" }))).resolves.toEqual([
      {
        eventId: 11,
        eventKind: "amp_raw_event",
        data: { type: "assistant", message: { content: "hello" } },
      },
      {
        eventId: 13,
        eventKind: "message",
        data: { type: "unknown", raw: "not-json" },
      },
    ]);
  });

  it("URL encodes Slack thread keys in event stream URLs", async () => {
    const fetchMock = vi.fn(async () => sseResponse(""));
    vi.stubGlobal("fetch", fetchMock);
    const client = new CentaurClient({
      apiUrl: "http://api.local",
      apiKey: "test-key",
    });

    await collectEvents(client.streamEvents({
      threadKey: "slack:T123:C123:1700000000.000100",
      executionId: "exe-1",
      afterEventId: 42,
      pollMs: 250,
    }));

    expect(fetchMock).toHaveBeenCalledWith(
      "http://api.local/api/session/slack%3AT123%3AC123%3A1700000000.000100/events?after_event_id=42&execution_id=exe-1&poll_ms=250",
      expect.objectContaining({
        method: "GET",
        headers: {
          Authorization: "Bearer test-key",
          "X-Centaur-Thread-Key": "slack:T123:C123:1700000000.000100",
        },
      }),
    );
  });

  it("uses session API routes for path-based session calls", async () => {
    const client = new CentaurClient({
      apiUrl: "http://api.local",
      apiKey: "test-key",
    });
    const postMock = vi.spyOn(client.http, "post").mockResolvedValue({ data: { ok: true } });
    const threadKey = "slack:T123:C123:1700000000.000100";

    await client.spawn({
      threadKey,
      harness: "codex",
      spawnId: "spawn:1",
      personaId: "persona-1",
      agentsMdOverride: "custom instructions",
    });
    await client.message({
      threadKey,
      assignmentGeneration: 3,
      messageId: "msg:1",
      parts: [{ type: "text", text: "hello" }],
      userId: "U123",
      metadata: { platform: "slack" },
    });
    await client.execute({
      threadKey,
      assignmentGeneration: 3,
      executeId: "exec:1",
      harness: "codex",
      platform: "slack",
      userId: "U123",
      metadata: { source: "test" },
    });
    await client.releaseThread(threadKey, {
      releaseId: "release:1",
      expectedSandboxId: "sbx:1",
      cancelInflight: true,
    });

    expect(postMock).toHaveBeenNthCalledWith(
      1,
      "/api/session/slack%3AT123%3AC123%3A1700000000.000100",
      {
        harness_type: "codex",
        persona_id: "persona-1",
        metadata: {
          spawn_id: "spawn:1",
          agents_md_override: "custom instructions",
        },
      },
    );
    expect(postMock).toHaveBeenNthCalledWith(
      2,
      "/api/session/slack%3AT123%3AC123%3A1700000000.000100/messages",
      {
        messages: [
          {
            client_message_id: "msg:1",
            role: "user",
            parts: [{ type: "text", text: "hello" }],
            metadata: {
              platform: "slack",
              assignment_generation: 3,
              user_id: "U123",
            },
          },
        ],
      },
    );
    expect(postMock).toHaveBeenNthCalledWith(
      3,
      "/api/session/slack%3AT123%3AC123%3A1700000000.000100/execute",
      {
        idempotency_key: "exec:1",
        metadata: {
          source: "test",
          assignment_generation: 3,
          harness: "codex",
          platform: "slack",
          user_id: "U123",
        },
        input_lines: [],
      },
    );
    expect(postMock).toHaveBeenNthCalledWith(
      4,
      "/api/session/slack%3AT123%3AC123%3A1700000000.000100/release",
      {
        release_id: "release:1",
        expected_sandbox_id: "sbx:1",
        cancel_inflight: true,
      },
    );
  });

  it("throws useful errors for non-OK event stream responses", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response(
      "upstream unavailable",
      { status: 503, statusText: "Service Unavailable" },
    )));
    const client = new CentaurClient({
      apiUrl: "http://api.local",
      apiKey: "test-key",
    });

    await expect(
      collectEvents(client.streamEvents({ threadKey: "slack:T123:C123:1700000000.000100" })),
    ).rejects.toThrow(
      "/api/session/{thread}/events failed (503): upstream unavailable",
    );
  });
});
