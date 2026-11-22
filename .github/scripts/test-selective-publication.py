"""Exercise the planner and immutable aliases using a disposable local registry and real BuildKit."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("planner", Path(__file__).with_name("plan-reviewed-images.py"))
planner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(planner)
planner.REGISTRY_NAMESPACE = "localhost:5000/e2e"
services = ("codemode", "agent")
upstream = "a" * 40
manifest = {"result_tree": "b" * 40, "upstream_revision": upstream}
series_path = Path(".github/core-patches/series.json")


def command(*args):
    return subprocess.check_output(args, text=True).strip()


def commit():
    command("git", "add", ".")
    command("git", "commit", "-qm", "fixture")
    return command("git", "rev-parse", "HEAD")


def image(service):
    return f"{planner.REGISTRY_NAMESPACE}/centaur-{service}"


def directory(service):
    return Path("services") / ("sandbox" if service == "agent" else service)


def build(service, revision):
    command("docker", "buildx", "build", "--platform", ",".join(planner.PLATFORMS),
            "--provenance=mode=max", "--push", "-f", str(directory(service) / "Dockerfile"),
            "--label", f"io.centaur.publication-revision={revision}",
            "--label", f"io.centaur.source-tree={manifest['result_tree']}",
            "--label", f"io.centaur.patch-series.sha256={hashlib.sha256(series_path.read_bytes()).hexdigest()}",
            "--label", f"org.opencontainers.image.revision={upstream}",
            "--label", "org.opencontainers.image.source=https://github.com/paradigmxyz/centaur",
            "-t", f"{image(service)}:reviewed-{revision}", ".")


def expect_build(previous, current, expected):
    plan = planner.plan(previous, current, services)
    assert {(i["service"], i["platform"]) for i in plan["build"]["include"]} == {
        (s, p) for s in expected for p in planner.PLATFORMS}, plan
    return plan["reuse"]


def test(work):
    os.chdir(work)
    command("git", "init", "-q")
    command("git", "config", "user.name", "Fixture")
    command("git", "config", "user.email", "fixture@example.invalid")
    series_path.parent.mkdir(parents=True)
    series_path.write_text(json.dumps(manifest))
    Path("shared").write_text("shared v1")
    for service in services:
        folder = directory(service)
        folder.mkdir(parents=True)
        (folder / "payload").write_text(service)
        (folder / "Dockerfile").write_text(
            "# syntax=docker/dockerfile:1.7\nFROM busybox:1.37\nCOPY shared /shared\n"
            f"COPY {folder}/payload /payload\nCOPY connectors/ /connectors/\n")
    Path("connectors").mkdir()
    Path("connectors/example.ts").write_text("export const value = 1")
    first = commit()
    expect_build("", first, services)
    for service in services:
        build(service, first)
    (directory("codemode") / "payload").write_text("updated codemode")
    second = commit()
    reuse = expect_build(first, second, ["codemode"])
    build("codemode", second)
    digest = reuse["centaur-agent"]["digest"]
    command("docker", "buildx", "imagetools", "create", "-t", f"{image('agent')}:reviewed-{second}",
            f"{image('agent')}@{digest}")
    assert planner.manifest(f"{image('agent')}:reviewed-{second}")[0] == digest
    for service, expected in (("agent", "agent"), ("codemode", "updated codemode")):
        assert command("docker", "run", "--rm", f"{image(service)}:reviewed-{second}", "cat", "/payload") == expected
    Path("notes").write_text("outside both images")
    expect_build(second, commit(), [])
    with patch.object(planner, "inspect", return_value={}):
        expect_build(second, command("git", "rev-parse", "HEAD"), services)
    lookup = planner.manifest

    def moved_material(reference):
        if reference == "busybox:1.37":
            return "sha256:" + "0" * 64, {}
        return lookup(reference)

    with patch.object(planner, "manifest", moved_material):
        expect_build(second, command("git", "rev-parse", "HEAD"), services)
    recipe = directory("agent") / "Dockerfile"
    recipe.write_text(recipe.read_text() + "\nENV EXAMPLE=1\n")
    expect_build(second, commit(), ["agent"])
    Path(".dockerignore").write_text("notes\n")
    expect_build(second, commit(), services)
    Path(".dockerignore").unlink()
    Path("shared").unlink()
    expect_build(second, commit(), services)
    command("git", "restore", "--source", second, "shared", str(recipe))
    Path("connectors/example.ts").write_text("export const value = 2")
    expect_build(second, commit(), services)
    print("PASS: native runtime reads, identical reused index, isolated change, all-reuse and shared deletion")


if __name__ == "__main__":
    with tempfile.TemporaryDirectory() as work:
        test(work)
