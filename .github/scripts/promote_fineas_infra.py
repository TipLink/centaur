from __future__ import annotations

import base64
import json
import os
import re
import subprocess
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path


OWNER = "TipLink"
REPO = "fineas-centaur-infra"
BASE_BRANCH = "main"
ROOT = Path(os.environ.get("FINEAS_INFRA_ROOT", "fineas-centaur-infra"))
CENTAUR_SHA = os.environ["CENTAUR_SHA"]
CENTAUR_SHORT = CENTAUR_SHA[:7]
BRANCH = f"automation/promote-centaur-{CENTAUR_SHORT}"
MESSAGE = f"chore: stage centaur {CENTAUR_SHORT} console migration"
API = f"https://api.github.com/repos/{OWNER}/{REPO}"

HEADERS = {
    "Authorization": f"Bearer {os.environ['FINEAS_INFRA_TOKEN']}",
    "Accept": "application/vnd.github+json",
    "X-GitHub-Api-Version": "2022-11-28",
    "User-Agent": "centaur-promote-fineas",
}
CENTAUR_HEADERS = {
    **HEADERS,
    "Authorization": f"Bearer {os.environ['CENTAUR_TOKEN']}",
}


def request(
    method: str,
    url: str,
    payload: dict | None = None,
    *,
    ok: tuple[int, ...] = (200, 201, 204),
    headers: dict[str, str] | None = None,
):
    data = None
    request_headers = dict(headers or HEADERS)
    if payload is not None:
        data = json.dumps(payload).encode()
        request_headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=request_headers, method=method)
    try:
        with urllib.request.urlopen(req) as response:
            body = response.read().decode()
            if response.status not in ok:
                raise RuntimeError(f"{method} {url} returned {response.status}: {body}")
            return json.loads(body) if body else None
    except urllib.error.HTTPError as error:
        body = error.read().decode()
        if error.code in ok:
            if error.code == 404:
                return None
            return json.loads(body) if body else None
        raise RuntimeError(f"{method} {url} returned {error.code}: {body}") from None


def git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(ROOT), *args], text=True)


def centaur_pr_url_for_commit() -> str:
    repo = os.environ["CENTAUR_REPO"]
    url = f"https://api.github.com/repos/{repo}/commits/{CENTAUR_SHA}/pulls"
    try:
        pulls = request("GET", url, headers=CENTAUR_HEADERS)
    except RuntimeError as error:
        print(f"warning: could not look up Centaur PR for {CENTAUR_SHA}: {error}")
        return ""
    merged = [pull for pull in pulls if pull.get("merged_at")]
    pull = (merged or pulls or [None])[0]
    return pull["html_url"] if pull else ""


def changed_paths() -> list[tuple[str, str]]:
    changed: list[tuple[str, str]] = []
    for line in git("status", "--porcelain").splitlines():
        if not line:
            continue
        status = line[:2]
        path = line[3:]
        if " -> " in path:
            path = path.split(" -> ", 1)[1]
        kind = "A" if status == "??" else "D" if "D" in status else "M"
        changed.append((kind, path))
    return changed


def ensure_branch() -> str:
    encoded = urllib.parse.quote(BRANCH, safe="")
    existing = request("GET", f"{API}/git/ref/heads/{encoded}", ok=(200, 404))
    if not existing:
        base_ref = request("GET", f"{API}/git/ref/heads/{BASE_BRANCH}")
        request(
            "POST",
            f"{API}/git/refs",
            {"ref": f"refs/heads/{BRANCH}", "sha": base_ref["object"]["sha"]},
        )
    return encoded


def put_file(rel: str, encoded_branch: str) -> bool:
    encoded_path = urllib.parse.quote(rel, safe="")
    url = f"{API}/contents/{encoded_path}"
    existing = request("GET", f"{url}?ref={encoded_branch}", ok=(200, 404))
    content = (ROOT / rel).read_bytes()
    if existing:
        current = base64.b64decode(existing["content"]).replace(b"\r\n", b"\n")
        if current == content:
            return False
    payload = {
        "message": MESSAGE,
        "content": base64.b64encode(content).decode(),
        "branch": BRANCH,
    }
    if existing:
        payload["sha"] = existing["sha"]
    request("PUT", url, payload)
    return True


def delete_file(rel: str, encoded_branch: str) -> bool:
    encoded_path = urllib.parse.quote(rel, safe="")
    url = f"{API}/contents/{encoded_path}"
    existing = request("GET", f"{url}?ref={encoded_branch}", ok=(200, 404))
    if not existing:
        return False
    request(
        "DELETE",
        url,
        {"message": MESSAGE, "branch": BRANCH, "sha": existing["sha"]},
    )
    return True


def set_application_annotation(key: str, value: str) -> bool:
    path = ROOT / "clusters/centaur-sandbox/argocd/applications/centaur-sandbox.yaml"
    text = path.read_text(encoding="utf-8")
    pattern = re.compile(rf"^    {re.escape(key)}: .*$", flags=re.MULTILINE)
    if value:
        line = f"    {key}: {json.dumps(value)}"
        if pattern.search(text):
            updated = pattern.sub(line, text, count=1)
        else:
            marker = "    argocd.argoproj.io/compare-options: ServerSideDiff=true\n"
            if marker not in text:
                raise RuntimeError(f"could not place Application annotation {key}")
            updated = text.replace(marker, f"{marker}{line}\n", 1)
    else:
        updated = pattern.sub("", text)
        updated = re.sub(r"\n{3,}", "\n\n", updated)
    if updated == text:
        return False
    path.write_text(updated, encoding="utf-8")
    return True


def pull_request_body(centaur_pr_url: str) -> str:
    source = [
        f"- Centaur commit: https://github.com/{os.environ['CENTAUR_REPO']}/commit/{CENTAUR_SHA}",
        f"- Image publish run: {os.environ['CENTAUR_RUN_URL']}",
    ]
    if centaur_pr_url:
        source.append(f"- Centaur PR: {centaur_pr_url}")
    return f"""## Summary
- stage the Fineas Console image and Centaur chart at `sha-{CENTAUR_SHORT}`
- keep api-rs, Slackbot, sandbox, and proxy runtime images on their previous pins
- keep `apiRs.runMigrations=false`; this is the Console-only migration stage

## Source
{chr(10).join(source)}

## Tests
- `bash scripts/bump-centaur-pins.sh --stage console {CENTAUR_SHA}`
- `scripts/audit-supply-chain.sh`
"""


def main() -> None:
    paths = changed_paths()
    if not paths:
        print("Fineas infra pins already match; no promotion PR needed")
        return

    encoded_branch = ensure_branch()
    changed = False
    for kind, rel in paths:
        changed |= delete_file(rel, encoded_branch) if kind == "D" else put_file(rel, encoded_branch)

    centaur_pr_url = centaur_pr_url_for_commit()
    title = f"Stage Centaur {CENTAUR_SHORT} Console migration in Fineas"
    body = pull_request_body(centaur_pr_url)
    head = urllib.parse.quote(f"{OWNER}:{BRANCH}", safe="")
    pulls = request("GET", f"{API}/pulls?state=open&head={head}")
    if pulls:
        pull = request("PATCH", f"{API}/pulls/{pulls[0]['number']}", {"title": title, "body": body})
    elif changed:
        pull = request(
            "POST",
            f"{API}/pulls",
            {"title": title, "head": BRANCH, "base": BASE_BRANCH, "body": body},
        )
    else:
        pull = None

    if not pull:
        print("Promotion branch already matches and no open PR was found")
        return

    annotations_changed = set_application_annotation(
        "fineas.dev/deployment-pr-url", pull["html_url"]
    )
    annotations_changed |= set_application_annotation(
        "fineas.dev/centaur-pr-url", centaur_pr_url
    )
    if annotations_changed:
        put_file(
            "clusters/centaur-sandbox/argocd/applications/centaur-sandbox.yaml",
            encoded_branch,
        )
    print(pull["html_url"])


if __name__ == "__main__":
    main()
