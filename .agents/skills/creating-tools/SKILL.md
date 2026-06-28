---
name: creating-tools
description: "Scaffold and build new Centaur tool integrations in tools/. Use when asked to create a new tool, add an API integration, or build a new client for an external service."
---

# Creating Tools

Build tools for the current Centaur runtime model: API metadata plus sandbox
CLI shims. Agents use `centaur-tools list`, `<tool> --help`, and direct tool
CLIs. Do not scaffold new tools around legacy `/tools/{name}/{method}` HTTP
routes.

## File Structure

Prefer the existing categorized layout under `tools/<category>/<name>/`.
Match nearby tools when choosing the category.

```text
tools/<category>/<name>/
|-- __init__.py
|-- .env.example
|-- client.py
|-- cli.py
|-- pyproject.toml
`-- tests/
```

## Metadata

Every tool needs a `pyproject.toml` with `[project.scripts]` and
`[tool.centaur]`.

```toml
[project]
name = "<name>"
description = "<One-line description of what the tool does>"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = [
    "httpx>=0.27.0",
    "typer>=0.12.0",
    "rich>=13.0.0",
    "python-dotenv>=1.0.0",
]

[project.scripts]
<name> = "<package>.cli:app"

[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"

[tool.centaur]
module = "client.py"
secrets = [
    {type = "http", name = "<NAME>_API_KEY", mode = "inject", inject_header = "Authorization", inject_formatter = "Bearer {{ .Value }}", hosts = ["api.example.com"]},
]
```

Use `optional_secrets` when a credential unlocks optional behavior but the
tool can still run without it. Use `type = "pg_dsn"` for Postgres access; set
`name` to the environment variable the sandbox should see and set `database`
to the upstream database name.

## Client

Rules:

- Do not call `load_dotenv()` in `client.py`.
- Import `secret` from `centaur_sdk.tool_sdk`.
- Keep a class-based client plus a `_client()` factory.
- Use `secret("KEY", default="")` for credentials.
- Public methods should have clear type hints and return JSON-serializable
  values.
- Keep mutating methods explicit with names like `create_`, `update_`, and
  `delete_`.

```python
"""<Name> API client."""

from __future__ import annotations

import httpx

from centaur_sdk.tool_sdk import secret


class NameClient:
    def __init__(self, api_key: str | None = None, timeout: float = 30.0) -> None:
        self._api_key = api_key
        self._timeout = timeout
        self._base_url = "https://api.example.com"

    def _api_key_or_raise(self) -> str:
        api_key = self._api_key or secret("<NAME>_API_KEY", default="")
        if not api_key:
            raise RuntimeError("<NAME>_API_KEY not set")
        return api_key

    def search(self, query: str, limit: int = 10) -> dict:
        response = httpx.get(
            f"{self._base_url}/search",
            headers={"Authorization": f"Bearer {self._api_key_or_raise()}"},
            params={"q": query, "limit": limit},
            timeout=self._timeout,
        )
        response.raise_for_status()
        return response.json()

    def health(self) -> dict:
        return {"status": "ok"}


def _client() -> NameClient:
    return NameClient()
```

## CLI

CLIs run inside agent sandboxes and locally. They may call `load_dotenv()` so
local development can use `.env`, but keep the implementation as a thin wrapper
around `client.py`.

```python
"""CLI for <Name>."""

from __future__ import annotations

from dotenv import load_dotenv

load_dotenv()

import json

import typer

from .client import _client

app = typer.Typer(name="<name>", help="<Description>")


@app.command()
def search(query: str, limit: int = 10) -> None:
    print(json.dumps(_client().search(query, limit=limit), indent=2))


@app.command()
def health() -> None:
    print(json.dumps(_client().health()))


if __name__ == "__main__":
    app()
```

Use a `health` command for credentialed tools whenever possible. It gives QA a
safe, non-mutating deployment check.

## Secrets

Document required secrets in `.env.example`:

```text
NAME_API_KEY=your-api-key-here
```

For production credentials, create a matching secret source and request rule
through the deployment's secret manager. The tool should keep using
`secret("KEY")`; iron-proxy and the sandbox runtime decide whether that becomes
a placeholder, injected header, OAuth token, brokered token, GCP auth token, or
local proxy DSN.

## Tests

Add focused tests for client behavior and CLI output. Avoid tests that hit a
real third-party API unless they are explicitly marked as live smoke tests.

Run the packaging validator before staging:

```bash
python3 scripts/validate_cli_packaging.py
```

## Verification

From a fresh sandbox or dev shell with shims installed:

```bash
centaur-tools list
<name> --help
<name> health
```

Use direct CLI commands for normal agent use. Use
`centaur-tools call <tool> <method> '<json>'` only for workflow-host
compatibility or for a method that intentionally has no standalone CLI command.

If the tool is missing, inspect:

- repo-cache source/ref and `TOOL_DIRS`
- the tool directory name
- `[tool.centaur] module = "client.py"`
- `[project.scripts]`
- sandbox shim install logs

Do not use `GET /tools`, `POST /admin/reload-tools`, or `/tools/...` curl calls
as the verification path for new tools.
