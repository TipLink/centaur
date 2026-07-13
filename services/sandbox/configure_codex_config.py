#!/usr/bin/env python3
from __future__ import annotations

import os
import stat
import sys
import tempfile
import tomllib
from collections.abc import Mapping
from pathlib import Path


VALID_REASONING_SUMMARIES = {"auto", "concise", "detailed", "none"}
VALID_REASONING_EFFORTS = {"none", "minimal", "low", "medium", "high", "xhigh", "max"}


def _deep_merge(base: dict, overlay: dict) -> dict:
    for key, value in overlay.items():
        if isinstance(value, dict) and isinstance(base.get(key), dict):
            _deep_merge(base[key], value)
        else:
            base[key] = value
    return base


def _parse_config(text: str) -> dict:
    try:
        return tomllib.loads(text)
    except tomllib.TOMLDecodeError as exc:
        print(f"invalid generated Codex config: {exc}", file=sys.stderr)
        raise SystemExit(1) from exc


def _write_atomic(path: Path, text: str) -> None:
    mode = stat.S_IMODE(path.stat().st_mode)
    fd, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temporary, mode)
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def configure(path: Path, env: Mapping[str, str]) -> None:
    config = _parse_config(path.read_text(encoding="utf-8"))

    summary = env.get("CODEX_MODEL_REASONING_SUMMARY", "").strip()
    if summary:
        if summary not in VALID_REASONING_SUMMARIES:
            print(
                f"ignoring invalid CODEX_MODEL_REASONING_SUMMARY: {summary!r} "
                "(expected auto, concise, detailed, or none)",
                file=sys.stderr,
            )
        else:
            config["model_reasoning_summary"] = summary

    features = config.setdefault("features", {})
    if not isinstance(features, dict):
        print(
            "invalid generated Codex config: features must be a table", file=sys.stderr
        )
        raise SystemExit(1)
    features["multi_agent"] = False
    multi_agent_v2 = features.get("multi_agent_v2")
    if isinstance(multi_agent_v2, dict):
        multi_agent_v2["enabled"] = False
    else:
        features["multi_agent_v2"] = False

    effort = env.get("CODEX_MODEL_REASONING_EFFORT", "").strip().lower()
    if effort:
        if effort not in VALID_REASONING_EFFORTS:
            print(
                f"ignoring invalid CODEX_MODEL_REASONING_EFFORT={effort!r}; "
                f"expected one of {sorted(VALID_REASONING_EFFORTS)}",
                file=sys.stderr,
            )
        else:
            config["model_reasoning_effort"] = effort

    # Keep the client and iron-proxy on the same Bedrock signing region. A
    # valid operator overlay is applied afterward and may deliberately win.
    bedrock_region = env.get("CODEX_BEDROCK_REGION", "").strip()
    if bedrock_region:
        _deep_merge(
            config,
            {
                "model_providers": {
                    "amazon-bedrock": {"aws": {"region": bedrock_region}}
                }
            },
        )

    overlay_raw = env.get("CODEX_CONFIG_OVERLAY", "").strip()
    if overlay_raw:
        try:
            overlay = tomllib.loads(overlay_raw)
        except tomllib.TOMLDecodeError as exc:
            print(f"ignoring invalid CODEX_CONFIG_OVERLAY: {exc}", file=sys.stderr)
        else:
            _deep_merge(config, overlay)

    import tomli_w

    text = tomli_w.dumps(config)
    _parse_config(text)
    _write_atomic(path, text)


def main() -> int:
    config_path = os.environ.get("CODEX_CONFIG_PATH", "").strip()
    if not config_path:
        print("CODEX_CONFIG_PATH is required", file=sys.stderr)
        return 2

    configure(Path(config_path), os.environ)
    return 0


if __name__ == "__main__":
    sys.exit(main())
