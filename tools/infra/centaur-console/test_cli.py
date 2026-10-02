import importlib
import importlib.util
import json
import sys
from pathlib import Path
from unittest.mock import MagicMock, patch

from typer.testing import CliRunner

PACKAGE = Path(__file__).parent
spec = importlib.util.spec_from_file_location(
    "console_cli_test_package", PACKAGE / "__init__.py", submodule_search_locations=[str(PACKAGE)]
)
package = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = package
spec.loader.exec_module(package)
cli = importlib.import_module("console_cli_test_package.cli")


def test_approval_group_and_compatibility_alias_have_the_same_commands():
    runner = CliRunner()
    for app, args in [(cli.app, ["approvals", "--help"]), (cli.approvals, ["--help"])]:
        result = runner.invoke(app, args)
        assert result.exit_code == 0
        for command in ["actions", "call", "status", "cancel"]:
            assert command in result.output


def test_call_waits_once_and_returns_public_output():
    client = MagicMock()
    client.__enter__.return_value = client
    client.request_approval.return_value = {"id": "request-1"}
    completed = {"status": "succeeded", "result": {"output": {"message": "Hello world!"}}}
    client.wait_for_approval.return_value = completed
    with patch.object(cli, "get_client", return_value=client):
        result = CliRunner().invoke(
            cli.app, ["approvals", "call", "hello-world", "--arguments", "{}"]
        )
    assert result.exit_code == 0
    assert json.loads(result.stdout) == completed
    client.request_approval.assert_called_once_with("hello-world", {}, None)
    client.wait_for_approval.assert_called_once_with("request-1")


def test_no_wait_and_decline_never_resubmit():
    for extra, expected in [(["--no-wait"], 0), ([], 1)]:
        client = MagicMock()
        client.__enter__.return_value = client
        client.request_approval.return_value = {"id": "request-1"}
        client.wait_for_approval.return_value = {"status": "declined"}
        with patch.object(cli, "get_client", return_value=client):
            result = CliRunner().invoke(
                cli.app, ["approvals", "call", "hello-world", "--arguments", "{}", *extra]
            )
        assert result.exit_code == expected
        client.request_approval.assert_called_once()
        assert client.wait_for_approval.call_count == (0 if extra else 1)


def test_shim_discovery_finds_the_console_command():
    root = PACKAGE.parents[2]
    spec = importlib.util.spec_from_file_location(
        "approval_shim_test", root / "services/sandbox/install_tool_shims.py"
    )
    shims = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(shims)
    with patch.dict("os.environ", {"TOOL_ALLOWLIST": "centaur-console", "TOOL_BLOCKLIST": ""}):
        scripts = shims._discover_scripts([PACKAGE.parent])
    assert "centaur-console" in scripts
