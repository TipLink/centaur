"""CLI for approval-gated tool calls."""

import json

import typer

from .client import ApprovalsClient

app = typer.Typer(help="Request a privileged action in your Slack thread and await its result.")


@app.command()
def actions() -> None:
    """List configured actions for this Slack execution (JSON)."""
    client = ApprovalsClient()
    try:
        typer.echo(json.dumps(client.actions(), indent=2))
    finally:
        client.close()


@app.command()
def call(
    action: str,
    arguments: str = typer.Option(
        ..., "--arguments", help="Complete JSON object; contents are shown in Slack."
    ),
    idempotency_key: str | None = typer.Option(
        None, "--idempotency-key", help="Reuse this UUID only for an identical request."
    ),
    wait: bool = typer.Option(True, "--wait/--no-wait"),
) -> None:
    """Request once; await approval, outcome, and any explicitly public result fields."""
    client = ApprovalsClient()
    try:
        request = client.request(action, json.loads(arguments), idempotency_key)
        typer.echo(f"Approval request: {request['id']}", err=True)
        result = client.wait(request["id"]) if wait else request
        typer.echo(json.dumps(result, indent=2))
        if wait and result["status"] != "succeeded":
            raise typer.Exit(1)
    finally:
        client.close()


@app.command()
def status(request_id: str) -> None:
    """Read a request's status and result without executing it again (JSON)."""
    client = ApprovalsClient()
    try:
        typer.echo(json.dumps(client.status(request_id), indent=2))
    finally:
        client.close()


@app.command()
def cancel(request_id: str) -> None:
    """Cancel before execution is claimed; never undo an external side effect."""
    client = ApprovalsClient()
    try:
        typer.echo(json.dumps(client.cancel(request_id), indent=2))
    finally:
        client.close()
