from typer.testing import CliRunner

from gsuite import client
from gsuite.cli import app

runner = CliRunner()


def test_gmail_download_attachment_writes_selected_part(tmp_path, monkeypatch):
    output_path = tmp_path / "downloaded.pdf"
    monkeypatch.setattr(
        client,
        "_gmail_download_attachment_bytes",
        lambda message_id, part_id, attachment_id: (
            {
                "filename": "report.pdf",
                "part_id": part_id,
                "attachment_id": attachment_id or "",
            },
            b"pdfdata",
        ),
    )

    result = runner.invoke(
        app,
        [
            "gmail",
            "download-attachment",
            "msg-1",
            "--part-id",
            "2",
            "--output",
            str(output_path),
        ],
    )

    assert result.exit_code == 0
    assert output_path.read_bytes() == b"pdfdata"
    assert "Downloaded report.pdf" in result.output


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
