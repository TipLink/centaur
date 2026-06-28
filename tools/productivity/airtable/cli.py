"""CLI for Airtable."""

import json
import re

import typer
from dotenv import load_dotenv
from rich.console import Console

from .client import AirtableClient

load_dotenv()

app = typer.Typer(name="airtable", help="Airtable API client")


_SENSITIVE_KEY_PARTS = (
    "api_key",
    "apikey",
    "authorization",
    "credential",
    "password",
    "secret",
    "token",
)


def _redact_for_output(value: object) -> object:
    if isinstance(value, dict):
        redacted: dict[str, object] = {}
        for key, child in value.items():
            key_text = str(key)
            if any(part in key_text.casefold() for part in _SENSITIVE_KEY_PARTS):
                redacted[key_text] = "<redacted>"
            else:
                redacted[key_text] = _redact_for_output(child)
        return redacted
    if isinstance(value, list):
        return [_redact_for_output(child) for child in value]
    return value


def _safe_error(exc: BaseException) -> str:
    text = str(exc)
    for part in _SENSITIVE_KEY_PARTS:
        text = re.sub(
            rf"({re.escape(part)}[^:=]*[:=]\s*)[^,\s)\]}}]+",
            r"\1<redacted>",
            text,
            flags=re.IGNORECASE,
        )
    return text


@app.command("health")
def health():
    """Assert airtable connectivity and auth with a safe read-only check."""
    from .client import _client

    client = _client()
    try:
        details = client.preflight_access()
        payload = {"ok": True, "tool": "airtable", "error": None, "details": details}
    except Exception as exc:
        payload = {"ok": False, "tool": "airtable", "error": _safe_error(exc), "details": {}}
        # Health payload is recursively redacted.
        # codeql[py/clear-text-logging-sensitive-data]
        print(json.dumps(_redact_for_output(payload), indent=2, ensure_ascii=False, default=str))
        raise typer.Exit(1) from exc
    finally:
        close = getattr(client, "close", None)
        if callable(close):
            close()
    # Health payload is recursively redacted.
    # codeql[py/clear-text-logging-sensitive-data]
    print(json.dumps(_redact_for_output(payload), indent=2, ensure_ascii=False, default=str))


console = Console()


def _print(data: object) -> None:
    console.print_json(json.dumps(data, default=str))


@app.command()
def bases(limit: int = typer.Option(100, "--limit", "-n")) -> None:
    """List visible Airtable bases."""
    _print(AirtableClient().list_bases(limit=limit))


@app.command()
def schema(base_id: str) -> None:
    """Get a base schema."""
    _print(AirtableClient().schema(base_id))


@app.command()
def records(
    base_id: str,
    table: str,
    view: str | None = typer.Option(None, "--view"),
    max_records: int = typer.Option(100, "--max-records", "-n"),
) -> None:
    """List records from a table or view."""
    _print(AirtableClient().list_records(base_id, table, view=view, max_records=max_records))


@app.command()
def from_url(url: str, max_records: int = typer.Option(50, "--max-records", "-n")) -> None:
    """Read a compact snapshot from an Airtable table/view URL."""
    _print(AirtableClient().snapshot_from_url(url, max_records=max_records))


if __name__ == "__main__":
    app()
