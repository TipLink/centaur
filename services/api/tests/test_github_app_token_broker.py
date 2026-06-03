from __future__ import annotations

from datetime import datetime, timedelta, timezone

import httpx
import pytest
from fastapi import Response

from api import github_app_token_broker as broker


@pytest.fixture(autouse=True)
def reset_broker(monkeypatch: pytest.MonkeyPatch) -> None:
    broker._cached_token = None
    monkeypatch.setenv("IRON_BROKER_TOKEN", "broker-secret")
    monkeypatch.setenv("GITHUB_APP_BROKER_CREDENTIAL_ID", "fineas-github-app")


@pytest.mark.asyncio
async def test_access_token_requires_broker_auth() -> None:
    transport = httpx.ASGITransport(app=broker.app)
    async with httpx.AsyncClient(transport=transport, base_url="http://test") as client:
        response = await client.get("/credentials/fineas-github-app/access_token")

    assert response.status_code == 401


@pytest.mark.asyncio
async def test_access_token_mints_and_caches_github_token(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls = 0

    async def fake_mint() -> broker._CachedToken:
        nonlocal calls
        calls += 1
        return broker._CachedToken(
            access_token="ghs_fresh",
            expires_at=datetime.now(timezone.utc) + timedelta(hours=1),
        )

    monkeypatch.setattr(broker, "_mint_github_token", fake_mint)

    transport = httpx.ASGITransport(app=broker.app)
    async with httpx.AsyncClient(transport=transport, base_url="http://test") as client:
        first = await client.get(
            "/credentials/fineas-github-app/access_token",
            headers={"Authorization": "Bearer broker-secret"},
        )
        second = await client.get(
            "/credentials/fineas-github-app/access_token",
            headers={"Authorization": "Bearer broker-secret"},
        )

    assert first.status_code == 200
    assert first.json()["access_token"] == "ghs_fresh"
    assert second.status_code == 200
    assert second.json()["access_token"] == "ghs_fresh"
    assert calls == 1


@pytest.mark.asyncio
async def test_access_token_forwards_other_credentials(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    seen: dict[str, str | None] = {}

    async def fake_forward(credential_id: str, authorization: str | None) -> Response:
        seen["credential_id"] = credential_id
        seen["authorization"] = authorization
        return Response(
            content=b'{"access_token":"other"}',
            media_type="application/json",
        )

    monkeypatch.setattr(broker, "_forward_to_upstream", fake_forward)

    transport = httpx.ASGITransport(app=broker.app)
    async with httpx.AsyncClient(transport=transport, base_url="http://test") as client:
        response = await client.get(
            "/credentials/openai-codex/access_token",
            headers={"Authorization": "Bearer broker-secret"},
        )

    assert response.status_code == 200
    assert response.json() == {"access_token": "other"}
    assert seen == {
        "credential_id": "openai-codex",
        "authorization": "Bearer broker-secret",
    }
