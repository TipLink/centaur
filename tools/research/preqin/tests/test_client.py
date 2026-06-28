from __future__ import annotations

import pytest

from centaur_sdk.backends import StubBackend, configure

from centaur_tool_preqin.client import OPERATIONAL_TOKEN_PLACEHOLDER, PreqinClient


class FakeResponse:
    def __init__(
        self,
        payload: dict | None,
        status_code: int = 200,
        text: str = "",
        json_error: bool = False,
    ):
        self._payload = payload
        self.status_code = status_code
        self.text = text
        self._json_error = json_error

    def json(self) -> dict:
        if self._json_error:
            raise ValueError("not json")
        assert self._payload is not None
        return self._payload

    def raise_for_status(self) -> None:
        if self.status_code >= 400:
            raise RuntimeError(self.status_code)


class FakeHttpClient:
    def __init__(self):
        self.posts: list[dict] = []
        self.gets: list[dict] = []

    def post(self, url: str, **kwargs):
        self.posts.append({"url": url, **kwargs})
        return FakeResponse({"access_token": "token-123"})

    def get(self, url: str, **kwargs):
        self.gets.append({"url": url, **kwargs})
        return FakeResponse({"data": []})


def test_credential_status_does_not_treat_stub_placeholders_as_present():
    configure(StubBackend())
    status = PreqinClient().credential_status()

    assert status["PREQIN_OPERATIONAL_TOKEN"]["present"] is False
    assert status["PREQIN_USERNAME"]["present"] is False
    assert status["PREQIN_API_KEY"]["present"] is False
    assert "length" not in status["PREQIN_OPERATIONAL_TOKEN"]


def test_credential_status_does_not_disclose_secret_lengths():
    client = PreqinClient(username="user@example.com", api_key="super-secret-api-key")

    status = client.credential_status()

    assert status["PREQIN_USERNAME"] == {"present": True}
    assert status["PREQIN_API_KEY"] == {"present": True}


def test_auth_uses_username_and_api_key_multipart_form():
    fake = FakeHttpClient()
    client = PreqinClient(username="user", api_key="api-key")
    client._client = fake

    assert client._operational_access_token(force_refresh=True) == "token-123"

    request = fake.posts[0]
    assert request["url"] == "https://api.preqin.com/connect/token"
    assert request["files"] == {"username": (None, "user"), "apikey": (None, "api-key")}
    assert request["headers"] == {"Accept": "application/json"}


def test_auth_error_redacts_response_body_and_known_secret_values():
    class FailingAuthClient(FakeHttpClient):
        def post(self, url: str, **kwargs):
            self.posts.append({"url": url, **kwargs})
            return FakeResponse(
                {
                    "error": "invalid_client",
                    "username": "user@example.com",
                    "apikey": "super-secret-api-key",
                    "access_token": "leaked-token",
                },
                status_code=401,
            )

    client = PreqinClient(username="user@example.com", api_key="super-secret-api-key")
    client._client = FailingAuthClient()

    with pytest.raises(RuntimeError) as error:
        client._operational_access_token(force_refresh=True)

    message = str(error.value)
    assert "user@example.com" not in message
    assert "super-secret-api-key" not in message
    assert "leaked-token" not in message
    assert "<redacted>" in message


def test_auth_error_redacts_known_secret_values_from_neutral_json_fields():
    class FailingAuthClient(FakeHttpClient):
        def post(self, url: str, **kwargs):
            self.posts.append({"url": url, **kwargs})
            return FakeResponse(
                {
                    "message": (
                        "bad token super-secret-api-key for user@example.com "
                        "with bearer brokered-operational-token"
                    ),
                },
                status_code=401,
            )

    client = PreqinClient(username="user@example.com", api_key="super-secret-api-key")
    client._operational_token = "brokered-operational-token"
    client._client = FailingAuthClient()

    with pytest.raises(RuntimeError) as error:
        client._operational_access_token(force_refresh=True)

    message = str(error.value)
    assert "user@example.com" not in message
    assert "super-secret-api-key" not in message
    assert "brokered-operational-token" not in message
    assert "<redacted>" in message


def test_operational_get_error_redacts_raw_response_text(monkeypatch):
    class FailingGetClient(FakeHttpClient):
        def get(self, url: str, **kwargs):
            self.gets.append({"url": url, **kwargs})
            return FakeResponse(
                None,
                status_code=403,
                text="Authorization: Bearer operational-secret-token username=user@example.com",
                json_error=True,
            )

    monkeypatch.setenv(OPERATIONAL_TOKEN_PLACEHOLDER, "")
    client = PreqinClient(username="user@example.com", api_key="super-secret-api-key")
    client._operational_token = "operational-secret-token"
    client._client = FailingGetClient()

    with pytest.raises(RuntimeError) as error:
        client.get_funds(fund_name="Paradigm", size=1)

    message = str(error.value)
    assert "operational-secret-token" not in message
    assert "user@example.com" not in message
    assert "<redacted>" in message


def test_operational_get_error_redacts_brokered_token_from_raw_response_text(monkeypatch):
    class FailingGetClient(FakeHttpClient):
        def get(self, url: str, **kwargs):
            self.gets.append({"url": url, **kwargs})
            return FakeResponse(
                None,
                status_code=403,
                text="Authorization: Bearer brokered-operational-token",
                json_error=True,
            )

    monkeypatch.setenv(OPERATIONAL_TOKEN_PLACEHOLDER, "brokered-operational-token")
    client = PreqinClient()
    client._client = FailingGetClient()

    with pytest.raises(RuntimeError) as error:
        client.get_funds(fund_name="Paradigm", size=1)

    message = str(error.value)
    assert "brokered-operational-token" not in message
    assert "Bearer <redacted>" in message


def test_operational_get_uses_proxy_token_placeholder_before_direct_auth():
    configure(StubBackend())
    fake = FakeHttpClient()
    client = PreqinClient()
    client._client = fake

    client.get_funds(fund_name="Paradigm", size=1)

    assert not fake.posts
    assert fake.gets[0]["headers"]["Authorization"] == f"Bearer {OPERATIONAL_TOKEN_PLACEHOLDER}"


def test_operational_get_without_proxy_token_or_direct_credentials_sends_no_auth(monkeypatch):
    monkeypatch.setenv(OPERATIONAL_TOKEN_PLACEHOLDER, "")
    configure(StubBackend())
    fake = FakeHttpClient()
    client = PreqinClient()
    client._client = fake

    client.get_funds(fund_name="Paradigm", size=1)

    assert not fake.posts
    assert fake.gets[0]["headers"] == {"Accept": "application/json"}


def test_auth_health_uses_proxy_token_placeholder_without_direct_credentials():
    configure(StubBackend())
    fake = FakeHttpClient()
    client = PreqinClient()
    client._client = fake

    result = client.auth_health()

    assert result["ok"] is True
    assert result["method"] == "operational_get"
    assert not fake.posts
    assert fake.gets[0]["url"] == "https://api.preqin.com/api/FundManager"
    assert fake.gets[0]["params"] == {"Size": 1, "Page": 1}
    assert fake.gets[0]["headers"]["Authorization"] == f"Bearer {OPERATIONAL_TOKEN_PLACEHOLDER}"


def test_auth_health_uses_direct_credentials_for_local_runs(monkeypatch):
    monkeypatch.setenv(OPERATIONAL_TOKEN_PLACEHOLDER, "")
    fake = FakeHttpClient()
    client = PreqinClient(username="user", api_key="api-key")
    client._client = fake

    result = client.auth_health()

    assert result["ok"] is True
    assert result["method"] == "operational_get"
    assert fake.posts[0]["url"] == "https://api.preqin.com/connect/token"
    assert fake.posts[0]["files"] == {"username": (None, "user"), "apikey": (None, "api-key")}
    assert fake.gets[0]["url"] == "https://api.preqin.com/api/FundManager"
    assert fake.gets[0]["headers"]["Authorization"] == "Bearer token-123"
