"""CLI for Attio CRM."""

import json
import sys
from typing import Any

import typer
from dotenv import load_dotenv
from rich.console import Console
from centaur_sdk import Table

from .client import AttioClient

load_dotenv()

app = typer.Typer(name="attio", help="Attio CRM CLI for AI agents")
console = Console()


def _get_client() -> AttioClient:
    return AttioClient()


def _extract_value(val: dict | list | None) -> str:
    """Extract display value from Attio value object."""
    if val is None:
        return ""
    if isinstance(val, list):
        if not val:
            return ""
        val = val[0]
    if isinstance(val, dict):
        for key in ["value", "full_name", "name", "domain", "email_address", "phone_number"]:
            if key in val:
                return str(val[key])
        if "first_name" in val:
            return f"{val.get('first_name', '')} {val.get('last_name', '')}".strip()
    return str(val)


def _record_id(record: dict) -> str:
    return record.get("id", {}).get("record_id", "")


def _value_list(values: dict, slug: str) -> list[dict]:
    value = values.get(slug)
    return value if isinstance(value, list) else []


def _first_text(values: dict, slug: str) -> str:
    return _extract_value(_value_list(values, slug))


def _status_title(values: dict, slug: str = "deal_stage") -> str:
    values_list = _value_list(values, slug)
    if not values_list:
        return ""
    status = values_list[0].get("status", {})
    return status.get("title", "")


def _option_titles(values: dict, slug: str) -> list[str]:
    titles: list[str] = []
    for value in _value_list(values, slug):
        option = value.get("option", {})
        title = option.get("title")
        if title:
            titles.append(title)
    return titles


def _actor_id(values: dict, slug: str = "deal_owner") -> str:
    values_list = _value_list(values, slug)
    if not values_list:
        return ""
    return values_list[0].get("referenced_actor_id", "")


def _record_refs(values: dict, slug: str) -> list[str]:
    return [
        value.get("target_record_id", "")
        for value in _value_list(values, slug)
        if value.get("target_record_id")
    ]


def _phone_values(values: dict) -> list[str]:
    phones: list[str] = []
    for value in _value_list(values, "phone_numbers"):
        phone = value.get("original_phone_number") or value.get("phone_number")
        if phone:
            phones.append(phone)
    return phones


def _email_values(values: dict) -> list[str]:
    emails: list[str] = []
    for value in _value_list(values, "email_addresses"):
        email = value.get("email_address")
        if email:
            emails.append(email)
    return emails


def _normalize_phone(value: str) -> str:
    digits = "".join(ch for ch in value if ch.isdigit())
    if len(digits) == 10:
        return f"+1{digits}"
    if value.strip().startswith("+") and digits:
        return f"+{digits}"
    return value.strip()


def _person_name_value(name: str) -> dict[str, str]:
    parts = [part for part in name.strip().split() if part]
    first_name = parts[0] if parts else name.strip()
    last_name = " ".join(parts[1:]) if len(parts) > 1 else ""
    return {
        "first_name": first_name,
        "last_name": last_name,
        "full_name": name.strip(),
    }


def _option_values(csv_value: str | None) -> list[dict[str, str]]:
    return [{"option": value} for value in _csv_option(csv_value)]


def _record_reference(object_slug: str, record_id: str) -> dict[str, str]:
    return {"target_object": object_slug, "target_record_id": record_id}


def _record_url(record: dict) -> str:
    return record.get("web_url", "")


def _matches_any(actual: list[str], expected: list[str]) -> bool:
    if not expected:
        return True
    actual_norm = {item.casefold() for item in actual}
    return any(item.casefold() in actual_norm for item in expected)


def _csv_option(value: str | None) -> list[str]:
    if not value:
        return []
    return [part.strip() for part in value.split(",") if part.strip()]


def _pipeline_filter(
    owner_id: str | None,
    stage_id: str | None,
) -> dict[str, Any] | None:
    filters: list[dict[str, Any]] = []
    if owner_id:
        filters.append(
            {
                "deal_owner": {
                    "referenced_actor_type": "workspace-member",
                    "referenced_actor_id": owner_id,
                }
            }
        )
    if stage_id:
        filters.append({"deal_stage": {"status": stage_id}})
    if not filters:
        return None
    if len(filters) == 1:
        return filters[0]
    return {"$and": filters}


def _shape_pipeline_record(record: dict, people_by_id: dict[str, dict] | None = None) -> dict:
    values = record.get("values", {})
    people_by_id = people_by_id or {}
    person_ids = _record_refs(values, "associated_people")
    people: list[str] = []
    phones: list[str] = []
    emails: list[str] = []
    for person_id in person_ids:
        person = people_by_id.get(person_id)
        if not person:
            continue
        person_values = person.get("values", {})
        name = _first_text(person_values, "name")
        if name:
            people.append(name)
        phones.extend(_phone_values(person_values))
        emails.extend(_email_values(person_values))

    return {
        "record_id": _record_id(record),
        "web_url": record.get("web_url", ""),
        "deal_name": _first_text(values, "deal_name"),
        "stage": _status_title(values),
        "owner_id": _actor_id(values),
        "source": _option_titles(values, "source_v2"),
        "channel": _option_titles(values, "channel"),
        "crypto_platforms": _option_titles(values, "crypto_platforms"),
        "location": _first_text(values, "location"),
        "people": people,
        "phones": phones,
        "emails": emails,
        "associated_people": person_ids,
        "associated_company": _record_refs(values, "associated_company"),
    }


def _first_record(records: list[dict]) -> dict | None:
    return records[0] if records else None


def _existing_record_refs(record: dict, slug: str) -> list[dict[str, str]]:
    values = record.get("values", {})
    refs: list[dict[str, str]] = []
    for value in _value_list(values, slug):
        target_record_id = value.get("target_record_id")
        target_object = value.get("target_object")
        if target_record_id and target_object:
            refs.append(_record_reference(target_object, target_record_id))
    return refs


def _merge_record_refs(*groups: list[dict[str, str]]) -> list[dict[str, str]]:
    merged: dict[tuple[str, str], dict[str, str]] = {}
    for refs in groups:
        for ref in refs:
            key = (ref["target_object"], ref["target_record_id"])
            merged[key] = ref
    return list(merged.values())


@app.command()
def whoami():
    """Show info about current API token."""
    client = _get_client()
    info = client.get_self()
    console.print(f"[bold]Workspace:[/] {info.get('workspace', {}).get('name', 'N/A')}")
    console.print(f"[bold]Workspace ID:[/] {info.get('workspace', {}).get('id', 'N/A')}")


@app.command()
def objects():
    """List all objects in the workspace."""
    client = _get_client()
    objs = client.list_objects()

    if not objs:
        console.print("[yellow]No objects found.[/]")
        raise typer.Exit()

    table = Table(title=f"Objects ({len(objs)})")
    table.add_column("Slug", style="cyan", max_width=25)
    table.add_column("Name", style="white", max_width=25)
    table.add_column("Type", style="green", max_width=15)

    for obj in objs:
        api_slug = obj.get("api_slug", "")
        singular = obj.get("singular_noun", "")
        obj_type = "standard" if obj.get("is_standard", False) else "custom"
        table.add_row(api_slug, singular, obj_type)

    console.print(table)


@app.command()
def attributes(
    object_slug: str = typer.Argument(..., help="Object slug (e.g., 'people', 'companies')"),
):
    """List attributes for an object."""
    client = _get_client()
    attrs = client.list_attributes(object_slug)

    if not attrs:
        console.print("[yellow]No attributes found.[/]")
        raise typer.Exit()

    table = Table(title=f"Attributes for {object_slug} ({len(attrs)})")
    table.add_column("Slug", style="cyan", max_width=25)
    table.add_column("Title", style="white", max_width=25)
    table.add_column("Type", style="green", max_width=15)
    table.add_column("Required", style="yellow", max_width=8)

    for attr in attrs:
        api_slug = attr.get("api_slug", "")
        title = attr.get("title", "")
        attr_type = attr.get("type", "")
        required = "yes" if attr.get("is_required", False) else ""
        table.add_row(api_slug, title, attr_type, required)

    console.print(table)


@app.command()
def people(
    limit: int = typer.Option(25, "--limit", "-n", help="Max results"),
    filter_name: str = typer.Option(None, "--name", help="Filter by name"),
    filter_email: str = typer.Option(None, "--email", help="Filter by email"),
    json_output: bool = typer.Option(False, "--json", "-j", help="Output as JSON"),
):
    """List people records."""
    client = _get_client()

    filter_obj = None
    if filter_name:
        filter_obj = {"name": filter_name}
    elif filter_email:
        filter_obj = {"email_addresses": filter_email}

    records = client.query_records("people", filter_obj=filter_obj, limit=limit)

    if json_output:
        print(json.dumps(records, indent=2, ensure_ascii=False), file=sys.stdout)
        raise typer.Exit()

    if not records:
        console.print("[yellow]No people found.[/]")
        raise typer.Exit()

    table = Table(title=f"People ({len(records)})")
    table.add_column("ID", style="dim", max_width=36)
    table.add_column("Name", style="cyan", max_width=30)
    table.add_column("Email", style="white", max_width=35)

    for record in records:
        record_id = record.get("id", {}).get("record_id", "")
        values = record.get("values", {})
        name = _extract_value(values.get("name"))
        email = _extract_value(values.get("email_addresses"))
        table.add_row(record_id[:8] + "...", name, email)

    console.print(table)


@app.command()
def companies(
    limit: int = typer.Option(25, "--limit", "-n", help="Max results"),
    filter_name: str = typer.Option(None, "--name", help="Filter by name"),
    filter_domain: str = typer.Option(None, "--domain", help="Filter by domain"),
    json_output: bool = typer.Option(False, "--json", "-j", help="Output as JSON"),
):
    """List company records."""
    client = _get_client()

    filter_obj = None
    if filter_name:
        filter_obj = {"name": filter_name}
    elif filter_domain:
        filter_obj = {"domains": filter_domain}

    records = client.query_records("companies", filter_obj=filter_obj, limit=limit)

    if json_output:
        print(json.dumps(records, indent=2, ensure_ascii=False), file=sys.stdout)
        raise typer.Exit()

    if not records:
        console.print("[yellow]No companies found.[/]")
        raise typer.Exit()

    table = Table(title=f"Companies ({len(records)})")
    table.add_column("ID", style="dim", max_width=36)
    table.add_column("Name", style="cyan", max_width=30)
    table.add_column("Domain", style="white", max_width=30)

    for record in records:
        record_id = record.get("id", {}).get("record_id", "")
        values = record.get("values", {})
        name = _extract_value(values.get("name"))
        domain = _extract_value(values.get("domains"))
        table.add_row(record_id[:8] + "...", name, domain)

    console.print(table)


@app.command()
def records(
    object_slug: str = typer.Argument(..., help="Object slug (e.g., 'people', 'companies')"),
    limit: int = typer.Option(25, "--limit", "-n", help="Max results"),
    filter_json: str = typer.Option(None, "--filter", "-f", help="Filter as JSON"),
    json_output: bool = typer.Option(False, "--json", "-j", help="Output as JSON"),
):
    """Query records for any object."""
    client = _get_client()

    filter_obj = json.loads(filter_json) if filter_json else None
    records_list = client.query_records(object_slug, filter_obj=filter_obj, limit=limit)

    if json_output:
        print(json.dumps(records_list, indent=2, ensure_ascii=False), file=sys.stdout)
        raise typer.Exit()

    if not records_list:
        console.print("[yellow]No records found.[/]")
        raise typer.Exit()

    console.print(f"[bold]{object_slug}[/] ({len(records_list)} records)\n")
    for record in records_list:
        record_id = record.get("id", {}).get("record_id", "")
        console.print(f"[cyan]{record_id}[/]")
        values = record.get("values", {})
        for key, val in list(values.items())[:5]:
            console.print(f"  {key}: {_extract_value(val)}")
        console.print()


@app.command("pipeline-export")
def pipeline_export(
    owner_id: str | None = typer.Option(None, "--owner-id", help="Workspace member UUID for deal_owner"),
    stage_id: str | None = typer.Option(None, "--stage-id", help="Pipeline stage status UUID"),
    source_title: str | None = typer.Option(
        None,
        "--source-title",
        help="Comma-separated source_v2 option titles to match locally",
    ),
    channel_title: str | None = typer.Option(
        None,
        "--channel-title",
        help="Comma-separated channel option titles to match locally",
    ),
    platform_title: str | None = typer.Option(
        None,
        "--platform-title",
        help="Comma-separated crypto_platforms option titles to match locally",
    ),
    stage_title: str | None = typer.Option(
        None,
        "--stage-title",
        help="Comma-separated stage titles to match locally when stage-id is unavailable",
    ),
    has_phone: bool = typer.Option(
        False, "--has-phone", help="Only include rows with linked person phone numbers"
    ),
    has_email: bool = typer.Option(False, "--has-email", help="Only include rows with linked person emails"),
    include_people: bool = typer.Option(
        False,
        "--include-people",
        help="Fetch linked people and include names, phones, and emails",
    ),
    max_records: int = typer.Option(500, "--max-records", min=1, help="Max pipeline records to scan"),
    page_size: int = typer.Option(200, "--page-size", min=1, help="Attio page size"),
    max_people: int = typer.Option(500, "--max-people", min=1, help="Max linked people to fetch"),
    limit: int = typer.Option(50, "--limit", "-n", min=1, help="Max shaped rows to print"),
    json_output: bool = typer.Option(False, "--json", "-j", help="Output shaped JSON"),
):
    """Export shaped pipeline rows for common CRM list/filter questions.

    Server-side filters are used for stable IDs such as owner and stage. Select
    title filters are applied locally because Attio rejects title/value filters
    for select attributes in this workspace.
    """
    client = _get_client()
    filter_obj = _pipeline_filter(owner_id, stage_id)
    records_list = client.query_all_records(
        "pipeline",
        filter_obj=filter_obj,
        page_size=page_size,
        max_records=max_records,
    )

    source_titles = _csv_option(source_title)
    channel_titles = _csv_option(channel_title)
    platform_titles = _csv_option(platform_title)
    stage_titles = _csv_option(stage_title)

    filtered: list[dict] = []
    for record in records_list:
        values = record.get("values", {})
        if source_titles and not _matches_any(_option_titles(values, "source_v2"), source_titles):
            continue
        if channel_titles and not _matches_any(_option_titles(values, "channel"), channel_titles):
            continue
        if platform_titles and not _matches_any(
            _option_titles(values, "crypto_platforms"), platform_titles
        ):
            continue
        if stage_titles and not _matches_any([_status_title(values)], stage_titles):
            continue
        filtered.append(record)

    people_by_id: dict[str, dict] = {}
    if include_people or has_phone or has_email:
        person_ids: list[str] = []
        for record in filtered:
            person_ids.extend(_record_refs(record.get("values", {}), "associated_people"))
        unique_person_ids = list(dict.fromkeys(person_ids))[:max_people]
        people_by_id = client.get_records("people", unique_person_ids)

    shaped = [_shape_pipeline_record(record, people_by_id=people_by_id) for record in filtered]
    if has_phone:
        shaped = [row for row in shaped if row["phones"]]
    if has_email:
        shaped = [row for row in shaped if row["emails"]]

    result = {
        "scanned_count": len(records_list),
        "matched_count": len(shaped),
        "returned_count": min(len(shaped), limit),
        "truncated": len(shaped) > limit,
        "records": shaped[:limit],
    }

    if json_output:
        print(json.dumps(result, indent=2, ensure_ascii=False), file=sys.stdout)
        raise typer.Exit()

    table = Table(title=f"Pipeline Records ({result['matched_count']} matched)")
    table.add_column("Lead", style="cyan", max_width=34)
    table.add_column("Stage", style="green", max_width=22)
    table.add_column("Source", style="white", max_width=28)
    table.add_column("People", style="white", max_width=28)
    table.add_column("Phone", style="yellow", max_width=22)
    table.add_column("URL", style="blue", max_width=48)

    for row in result["records"]:
        table.add_row(
            row["deal_name"],
            row["stage"],
            ", ".join(row["source"]),
            ", ".join(row["people"]),
            ", ".join(row["phones"]),
            row["web_url"],
        )
    console.print(table)
    if result["truncated"]:
        console.print(f"[yellow]Showing {result['returned_count']} of {result['matched_count']} rows.[/]")


@app.command("lead-upsert")
def lead_upsert(
    company_name: str = typer.Argument(..., help="Company or account name"),
    contact_name: str | None = typer.Option(None, "--contact-name", help="Contact person name"),
    phone: list[str] = typer.Option([], "--phone", help="Phone number; repeat for multiple"),
    email: list[str] = typer.Option([], "--email", help="Email address; repeat for multiple"),
    owner_id: str = typer.Option(..., "--owner-id", help="Workspace member UUID for deal_owner"),
    stage_id: str = typer.Option(..., "--stage-id", help="Pipeline stage status UUID"),
    source_option: str | None = typer.Option(
        None,
        "--source-option",
        help="Comma-separated source_v2 option titles/values",
    ),
    channel_option: str | None = typer.Option(
        None,
        "--channel-option",
        help="Comma-separated channel option titles/values",
    ),
    platform_option: str | None = typer.Option(
        None,
        "--platform-option",
        help="Comma-separated crypto_platforms option titles/values",
    ),
    current_setup: str | None = typer.Option(None, "--current-setup", help="Pipeline current_setup text"),
    note: str | None = typer.Option(None, "--note", help="Note content to attach to the pipeline record"),
    task: str | None = typer.Option(None, "--task", help="Follow-up task content"),
    task_deadline: str | None = typer.Option(None, "--task-deadline", help="Task deadline ISO timestamp"),
    task_assignee_email: str | None = typer.Option(
        None,
        "--task-assignee-email",
        help="Task assignee email address",
    ),
    dry_run: bool = typer.Option(False, "--dry-run", help="Print the planned payload without writing"),
    json_output: bool = typer.Option(False, "--json", "-j", help="Output shaped JSON"),
):
    """Create or update one lead through the company/person/pipeline chain."""
    phones = [_normalize_phone(item) for item in phone if item.strip()]
    emails = [item.strip() for item in email if item.strip()]
    person_name = contact_name or (company_name if phones or emails else None)
    deal_name = f"{company_name} ({contact_name})" if contact_name else company_name

    plan = {
        "company_name": company_name,
        "person_name": person_name,
        "deal_name": deal_name,
        "owner_id": owner_id,
        "stage_id": stage_id,
        "phones": phones,
        "emails": emails,
        "source": _csv_option(source_option),
        "channel": _csv_option(channel_option),
        "crypto_platforms": _csv_option(platform_option),
        "current_setup": current_setup,
        "note": note,
        "task": task,
        "task_deadline": task_deadline,
        "task_assignee_email": task_assignee_email,
    }
    if dry_run:
        print(json.dumps({"dry_run": True, "plan": plan}, indent=2, ensure_ascii=False))
        raise typer.Exit()

    client = _get_client()
    company = _first_record(
        client.query_records("companies", filter_obj={"name": {"value": company_name}}, limit=1)
    )
    company_created = False
    if not company:
        company = client.create_record("companies", {"name": [{"value": company_name}]})
        company_created = True
    company_id = _record_id(company)

    person = None
    person_created = False
    if person_name:
        if emails:
            person = _first_record(
                client.query_records(
                    "people",
                    filter_obj={"email_addresses": {"email_address": emails[0]}},
                    limit=1,
                )
            )
        if not person and phones:
            person = _first_record(
                client.query_records(
                    "people",
                    filter_obj={"phone_numbers": {"phone_number": phones[0]}},
                    limit=1,
                )
            )
        if not person and contact_name:
            person = _first_record(
                client.query_records(
                    "people",
                    filter_obj={"name": {"full_name": person_name}},
                    limit=1,
                )
            )

        person_values: dict[str, list[dict[str, Any]]] = {
            "name": [_person_name_value(person_name)],
            "company": [_record_reference("companies", company_id)],
        }
        if phones:
            person_values["phone_numbers"] = [
                {"original_phone_number": item} for item in phones
            ]
        if emails:
            person_values["email_addresses"] = [{"email_address": item} for item in emails]

        if person:
            client.update_record("people", _record_id(person), person_values)
            person = client.get_record("people", _record_id(person))
        else:
            person = client.create_record("people", person_values)
            person_created = True

    company_ref = _record_reference("companies", company_id)
    person_refs = [_record_reference("people", _record_id(person))] if person else []

    pipeline = _first_record(
        client.query_records("pipeline", filter_obj={"deal_name": {"value": deal_name}}, limit=1)
    )
    if not pipeline and person_refs:
        pipeline = _first_record(
            client.query_records(
                "pipeline",
                filter_obj={"associated_people": person_refs[0]},
                limit=1,
            )
        )
    if not pipeline:
        pipeline = _first_record(
            client.query_records(
                "pipeline",
                filter_obj={"associated_company": company_ref},
                limit=1,
            )
        )

    pipeline_values: dict[str, list[dict[str, Any]]] = {
        "deal_name": [{"value": deal_name}],
        "deal_owner": [
            {
                "referenced_actor_type": "workspace-member",
                "referenced_actor_id": owner_id,
            }
        ],
        "deal_stage": [{"status": stage_id}],
        "associated_company": [company_ref],
    }
    if person_refs:
        existing_people = _existing_record_refs(pipeline, "associated_people") if pipeline else []
        pipeline_values["associated_people"] = _merge_record_refs(existing_people, person_refs)
    if current_setup:
        pipeline_values["current_setup"] = [{"value": current_setup}]
    source_values = _option_values(source_option)
    if source_values:
        pipeline_values["source_v2"] = source_values
    channel_values = _option_values(channel_option)
    if channel_values:
        pipeline_values["channel"] = channel_values
    platform_values = _option_values(platform_option)
    if platform_values:
        pipeline_values["crypto_platforms"] = platform_values

    pipeline_created = False
    if pipeline:
        client.update_record("pipeline", _record_id(pipeline), pipeline_values)
        pipeline = client.get_record("pipeline", _record_id(pipeline))
    else:
        pipeline = client.create_record("pipeline", pipeline_values)
        pipeline_created = True
    pipeline_id = _record_id(pipeline)

    note_id = None
    if note:
        existing_notes = client.list_notes("pipeline", pipeline_id)
        duplicate_note = any((item.get("content") or "") == note for item in existing_notes)
        if not duplicate_note:
            created_note = client.create_note("pipeline", pipeline_id, "Lead context", note)
            note_id = created_note.get("id", {}).get("note_id")

    task_id = None
    if task:
        task_payload: dict[str, Any] = {
            "data": {
                "content": task,
                "format": "plaintext",
                "is_completed": False,
                "linked_records": [_record_reference("pipeline", pipeline_id)],
            }
        }
        if task_deadline:
            task_payload["data"]["deadline_at"] = task_deadline
        if task_assignee_email:
            task_payload["data"]["assignees"] = [
                {"workspace_member_email_address": task_assignee_email}
            ]
        created_task = client.raw_request("POST", "/tasks", json=task_payload)
        task_id = created_task.get("data", {}).get("id", {}).get("task_id")

    result = {
        "company": {
            "record_id": company_id,
            "web_url": _record_url(company),
            "created": company_created,
        },
        "person": {
            "record_id": _record_id(person) if person else None,
            "web_url": _record_url(person) if person else None,
            "created": person_created,
        },
        "pipeline": {
            "record_id": pipeline_id,
            "web_url": _record_url(pipeline),
            "created": pipeline_created,
        },
        "note_id": note_id,
        "task_id": task_id,
    }

    if json_output:
        print(json.dumps(result, indent=2, ensure_ascii=False))
        raise typer.Exit()

    table = Table(title="Lead Upsert")
    table.add_column("Object", style="cyan", max_width=12)
    table.add_column("Record ID", style="white", max_width=36)
    table.add_column("Created", style="green", max_width=8)
    table.add_column("URL", style="blue", max_width=56)
    table.add_row("company", company_id, str(company_created), _record_url(company))
    if person:
        table.add_row("person", _record_id(person), str(person_created), _record_url(person))
    table.add_row("pipeline", pipeline_id, str(pipeline_created), _record_url(pipeline))
    console.print(table)
    if note_id:
        console.print(f"[cyan]Note:[/] {note_id}")
    if task_id:
        console.print(f"[cyan]Task:[/] {task_id}")


@app.command()
def get(
    object_slug: str = typer.Argument(..., help="Object slug"),
    record_id: str = typer.Argument(..., help="Record ID"),
    json_output: bool = typer.Option(False, "--json", "-j", help="Output as JSON"),
):
    """Get a single record by ID."""
    client = _get_client()
    record = client.get_record(object_slug, record_id)

    if json_output:
        print(json.dumps(record, indent=2, ensure_ascii=False), file=sys.stdout)
        raise typer.Exit()

    console.print(f"[bold]{object_slug}[/] Record")
    console.print(f"[cyan]ID:[/] {record.get('id', {}).get('record_id', '')}\n")

    values = record.get("values", {})
    for key, val in values.items():
        console.print(f"[bold]{key}:[/] {_extract_value(val)}")


@app.command()
def create(
    object_slug: str = typer.Argument(..., help="Object slug (e.g., 'people', 'companies')"),
    values_json: str = typer.Argument(..., help="Record values as JSON"),
):
    """Create a new record.

    Examples:
        attio create people '{"name": [{"first_name": "John", "last_name": "Doe"}]}'
        attio create companies '{"name": [{"value": "Acme Inc"}], "domains": [{"domain": "acme.com"}]}'
    """
    client = _get_client()
    values = json.loads(values_json)
    record = client.create_record(object_slug, values)

    record_id = record.get("id", {}).get("record_id", "")
    console.print(f"[green]✓ Created {object_slug} record[/]")
    console.print(f"[cyan]ID:[/] {record_id}")


@app.command()
def update(
    object_slug: str = typer.Argument(..., help="Object slug"),
    record_id: str = typer.Argument(..., help="Record ID"),
    values_json: str = typer.Argument(..., help="Values to update as JSON"),
):
    """Update an existing record.

    Examples:
        attio update people abc123 '{"email_addresses": [{"email_address": "new@email.com"}]}'
    """
    client = _get_client()
    values = json.loads(values_json)
    client.update_record(object_slug, record_id, values)

    console.print(f"[green]✓ Updated {object_slug} record {record_id[:8]}...[/]")


@app.command()
def delete(
    object_slug: str = typer.Argument(..., help="Object slug"),
    record_id: str = typer.Argument(..., help="Record ID"),
    confirm: bool = typer.Option(False, "--yes", "-y", help="Skip confirmation"),
):
    """Delete a record."""
    if not confirm:
        typer.confirm(f"Delete {object_slug} record {record_id}?", abort=True)

    client = _get_client()
    client.delete_record(object_slug, record_id)
    console.print(f"[green]✓ Deleted {object_slug} record {record_id[:8]}...[/]")


@app.command()
def upsert(
    object_slug: str = typer.Argument(..., help="Object slug"),
    matching_attr: str = typer.Argument(
        ..., help="Attribute to match on (e.g., 'email_addresses')"
    ),
    values_json: str = typer.Argument(..., help="Record values as JSON"),
):
    """Create or update a record based on matching attribute.

    Examples:
        attio upsert people email_addresses '{"email_addresses": [{"email_address": "john@example.com"}], "name": [{"first_name": "John"}]}'
    """
    client = _get_client()
    values = json.loads(values_json)
    record = client.assert_record(object_slug, matching_attr, values)

    record_id = record.get("id", {}).get("record_id", "")
    console.print(f"[green]✓ Upserted {object_slug} record[/]")
    console.print(f"[cyan]ID:[/] {record_id}")


@app.command()
def lists():
    """List all lists in the workspace."""
    client = _get_client()
    lists_data = client.list_lists()

    if not lists_data:
        console.print("[yellow]No lists found.[/]")
        raise typer.Exit()

    table = Table(title=f"Lists ({len(lists_data)})")
    table.add_column("ID", style="dim", max_width=36)
    table.add_column("Name", style="cyan", max_width=25)
    table.add_column("Object", style="white", max_width=20)

    for lst in lists_data:
        list_id = lst.get("id", {}).get("list_id", "")
        name = lst.get("name", "")
        parent_object = lst.get("parent_object", [])
        if isinstance(parent_object, list):
            parent_object = ", ".join(parent_object)
        table.add_row(list_id[:8] + "...", name, parent_object)

    console.print(table)


@app.command()
def entries(
    list_id: str = typer.Argument(..., help="List ID or slug"),
    limit: int = typer.Option(25, "--limit", "-n", help="Max results"),
    json_output: bool = typer.Option(False, "--json", "-j", help="Output as JSON"),
):
    """Query entries in a list."""
    client = _get_client()
    entries_list = client.query_entries(list_id, limit=limit)

    if json_output:
        print(json.dumps(entries_list, indent=2, ensure_ascii=False), file=sys.stdout)
        raise typer.Exit()

    if not entries_list:
        console.print("[yellow]No entries found.[/]")
        raise typer.Exit()

    console.print(f"[bold]List Entries[/] ({len(entries_list)})\n")
    for entry in entries_list:
        entry_id = entry.get("id", {}).get("entry_id", "")
        record_id = entry.get("id", {}).get("record_id", "")
        console.print(f"[cyan]Entry:[/] {entry_id[:8]}... [dim](record: {record_id[:8]}...)[/]")
        values = entry.get("entry_values", {})
        for key, val in list(values.items())[:3]:
            console.print(f"  {key}: {_extract_value(val)}")
        console.print()


@app.command()
def notes(
    object_slug: str = typer.Argument(..., help="Parent object slug"),
    record_id: str = typer.Argument(..., help="Parent record ID"),
    json_output: bool = typer.Option(False, "--json", "-j", help="Output as JSON"),
):
    """List notes for a record."""
    client = _get_client()
    notes_list = client.list_notes(object_slug, record_id)

    if json_output:
        print(json.dumps(notes_list, indent=2, ensure_ascii=False), file=sys.stdout)
        raise typer.Exit()

    if not notes_list:
        console.print("[yellow]No notes found.[/]")
        raise typer.Exit()

    console.print(f"[bold]Notes[/] ({len(notes_list)})\n")
    for note in notes_list:
        note_id = note.get("id", {}).get("note_id", "")
        title = note.get("title", "Untitled")
        console.print(f"[cyan]{title}[/] [dim]({note_id[:8]}...)[/]")


@app.command("add-note")
def add_note(
    object_slug: str = typer.Argument(..., help="Parent object slug"),
    record_id: str = typer.Argument(..., help="Parent record ID"),
    title: str = typer.Argument(..., help="Note title"),
    content: str = typer.Argument(..., help="Note content"),
):
    """Add a note to a record."""
    client = _get_client()
    note = client.create_note(object_slug, record_id, title, content)
    note_id = note.get("id", {}).get("note_id", "")
    console.print("[green]✓ Created note[/]")
    console.print(f"[cyan]ID:[/] {note_id}")


@app.command()
def tasks(
    object_slug: str = typer.Option(None, "--object", "-o", help="Filter by linked object"),
    record_id: str = typer.Option(None, "--record", "-r", help="Filter by linked record ID"),
    completed: bool = typer.Option(None, "--completed", "-c", help="Filter by completion"),
    limit: int = typer.Option(25, "--limit", "-n", help="Max results"),
    json_output: bool = typer.Option(False, "--json", "-j", help="Output as JSON"),
):
    """List tasks."""
    client = _get_client()
    tasks_list = client.list_tasks(
        linked_object=object_slug,
        linked_record_id=record_id,
        is_completed=completed,
        limit=limit,
    )

    if json_output:
        print(json.dumps(tasks_list, indent=2, ensure_ascii=False), file=sys.stdout)
        raise typer.Exit()

    if not tasks_list:
        console.print("[yellow]No tasks found.[/]")
        raise typer.Exit()

    table = Table(title=f"Tasks ({len(tasks_list)})")
    table.add_column("ID", style="dim", max_width=10)
    table.add_column("Content", style="white", max_width=50)
    table.add_column("Status", style="green", max_width=10)
    table.add_column("Deadline", style="yellow", max_width=12)

    for task in tasks_list:
        task_id = task.get("id", {}).get("task_id", "")
        content = task.get("content_plaintext", "")[:50]
        is_completed = "✓" if task.get("is_completed") else ""
        deadline = task.get("deadline_at", "")[:10] if task.get("deadline_at") else ""
        table.add_row(task_id[:8] + "..", content, is_completed, deadline)

    console.print(table)


@app.command("add-task")
def add_task(
    content: str = typer.Argument(..., help="Task content"),
    deadline: str = typer.Option(None, "--deadline", "-d", help="Deadline (ISO format)"),
    assignee: str = typer.Option(None, "--assignee", "-a", help="Workspace member ID"),
):
    """Create a new task."""
    client = _get_client()
    assignees = [assignee] if assignee else None
    task = client.create_task(content, deadline=deadline, assignees=assignees)

    task_id = task.get("id", {}).get("task_id", "")
    console.print("[green]✓ Created task[/]")
    console.print(f"[cyan]ID:[/] {task_id}")


@app.command()
def members():
    """List workspace members."""
    client = _get_client()
    members_list = client.list_workspace_members()

    if not members_list:
        console.print("[yellow]No members found.[/]")
        raise typer.Exit()

    table = Table(title=f"Workspace Members ({len(members_list)})")
    table.add_column("ID", style="dim", max_width=10)
    table.add_column("Name", style="cyan", max_width=25)
    table.add_column("Email", style="white", max_width=35)
    table.add_column("Role", style="green", max_width=15)

    for member in members_list:
        member_id = member.get("id", {}).get("workspace_member_id", "")
        name = f"{member.get('first_name', '')} {member.get('last_name', '')}".strip()
        email = member.get("email_address", "")
        role = member.get("access_level", "")
        table.add_row(member_id[:8] + "..", name, email, role)

    console.print(table)


if __name__ == "__main__":
    app()
