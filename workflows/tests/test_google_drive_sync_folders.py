from __future__ import annotations

import importlib
import json
import sys
import types
from pathlib import Path


def _load_drive_sync():
    repo_root = Path(__file__).resolve().parents[2]
    if str(repo_root) not in sys.path:
        sys.path.insert(0, str(repo_root))

    api_module = sys.modules.get("api") or types.ModuleType("api")
    runtime_control = sys.modules.get("api.runtime_control") or types.ModuleType(
        "api.runtime_control"
    )
    runtime_control.canonical_json = lambda value: json.dumps(value, sort_keys=True)

    vm_metrics = types.ModuleType("api.vm_metrics")
    for name in (
        "record_etl_items_failed",
        "record_etl_items_seen",
        "record_etl_items_upserted",
        "record_slack_etl_rate_limit",
        "set_etl_active_scopes",
        "set_etl_failed_scopes",
        "set_etl_scope_sync_freshness_seconds",
    ):
        setattr(vm_metrics, name, lambda *_args, **_kwargs: None)

    workflow_engine = types.ModuleType("api.workflow_engine")
    workflow_engine.WorkflowContext = object

    api_module.runtime_control = runtime_control
    api_module.vm_metrics = vm_metrics
    api_module.workflow_engine = workflow_engine
    sys.modules.setdefault("api", api_module)
    sys.modules.setdefault("api.runtime_control", runtime_control)
    sys.modules["api.vm_metrics"] = vm_metrics
    sys.modules.setdefault("api.workflow_engine", workflow_engine)

    centaur_sdk = types.ModuleType("centaur_sdk")
    centaur_sdk.secret = lambda _name, default=None: default
    sys.modules.setdefault("centaur_sdk", centaur_sdk)

    return importlib.import_module("workflows.gsuite.drive_sync")


drive_sync = _load_drive_sync()


def _doc(file_id: str) -> dict:
    return {
        "id": file_id,
        "name": f"Doc {file_id}",
        "mimeType": drive_sync.GOOGLE_DOC_MIME_TYPE,
    }


def _folder(file_id: str) -> dict:
    return {
        "id": file_id,
        "name": f"Folder {file_id}",
        "mimeType": drive_sync.GOOGLE_FOLDER_MIME_TYPE,
    }


def test_configured_folder_ids_accept_ids_urls_and_dedupe(monkeypatch):
    monkeypatch.setenv(
        "GOOGLE_DRIVE_ETL_FOLDER_IDS",
        "folder_a1234, https://drive.google.com/drive/folders/folder_b1234?usp=drive_link",
    )

    assert drive_sync._configured_folder_ids(
        ["folder_b1234", "<https://drive.google.com/drive/folders/folder_c1234>"]
    ) == ["folder_a1234", "folder_b1234", "folder_c1234"]


def test_build_scopes_adds_non_checkpointed_recursive_folder_scopes():
    scopes = drive_sync._build_scopes(modified_after=None, folder_ids=["folder_a"])

    assert [scope.scope_id for scope in scopes] == ["all_visible", "folder:folder_a"]
    assert scopes[0].checkpointed is True
    assert scopes[1].checkpointed is False
    assert scopes[1].recursive is True
    assert scopes[1].query == "'folder_a' in parents and trashed = false"


def test_iter_scope_files_recursively_walks_folder_docs():
    root_query = drive_sync._build_folder_query("root")
    nested_query = drive_sync._build_folder_query("nested")

    class FakeClient:
        def __init__(self) -> None:
            self.calls: list[tuple[str, str | None]] = []

        def list_files(self, *, query, page_size, page_token=None):
            del page_size
            self.calls.append((query, page_token))
            if query == root_query and page_token is None:
                return {
                    "files": [_folder("nested"), _doc("root_doc_1")],
                    "nextPageToken": "page-2",
                }
            if query == root_query and page_token == "page-2":
                return {"files": [_doc("root_doc_2")]}
            if query == nested_query:
                return {"files": [_doc("nested_doc")]}
            raise AssertionError(f"unexpected query {query!r} token {page_token!r}")

    client = FakeClient()
    files = drive_sync._iter_scope_files(
        client,
        scope=drive_sync.DriveScope(
            scope_id="folder:root",
            query=root_query,
            recursive=True,
            checkpointed=False,
        ),
        page_size=100,
    )

    assert [file["id"] for file in files] == [
        "root_doc_1",
        "root_doc_2",
        "nested_doc",
    ]
    assert client.calls == [
        (root_query, None),
        (root_query, "page-2"),
        (nested_query, None),
    ]
