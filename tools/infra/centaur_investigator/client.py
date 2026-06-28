"""Centaur readonly PostgreSQL investigation helper."""

from __future__ import annotations

import asyncio
import importlib.util
import json
import os
import re
from datetime import UTC, datetime
from pathlib import Path
from typing import Any
from urllib.parse import parse_qs, unquote, urlparse, urlunparse

import asyncpg

from centaur_sdk import secret

CENTAUR_POSTGRES_DSN_ENV = "CENTAUR_POSTGRES_DSN"
CENTAUR_INVESTIGATOR_DATABASE_ENV = "CENTAUR_INVESTIGATOR_POSTGRES_DATABASE"
CENTAUR_CURRENT_THREAD_ENV = "CENTAUR_THREAD_KEY"
CENTAUR_INVESTIGATOR_OPERATOR_ENV = "CENTAUR_INVESTIGATOR_OPERATOR_MODE"
DEFAULT_POSTGRES_DATABASE = "ai_v2"
DEFAULT_LIMIT = 25
MAX_LIMIT = 200
DEFAULT_WINDOW_HOURS = 24
MAX_WINDOW_HOURS = 24 * 30
MAX_LOG_LIMIT = 500

_SLACK_URL_RE = re.compile(r"https?://[^\s<>|]+/archives/[A-Z0-9]+/p\d{10,20}[^\s<>|]*")
_SLACK_THREAD_KEY_RE = re.compile(
    r"\b(?P<thread_key>[A-Za-z][A-Za-z0-9_.-]*:"
    r"(?:(?P<team>T[A-Z0-9]+):)?(?P<channel>[CDG][A-Z0-9]+):"
    r"(?P<thread_ts>\d{10}\.\d{1,6}))\b"
)
_CHANNEL_TS_RE = re.compile(r"\b(?P<channel>[CDG][A-Z0-9]+):(?P<thread_ts>\d{10}\.\d{1,6})\b")
_KEY_SOURCE_RE = re.compile(r"^[A-Za-z][A-Za-z0-9_.-]*:")


def _clamp(value: int, *, minimum: int, maximum: int) -> int:
    return max(minimum, min(maximum, int(value)))


def _scoped_database_url() -> str:
    value = os.getenv(CENTAUR_POSTGRES_DSN_ENV)  # noqa: TID251
    if value is None:
        value = secret(CENTAUR_POSTGRES_DSN_ENV, default="")
    value = value.strip()
    if value == CENTAUR_POSTGRES_DSN_ENV:
        return ""
    return value


def _database_url_with_name(value: str, database: str) -> str:
    parsed = urlparse(value)
    if parsed.scheme and parsed.netloc and parsed.path in ("", "/"):
        return urlunparse(parsed._replace(path=f"/{database}"))
    return value


def _postgres_database_name() -> str:
    value = os.getenv(CENTAUR_INVESTIGATOR_DATABASE_ENV, DEFAULT_POSTGRES_DATABASE)  # noqa: TID251
    return value.strip() or DEFAULT_POSTGRES_DATABASE


def _isoformat(value: Any) -> str | None:
    if isinstance(value, datetime):
        return value.isoformat()
    return None


def _serialize(value: Any) -> Any:
    if isinstance(value, datetime):
        return value.isoformat()
    if isinstance(value, (dict, list, str, int, float, bool)) or value is None:
        return value
    try:
        return json.loads(json.dumps(value))
    except TypeError:
        return str(value)


def _record_to_dict(row: Any) -> dict[str, Any]:
    if isinstance(row, dict):
        return {key: _serialize(value) for key, value in row.items()}
    return {key: _serialize(row[key]) for key in row}


def _connection_role(connection: dict[str, Any]) -> str | None:
    row = connection.get("row") if isinstance(connection, dict) else None
    if not isinstance(row, dict):
        return None
    return str(row.get("active_role") or row.get("current_user") or "") or None


def _truthy_env(name: str) -> bool:
    return os.getenv(name, "").strip().lower() in {"1", "true", "yes", "on"}


def _operator_mode_enabled() -> bool:
    return _truthy_env(CENTAUR_INVESTIGATOR_OPERATOR_ENV)


def _current_thread_key() -> str | None:
    value = os.getenv(CENTAUR_CURRENT_THREAD_ENV, "").strip()  # noqa: TID251
    return value or None


def _normalize_ts(value: str | None) -> str | None:
    if not value:
        return None
    text = unquote(str(value)).strip()
    if not text:
        return None
    if "." in text:
        left, right = text.split(".", 1)
        if left.isdigit() and right.isdigit():
            return f"{left}.{right[:6].ljust(6, '0')}"
        return None
    digits = re.sub(r"\D", "", text)
    if len(digits) <= 10:
        return None
    return f"{digits[:10]}.{digits[10:16].ljust(6, '0')}"


def _slack_ts_to_datetime(ts: str | None) -> datetime | None:
    if not ts:
        return None
    try:
        return datetime.fromtimestamp(float(ts), tz=UTC)
    except (TypeError, ValueError, OSError):
        return None


def _dedupe(values: list[str]) -> list[str]:
    seen: set[str] = set()
    result: list[str] = []
    for value in values:
        if value in seen:
            continue
        seen.add(value)
        result.append(value)
    return result


def _log_field_expr(field: str, value: str) -> str:
    escaped = value.replace("\\", "\\\\").replace('"', '\\"')
    return f'{field}:"{escaped}"'


def _thread_key_candidates(
    *,
    channel_id: str,
    thread_ts: str,
    team_id: str | None = None,
    source: str = "slack",
) -> list[str]:
    candidates = []
    if team_id:
        candidates.extend(
            [
                f"{source}:{team_id}:{channel_id}:{thread_ts}",
                f"slack:{team_id}:{channel_id}:{thread_ts}",
                f"chat:{team_id}:{channel_id}:{thread_ts}",
            ]
        )
    candidates.extend(
        [
            f"{source}:{channel_id}:{thread_ts}",
            f"slack:{channel_id}:{thread_ts}",
            f"chat:{channel_id}:{thread_ts}",
        ]
    )
    return _dedupe(candidates)


def _first_qs(query: dict[str, list[str]], *names: str) -> str | None:
    for name in names:
        values = query.get(name)
        if values:
            return values[0]
    return None


def _clean_reference_text(reference: str) -> str:
    text = reference.strip()
    if text.startswith("<") and ">" in text:
        text = text[1 : text.index(">")]
    if "|" in text and text.startswith("http"):
        text = text.split("|", 1)[0]
    return text.strip()


def parse_slack_reference(reference: str) -> dict[str, Any]:
    """Parse a Slack permalink or Centaur thread key into identifiers only."""
    text = _clean_reference_text(reference)
    direct = _SLACK_THREAD_KEY_RE.search(text)
    if direct:
        thread_key = direct.group("thread_key")
        channel_id = direct.group("channel")
        team_id = direct.group("team")
        thread_ts = _normalize_ts(direct.group("thread_ts"))
        if not thread_ts:
            return {"status": "error", "error": "invalid thread timestamp"}
        source = thread_key.split(":", 1)[0]
        return {
            "status": "ok",
            "input": reference,
            "kind": "thread_key",
            "source": source,
            "team_id": team_id,
            "channel_id": channel_id,
            "message_ts": thread_ts,
            "thread_ts": thread_ts,
            "thread_datetime": _isoformat(_slack_ts_to_datetime(thread_ts)),
            "thread_key": thread_key,
            "thread_key_candidates": _thread_key_candidates(
                channel_id=channel_id,
                thread_ts=thread_ts,
                team_id=team_id,
                source=source,
            ),
            "thread_key_like": f"%:{channel_id}:{thread_ts}",
            "channel_key_like": f"%:{channel_id}:%",
        }

    channel_ts = _CHANNEL_TS_RE.search(text)
    if channel_ts:
        channel_id = channel_ts.group("channel")
        thread_ts = _normalize_ts(channel_ts.group("thread_ts"))
        if thread_ts:
            return {
                "status": "ok",
                "input": reference,
                "kind": "channel_ts",
                "source": "slack",
                "team_id": None,
                "channel_id": channel_id,
                "message_ts": thread_ts,
                "thread_ts": thread_ts,
                "thread_datetime": _isoformat(_slack_ts_to_datetime(thread_ts)),
                "thread_key": f"slack:{channel_id}:{thread_ts}",
                "thread_key_candidates": _thread_key_candidates(
                    channel_id=channel_id,
                    thread_ts=thread_ts,
                ),
                "thread_key_like": f"%:{channel_id}:{thread_ts}",
                "channel_key_like": f"%:{channel_id}:%",
            }

    url_match = _SLACK_URL_RE.search(text)
    if not url_match and text.startswith(("http://", "https://", "slack://")):
        url = text
    elif url_match:
        url = url_match.group(0)
    else:
        return {"status": "error", "error": "no Slack permalink or thread_key found"}

    parsed = urlparse(url)
    query = parse_qs(parsed.query)
    team_id = _first_qs(query, "team", "team_id")
    channel_id = _first_qs(query, "cid", "channel", "channel_id", "id")
    message_ts = _normalize_ts(_first_qs(query, "message", "ts"))

    path_match = re.search(r"/archives/(?P<channel>[A-Z0-9]+)/p(?P<ts>\d+)", parsed.path)
    if path_match:
        channel_id = channel_id or path_match.group("channel")
        message_ts = message_ts or _normalize_ts(path_match.group("ts"))

    thread_ts = _normalize_ts(_first_qs(query, "thread_ts")) or message_ts
    if parsed.scheme == "slack":
        channel_id = channel_id or _first_qs(query, "id")
        thread_ts = _normalize_ts(_first_qs(query, "thread_ts", "message", "ts")) or thread_ts
        message_ts = message_ts or _normalize_ts(_first_qs(query, "message", "ts"))

    if not channel_id or not thread_ts:
        return {"status": "error", "error": "could not parse Slack channel and thread timestamp"}

    message_ts = message_ts or thread_ts
    return {
        "status": "ok",
        "input": reference,
        "kind": "slack_permalink",
        "source": "slack",
        "team_id": team_id,
        "channel_id": channel_id,
        "message_ts": message_ts,
        "thread_ts": thread_ts,
        "thread_datetime": _isoformat(_slack_ts_to_datetime(thread_ts)),
        "message_datetime": _isoformat(_slack_ts_to_datetime(message_ts)),
        "thread_key": f"slack:{channel_id}:{thread_ts}",
        "thread_key_candidates": _thread_key_candidates(
            channel_id=channel_id,
            thread_ts=thread_ts,
            team_id=team_id,
        ),
        "thread_key_like": f"%:{channel_id}:{thread_ts}",
        "channel_key_like": f"%:{channel_id}:%",
        "permalink": f"https://slack.com/archives/{channel_id}/p{message_ts.replace('.', '')}",
    }


def _thread_key_equivalents(thread_key: str | None) -> list[str]:
    if not thread_key:
        return []
    text = thread_key.strip()
    if not text:
        return []
    values = [text]
    parsed = parse_slack_reference(text)
    if parsed.get("status") == "ok":
        values.extend(str(value) for value in parsed.get("thread_key_candidates") or [] if value)
    return _dedupe(values)


def _thread_keys_intersect(left: list[str], right: list[str]) -> bool:
    left_values: set[str] = set()
    for value in left:
        left_values.update(_thread_key_equivalents(value))
    right_values: set[str] = set()
    for value in right:
        right_values.update(_thread_key_equivalents(value))
    return bool(left_values & right_values)


def _parsed_for_thread_key(thread_key: str) -> dict[str, Any]:
    parsed = parse_slack_reference(thread_key)
    if parsed.get("status") == "ok":
        return parsed
    return {
        "status": "ok",
        "kind": "thread_key",
        "thread_key": thread_key.strip(),
        "thread_key_candidates": [thread_key.strip()],
        "thread_key_like": None,
        "channel_key_like": None,
        "channel_id": None,
        "thread_ts": None,
    }


def _safe_load_module(module_name: str, path: Path) -> Any | None:
    if not path.exists():
        return None
    spec = importlib.util.spec_from_file_location(module_name, path)
    if spec is None or spec.loader is None:
        return None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class CentaurInvestigatorClient:
    """Investigate Centaur state through readonly Postgres access."""

    def __init__(self, database_url: str | None = None) -> None:
        self._database_url = (database_url or _scoped_database_url()).strip()

    @staticmethod
    def _scope_error(target: str) -> dict[str, Any]:
        current = _current_thread_key()
        if current:
            error = (
                "centaur-investigator is self-scoped by default and cannot inspect "
                f"{target!r} from current thread {current!r}. "
                f"Set {CENTAUR_INVESTIGATOR_OPERATOR_ENV}=1 only in an operator sandbox."
            )
        else:
            error = (
                "centaur-investigator is self-scoped by default and requires "
                f"{CENTAUR_CURRENT_THREAD_ENV}. Set {CENTAUR_INVESTIGATOR_OPERATOR_ENV}=1 "
                "only in an operator sandbox for cross-thread investigation."
            )
        return {"status": "error", "error": error}

    @staticmethod
    def _authorize_thread_candidates(
        candidates: list[str],
        *,
        target: str,
    ) -> dict[str, Any]:
        if _operator_mode_enabled():
            return {
                "status": "ok",
                "scope": "operator",
                "current_thread_key": _current_thread_key(),
            }
        current = _current_thread_key()
        if not current:
            return CentaurInvestigatorClient._scope_error(target)
        if not _thread_keys_intersect([current], candidates):
            return CentaurInvestigatorClient._scope_error(target)
        return {"status": "ok", "scope": "self", "current_thread_key": current}

    @staticmethod
    def _apply_self_scope(parsed: dict[str, Any], authz: dict[str, Any]) -> dict[str, Any]:
        if authz.get("scope") != "self":
            return parsed
        current = str(authz.get("current_thread_key") or "").strip()
        candidates = _dedupe(
            _thread_key_equivalents(current)
            + [
                str(value)
                for value in parsed.get("thread_key_candidates") or []
                if value and _thread_keys_intersect([current], [str(value)])
            ]
        )
        scoped = dict(parsed)
        scoped["thread_key"] = current or scoped.get("thread_key")
        scoped["thread_key_candidates"] = candidates or [current]
        scoped["thread_key_like"] = None
        scoped["channel_key_like"] = None
        scoped["scope"] = "self"
        return scoped

    def _require_database_url(self) -> str:
        if not self._database_url:
            raise RuntimeError(f"{CENTAUR_POSTGRES_DSN_ENV} is required")
        return self._database_url

    async def _connect(self) -> asyncpg.Connection:
        return await asyncpg.connect(
            _database_url_with_name(self._require_database_url(), _postgres_database_name()),
            command_timeout=30,
        )

    async def _safe_fetch(
        self,
        conn: asyncpg.Connection,
        label: str,
        query: str,
        *args: Any,
    ) -> dict[str, Any]:
        try:
            rows = await conn.fetch(query, *args)
            return {
                "status": "ok",
                "count": len(rows),
                "rows": [_record_to_dict(row) for row in rows],
            }
        except Exception as exc:
            return {"status": "unavailable", "label": label, "error": str(exc), "rows": []}

    async def _safe_fetchrow(
        self,
        conn: asyncpg.Connection,
        label: str,
        query: str,
        *args: Any,
    ) -> dict[str, Any]:
        try:
            row = await conn.fetchrow(query, *args)
            return {"status": "ok", "row": _record_to_dict(row) if row else None}
        except Exception as exc:
            return {"status": "unavailable", "label": label, "error": str(exc), "row": None}

    def parse_thread_reference(self, reference: str) -> dict[str, Any]:
        """Parse a Slack thread permalink or Centaur thread key."""
        return parse_slack_reference(reference)

    async def _session_state_async(
        self,
        thread_key: str,
        *,
        limit: int,
        include_observability: bool,
        window_hours: int,
        logs_limit: int,
    ) -> dict[str, Any]:
        if not thread_key.strip() or not _KEY_SOURCE_RE.match(thread_key):
            return {"status": "error", "error": "thread_key must be namespaced"}

        parsed = _parsed_for_thread_key(thread_key.strip())
        authz = self._authorize_thread_candidates(
            [str(value) for value in parsed.get("thread_key_candidates") or [thread_key.strip()]],
            target=thread_key.strip(),
        )
        if authz.get("status") != "ok":
            return authz
        parsed = self._apply_self_scope(parsed, authz)

        conn = await self._connect()
        try:
            result = await self._collect_state(
                conn,
                parsed=parsed,
                limit=limit,
            )
        finally:
            await conn.close()

        result["scope"] = authz.get("scope")
        if include_observability:
            result["observability"] = self._observability(
                thread_keys=result.get("thread_keys") or [thread_key.strip()],
                execution_ids=result.get("execution_ids") or [],
                window_hours=window_hours,
                logs_limit=logs_limit,
            )
        return result

    def session_state(
        self,
        thread_key: str,
        limit: int = DEFAULT_LIMIT,
        include_observability: bool = True,
        window_hours: int = DEFAULT_WINDOW_HOURS,
        logs_limit: int = 100,
    ) -> dict[str, Any]:
        """Inspect source-of-truth state for a known thread_key."""
        try:
            return asyncio.run(
                self._session_state_async(
                    thread_key,
                    limit=_clamp(limit, minimum=1, maximum=MAX_LIMIT),
                    include_observability=include_observability,
                    window_hours=_clamp(window_hours, minimum=1, maximum=MAX_WINDOW_HOURS),
                    logs_limit=_clamp(logs_limit, minimum=1, maximum=MAX_LOG_LIMIT),
                )
            )
        except Exception as exc:
            return {"status": "error", "error": str(exc)}

    def self_state(
        self,
        limit: int = DEFAULT_LIMIT,
        include_observability: bool = True,
        window_hours: int = DEFAULT_WINDOW_HOURS,
        logs_limit: int = 100,
    ) -> dict[str, Any]:
        """Inspect only the current sandbox thread."""
        thread_key = _current_thread_key()
        if not thread_key:
            return self._scope_error("current thread")
        result = self.session_state(
            thread_key,
            limit=limit,
            include_observability=include_observability,
            window_hours=window_hours,
            logs_limit=logs_limit,
        )
        if result.get("status") == "error" and not _operator_mode_enabled():
            return self._local_self_state(
                thread_key,
                postgres_error=str(result.get("error") or "Postgres state unavailable"),
                include_observability=include_observability,
                window_hours=_clamp(window_hours, minimum=1, maximum=MAX_WINDOW_HOURS),
                logs_limit=_clamp(logs_limit, minimum=1, maximum=MAX_LOG_LIMIT),
            )
        return result

    def _local_self_state(
        self,
        thread_key: str,
        *,
        postgres_error: str,
        include_observability: bool,
        window_hours: int,
        logs_limit: int,
    ) -> dict[str, Any]:
        parsed = self._apply_self_scope(
            _parsed_for_thread_key(thread_key),
            {"status": "ok", "scope": "self", "current_thread_key": thread_key},
        )
        thread_keys = parsed.get("thread_key_candidates") or [thread_key]
        result: dict[str, Any] = {
            "status": "ok",
            "scope": "self",
            "parsed": parsed,
            "thread_keys": thread_keys,
            "execution_ids": [],
            "analysis": {
                "summary": (
                    "Current sandbox thread scope resolved. Postgres session state is unavailable."
                ),
                "findings": ["Resolved current sandbox thread key from CENTAUR_THREAD_KEY."],
                "warnings": [postgres_error],
                "primary_source": "local_sandbox_context",
            },
            "sandbox": {
                "hostname": os.getenv("HOSTNAME", ""),
                "workload": os.getenv("CENTAUR_WORKLOAD", ""),
                "harness_type": os.getenv("CENTAUR_HARNESS_TYPE", ""),
                "observability_enabled": os.getenv("CENTAUR_SANDBOX_OBSERVABILITY_ENABLED", ""),
                "repo_cache_enabled": os.getenv("CENTAUR_SANDBOX_REPO_CACHE_ENABLED", ""),
                "cwd": str(Path.cwd()),
            },
            "postgres": {
                "status": "unavailable",
                "error": postgres_error,
            },
        }
        if include_observability:
            result["observability"] = self._observability(
                thread_keys=[str(value) for value in thread_keys if value],
                execution_ids=[],
                window_hours=window_hours,
                logs_limit=logs_limit,
            )
        return result

    async def _collect_state(
        self,
        conn: asyncpg.Connection,
        *,
        parsed: dict[str, Any],
        limit: int,
    ) -> dict[str, Any]:
        candidates = parsed.get("thread_key_candidates") or [parsed.get("thread_key")]
        candidates = [str(value) for value in candidates if value]
        thread_key_like = parsed.get("thread_key_like")
        channel_key_like = parsed.get("channel_key_like")
        channel_id = parsed.get("channel_id")
        thread_ts = parsed.get("thread_ts")
        thread_dt = _slack_ts_to_datetime(thread_ts)

        connection = await self._safe_fetchrow(
            conn,
            "connection_role",
            """
            SELECT
                session_user,
                current_user,
                current_setting('role', true) AS active_role
            """,
        )
        sessions = await self._safe_fetch(
            conn,
            "sessions",
            """
            SELECT
                thread_key,
                sandbox_id,
                harness_type,
                harness_thread_id,
                persona_id,
                status,
                metadata ->> 'source' AS source,
                metadata ->> 'platform' AS platform,
                metadata ->> 'thread_id' AS external_thread_id,
                created_at,
                updated_at
            FROM sessions
            WHERE thread_key = ANY($1::text[])
               OR ($2::text IS NOT NULL AND thread_key LIKE $2)
            ORDER BY updated_at DESC NULLS LAST, created_at DESC
            LIMIT $3
            """,
            candidates,
            thread_key_like,
            limit,
        )
        matched_thread_keys = _dedupe(
            [str(row.get("thread_key")) for row in sessions["rows"] if row.get("thread_key")]
            + candidates
        )

        executions = await self._safe_fetch(
            conn,
            "session_executions",
            """
            SELECT
                execution_id,
                thread_key,
                status,
                metadata ->> 'model' AS model,
                metadata ->> 'harness_run_id' AS harness_run_id,
                metadata ->> 'base_image_ref' AS base_image_ref,
                metadata ->> 'base_image_hash' AS base_image_hash,
                metadata ->> 'overlay_hash' AS overlay_hash,
                metadata ->> 'source' AS source,
                metadata ->> 'platform' AS platform,
                metadata ->> 'action' AS action,
                CASE
                    WHEN metadata ->> 'idle_timeout_ms' ~ '^[0-9]+$'
                    THEN (metadata ->> 'idle_timeout_ms')::bigint
                END AS idle_timeout_ms,
                CASE
                    WHEN metadata ->> 'max_duration_ms' ~ '^[0-9]+$'
                    THEN (metadata ->> 'max_duration_ms')::bigint
                END AS max_duration_ms,
                created_at,
                updated_at,
                started_at,
                completed_at,
                extract(epoch FROM completed_at - started_at) AS duration_seconds
            FROM session_executions
            WHERE thread_key = ANY($1::text[])
               OR ($2::text IS NOT NULL AND thread_key LIKE $2)
            ORDER BY created_at DESC
            LIMIT $3
            """,
            matched_thread_keys,
            thread_key_like,
            limit,
        )
        execution_ids = _dedupe(
            [str(row.get("execution_id")) for row in executions["rows"] if row.get("execution_id")]
        )

        messages = await self._safe_fetch(
            conn,
            "session_messages",
            """
            SELECT
                message_id,
                thread_key,
                role,
                CASE
                    WHEN jsonb_typeof(parts) = 'array' THEN jsonb_array_length(parts)
                    ELSE 0
                END AS part_count,
                coalesce(
                    (
                        SELECT jsonb_agg(distinct coalesce(part_values.part ->> 'type', 'unknown'))
                        FROM jsonb_array_elements(
                            CASE
                                WHEN jsonb_typeof(parts) = 'array' THEN parts
                                ELSE '[]'::jsonb
                            END
                        ) AS part_values(part)
                    ),
                    '[]'::jsonb
                ) AS part_types,
                metadata ->> 'source' AS source,
                metadata ->> 'platform' AS platform,
                metadata ->> 'action' AS action,
                metadata ->> 'user_id' AS user_id,
                metadata ->> 'user_name' AS user_name,
                created_at
            FROM session_messages
            WHERE thread_key = ANY($1::text[])
               OR ($2::text IS NOT NULL AND thread_key LIKE $2)
            ORDER BY created_at ASC, message_id ASC
            LIMIT $3
            """,
            matched_thread_keys,
            thread_key_like,
            limit,
        )
        events = await self._safe_fetch(
            conn,
            "session_events",
            """
            SELECT
                event_id,
                thread_key,
                execution_id,
                event_type,
                payload ->> 'type' AS payload_type,
                payload ->> 'subtype' AS payload_subtype,
                payload ->> 'status' AS status,
                payload ->> 'terminal_reason' AS terminal_reason,
                payload ->> 'turn_id' AS turn_id,
                payload ? 'error' AS has_error,
                CASE
                    WHEN payload ? 'error' THEN octet_length(payload ->> 'error')
                END AS error_length,
                coalesce(
                    (
                        SELECT jsonb_agg(payload_keys.key)
                        FROM jsonb_object_keys(payload) AS payload_keys(key)
                    ),
                    '[]'::jsonb
                ) AS payload_keys,
                created_at
            FROM session_events
            WHERE thread_key = ANY($1::text[])
               OR ($2::text IS NOT NULL AND thread_key LIKE $2)
               OR (execution_id = ANY($3::text[]))
            ORDER BY event_id ASC
            LIMIT $4
            """,
            matched_thread_keys,
            thread_key_like,
            execution_ids,
            limit * 4,
        )
        legacy_runtime = await self._safe_fetch(
            conn,
            "agent_runtime_assignments",
            """
            SELECT
                thread_key,
                assignment_generation,
                runtime_id,
                harness,
                engine,
                persona_id,
                prompt_ref,
                effective_agents_md_sha256,
                state,
                created_at,
                updated_at,
                released_at
            FROM agent_runtime_assignments
            WHERE thread_key = ANY($1::text[])
               OR ($2::text IS NOT NULL AND thread_key LIKE $2)
            ORDER BY updated_at DESC NULLS LAST
            LIMIT $3
            """,
            matched_thread_keys,
            thread_key_like,
            limit,
        )
        legacy_executions = await self._safe_fetch(
            conn,
            "agent_execution_requests",
            """
            SELECT
                execution_id,
                thread_key,
                assignment_generation,
                execute_id,
                durable_turn_id,
                status,
                created_at,
                claimed_at,
                started_at,
                last_progress_at,
                silence_deadline_at,
                hard_deadline_at,
                stream_break_count,
                last_stream_break_at,
                completed_at,
                terminal_reason,
                worker_id IS NOT NULL AS claimed,
                updated_at
            FROM agent_execution_requests
            WHERE thread_key = ANY($1::text[])
               OR ($2::text IS NOT NULL AND thread_key LIKE $2)
            ORDER BY created_at DESC
            LIMIT $3
            """,
            matched_thread_keys,
            thread_key_like,
            limit,
        )
        sandbox_sessions = await self._safe_fetch(
            conn,
            "sandbox_sessions",
            """
            SELECT
                thread_key,
                sandbox_id,
                harness,
                engine,
                state,
                last_delivered_id,
                agent_thread_id,
                inflight_turn_id,
                inflight_started_at,
                inflight_attempts,
                last_result_at,
                trace_id,
                started_at,
                updated_at,
                wire_connected_at,
                wire_last_seen_at
            FROM sandbox_sessions
            WHERE thread_key = ANY($1::text[])
               OR ($2::text IS NOT NULL AND thread_key LIKE $2)
            ORDER BY updated_at DESC NULLS LAST
            LIMIT $3
            """,
            matched_thread_keys,
            thread_key_like,
            limit,
        )
        traces = await self._safe_fetch(
            conn,
            "thread_traces",
            """
            SELECT
                thread_key,
                trace_id,
                root_span_id,
                created_at,
                updated_at
            FROM thread_traces
            WHERE thread_key = ANY($1::text[])
               OR ($2::text IS NOT NULL AND thread_key LIKE $2)
            ORDER BY updated_at DESC NULLS LAST
            LIMIT $3
            """,
            matched_thread_keys,
            thread_key_like,
            limit,
        )

        nearby_sessions = {"status": "ok", "count": 0, "rows": []}
        if channel_key_like and thread_dt is not None:
            nearby_sessions = await self._safe_fetch(
                conn,
                "nearby_sessions",
                """
                SELECT
                    thread_key,
                    sandbox_id,
                    harness_type,
                    harness_thread_id,
                    persona_id,
                    status,
                    metadata ->> 'source' AS source,
                    metadata ->> 'platform' AS platform,
                    metadata ->> 'thread_id' AS external_thread_id,
                    created_at,
                    updated_at
                FROM sessions
                WHERE thread_key LIKE $1
                  AND created_at BETWEEN
                      ($2::timestamptz - ($3::int * interval '1 hour'))
                      AND ($2::timestamptz + ($3::int * interval '1 hour'))
                ORDER BY abs(extract(epoch FROM created_at - $2::timestamptz)) ASC
                LIMIT $4
                """,
                channel_key_like,
                thread_dt,
                24,
                limit,
            )

        slack: dict[str, Any] = {}
        if channel_id:
            slack["channel"] = await self._safe_fetchrow(
                conn,
                "slack_sync_channel",
                """
                SELECT
                    channel_id,
                    channel_name,
                    is_archived,
                    is_syncable,
                    member_count,
                    first_seen_at,
                    last_seen_at,
                    updated_at
                FROM slack_sync_channels
                WHERE channel_id = $1
                """,
                channel_id,
            )
            slack["checkpoint"] = await self._safe_fetchrow(
                conn,
                "slack_sync_checkpoint",
                """
                SELECT
                    channel_id,
                    watermark_ts,
                    last_run_id,
                    last_success_at,
                    last_error <> '' AS has_error,
                    created_at,
                    updated_at
                FROM slack_sync_checkpoints
                WHERE channel_id = $1
                """,
                channel_id,
            )
            slack["messages"] = await self._safe_fetch(
                conn,
                "slack_sync_messages",
                """
                SELECT
                    channel_id,
                    message_ts,
                    occurred_at,
                    thread_ts,
                    parent_message_ts,
                    is_thread_root,
                    user_id,
                    bot_id <> '' AS has_bot_id,
                    message_type,
                    message_subtype,
                    permalink,
                    reply_count,
                    latest_reply_ts,
                    thread_refreshed_at,
                    source_run_id,
                    first_seen_at,
                    last_seen_at,
                    updated_at
                FROM slack_sync_messages
                WHERE channel_id = $1
                  AND (
                      $2::text IS NULL
                      OR message_ts = $2
                      OR thread_ts = $2
                      OR parent_message_ts = $2
                  )
                ORDER BY occurred_at ASC NULLS LAST, message_ts ASC
                LIMIT $3
                """,
                channel_id,
                thread_ts,
                limit * 4,
            )
            message_ts_values = [
                str(row["message_ts"]) for row in slack["messages"]["rows"] if row.get("message_ts")
            ]
            slack["message_attachments"] = await self._safe_fetch(
                conn,
                "slack_sync_message_attachments",
                """
                SELECT
                    channel_id,
                    message_ts,
                    slack_file_id,
                    name,
                    title,
                    mimetype,
                    filetype,
                    size_bytes,
                    permalink,
                    download_status,
                    download_error <> '' AS has_download_error,
                    content_sha256 IS NOT NULL AS has_content_hash,
                    source_run_id,
                    first_seen_at,
                    last_seen_at,
                    updated_at
                FROM slack_sync_message_attachments
                WHERE channel_id = $1
                  AND message_ts = ANY($2::text[])
                ORDER BY updated_at DESC, slack_file_id ASC
                LIMIT $3
                """,
                channel_id,
                message_ts_values,
                limit * 2,
            )
            slack["backfill_jobs"] = await self._safe_fetch(
                conn,
                "slack_sync_backfill_jobs",
                """
                SELECT
                    job_id,
                    job_key,
                    job_type,
                    channel_id,
                    payload_json ->> 'thread_ts' AS thread_ts,
                    status,
                    priority,
                    attempt_count,
                    last_run_id,
                    last_enqueued_at,
                    last_started_at,
                    last_completed_at,
                    last_error <> '' AS has_error,
                    created_at,
                    updated_at
                FROM slack_sync_backfill_jobs
                WHERE channel_id = $1
                  AND ($2::text IS NULL OR payload_json ->> 'thread_ts' = $2)
                ORDER BY updated_at DESC
                LIMIT $3
                """,
                channel_id,
                thread_ts,
                limit,
            )
            slack["recent_sync_runs"] = await self._safe_fetch(
                conn,
                "slack_sync_runs",
                """
                SELECT
                    run_id,
                    workflow_run_id,
                    mode,
                    status,
                    channels_requested,
                    channels_synced,
                    channels_skipped,
                    channels_failed,
                    messages_fetched,
                    messages_upserted,
                    threads_fetched,
                    replies_fetched,
                    replies_upserted,
                    started_at,
                    finished_at,
                    error_text <> '' AS has_error,
                    metadata ->> 'source' AS source
                FROM slack_sync_runs
                WHERE channels_requested ? $1
                   OR channels_synced ? $1
                   OR channels_failed ? $1
                   OR channels_skipped ? $1
                ORDER BY started_at DESC
                LIMIT $2
                """,
                channel_id,
                min(limit, 20),
            )

        result = {
            "status": "ok",
            "parsed": parsed,
            "thread_keys": matched_thread_keys,
            "execution_ids": execution_ids,
            "analysis": self._summarize(
                parsed=parsed,
                sessions=sessions,
                executions=executions,
                messages=messages,
                events=events,
                legacy_runtime=legacy_runtime,
                legacy_executions=legacy_executions,
                sandbox_sessions=sandbox_sessions,
                slack=slack,
            ),
            "postgres": {
                "status": "ok",
                "role": _connection_role(connection),
                "connection": connection,
                "sessions": sessions,
                "nearby_sessions": nearby_sessions,
                "session_executions": executions,
                "session_messages": messages,
                "session_events": events,
                "legacy_agent_runtime_assignments": legacy_runtime,
                "legacy_agent_execution_requests": legacy_executions,
                "legacy_sandbox_sessions": sandbox_sessions,
                "thread_traces": traces,
                "slack": slack,
            },
        }
        return result

    @staticmethod
    def _summarize(
        *,
        parsed: dict[str, Any],
        sessions: dict[str, Any],
        executions: dict[str, Any],
        messages: dict[str, Any],
        events: dict[str, Any],
        legacy_runtime: dict[str, Any],
        legacy_executions: dict[str, Any],
        sandbox_sessions: dict[str, Any],
        slack: dict[str, Any],
    ) -> dict[str, Any]:
        findings: list[str] = []
        warnings: list[str] = []

        if sessions.get("rows"):
            statuses = sorted(
                {str(row.get("status")) for row in sessions["rows"] if row.get("status")}
            )
            findings.append(f"Found {len(sessions['rows'])} session row(s): {', '.join(statuses)}.")
        else:
            warnings.append("No session row matched the parsed thread key candidates.")

        if executions.get("rows"):
            terminal = [row for row in executions["rows"] if row.get("completed_at")]
            active = [
                row for row in executions["rows"] if row.get("status") in {"queued", "running"}
            ]
            findings.append(
                f"Found {len(executions['rows'])} execution row(s), "
                f"{len(active)} active and {len(terminal)} completed."
            )
            latest = executions["rows"][0]
            findings.append(
                "Latest execution "
                f"{latest.get('execution_id')} is {latest.get('status')}"
                + (
                    f" after {latest.get('duration_seconds')}s."
                    if latest.get("duration_seconds")
                    else "."
                )
            )
        else:
            warnings.append("No session execution matched this thread.")

        event_errors = [row for row in events.get("rows", []) if row.get("has_error")]
        if events.get("rows"):
            findings.append(f"Found {len(events['rows'])} sanitized session event row(s).")
        if event_errors:
            warnings.append(f"{len(event_errors)} session event row(s) indicate an error payload.")

        if messages.get("rows"):
            roles = sorted({str(row.get("role")) for row in messages["rows"] if row.get("role")})
            findings.append(
                f"Found {len(messages['rows'])} sanitized message row(s): {', '.join(roles)}."
            )

        if (
            legacy_runtime.get("rows")
            or legacy_executions.get("rows")
            or sandbox_sessions.get("rows")
        ):
            findings.append(
                "Runtime state is present: "
                f"{len(legacy_runtime.get('rows', []))} assignment(s), "
                f"{len(legacy_executions.get('rows', []))} execution request(s), "
                f"{len(sandbox_sessions.get('rows', []))} sandbox session(s)."
            )

        slack_messages = slack.get("messages", {}).get("rows", [])
        if parsed.get("channel_id") and not slack:
            warnings.append("Slack sync tables were not queried.")
        elif parsed.get("channel_id") and slack_messages:
            roots = [row for row in slack_messages if row.get("is_thread_root")]
            findings.append(
                f"Slack sync has {len(slack_messages)} message row(s) for the thread, "
                f"including {len(roots)} root row(s)."
            )
        elif parsed.get("channel_id"):
            warnings.append("Slack sync has no sanitized message row for this thread.")

        backfills = slack.get("backfill_jobs", {}).get("rows", [])
        failed_backfills = [row for row in backfills if row.get("has_error")]
        active_backfills = [
            row for row in backfills if row.get("status") in {"pending", "running", "claimed"}
        ]
        if active_backfills:
            findings.append(f"{len(active_backfills)} Slack backfill job(s) are still active.")
        if failed_backfills:
            warnings.append(f"{len(failed_backfills)} Slack backfill job(s) have errors.")

        channel = slack.get("channel", {}).get("row") if slack else None
        if channel:
            findings.append(
                "Slack channel "
                f"{channel.get('channel_id')} #{channel.get('channel_name')} "
                f"syncable={channel.get('is_syncable')} archived={channel.get('is_archived')}."
            )

        return {
            "summary": (
                " ".join(findings)
                if findings
                else "No matching Centaur source-of-truth state found."
            ),
            "findings": findings,
            "warnings": warnings,
            "primary_source": "postgres_readonly_tables",
        }

    async def _investigate_slack_thread_async(
        self,
        reference: str,
        *,
        limit: int,
        include_observability: bool,
        window_hours: int,
        logs_limit: int,
    ) -> dict[str, Any]:
        parsed = parse_slack_reference(reference)
        if parsed.get("status") != "ok":
            return parsed
        authz = self._authorize_thread_candidates(
            [str(value) for value in parsed.get("thread_key_candidates") or [] if value],
            target=reference,
        )
        if authz.get("status") != "ok":
            return authz
        parsed = self._apply_self_scope(parsed, authz)

        conn = await self._connect()
        try:
            result = await self._collect_state(conn, parsed=parsed, limit=limit)
        finally:
            await conn.close()

        result["scope"] = authz.get("scope")
        if include_observability:
            result["observability"] = self._observability(
                thread_keys=result.get("thread_keys") or parsed.get("thread_key_candidates") or [],
                execution_ids=result.get("execution_ids") or [],
                window_hours=window_hours,
                logs_limit=logs_limit,
            )
        return result

    def investigate_slack_thread(
        self,
        reference: str,
        limit: int = DEFAULT_LIMIT,
        include_observability: bool = True,
        window_hours: int = DEFAULT_WINDOW_HOURS,
        logs_limit: int = 100,
    ) -> dict[str, Any]:
        """Investigate a Slack thread link with sanitized readonly Postgres metadata."""
        try:
            return asyncio.run(
                self._investigate_slack_thread_async(
                    reference,
                    limit=_clamp(limit, minimum=1, maximum=MAX_LIMIT),
                    include_observability=include_observability,
                    window_hours=_clamp(window_hours, minimum=1, maximum=MAX_WINDOW_HOURS),
                    logs_limit=_clamp(logs_limit, minimum=1, maximum=MAX_LOG_LIMIT),
                )
            )
        except Exception as exc:
            return {"status": "error", "error": str(exc)}

    def investigate(
        self,
        query: str,
        limit: int = DEFAULT_LIMIT,
        include_observability: bool = True,
        window_hours: int = DEFAULT_WINDOW_HOURS,
        logs_limit: int = 100,
    ) -> dict[str, Any]:
        """Investigate natural-language text containing a Slack link or thread_key."""
        parsed = parse_slack_reference(query)
        if parsed.get("status") == "ok":
            return self.investigate_slack_thread(
                query,
                limit=limit,
                include_observability=include_observability,
                window_hours=window_hours,
                logs_limit=logs_limit,
            )
        direct_key = re.search(r"\b[A-Za-z][A-Za-z0-9_.-]*:[^\s<>|]+\b", query)
        if direct_key:
            return self.session_state(
                direct_key.group(0),
                limit=limit,
                include_observability=include_observability,
                window_hours=window_hours,
                logs_limit=logs_limit,
            )
        return {
            "status": "error",
            "error": "query must contain a Slack permalink or Centaur thread_key",
        }

    async def _search_sessions_async(
        self,
        *,
        query: str,
        channel_id: str,
        status: str,
        limit: int,
    ) -> dict[str, Any]:
        if not _operator_mode_enabled():
            return {
                "status": "error",
                "error": (
                    "search-sessions is an operator-only command because it scans multiple "
                    f"threads. Use `centaur-investigator self`, or set "
                    f"{CENTAUR_INVESTIGATOR_OPERATOR_ENV}=1 only in an operator sandbox."
                ),
            }
        conn = await self._connect()
        try:
            rows = await conn.fetch(
                """
                SELECT
                    thread_key,
                    sandbox_id,
                    harness_type,
                    harness_thread_id,
                    persona_id,
                    status,
                    metadata ->> 'source' AS source,
                    metadata ->> 'platform' AS platform,
                    metadata ->> 'thread_id' AS external_thread_id,
                    created_at,
                    updated_at
                FROM sessions
                WHERE ($1::text = '' OR thread_key ILIKE '%' || $1 || '%')
                  AND ($2::text = '' OR thread_key LIKE '%:' || $2 || ':%')
                  AND ($3::text = '' OR status = $3)
                ORDER BY updated_at DESC NULLS LAST, created_at DESC
                LIMIT $4
                """,
                query.strip(),
                channel_id.strip(),
                status.strip(),
                limit,
            )
            return {
                "status": "ok",
                "count": len(rows),
                "sessions": [_record_to_dict(row) for row in rows],
            }
        finally:
            await conn.close()

    def search_sessions(
        self,
        query: str = "",
        channel_id: str = "",
        status: str = "",
        limit: int = DEFAULT_LIMIT,
    ) -> dict[str, Any]:
        """Search recent Centaur sessions by thread_key substring, Slack channel, or status."""
        try:
            return asyncio.run(
                self._search_sessions_async(
                    query=query,
                    channel_id=channel_id,
                    status=status,
                    limit=_clamp(limit, minimum=1, maximum=MAX_LIMIT),
                )
            )
        except Exception as exc:
            return {"status": "error", "error": str(exc)}

    def _observability(
        self,
        *,
        thread_keys: list[str],
        execution_ids: list[str],
        window_hours: int,
        logs_limit: int,
    ) -> dict[str, Any]:
        result: dict[str, Any] = {
            "source": "best_effort_vlogs_vmetrics",
            "window_hours": window_hours,
            "privacy_note": (
                "Only aggregate observability metadata is returned. Raw log rows, "
                "Slack message text, and stored transcript context are never requested."
            ),
            "vlogs": {"status": "skipped"},
            "vmetrics": {"status": "skipped"},
        }

        infra_dir = Path(__file__).resolve().parent.parent
        vlogs_module = _safe_load_module(
            "_centaur_investigator_vlogs_client",
            infra_dir / "vlogs" / "client.py",
        )
        if vlogs_module is not None:
            try:
                vlogs = vlogs_module.VictoriaLogsClient()
                primary_thread = thread_keys[0] if thread_keys else ""
                thread_query = (
                    f"_time:{window_hours}h {_log_field_expr('thread_key', primary_thread)}"
                    if primary_thread
                    else ""
                )
                result["vlogs"] = {
                    "status": "ok",
                    "thread_key": primary_thread,
                    "log_hits": vlogs.hits(thread_query, step="1h") if thread_query else {},
                    "error_hits": (
                        vlogs.hits(f"{thread_query} AND level:error", step="1h")
                        if thread_query
                        else {}
                    ),
                    "event_names": (
                        vlogs.field_values("event", query=thread_query, limit=min(100, logs_limit))
                        if thread_query
                        else []
                    ),
                    "services": (
                        vlogs.field_values("service", query=thread_query, limit=min(50, logs_limit))
                        if thread_query
                        else []
                    ),
                    "tool_usage": (
                        vlogs.tool_usage_by_thread(
                            thread_key=primary_thread,
                            start=f"{window_hours}h",
                            limit=min(100, logs_limit),
                        )
                        if primary_thread
                        else []
                    ),
                    "execution_log_hits": {
                        execution_id: vlogs.hits(
                            (
                                f"_time:{window_hours}h "
                                f"{_log_field_expr('execution_id', execution_id)}"
                            ),
                            step="1h",
                        )
                        for execution_id in execution_ids[:3]
                    },
                }
            except Exception as exc:
                result["vlogs"] = {"status": "error", "error": str(exc)}

        vmetrics_module = _safe_load_module(
            "_centaur_investigator_vmetrics_client",
            infra_dir / "vmetrics" / "client.py",
        )
        if vmetrics_module is not None:
            try:
                vmetrics = vmetrics_module.VictoriaMetricsClient()
                result["vmetrics"] = {
                    "status": "ok",
                    "ready": vmetrics.ready(),
                    "session_metric_names": vmetrics.metric_names(prefix="session_")[:50],
                    "centaur_metric_names": vmetrics.metric_names(prefix="centaur_")[:50],
                }
            except Exception as exc:
                result["vmetrics"] = {"status": "error", "error": str(exc)}

        return result


def _client() -> CentaurInvestigatorClient:
    return CentaurInvestigatorClient()
