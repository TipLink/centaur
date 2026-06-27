import { EventSourceParserStream, type EventSourceMessage } from "eventsource-parser/stream";
import axios, { type AxiosInstance } from "axios";

export type InputContentBlock =
  | { type: "text"; text: string }
  | {
      type: "image";
      source_path?: string;
      source: { type: "base64"; media_type: string; data: string };
    }
  | {
      type: "document";
      source_path?: string;
      source: { type: "base64"; media_type: string; data: string };
    };

export interface SpawnOptions {
  threadKey: string;
  spawnId?: string;
  harness?: string;
  engine?: string;
  personaId?: string;
  agentsMdOverride?: string;
}

export interface SpawnResult {
  thread_key: string;
  sandbox_id?: string | null;
  sandbox_capabilities?: {
    repo_cache_enabled: boolean;
    observability_enabled: boolean;
  } | null;
  harness_type: string;
  harness_thread_id?: string | null;
  persona_id?: string | null;
  status: string;
  iron_control_principal?: string | null;
  created_at?: string | null;
  updated_at?: string | null;
  harness_switched: boolean;
}

export interface MessageOptions {
  threadKey: string;
  assignmentGeneration: number;
  messageId?: string;
  role?: string;
  parts?: InputContentBlock[];
  userId?: string;
  metadata?: Record<string, unknown>;
}

export interface ExecuteOptions {
  threadKey: string;
  assignmentGeneration: number;
  executeId?: string;
  harness?: string;
  platform?: string;
  userId?: string;
  metadata?: Record<string, unknown>;
  delivery?: Record<string, unknown>;
}

export interface ExecutionAccepted {
  ok: boolean;
  execution_id: string;
  status: string;
  thread_key: string;
}

export interface WorkflowRunOptions {
  workflowName: string;
  triggerKey?: string;
  input?: Record<string, unknown>;
  eagerStart?: boolean;
  timeoutMs?: number;
}

export interface WorkflowRunAccepted {
  ok: boolean;
  run_id: string;
  workflow_name: string;
  workflow_version?: string;
  workflow_source_path?: string | null;
  parent_run_id?: string | null;
  root_run_id?: string | null;
  status: string;
  thread_key?: string | null;
  execution_id?: string | null;
  output_json?: Record<string, unknown> | null;
  error_text?: string | null;
  latest_checkpoint_name?: string | null;
  latest_step_kind?: string | null;
  waiting_on?: Record<string, unknown> | null;
  child_runs_count?: number;
  created_at?: string | null;
  started_at?: string | null;
  completed_at?: string | null;
  idempotent?: boolean;
}

export interface ThreadMessageRecord {
  id: string;
  role: string;
  parts: Array<Record<string, unknown>>;
  user_id?: string | null;
  metadata?: Record<string, unknown> | null;
  created_at?: string | null;
}

export interface StreamEvent {
  eventId: number;
  eventKind: string;
  data: Record<string, unknown>;
}

export class CentaurClient {
  readonly http: AxiosInstance;
  private log?: { info: Function; warn: Function; error: Function };

  constructor(opts: {
    apiUrl: string;
    apiKey: string;
    timeoutMs?: number;
    logger?: { info: Function; warn: Function; error: Function };
  }) {
    this.log = opts.logger;
    this.http = axios.create({
      baseURL: opts.apiUrl,
      headers: { Authorization: `Bearer ${opts.apiKey}` },
      timeout: opts.timeoutMs ?? 30_000,
    });
  }

  private get authHeader(): string {
    return (this.http.defaults.headers["Authorization"] ??
      this.http.defaults.headers.common?.["Authorization"]) as string;
  }

  async spawn(opts: SpawnOptions): Promise<SpawnResult> {
    const metadata: Record<string, unknown> = {
      ...(opts.spawnId === undefined ? {} : { spawn_id: opts.spawnId }),
      ...(opts.agentsMdOverride === undefined
        ? {}
        : { agents_md_override: opts.agentsMdOverride }),
    };
    const { data } = await this.http.post(`/api/session/${encodeURIComponent(opts.threadKey)}`, {
      harness_type: opts.harness ?? opts.engine ?? "codex",
      persona_id: opts.personaId,
      metadata,
    });
    return data as SpawnResult;
  }

  async message(opts: MessageOptions): Promise<{ ok: boolean; message_ids: string[] }> {
    const { data } = await this.http.post(
      `/api/session/${encodeURIComponent(opts.threadKey)}/messages`,
      {
        messages: [
          {
            client_message_id: opts.messageId,
            role: opts.role ?? "user",
            parts: opts.parts ?? [],
            metadata: {
              ...(opts.metadata ?? {}),
              ...(opts.assignmentGeneration === undefined
                ? {}
                : { assignment_generation: opts.assignmentGeneration }),
              ...(opts.userId === undefined ? {} : { user_id: opts.userId }),
            },
          },
        ],
      },
    );
    return data as { ok: boolean; message_ids: string[] };
  }

  async execute(opts: ExecuteOptions): Promise<ExecutionAccepted> {
    const metadata = {
      ...(opts.metadata ?? {}),
      ...(opts.assignmentGeneration === undefined
        ? {}
        : { assignment_generation: opts.assignmentGeneration }),
      ...(opts.harness === undefined ? {} : { harness: opts.harness }),
      ...(opts.platform === undefined ? {} : { platform: opts.platform }),
      ...(opts.userId === undefined ? {} : { user_id: opts.userId }),
      ...(opts.delivery === undefined ? {} : { delivery: opts.delivery }),
    };
    const { data } = await this.http.post(
      `/api/session/${encodeURIComponent(opts.threadKey)}/execute`,
      {
        idempotency_key: opts.executeId,
        metadata,
        input_lines: [],
      },
    );
    return data as ExecutionAccepted;
  }

  async startWorkflowRun(opts: WorkflowRunOptions): Promise<WorkflowRunAccepted> {
    const { data } = await this.http.post("/workflows/runs", {
      workflow_name: opts.workflowName,
      trigger_key: opts.triggerKey,
      input: opts.input ?? {},
      eager_start: opts.eagerStart ?? false,
    }, {
      timeout: opts.timeoutMs,
    });
    return data as WorkflowRunAccepted;
  }

  async getWorkflowRun(runId: string): Promise<WorkflowRunAccepted> {
    const { data } = await this.http.get(`/workflows/runs/${encodeURIComponent(runId)}`);
    return data as WorkflowRunAccepted;
  }

  async listWorkflowRuns(opts?: {
    workflowName?: string;
    threadKey?: string;
    status?: string;
    parentRunId?: string;
    limit?: number;
  }): Promise<{ ok: boolean; items: WorkflowRunAccepted[] }> {
    const { data } = await this.http.get("/workflows/runs", {
      params: {
        workflow_name: opts?.workflowName,
        thread_key: opts?.threadKey,
        status: opts?.status,
        parent_run_id: opts?.parentRunId,
        limit: opts?.limit,
      },
    });
    return data as { ok: boolean; items: WorkflowRunAccepted[] };
  }

  async getWorkflowChildren(runId: string, limit = 200): Promise<{ ok: boolean; items: WorkflowRunAccepted[] }> {
    return this.listWorkflowRuns({ parentRunId: runId, limit });
  }

  async cancelWorkflowRun(runId: string): Promise<WorkflowRunAccepted> {
    const { data } = await this.http.post(`/workflows/runs/${encodeURIComponent(runId)}/cancel`);
    return data as WorkflowRunAccepted;
  }

  async sendWorkflowEvent(opts: {
    eventName: string;
    payload?: Record<string, unknown>;
  }): Promise<Record<string, unknown>> {
    const { data } = await this.http.post("/workflows/events", {
      event_name: opts.eventName,
      payload: opts.payload ?? {},
    });
    return data as Record<string, unknown>;
  }

  async *streamEvents(opts: {
    threadKey: string;
    afterEventId?: number;
    executionId?: string;
    pollMs?: number;
    signal?: AbortSignal;
  }): AsyncGenerator<StreamEvent, void, undefined> {
    const params = new URLSearchParams();
    if (opts.afterEventId !== undefined) params.set("after_event_id", String(opts.afterEventId));
    if (opts.executionId) params.set("execution_id", opts.executionId);
    if (opts.pollMs !== undefined) params.set("poll_ms", String(opts.pollMs));

    const url = new URL(
      `/api/session/${encodeURIComponent(opts.threadKey)}/events`,
      this.ensureBaseUrl(),
    );
    for (const [key, value] of params) url.searchParams.set(key, value);
    const res = await fetch(url.toString(), {
      method: "GET",
      headers: {
        Authorization: this.authHeader,
        "X-Centaur-Thread-Key": opts.threadKey,
      },
      signal: opts.signal,
    });

    if (!res.ok) {
      const text = await res.text().catch(() => "");
      throw new Error(`/api/session/{thread}/events failed (${res.status}): ${text.slice(0, 300)}`);
    }
    if (!res.body) return;

    const stream = (res.body as ReadableStream<Uint8Array>)
      .pipeThrough(new TextDecoderStream() as unknown as TransformStream<Uint8Array, string>)
      .pipeThrough(new EventSourceParserStream());

    for await (const event of stream as unknown as AsyncIterable<EventSourceMessage>) {
      if (!event.data || event.data === "[DONE]") continue;
      let parsed: Record<string, unknown> = { type: "unknown", raw: event.data };
      try {
        parsed = JSON.parse(event.data) as Record<string, unknown>;
      } catch {
        // keep raw fallback
      }
      yield {
        eventId: Number(event.id || 0),
        eventKind: event.event || "message",
        data: parsed,
      };
    }
  }

  async releaseThread(threadKey: string, opts?: { releaseId?: string; cancelInflight?: boolean }) {
    const { data } = await this.http.post(
      `/api/session/${encodeURIComponent(threadKey)}/release`,
      {
        release_id: opts?.releaseId,
        cancel_inflight: opts?.cancelInflight ?? false,
      },
    );
    return data as Record<string, unknown>;
  }

  private ensureBaseUrl(): string {
    const baseURL = this.http.defaults.baseURL;
    if (!baseURL) throw new Error("CentaurClient apiUrl is required");
    return baseURL.endsWith("/") ? baseURL : `${baseURL}/`;
  }
}
