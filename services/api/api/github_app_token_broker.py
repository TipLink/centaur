"""Token-broker shim for GitHub App installation tokens.

iron-token-broker owns OAuth refresh-token credentials. GitHub App
installation tokens use a different flow: sign a short-lived app JWT, exchange
it for an installation access token, and cache that token until GitHub expires
it. This shim speaks iron-token-broker's read API so iron-proxy can keep using
the same ``token_broker`` source for GitHub traffic.
"""

from __future__ import annotations

import asyncio
import os
import time
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from urllib.parse import quote

import httpx
import jwt
from fastapi import FastAPI, Header, HTTPException
from fastapi.responses import JSONResponse, Response


DEFAULT_CREDENTIAL_ID = "github-app"
DEFAULT_REFRESH_SKEW_SECONDS = 300
DEFAULT_TIMEOUT_SECONDS = 20.0

app = FastAPI(title="github-app-token-broker")


@dataclass(frozen=True)
class _CachedToken:
    access_token: str
    expires_at: datetime


_cached_token: _CachedToken | None = None
_cache_lock = asyncio.Lock()


def _env(name: str, default: str = "") -> str:
    return (os.environ.get(name) or default).strip()


def _credential_id() -> str:
    return (
        _env("GITHUB_APP_BROKER_CREDENTIAL_ID", DEFAULT_CREDENTIAL_ID)
        or DEFAULT_CREDENTIAL_ID
    )


def _refresh_skew() -> timedelta:
    raw = _env(
        "GITHUB_APP_TOKEN_REFRESH_SKEW_SECONDS",
        str(DEFAULT_REFRESH_SKEW_SECONDS),
    )
    try:
        seconds = int(raw)
    except ValueError as exc:
        raise HTTPException(
            status_code=500,
            detail="GITHUB_APP_TOKEN_REFRESH_SKEW_SECONDS must be an integer",
        ) from exc
    return timedelta(seconds=max(seconds, 0))


def _expires_at(value: str) -> datetime:
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def _format_expires_at(value: datetime) -> str:
    return value.astimezone(timezone.utc).isoformat().replace("+00:00", "Z")


def _private_key() -> str:
    key_file = _env("GITHUB_APP_PRIVATE_KEY_FILE")
    key = ""
    if key_file:
        try:
            with open(key_file, encoding="utf-8") as fp:
                key = fp.read().strip()
        except OSError as exc:
            raise HTTPException(
                status_code=500,
                detail="unable to read GITHUB_APP_PRIVATE_KEY_FILE",
            ) from exc
    else:
        key = _env("GITHUB_APP_PRIVATE_KEY")

    if not key:
        raise HTTPException(
            status_code=500,
            detail="GitHub App private key is not configured",
        )
    if "\\n" in key and "\n" not in key:
        key = key.replace("\\n", "\n")
    return key


def _github_app_jwt() -> str:
    app_id = _env("GITHUB_APP_ID")
    if not app_id:
        raise HTTPException(status_code=500, detail="GITHUB_APP_ID is not configured")
    now = int(time.time())
    payload = {
        "iat": now - 60,
        "exp": now + 600,
        "iss": app_id,
    }
    return jwt.encode(payload, _private_key(), algorithm="RS256")


def _github_token_endpoint() -> str:
    explicit = _env("GITHUB_APP_TOKEN_ENDPOINT")
    if explicit:
        return explicit
    installation_id = _env("GITHUB_APP_INSTALLATION_ID")
    if not installation_id:
        raise HTTPException(
            status_code=500,
            detail="GITHUB_APP_INSTALLATION_ID is not configured",
        )
    return f"https://api.github.com/app/installations/{installation_id}/access_tokens"


def _assert_authorized(authorization: str | None) -> None:
    expected = _env("IRON_BROKER_TOKEN")
    if not expected:
        raise HTTPException(
            status_code=500,
            detail="IRON_BROKER_TOKEN is not configured",
        )
    if authorization != f"Bearer {expected}":
        raise HTTPException(status_code=401, detail="unauthorized")


def _token_payload(token: _CachedToken) -> dict[str, str]:
    return {
        "access_token": token.access_token,
        "expires_at": _format_expires_at(token.expires_at),
    }


async def _mint_github_token() -> _CachedToken:
    async with httpx.AsyncClient(
        timeout=DEFAULT_TIMEOUT_SECONDS,
        trust_env=False,
    ) as client:
        response = await client.post(
            _github_token_endpoint(),
            headers={
                "Authorization": f"Bearer {_github_app_jwt()}",
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28",
            },
        )
    if response.status_code >= 400:
        raise HTTPException(status_code=502, detail="GitHub App token mint failed")
    body = response.json()
    token = body.get("token")
    expires_at = body.get("expires_at")
    if not isinstance(token, str) or not token:
        raise HTTPException(
            status_code=502,
            detail="GitHub App token response missing token",
        )
    if not isinstance(expires_at, str) or not expires_at:
        raise HTTPException(
            status_code=502,
            detail="GitHub App token response missing expires_at",
        )
    return _CachedToken(access_token=token, expires_at=_expires_at(expires_at))


async def _github_token_payload() -> dict[str, str]:
    global _cached_token

    now = datetime.now(timezone.utc)
    skew = _refresh_skew()
    if _cached_token and _cached_token.expires_at - skew > now:
        return _token_payload(_cached_token)

    async with _cache_lock:
        now = datetime.now(timezone.utc)
        if _cached_token and _cached_token.expires_at - skew > now:
            return _token_payload(_cached_token)
        _cached_token = await _mint_github_token()
        return _token_payload(_cached_token)


async def _forward_to_upstream(
    credential_id: str,
    authorization: str | None,
) -> Response:
    upstream = _env("TOKEN_BROKER_UPSTREAM_URL")
    if not upstream:
        raise HTTPException(status_code=404, detail="credential not found")
    url = f"{upstream.rstrip('/')}/credentials/{quote(credential_id, safe='')}/access_token"
    headers = {"Authorization": authorization} if authorization else {}
    async with httpx.AsyncClient(
        timeout=DEFAULT_TIMEOUT_SECONDS,
        trust_env=False,
    ) as client:
        response = await client.get(url, headers=headers)

    out_headers = {
        "Cache-Control": "no-store",
        "Pragma": "no-cache",
    }
    content_type = response.headers.get("content-type")
    if content_type:
        out_headers["Content-Type"] = content_type
    return Response(
        content=response.content,
        status_code=response.status_code,
        headers=out_headers,
    )


@app.get("/healthz")
async def healthz() -> dict[str, str]:
    return {"status": "ok"}


@app.get("/credentials/{credential_id}/access_token", response_model=None)
async def access_token(
    credential_id: str,
    authorization: str | None = Header(default=None),
) -> Response | JSONResponse:
    _assert_authorized(authorization)
    if credential_id == _credential_id():
        return JSONResponse(
            await _github_token_payload(),
            headers={"Cache-Control": "no-store", "Pragma": "no-cache"},
        )
    return await _forward_to_upstream(credential_id, authorization)
