import { createHash } from "crypto";
import type { WebClient } from "@slack/web-api";
import { centaurApiKey, type AppConfig } from "../config";
import { slackReplyLimits } from "../constants";
import { logError, logInfo, logWarn } from "../logging";
import {
  activeSpanAttributes,
  clientSpanOptions,
  injectTraceHeaders,
  internalSpanOptions,
  spanAttributes,
  withSpan,
  withTraceHeaders,
} from "../otel";
import { markdownToRichText } from "../slack/streaming";

const CONSUMER_ID = `slackbot-${process.pid}`;
const FINAL_DELIVERY_CHUNK_CHARS = slackReplyLimits.text.maxFallbackChars;
const FINAL_DELIVERY_CHUNK_EVENT = "centaur_final_delivery_chunk";
const LIVE_DELIVERY_TAIL_DRIFT_CHARS = 3;
const NON_RETRYABLE_SLACK_ERRORS = new Set([
  "msg_too_long",
  "msg_blocks_too_long",
  "invalid_thread_ts",
  "thread_not_found",
  "channel_not_found",
  "user_not_found",
  "missing_slack_delivery_target",
  "account_inactive",
  "is_archived",
  "restricted_action",
  "not_in_channel",
  "channel_type_not_supported",
]);
const UNSUPPORTED_ASSISTANT_STATUS_ERRORS = new Set([
  "user_not_found",
  "channel_not_found",
  "channel_type_not_supported",
  "not_in_channel",
  "restricted_action",
]);

export function startFinalDeliveryPoller(
  config: AppConfig,
  client: WebClient,
): void {
  if (!centaurApiKey(config)) return;
  const tick = async () => {
    try {
      await pollFinalDeliveriesOnce(config, client);
    } catch (error) {
      logError("final_delivery_poll_failed", error);
    }
  };
  setInterval(tick, 2_000).unref?.();
  void tick();
}

export async function pollFinalDeliveriesOnce(
  config: AppConfig,
  client: WebClient,
): Promise<void> {
  const claimed = await withSpan(
    "centaur.slackbot.final_delivery.claim",
    clientSpanOptions({
      "centaur.delivery.platform": "slack",
      "centaur.final_delivery.limit": 5,
    }),
    async (span) => {
      const result = await centaur(config, "/agent/final-deliveries/claim", {
        consumer_id: CONSUMER_ID,
        platform: "slack",
        limit: 5,
        lease_seconds: 60,
      });
      spanAttributes(span, {
        "centaur.final_delivery.claimed_count": Array.isArray(result.deliveries)
          ? result.deliveries.length
          : 0,
      });
      return result;
    },
  );
  const deliveries = Array.isArray(claimed.deliveries)
    ? claimed.deliveries
    : [];
  for (const delivery of deliveries) {
    const executionId = String(delivery.execution_id);
    await withTraceHeaders(centaurTraceHeaders(delivery), async () => {
      await withSpan(
        "centaur.slackbot.final_delivery.deliver",
        internalSpanOptions({
          "centaur.execution_id": executionId,
          "centaur.thread_key": String(delivery.thread_key ?? ""),
          "centaur.delivery.platform": "slack",
        }),
        async (span) => {
          try {
            await deliver(client, delivery, config);
            await centaur(
              config,
              `/agent/final-deliveries/${executionId}/delivered`,
              {
                consumer_id: CONSUMER_ID,
              },
              delivery,
            );
            spanAttributes(span, { "centaur.final_delivery.status": "delivered" });
          } catch (error) {
            const errorMessage = slackDeliveryErrorMessage(error);
            const errorClass = slackDeliveryErrorClass(error);
            logWarn("slack_final_delivery_audit", {
              ...deliveryFailureAuditFields(delivery),
              delivery_action: "postMessage",
              slack_ok: false,
              error: errorMessage,
              error_class: errorClass,
            });
            spanAttributes(span, {
              "centaur.final_delivery.status": "failed",
              "centaur.final_delivery.error_class": errorClass,
            });
            await centaur(
              config,
              `/agent/final-deliveries/${executionId}/failed`,
              {
                consumer_id: CONSUMER_ID,
                error: errorMessage,
                retry_after_seconds: 10,
                ...(errorClass
                  ? { error_class: errorClass, non_retryable: true }
                  : {}),
              },
              delivery,
            ).catch((failError) =>
              logError("final_delivery_mark_failed_failed", failError),
            );
          }
        },
      );
    });
  }
}

async function deliver(
  client: WebClient,
  delivery: any,
  config: AppConfig,
): Promise<void> {
  const meta = delivery.delivery ?? {};
  const payload = delivery.final_payload ?? {};
  const target = targetFromDelivery(delivery);
  const channel = meta.channel_id ?? meta.channel ?? target.channel;
  const threadTs = meta.thread_ts ?? target.threadTs;
  if (!channel || !threadTs) throw new Error("missing_slack_delivery_target");
  const text = extractText(payload);
  const textToPost = deliveryText(payload, text);
  const audit = deliveryAuditFields(delivery, {
    channel,
    threadTs,
    payloadText: text,
    textToPost,
  });
  if (!textToPost) {
    activeSpanAttributes({
      "centaur.final_delivery.chunk_count": 0,
      "centaur.final_delivery.text_chars": 0,
      "centaur.final_delivery.skipped_reason": "live_delivery_tail_drift",
    });
    logInfo("slack_final_delivery_audit", {
      ...audit,
      delivery_action: "skip_live_delivery_tail_drift",
      slack_ok: true,
      chunk_count: 0,
      posted_chunk_count: 0,
      skipped_chunk_count: 0,
    });
    await clearAssistantStatus(client, channel, threadTs);
    return;
  }
  const chunks = splitFinalDeliveryText(textToPost);
  activeSpanAttributes({
    "centaur.final_delivery.chunk_count": chunks.length,
    "centaur.final_delivery.text_chars": textToPost.length,
  });
  const postResult = await postFollowups(
    client,
    channel,
    threadTs,
    executionId(delivery),
    config.SLACK_TEAM_ID,
    chunks,
  );
  logInfo("slack_final_delivery_audit", {
    ...audit,
    delivery_action: "postMessage",
    slack_ok: true,
    chunk_count: chunks.length,
    posted_chunk_count: postResult.posted,
    skipped_chunk_count: postResult.skipped,
  });
  await clearAssistantStatus(client, channel, threadTs);
}

function executionId(delivery: any): string {
  return String(delivery?.execution_id ?? "");
}

async function postFollowups(
  client: WebClient,
  channel: string,
  threadTs: string,
  executionId: string,
  teamId: string | undefined,
  chunks: string[],
): Promise<{ posted: number; skipped: number }> {
  const posted = await withSpan(
    "centaur.slackbot.slack.conversations_replies",
    clientSpanOptions({
      "slack.channel_id": channel,
      "slack.thread_ts": threadTs,
      "centaur.execution_id": executionId,
    }),
    () => postedChunkIndexes(client, channel, threadTs, executionId),
  );
  let postedCount = 0;
  let skippedCount = 0;
  for (const [index, chunk] of chunks.entries()) {
    if (posted.has(index)) {
      skippedCount += 1;
      continue;
    }
    const renderedChunk = rewriteSlackArchiveLinksForApp(chunk, teamId);
    const fallbackText = markdownLinksToSlackMrkdwn(renderedChunk);
    const response = await withSpan(
      "centaur.slackbot.slack.post_message",
      clientSpanOptions({
        "slack.channel_id": channel,
        "slack.thread_ts": threadTs,
        "centaur.execution_id": executionId,
        "centaur.final_delivery.chunk_index": index,
        "centaur.final_delivery.chunk_count": chunks.length,
        "centaur.final_delivery.chunk_chars": fallbackText.length,
      }),
      () =>
        client.chat.postMessage({
          channel,
          thread_ts: threadTs,
          text: fallbackText,
          blocks: [markdownToRichText(renderedChunk)],
          unfurl_links: false,
          unfurl_media: false,
          metadata: chunkMetadata(executionId, index, chunks.length),
        }),
    );
    if (!response.ok)
      throw new Error(response.error ?? "chat.postMessage failed");
    postedCount += 1;
  }
  return { posted: postedCount, skipped: skippedCount };
}

async function postedChunkIndexes(
  client: WebClient,
  channel: string,
  threadTs: string,
  executionId: string,
): Promise<Set<number>> {
  if (!executionId) return new Set();
  try {
    const response = await client.conversations.replies({
      channel,
      ts: threadTs,
      limit: 200,
      inclusive: true,
    });
    if (!response.ok) {
      const error = String(response.error ?? "conversations.replies failed");
      if (nonRetryableSlackErrorCode(error))
        throw slackApiError(error, response);
      return new Set();
    }
    if (!Array.isArray(response.messages)) return new Set();
    const indexes = response.messages
      .map((message: any) => message?.metadata)
      .filter(
        (metadata: any) => metadata?.event_type === FINAL_DELIVERY_CHUNK_EVENT,
      )
      .filter(
        (metadata: any) =>
          metadata?.event_payload?.execution_id === executionId,
      )
      .map((metadata: any) => Number(metadata.event_payload.chunk_index))
      .filter((index: number) => Number.isInteger(index) && index >= 0);
    return new Set(indexes);
  } catch (error) {
    if (slackDeliveryErrorClass(error)) throw error;
    return new Set();
  }
}

async function clearAssistantStatus(
  client: WebClient,
  channel: string,
  threadTs: string,
): Promise<void> {
  try {
    const response = await client.assistant.threads.setStatus({
      channel_id: channel,
      thread_ts: threadTs,
      status: "",
    });
    if (!response.ok) {
      const errorCode = response.error ?? "unknown_error";
      if (isUnsupportedAssistantStatusError(errorCode)) {
        logWarn("final_delivery_assistant_status_unsupported", {
          channel_id: channel,
          thread_ts: threadTs,
          error: errorCode,
        });
        return;
      }
      throw new Error(errorCode);
    }
  } catch (error) {
    const errorCode = slackDeliveryErrorClass(error);
    const message = error instanceof Error ? error.message : String(error);
    if (errorCode && isUnsupportedAssistantStatusError(errorCode)) {
      logWarn("final_delivery_assistant_status_unsupported", {
        channel_id: channel,
        thread_ts: threadTs,
        error: errorCode,
      });
      return;
    }
    if (isUnsupportedAssistantStatusError(message)) {
      logWarn("final_delivery_assistant_status_unsupported", {
        channel_id: channel,
        thread_ts: threadTs,
        error: message,
      });
      return;
    }
    throw error;
  }
}

function isUnsupportedAssistantStatusError(error: string): boolean {
  const trimmed = error.trim().toLowerCase();
  if (UNSUPPORTED_ASSISTANT_STATUS_ERRORS.has(trimmed)) return true;
  for (const code of UNSUPPORTED_ASSISTANT_STATUS_ERRORS) {
    if (trimmed.includes(code)) return true;
  }
  return false;
}

function chunkMetadata(
  executionId: string,
  chunkIndex: number,
  chunkCount: number,
): object | undefined {
  if (!executionId) return undefined;
  return {
    event_type: FINAL_DELIVERY_CHUNK_EVENT,
    event_payload: {
      execution_id: executionId,
      chunk_index: chunkIndex,
      chunk_count: chunkCount,
    },
  };
}

function deliveryAuditFields(
  delivery: any,
  opts: {
    channel: string;
    threadTs: string;
    payloadText: string;
    textToPost: string | null;
  },
): Record<string, unknown> {
  const payload = delivery.final_payload ?? {};
  const streamedAnswerChars = Number(payload?.slackbot_streamed_answer_chars);
  return {
    execution_id: executionId(delivery),
    centaur_thread_key: delivery?.thread_key,
    channel_id: opts.channel,
    thread_ts: opts.threadTs,
    payload_text_chars: opts.payloadText.length,
    payload_text_hash: textHash(opts.payloadText),
    text_to_post_chars: opts.textToPost?.length ?? 0,
    text_to_post_hash: textHash(opts.textToPost ?? ""),
    slackbot_streamed_answer_chars: Number.isFinite(streamedAnswerChars)
      ? streamedAnswerChars
      : undefined,
    attempt_count: delivery?.attempt_count,
  };
}

function deliveryFailureAuditFields(delivery: any): Record<string, unknown> {
  const meta = delivery.delivery ?? {};
  const target = targetFromDelivery(delivery);
  const channel = meta.channel_id ?? meta.channel ?? target.channel ?? "";
  const threadTs = meta.thread_ts ?? target.threadTs ?? "";
  const payload = delivery.final_payload ?? {};
  const payloadText = extractText(payload);
  return deliveryAuditFields(delivery, {
    channel,
    threadTs,
    payloadText,
    textToPost: deliveryText(payload, payloadText),
  });
}

function textHash(value: string): string {
  return value
    ? createHash("sha256").update(value).digest("hex").slice(0, 16)
    : "";
}

function extractText(payload: any): string {
  const value = firstNonEmpty(
    payload?.result_text,
    payload?.result,
    payload?.text,
    payload?.final_text,
    payload?.message,
  );
  if (value) return value;

  const executionId = String(payload?.execution_id ?? "").trim();
  const suffix = executionId ? ` Execution: \`${executionId}\`.` : "";
  return `Execution completed, but no final text was captured.${suffix}`;
}

function firstNonEmpty(...values: unknown[]): string {
  for (const value of values) {
    const text =
      value === undefined || value === null ? "" : String(value).trim();
    if (text) return text;
  }
  return "";
}

function deliveryText(payload: any, text: string): string | null {
  const continuation = continuationText(payload, text);
  return continuation === undefined ? text : continuation;
}

function continuationText(
  payload: any,
  text: string,
): string | null | undefined {
  const rawOffset = Number(payload?.slackbot_streamed_answer_chars);
  if (!Number.isFinite(rawOffset) || rawOffset <= 0) return undefined;
  const offset = Math.floor(rawOffset);
  if (offset >= text.length) return null;
  const continuation = text.slice(offset).trimStart();
  if (isLiveDeliveryTailDrift(continuation)) return null;
  return continuation;
}

function isLiveDeliveryTailDrift(text: string): boolean {
  const trimmed = text.trim();
  return (
    !trimmed ||
    (trimmed.length <= LIVE_DELIVERY_TAIL_DRIFT_CHARS && !/\s/.test(trimmed))
  );
}

function rewriteSlackArchiveLinksForApp(
  text: string,
  teamId: string | undefined,
): string {
  const team = String(teamId ?? "").trim();
  if (!team) return text;
  return text.replace(
    /\[([^\]]+)\]\((https:\/\/(?:[A-Za-z0-9-]+\.)*slack\.com\/archives\/([A-Z0-9]+)\/p(\d{16})(?:\?([^)]*))?)\)/g,
    (
      match,
      label: string,
      _url: string,
      channel: string,
      rawMessageTs: string,
      query: string | undefined,
    ) => {
      const messageTs = slackTsFromArchivePath(rawMessageTs);
      if (!messageTs) return match;
      const params = new URLSearchParams(query ?? "");
      const threadTs = params.get("thread_ts") || undefined;
      const deepLink = slackAppDeepLink({ team, channel, messageTs, threadTs });
      return `[${label}](${deepLink})`;
    },
  );
}

function markdownLinksToSlackMrkdwn(text: string): string {
  return text.replace(
    /(?<!!)\[([^\]\n]+)\]\(\s*<?((?:https?|slack):\/\/[^\s>)]+)>?\s*\)/g,
    (_match, label: string, url: string) =>
      `<${url}|${slackLinkLabel(label)}>`,
  );
}

function slackLinkLabel(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/\|/g, "/");
}

function slackTsFromArchivePath(raw: string): string | null {
  if (!/^\d{16}$/.test(raw)) return null;
  return `${raw.slice(0, 10)}.${raw.slice(10)}`;
}

function slackAppDeepLink(opts: {
  team: string;
  channel: string;
  messageTs: string;
  threadTs?: string;
}): string {
  const params = new URLSearchParams({
    team: opts.team,
    id: opts.channel,
    message: opts.messageTs,
  });
  if (opts.threadTs) params.set("thread_ts", opts.threadTs);
  return `slack://channel?${params.toString()}`;
}

function splitFinalDeliveryText(text: string): string[] {
  const trimmed = text.trim();
  if (!trimmed) return [];
  const chunks: string[] = [];
  let remaining = trimmed;
  while (remaining.length > FINAL_DELIVERY_CHUNK_CHARS) {
    let cut = remaining.lastIndexOf("\n\n", FINAL_DELIVERY_CHUNK_CHARS);
    if (cut <= FINAL_DELIVERY_CHUNK_CHARS * 0.3) {
      cut = remaining.lastIndexOf("\n", FINAL_DELIVERY_CHUNK_CHARS);
    }
    if (cut <= FINAL_DELIVERY_CHUNK_CHARS * 0.3) {
      cut = remaining.lastIndexOf(" ", FINAL_DELIVERY_CHUNK_CHARS);
    }
    if (cut <= FINAL_DELIVERY_CHUNK_CHARS * 0.3)
      cut = FINAL_DELIVERY_CHUNK_CHARS;
    chunks.push(remaining.slice(0, cut).trimEnd());
    remaining = remaining.slice(cut).trimStart();
  }
  if (remaining) chunks.push(remaining);
  return chunks;
}

function slackDeliveryErrorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  return String(error);
}

function slackDeliveryErrorClass(error: unknown): string | null {
  return nonRetryableSlackErrorCode(slackDeliveryErrorFingerprint(error));
}

function nonRetryableSlackErrorCode(value: string): string | null {
  const normalized = value.trim().toLowerCase();
  for (const errorClass of NON_RETRYABLE_SLACK_ERRORS) {
    if (normalized.includes(errorClass)) return errorClass;
  }
  return null;
}

function slackApiError(message: string, data: unknown): Error {
  const error = new Error(message);
  (error as { data?: unknown }).data = data;
  return error;
}

function slackDeliveryErrorFingerprint(error: unknown): string {
  const parts = [slackDeliveryErrorMessage(error)];
  const data = (error as { data?: unknown })?.data;
  if (data && typeof data === "object") {
    const slackError = (data as { error?: unknown }).error;
    if (slackError) parts.push(String(slackError));
  }
  const code = (error as { code?: unknown })?.code;
  if (code) parts.push(String(code));
  return parts.join(" ");
}

function targetFromDelivery(delivery: any): {
  teamId?: string;
  channel?: string;
  threadTs?: string;
} {
  const threadKey = String(delivery.thread_key ?? "");
  const parts = threadKey.split(":");
  if (parts[0] === "slack" && parts.length >= 4) {
    return {
      teamId: parts[1],
      channel: parts[2],
      threadTs: parts.slice(3).join(":"),
    };
  }
  return {};
}

async function centaur(
  config: AppConfig,
  path: string,
  body: unknown,
  trace?: any,
): Promise<any> {
  const apiKey = centaurApiKey(config);
  const traceHeaders = {
    ...centaurTraceHeaders(trace),
    ...injectTraceHeaders(centaurTraceHeaders(trace)),
  };
  const response = await fetch(new URL(path, config.CENTAUR_API_URL), {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      ...traceHeaders,
      ...(apiKey ? { Authorization: `Bearer ${apiKey}` } : {}),
    },
    body: JSON.stringify(body),
  });
  activeSpanAttributes({
    "http.response.status_code": response.status,
    "centaur.api.path": path,
  });
  const text = await response.text();
  const parsed: any = text ? JSON.parse(text) : {};
  if (!response.ok)
    throw new Error(
      parsed?.detail?.message ??
        parsed?.detail ??
        parsed?.error ??
        response.statusText,
    );
  return parsed;
}

function centaurTraceHeaders(trace: any): Record<string, string> {
  const traceId = String(trace?.trace_id ?? "").trim();
  const threadKey = String(trace?.thread_key ?? "").trim();
  const traceparent = String(trace?.traceparent ?? "").trim();
  return {
    ...(traceId ? { "X-Trace-Id": traceId } : {}),
    ...(threadKey ? { "X-Centaur-Thread-Key": threadKey } : {}),
    ...(traceparent ? { traceparent } : {}),
  };
}
