import { afterEach, describe, expect, it, vi } from "vitest";

import { CentaurClient } from "../src/client";

describe("CentaurClient", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("starts workflow runs through the workflow API", async () => {
    const client = new CentaurClient({
      apiUrl: "http://api.local",
      apiKey: "test-key",
    });
    const postMock = vi.spyOn(client.http, "post").mockResolvedValue({
      data: { ok: true, run_id: "run-123", task_id: "task-123", status: "queued", created: true },
    });

    await expect(
      client.startWorkflowRun({
        workflowName: "nightly",
        idempotencyKey: "trigger-1",
        input: { topic: "incidents" },
        harnessType: "codex",
        maxAttempts: 3,
        timeoutMs: 5000,
      }),
    ).resolves.toMatchObject({ run_id: "run-123" });

    expect(postMock).toHaveBeenCalledWith(
      "/api/workflows/runs",
      {
        workflow_name: "nightly",
        idempotency_key: "trigger-1",
        input: { topic: "incidents" },
        harness_type: "codex",
        max_attempts: 3,
      },
      { timeout: 5000 },
    );
  });

  it("reads and mutates workflow runs through workflow endpoints", async () => {
    const client = new CentaurClient({
      apiUrl: "http://api.local",
      apiKey: "test-key",
    });
    const getMock = vi.spyOn(client.http, "get").mockResolvedValue({
      data: { ok: true, run_id: "run:123", workflow_name: "nightly", status: "completed" },
    });
    const postMock = vi.spyOn(client.http, "post").mockResolvedValue({
      data: { ok: true, run_id: "run:123", workflow_name: "nightly", status: "cancelled" },
    });

    await client.getWorkflowRun("run:123");
    await client.listWorkflowRuns({
      workflowName: "nightly",
      threadKey: "slack:C:1",
      limit: 5,
    });
    await client.cancelWorkflowRun("run:123");

    expect(getMock).toHaveBeenNthCalledWith(1, "/api/workflows/runs/run%3A123");
    expect(getMock).toHaveBeenNthCalledWith(2, "/api/workflows/runs", {
      params: {
        workflow_name: "nightly",
        thread_key: "slack:C:1",
        limit: 5,
      },
    });
    expect(postMock).toHaveBeenCalledWith("/api/workflows/runs/run%3A123/cancel");
  });

  it("sends workflow events", async () => {
    const client = new CentaurClient({
      apiUrl: "http://api.local",
      apiKey: "test-key",
    });
    const postMock = vi.spyOn(client.http, "post").mockResolvedValue({ data: { ok: true } });

    await client.sendWorkflowEvent({
      eventName: "approval.received",
      payload: { approved: true, correlation_id: "corr-1" },
    });

    expect(postMock).toHaveBeenCalledWith("/api/workflows/events", {
      event_name: "approval.received",
      payload: { approved: true, correlation_id: "corr-1" },
    });
  });

  it("releases a session through the canonical owner-fenced endpoint", async () => {
    const client = new CentaurClient({
      apiUrl: "http://api.local",
      apiKey: "test-key",
    });
    const postMock = vi.spyOn(client.http, "post").mockResolvedValue({
      data: {
        ok: true,
        thread_key: "slack:T:C:1.2",
        cancel_inflight: true,
        sandbox_released: true,
        execution_cancelled: true,
      },
    });

    await client.releaseThread("slack:T:C:1.2", {
      releaseId: "rel-123",
      expectedSandboxId: "asbx-reviewed",
      cancelInflight: true,
    });

    expect(postMock).toHaveBeenCalledWith(
      "/api/session/slack%3AT%3AC%3A1.2/release",
      {
        release_id: "rel-123",
        expected_sandbox_id: "asbx-reviewed",
        cancel_inflight: true,
      },
    );
  });
});
