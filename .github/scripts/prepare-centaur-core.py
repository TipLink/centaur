#!/usr/bin/env python3
"""Materialize a pinned upstream tree plus reviewed patches; fail on any conflict."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "patches/centaur/series.json"


def git(checkout: Path, *arguments: str) -> str:
    return subprocess.check_output(
        ["git", "-C", str(checkout), *arguments], text=True, stderr=subprocess.PIPE
    ).strip()


def prepare(manifest_path: Path, upstream: Path, output: Path) -> str:
    manifest = json.loads(manifest_path.read_text())
    if manifest.get("schema_version") != 1 or manifest.get("upstream_repository") != "paradigmxyz/centaur":
        raise ValueError("patch series must start from official Centaur")
    revision = manifest.get("upstream_revision", "")
    expected_tree = manifest.get("result_tree", "")
    if not all(re.fullmatch(r"[0-9a-f]{40}", value) for value in (revision, expected_tree)):
        raise ValueError("upstream revision and result tree must be immutable Git object IDs")
    if git(upstream, "rev-parse", "HEAD") != revision:
        raise ValueError("upstream checkout differs from the pinned base")
    if output.exists() or output.is_symlink():
        raise ValueError("output must be a new directory")
    patches = []
    for entry in manifest.get("patches", []):
        name = entry["file"]
        if not re.fullmatch(r"[0-9]{4}-[a-z0-9-]+\.patch", name):
            raise ValueError("invalid patch filename")
        path = manifest_path.parent / name
        if path.is_symlink() or hashlib.sha256(path.read_bytes()).hexdigest() != entry["sha256"]:
            raise ValueError(f"patch content differs from its reviewed hash: {name}")
        if path in patches:
            raise ValueError("duplicate patch in series")
        patches.append(path.resolve())
    if not patches:
        raise ValueError("empty patch series")
    output.parent.mkdir(parents=True, exist_ok=True)
    # Failure leaves neither a partial release tree nor a modified input checkout.
    with tempfile.TemporaryDirectory(prefix=".centaur-patch-", dir=output.parent) as temporary:
        stage = Path(temporary) / "source"
        subprocess.run(["git", "clone", "--quiet", "--no-hardlinks", "--no-checkout",
                        str(upstream.resolve()), str(stage)], check=True, capture_output=True)
        git(stage, "checkout", "--detach", revision)
        for patch in patches:
            # Deliberately no --3way, --reject, whitespace repair, or conflict resolution.
            git(stage, "apply", "--check", "--index", "--whitespace=error-all", str(patch))
            git(stage, "apply", "--index", "--whitespace=error-all", str(patch))
        git(stage, "diff", "--cached", "--check")
        tree = git(stage, "write-tree")
        if tree != expected_tree:
            raise ValueError("patched tree differs from the reviewed result")
        stage.rename(output)
    return tree


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=MANIFEST)
    parser.add_argument("--upstream", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        tree = prepare(args.manifest, args.upstream, args.output)
    except subprocess.CalledProcessError as error:
        parser.exit(1, f"Patch preparation failed; no release created.\n{error.stderr or ''}\n")
    except (ValueError, KeyError, OSError) as error:
        parser.exit(1, f"Patch preparation failed; no release created: {error}\n")
    print(f"Reviewed upstream patch series applied cleanly: tree {tree}")


if __name__ == "__main__":
    main()
