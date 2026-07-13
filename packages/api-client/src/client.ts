import axios, { type AxiosInstance } from "axios";

export interface WorkflowRunOptions {
  workflowName: string;
  idempotencyKey?: string;
  input?: Record<string, unknown>;
  harnessType?: "codex" | "amp" | "claudecode";
  maxAttempts?: number;
  timeoutMs?: number;
}

export interface WorkflowRunCreated {
  ok: boolean;
  run_id: string;
  task_id: string;
  status: string;
  created: boolean;
}

export interface WorkflowRun {
  run_id: string;
  task_id: string;
  workflow_name: string;
  status: string;
  input: unknown;
  result: unknown | null;
  failure: unknown | null;
  attempts: number;
  created_at: string;
  updated_at: string;
}

export interface ReleaseThreadOptions {
  releaseId?: string;
  expectedSandboxId?: string;
  cancelInflight?: boolean;
}

export interface ReleaseThreadResponse {
  ok: boolean;
  thread_key: string;
  sandbox_id?: string | null;
  release_id?: string | null;
  expected_sandbox_id?: string | null;
  cancel_inflight: boolean;
  sandbox_released: boolean;
  sandbox_release_error?: string | null;
  execution_id?: string | null;
  execution_cancelled: boolean;
}

export class CentaurClient {
  readonly http: AxiosInstance;

  constructor(opts: {
    apiUrl: string;
    apiKey: string;
    timeoutMs?: number;
  }) {
    this.http = axios.create({
      baseURL: opts.apiUrl,
      headers: { Authorization: `Bearer ${opts.apiKey}` },
      timeout: opts.timeoutMs ?? 30_000,
    });
  }

  async startWorkflowRun(opts: WorkflowRunOptions): Promise<WorkflowRunCreated> {
    const { data } = await this.http.post(
      "/api/workflows/runs",
      {
        workflow_name: opts.workflowName,
        idempotency_key: opts.idempotencyKey,
        input: opts.input ?? {},
        harness_type: opts.harnessType,
        max_attempts: opts.maxAttempts,
      },
      {
        timeout: opts.timeoutMs,
      },
    );
    return data as WorkflowRunCreated;
  }

  async getWorkflowRun(runId: string): Promise<{ ok: boolean; run: WorkflowRun }> {
    const { data } = await this.http.get(`/api/workflows/runs/${encodeURIComponent(runId)}`);
    return data as { ok: boolean; run: WorkflowRun };
  }

  async listWorkflowRuns(opts?: {
    workflowName?: string;
    threadKey?: string;
    limit?: number;
  }): Promise<{ ok: boolean; runs: WorkflowRun[] }> {
    const { data } = await this.http.get("/api/workflows/runs", {
      params: {
        workflow_name: opts?.workflowName,
        thread_key: opts?.threadKey,
        limit: opts?.limit,
      },
    });
    return data as { ok: boolean; runs: WorkflowRun[] };
  }

  async cancelWorkflowRun(runId: string): Promise<{ ok: boolean; status: "cancelled" }> {
    const { data } = await this.http.post(`/api/workflows/runs/${encodeURIComponent(runId)}/cancel`);
    return data as { ok: boolean; status: "cancelled" };
  }

  async releaseThread(
    threadKey: string,
    opts: ReleaseThreadOptions = {},
  ): Promise<ReleaseThreadResponse> {
    const { data } = await this.http.post(
      `/api/session/${encodeURIComponent(threadKey)}/release`,
      {
        release_id: opts.releaseId,
        expected_sandbox_id: opts.expectedSandboxId,
        cancel_inflight: opts.cancelInflight ?? false,
      },
    );
    return data as ReleaseThreadResponse;
  }

  async sendWorkflowEvent(opts: {
    eventName: string;
    payload?: Record<string, unknown>;
  }): Promise<Record<string, unknown>> {
    const { data } = await this.http.post("/api/workflows/events", {
      event_name: opts.eventName,
      payload: opts.payload ?? {},
    });
    return data as Record<string, unknown>;
  }
}
