#!/usr/bin/env python3
from __future__ import annotations

import os
import sys
import tomllib
from collections.abc import Mapping
from pathlib import Path


VALID_REASONING_SUMMARIES = {"auto", "concise", "detailed", "none"}
VALID_REASONING_EFFORTS = {"none", "minimal", "low", "medium", "high", "xhigh"}


def _key_name(line: str) -> str | None:
    stripped = line.strip()
    if not stripped or stripped.startswith("#") or "=" not in stripped:
        return None
    return stripped.split("=", 1)[0].strip()


def _first_table(lines: list[str]) -> int:
    return next((i for i, line in enumerate(lines) if line.lstrip().startswith("[")), len(lines))


def _find_table(lines: list[str], header: str) -> int | None:
    return next((i for i, line in enumerate(lines) if line.strip() == header), None)


def _table_end(lines: list[str], start: int) -> int:
    return next(
        (i for i in range(start + 1, len(lines)) if lines[i].lstrip().startswith("[")),
        len(lines),
    )


def _first_child_table(lines: list[str], parent: str) -> int | None:
    prefix = f"[{parent}."
    return next((i for i, line in enumerate(lines) if line.strip().startswith(prefix)), None)


def _upsert_top_level(lines: list[str], key: str, value: str) -> list[str]:
    first_table = _first_table(lines)
    replacement = f"{key} = {value}"
    for i in range(first_table):
        if _key_name(lines[i]) == key:
            lines[i] = replacement
            break
    else:
        lines.insert(first_table, replacement)
    return lines


def _upsert_table_key(lines: list[str], header: str, key: str, value: str) -> list[str]:
    start = _find_table(lines, header)
    if start is None:
        if lines and lines[-1].strip():
            lines.append("")
        lines.extend([header, f"{key} = {value}"])
        return lines

    end = _table_end(lines, start)
    replacement = f"{key} = {value}"
    for i in range(start + 1, end):
        if _key_name(lines[i]) == key:
            lines[i] = replacement
            break
    else:
        lines.insert(end, replacement)
    return lines


def _ensure_features_table(lines: list[str], flags: set[str]) -> list[str]:
    features_start = _find_table(lines, "[features]")
    if features_start is None:
        insert_at = _first_child_table(lines, "features")
        section = ["[features]"] + [f"{name} = false" for name in sorted(flags)]
        if insert_at is None:
            if lines and lines[-1].strip():
                lines.append("")
            lines.extend(section)
        else:
            lines[insert_at:insert_at] = section + [""]
        return lines

    features_end = _table_end(lines, features_start)
    seen = set()
    rewritten = []
    for line in lines[features_start + 1 : features_end]:
        name = _key_name(line)
        if name in flags:
            rewritten.append(f"{name} = false")
            seen.add(name)
        elif name == "multi_agent_v2" and "multi_agent_v2" not in flags:
            # Current Codex config uses [features.multi_agent_v2]. Keeping the
            # legacy scalar beside that table makes TOML parsing fail.
            continue
        else:
            rewritten.append(line)

    for name in sorted(flags - seen):
        rewritten.append(f"{name} = false")

    return lines[: features_start + 1] + rewritten + lines[features_end:]


def _disable_multi_agent_features(lines: list[str]) -> list[str]:
    has_v2_table = _find_table(lines, "[features.multi_agent_v2]") is not None
    flags = {"multi_agent"}
    if not has_v2_table:
        flags.add("multi_agent_v2")

    lines = _ensure_features_table(lines, flags)
    if has_v2_table:
        lines = _upsert_table_key(lines, "[features.multi_agent_v2]", "enabled", "false")
    return lines


def _deep_merge(base: dict, overlay: dict) -> dict:
    for key, value in overlay.items():
        if isinstance(value, dict) and isinstance(base.get(key), dict):
            _deep_merge(base[key], value)
        else:
            base[key] = value
    return base


def configure(path: Path, env: Mapping[str, str]) -> None:
    lines = path.read_text().splitlines()

    # CODEX_MODEL_REASONING_SUMMARY overrides model_reasoning_summary so
    # deployments can re-enable reasoning summaries without rebuilding.
    summary = env.get("CODEX_MODEL_REASONING_SUMMARY", "").strip()
    if summary:
        if summary not in VALID_REASONING_SUMMARIES:
            print(
                f"ignoring invalid CODEX_MODEL_REASONING_SUMMARY: {summary!r} "
                "(expected auto, concise, detailed, or none)",
                file=sys.stderr,
            )
        else:
            lines = _upsert_top_level(lines, "model_reasoning_summary", f'"{summary}"')

    lines = _disable_multi_agent_features(lines)

    # Optional deploy-time override of the codex reasoning effort.
    effort = env.get("CODEX_MODEL_REASONING_EFFORT", "").strip().lower()
    if effort:
        if effort not in VALID_REASONING_EFFORTS:
            print(
                f"ignoring invalid CODEX_MODEL_REASONING_EFFORT={effort!r}; "
                f"expected one of {sorted(VALID_REASONING_EFFORTS)}",
                file=sys.stderr,
            )
        else:
            lines = _upsert_top_level(lines, "model_reasoning_effort", f'"{effort}"')

    text = "\n".join(lines).rstrip() + "\n"

    # Validate the generated config before an optional overlay. This catches
    # line-based rewrite mistakes before Codex starts with a broken config.
    try:
        tomllib.loads(text)
    except tomllib.TOMLDecodeError as exc:
        print(f"invalid generated Codex config: {exc}", file=sys.stderr)
        sys.exit(1)

    overlay_raw = env.get("CODEX_CONFIG_OVERLAY", "").strip()
    if overlay_raw:
        import tomli_w

        try:
            merged = _deep_merge(tomllib.loads(text), tomllib.loads(overlay_raw))
        except tomllib.TOMLDecodeError as exc:
            print(f"ignoring invalid CODEX_CONFIG_OVERLAY: {exc}", file=sys.stderr)
        else:
            text = tomli_w.dumps(merged)

    try:
        tomllib.loads(text)
    except tomllib.TOMLDecodeError as exc:
        print(f"invalid generated Codex config: {exc}", file=sys.stderr)
        sys.exit(1)

    path.write_text(text)


def main() -> int:
    config_path = os.environ.get("CODEX_CONFIG_PATH", "").strip()
    if not config_path:
        print("CODEX_CONFIG_PATH is required", file=sys.stderr)
        return 2

    configure(Path(config_path), os.environ)
    return 0


if __name__ == "__main__":
    sys.exit(main())
