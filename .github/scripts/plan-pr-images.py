"""Select PR image checks from native BuildKit inputs of a passing run on the target branch."""
import argparse
import importlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

images = importlib.import_module("plan-reviewed-images")
SNAPSHOT = "pr-image-validation-v1"
UNKNOWN = (OSError, ValueError, KeyError, IndexError, TypeError, AttributeError, subprocess.SubprocessError)


def base_snapshot(directory):
    repo = os.environ["GITHUB_REPOSITORY"]
    # Push runs execute the target branch's own workflow. A PR cannot write a
    # baseline for other PRs, even if it changes the workflow or its artifacts.
    runs = json.loads(images.run("gh", "run", "list", "-R", repo, "--workflow", "validate-images.yml",
                                 "--branch", os.environ["BASE_BRANCH"], "--event", "push", "--status", "success",
                                 "--limit", "1", "--json", "databaseId,headSha,url"))
    images.require(runs, "no passing validation on the base branch")
    baseline = runs[0]["headSha"]
    images.run("git", "merge-base", "--is-ancestor", baseline, os.environ["BASE_SHA"])
    images.run("gh", "run", "download", str(runs[0]["databaseId"]), "-R", repo,
               "-n", SNAPSHOT, "-D", str(directory))
    images.require((directory / "revision").read_text().strip() == baseline, "wrong validation revision")
    print(f"Comparing with {runs[0]['url']} at {baseline}", file=sys.stderr)
    return baseline


def record(destination, service, platform, build_ref):
    filename = destination / f"{service}-{platform.split('/')[1]}.json"
    try:
        images.require(build_ref, "missing build reference")
        provenance = json.loads(images.run("docker", "buildx", "history", "inspect", "attachment",
                                           "--type", "provenance", build_ref.rsplit("/", 1)[-1]))
        definition = provenance["buildDefinition"]
        images.require(definition["internalParameters"]["targetPlatform"] == platform, "platform mismatch")
        materials = [images.material_reference(material) for material in definition["resolvedDependencies"]]
        images.require(materials, "missing base image evidence")
        inputs = images.context_paths(definition, {digest for _, digest in materials})
        result = {"inputs": sorted(inputs), "materials": materials}
    except UNKNOWN as error:
        print(f"{service} {platform} requires validation on future PRs: {error}", file=sys.stderr)
        result = None
    filename.write_text(json.dumps(result))


def matrix(directory, baseline, current):
    selected = []
    try:
        paths = images.changed_paths(baseline, current) if baseline else None
    except UNKNOWN as error:
        print(f"Full validation: {error}", file=sys.stderr)
        paths = None
    for service in images.SERVICES:
        recipe = f"services/{'sandbox' if service == 'agent' else service}/Dockerfile"
        platforms = images.PLATFORMS if service in ("agent", "codemode") else ("linux/amd64",)
        for platform in platforms:
            try:
                images.require(paths is not None, "full validation")
                proof = json.loads((directory / f"{service}-{platform.split('/')[1]}.json").read_text())
                images.require(proof and proof["inputs"] and proof["materials"], "missing validation inputs")
                inputs = set(proof["inputs"]) | {recipe, recipe + ".dockerignore", ".dockerignore"}
                if service == "agent":
                    inputs.add(images.AGENT_PROBE)
                images.verify_inputs(paths, inputs, proof["materials"])
            except UNKNOWN as error:
                print(f"{service} {platform}: build ({error})", file=sys.stderr)
                selected.append(dict(service=service, platform=platform, image="centaur-" + service,
                                     dockerfile=recipe, target="sandbox" if service == "agent" else ""))
    return selected


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--carry", type=Path, help="Directory for still-valid input records")
    parser.add_argument("--record", nargs=4, metavar=("DIR", "SERVICE", "PLATFORM", "BUILD_REF"))
    args = parser.parse_args()
    if args.record:
        destination, service, platform, build_ref = args.record
        record(Path(destination), service, platform, build_ref)
    else:
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            try:
                baseline = base_snapshot(directory)
            except UNKNOWN as error:
                print(f"Full validation: {error}", file=sys.stderr)
                baseline = None
            selected = matrix(directory, baseline, "HEAD")
            if args.carry:
                args.carry.mkdir(parents=True, exist_ok=True)
                rebuilt = {f"{row['service']}-{row['platform'].split('/')[1]}.json" for row in selected}
                for proof in directory.glob("*.json"):
                    if proof.name not in rebuilt:
                        proof.rename(args.carry / proof.name)
                # Keep the inherited artifact present even on a full build.
                (args.carry / "revision").write_text(images.run("git", "rev-parse", "HEAD").decode())
        print(json.dumps({"matrix": {"include": selected}, "has_build": bool(selected)}))
