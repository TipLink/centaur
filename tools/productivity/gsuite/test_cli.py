import json

from typer.testing import CliRunner

from gsuite import client
from gsuite.cli import app

runner = CliRunner()


def test_docs_bullets_command_prints_verification_summary(monkeypatch):
    monkeypatch.setattr(
        client,
        "docs_bullets",
        lambda document_id, match_prefix, bullet_preset, tab_id, dry_run: {
            "document_id": document_id,
            "match_prefix": match_prefix,
            "bullet_preset": bullet_preset,
            "matched_paragraphs": 2,
            "updated_paragraphs": 2,
            "verified_paragraphs": 2,
            "already_bulleted_paragraphs": 1,
            "dry_run": dry_run,
            "paragraphs": [
                {
                    "tab_id": None,
                    "paragraph_index": 1,
                    "before": "- First item",
                    "after": "First item",
                },
                {
                    "tab_id": "tab-2",
                    "paragraph_index": 3,
                    "before": "- Second item",
                    "after": "Second item",
                },
            ],
        },
    )

    result = runner.invoke(app, ["docs", "bullets", "doc-123"])

    assert result.exit_code == 0
    assert "Converted 2 paragraph(s) into Google Docs bullets" in result.output
    assert "Verification: matched 2, updated 2, verified 2, already bulleted 1" in result.output
    assert "paragraph 2:" in result.output
    assert "tab tab-2 paragraph 4:" in result.output


def test_sheets_read_json_prints_valid_json_without_rich_rendering(monkeypatch):
    monkeypatch.setattr(
        client,
        "sheets_read",
        lambda spreadsheet_id, range_notation: {
            "rows": [{"Name": "Ada", "Title": "Money\nMovement"}]
        },
    )

    result = runner.invoke(app, ["sheets", "read", "sheet-123", "--json"])

    assert result.exit_code == 0
    assert json.loads(result.output) == [{"Name": "Ada", "Title": "Money\nMovement"}]
    assert "Money\\nMovement" in result.output


def test_sheets_formatting_json_outputs_highlight_summary(monkeypatch):
    monkeypatch.setattr(
        client,
        "sheets_highlighted_rows",
        lambda spreadsheet_id, range_notation: {
            "spreadsheet_id": spreadsheet_id,
            "range": range_notation,
            "conditional_format_count": 0,
            "highlighted_row_count": 1,
            "row_numbers_by_color": {"yellow": [2]},
            "highlighted_rows": [{"row_number": 2, "color_name": "yellow"}],
        },
    )

    result = runner.invoke(
        app,
        ["sheets", "formatting", "sheet-123", "--range", "A1:AF10", "--json"],
    )

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["range"] == "A1:AF10"
    assert payload["row_numbers_by_color"] == {"yellow": [2]}
