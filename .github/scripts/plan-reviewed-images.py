"""Plan publication from the previous reviewed OCI index and its native BuildKit attestations."""
import argparse
import functools
import hashlib
import json
import os
from pathlib import PurePosixPath
import re
import subprocess
import sys
from urllib.parse import parse_qs, urlsplit

PLATFORMS = ("linux/amd64", "linux/arm64")
SERVICES = ("api-rs", "company-context", "proxy-sync", "token-broker", "slackbotv2",
            "linearbot", "discordbot", "githubbot", "teamsbot", "agent", "iron-proxy", "console")
REGISTRY_NAMESPACE = "ghcr.io/tiplink/centaur"
SHA = r"[0-9a-f]{40}"
DIGEST = r"sha256:[0-9a-f]{64}"


def run(*args):
    return subprocess.check_output(args, stderr=subprocess.PIPE, timeout=90)


def require(condition, reason):
    if not condition:
        raise ValueError(reason)


@functools.lru_cache(maxsize=128)
def manifest(reference):
    raw = run("docker", "buildx", "imagetools", "inspect", "--raw", reference)
    return "sha256:" + hashlib.sha256(raw).hexdigest(), json.loads(raw)


def inspect(reference, field):
    return json.loads(run("docker", "buildx", "imagetools", "inspect", reference,
                          "--format", "{{json ." + field + "}}"))


@functools.cache
def series_bytes(revision):
    return run("git", "show", revision + ":.github/core-patches/series.json")


@functools.cache
def series(revision):
    return json.loads(series_bytes(revision))


@functools.cache
def changed_paths(origin, current):
    require(series(origin)["upstream_revision"] == series(current)["upstream_revision"], "official base changed")
    changes = run("git", "diff", "--no-renames", "--name-only", "-z", origin, current).split(b"\0")
    paths = {os.fsdecode(p) for p in changes if p}
    # The source validator proves that the checkout outside publisher orchestration
    # matches the prepared runtime. Patch packaging itself is absent from that context.
    paths = {p for p in paths if not p.startswith(".github/core-patches/")}
    require(not any(p.startswith(".github/") for p in paths), "publisher inputs changed")
    # BuildKit follows source symlinks. A change to their targets can live outside
    # followpaths. Treat these and submodules conservatively on either side.
    for rev in (origin, current):
        entries = run("git", "ls-tree", "-rz", rev).split(b"\0")
        require(not any(e.startswith((b"120000 ", b"160000 ")) for e in entries), "symlink or submodule in source")
    return paths


def material_reference(material):
    p = urlsplit(material["uri"])
    require(p.scheme == "pkg" and p.path.startswith("docker/") and not p.fragment, "unknown build material")
    image, tag = p.path.removeprefix("docker/").rsplit("@", 1)
    require(re.fullmatch(r"[a-z0-9._/-]+", image) and re.fullmatch(r"[A-Za-z0-9._-]+", tag), "unknown image name")
    q = parse_qs(p.query, strict_parsing=True)
    require(set(q) <= {"platform", "digest"} and all(len(v) == 1 for v in q.values()), "unknown image qualifier")
    digest = "sha256:" + material["digest"]["sha256"]
    require(re.fullmatch(DIGEST, digest), "unknown material digest")
    if "digest" in q:
        require(q["digest"] == [digest], "inconsistent pinned base")
        return image + "@" + digest, digest
    return image + ":" + tag, digest


def context_paths(definition):
    paths = set()
    for step in definition["internalParameters"]["buildConfig"]["llbDefinition"]:
        op = step["op"]["Op"] or {}
        if "source" not in op:
            continue
        src = op["source"]
        if src["identifier"].startswith("docker-image://"):
            continue
        require(src["identifier"] == "local://context", "external or unknown build source")
        followed = json.loads(src.get("attrs", {}).get("local.followpaths", "null"))
        require(isinstance(followed, list) and followed, "unbounded local build context")
        for path in followed:
            require(isinstance(path, str) and re.fullmatch(r"[A-Za-z0-9_./-]+", path)
                    and not path.startswith("/") and ".." not in PurePosixPath(path).parts and path != ".",
                    "unbounded or nonliteral source path")
            paths.add(path)
    require(paths, "missing local source evidence")
    return paths


def reusable(service, previous, current):
    repo = REGISTRY_NAMESPACE + "/centaur-" + service
    digest, index = manifest(repo + ":reviewed-" + previous)
    ref = repo + "@" + digest
    configs, attestations = inspect(ref, "Image"), inspect(ref, "Provenance")
    require(set(configs) == set(attestations) == set(PLATFORMS), "incomplete platforms or attestations")
    recipe = "services/" + ("sandbox" if service == "agent" else service) + "/Dockerfile"
    for platform in PLATFORMS:
        labels = configs[platform]["config"]["Labels"]
        origin = labels["io.centaur.publication-revision"]
        require(re.fullmatch(SHA, origin), "invalid original publication")
        require(labels["io.centaur.source-tree"] == series(origin)["result_tree"]
                and labels["io.centaur.patch-series.sha256"] == hashlib.sha256(series_bytes(origin)).hexdigest()
                and labels["org.opencontainers.image.revision"] == series(origin)["upstream_revision"]
                and labels["org.opencontainers.image.source"] == "https://github.com/paradigmxyz/centaur",
                "unexpected original source")
        # Verify this is exactly the original published index, including its attestations.
        require(manifest(repo + ":reviewed-" + origin)[0] == digest, "original index differs")
        paths = changed_paths(origin, current)
        definition = attestations[platform]["SLSA"]["buildDefinition"]
        inputs = context_paths(definition) | {recipe, recipe + ".dockerignore", ".dockerignore"}
        for path in sorted(paths):
            require(not any(path == p or path.startswith(p + "/") or p.startswith(path + "/") for p in inputs),
                    f"build input changed: {path}")
        materials = definition["resolvedDependencies"]
        require(materials, "no resolved build materials")
        for material in materials:
            name, expected = material_reference(material)
            require(manifest(name)[0] == expected, f"registry input changed: {name}")
    children = [m["digest"] for m in index["manifests"]
                if m.get("platform", {}).get("os") == "linux" and m["platform"]["architecture"] == "arm64"]
    require(len(children) == 1 and re.fullmatch(DIGEST, children[0]), "missing arm64 child")
    return {"digest": digest, "arm64": children[0]}


def plan(previous, current, services=SERVICES):
    require(re.fullmatch(SHA, current), "invalid publication SHA")
    build, reuse = [], {}
    for service in services:
        image = "centaur-" + service
        try:
            require(previous and re.fullmatch(SHA, previous), "full build")
            reuse[image] = reusable(service, previous, current)
            print(f"{image}: reuse {reuse[image]['digest']}", file=sys.stderr)
        except (ValueError, KeyError, IndexError, TypeError, subprocess.SubprocessError, OSError) as exc:
            print(f"{image}: rebuild ({exc})", file=sys.stderr)
            for platform in PLATFORMS:
                name = "sandbox" if service == "agent" else service
                build.append(dict(service=service, image=image, platform=platform, context=".",
                                  dockerfile=f"services/{name}/Dockerfile", target="sandbox" if service == "agent" else ""))
    return {"build": {"include": build}, "reuse": reuse}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous", default="")
    parser.add_argument("--current", required=True)
    args = parser.parse_args()
    print(json.dumps(plan(args.previous, args.current), separators=(",", ":")))
