"""Select PR image checks from native BuildKit inputs of a merged, passing PR run."""
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
PROBE = "scripts/probe-agent-harness-image.sh"
WORKFLOW = ".github/workflows/validate-images.yml"
UNKNOWN = (OSError, ValueError, KeyError, IndexError, TypeError, AttributeError, subprocess.SubprocessError)


def gh(*args):
    return json.loads(images.run("gh", "api", *args))


def merged_snapshot(directory):
    repo = os.environ["GITHUB_REPOSITORY"]
    base = os.environ["GITHUB_BASE_REF"]
    base_sha = os.environ["PR_BASE_SHA"]
    artifacts = gh(f"repos/{repo}/actions/artifacts?name={SNAPSHOT}&per_page=10")["artifacts"]
    for artifact in artifacts:
        if artifact["expired"]:
            continue
        run_id = artifact["workflow_run"]["id"]
        run = gh(f"repos/{repo}/actions/runs/{run_id}")
        if (run["event"] != "pull_request" or run["conclusion"] != "success" or
                run["path"] != WORKFLOW or run["head_repository"]["full_name"] != repo):
            continue
        pulls = gh(f"repos/{repo}/commits/{run['head_sha']}/pulls")
        for pr in pulls:
            if not (pr["merged_at"] and pr["base"]["ref"] == base and pr["base"]["repo"]["full_name"] == repo
                    and pr["head"]["sha"] == run["head_sha"] and pr["head"]["repo"]["full_name"] == repo):
                continue
            merged = pr["merge_commit_sha"]
            if subprocess.run(["git", "merge-base", "--is-ancestor", merged, base_sha]).returncode:
                continue
            images.run("gh", "run", "download", str(run_id), "-R", repo, "-n", SNAPSHOT, "-D", str(directory))
            tree = (directory / "tree").read_text().strip()
            images.require(tree == images.run("git", "rev-parse", merged + "^{tree}").decode().strip(),
                           "tested merge tree differs from landed tree")
            print(f"Comparing with validation {run['html_url']}", file=sys.stderr)
            return tree
    raise ValueError("no passing validation from a merged PR on this branch")


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
        platforms = images.PLATFORMS if service == "agent" else ("linux/amd64",)
        for platform in platforms:
            try:
                images.require(paths is not None, "full validation")
                proof = json.loads((directory / f"{service}-{platform.split('/')[1]}.json").read_text())
                images.require(proof and proof["inputs"] and proof["materials"], "missing validation inputs")
                inputs = set(proof["inputs"]) | {recipe, recipe + ".dockerignore", ".dockerignore"}
                if service == "agent":
                    inputs.add(PROBE)
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
                baseline = merged_snapshot(directory)
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
                (args.carry / "tree").write_bytes(images.run("git", "rev-parse", "HEAD^{tree}"))
        print(json.dumps({"matrix": {"include": selected}, "has_build": bool(selected)}))
