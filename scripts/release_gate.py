#!/usr/bin/env python3
"""Fail-closed gates for the existing PyPI trusted-publisher workflow."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import time
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import Request, urlopen
import zipfile

WORKFLOWS = {
    "ci.yml": {"rust", "python (3.10)", "python (3.14)"},
    "backend-integration.yml": {
        "docker-backend (postgres)",
        "docker-backend (clickhouse)",
        "docker-backend (umbra)",
        "datafusion",
    },
}


def request_json(url, token=None, missing_ok=False):
    headers = {"Accept": "application/json", "User-Agent": "dfc-release-gate"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    try:
        with urlopen(Request(url, headers=headers), timeout=30) as response:
            return json.load(response)
    except HTTPError as exc:
        if missing_ok and exc.code == 404:
            return None
        raise


class GitHub:
    def __init__(self, repository, token):
        self.base = f"https://api.github.com/repos/{repository}"
        self.token = token

    def get(self, path):
        return request_json(self.base + path, self.token)

    def pages(self, path, key):
        result = []
        separator = "&" if "?" in path else "?"
        for page in range(1, 101):
            values = self.get(f"{path}{separator}per_page=100&page={page}")[key]
            result.extend(values)
            if len(values) < 100:
                return result
        raise RuntimeError("GitHub pagination limit exceeded; refusing incomplete evidence")


def version_from_toml(text, section):
    sections = re.split(r"(?m)^\[([^\]\n]+)\]\s*$", text)
    bodies = [sections[i + 1] for i in range(1, len(sections), 2) if sections[i] == section]
    if len(bodies) != 1:
        raise RuntimeError(f"Expected exactly one [{section}] section")
    versions = re.findall(r'(?m)^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"\s*$', bodies[0])
    if len(versions) != 1:
        raise RuntimeError("Release requires one explicit stable x.y.z version")
    return versions[0]


def version_tuple(version):
    return tuple(map(int, version.split(".")))


def deliberate_version_bump():
    current = version_from_toml(Path("pyproject.toml").read_text(), "project")
    cargo = version_from_toml(Path("Cargo.toml").read_text(), "workspace.package")
    previous = subprocess.check_output(["git", "show", "HEAD^:pyproject.toml"], text=True)
    old = version_from_toml(previous, "project")
    if current != cargo:
        raise RuntimeError("Python and Cargo release versions differ")
    return current, version_tuple(current) > version_tuple(old)


def trusted_candidate(event_name, event, repository, ref, sha):
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise RuntimeError("Invalid candidate SHA")
    if event_name == "workflow_dispatch":
        return ref == "refs/heads/main"
    if event_name != "workflow_run":
        return False
    run = event["workflow_run"]
    return (
        run["event"] == "push"
        and run["head_branch"] == "main"
        and run["head_sha"] == sha
        and run["head_repository"]["full_name"] == repository
        and run["head_repository"]["id"] == event["repository"]["id"]
    )


def checks_passed(api, sha, repository_id):
    for workflow, expected_jobs in WORKFLOWS.items():
        runs = api.pages(
            f"/actions/workflows/{workflow}/runs?event=push&branch=main&head_sha={sha}",
            "workflow_runs",
        )
        runs = [
            run
            for run in runs
            if run["head_sha"] == sha
            and run["event"] == "push"
            and run["head_branch"] == "main"
            and run["head_repository"]["id"] == repository_id
        ]
        if not runs:
            return False
        # A newer failed/pending run must not be masked by an earlier green run.
        run = max(runs, key=lambda item: item["id"])
        if run["status"] != "completed" or run["conclusion"] != "success":
            return False
        jobs = api.pages(f"/actions/runs/{run['id']}/jobs?filter=latest", "jobs")
        if not expected_jobs.issubset({job["name"] for job in jobs}):
            return False
        if any(job["status"] != "completed" or job["conclusion"] != "success" for job in jobs):
            return False
    return True


def already_released(api, sha, this_run):
    runs = api.pages(f"/actions/workflows/pypi-publish.yml/runs?head_sha={sha}", "workflow_runs")
    for run in runs:
        if str(run["id"]) == this_run or run["status"] != "completed":
            continue
        if run["conclusion"] != "success" or run["head_sha"] != sha:
            continue
        jobs = api.pages(f"/actions/runs/{run['id']}/jobs?filter=latest", "jobs")
        if any(
            job["name"] == "Release"
            and job["status"] == "completed"
            and job["conclusion"] == "success"
            for job in jobs
        ):
            return True
    return False


def artifact_hashes(directory, version):
    files = sorted(Path(directory).iterdir())
    # Six manylinux, four musllinux, three Windows, two macOS wheels and one sdist.
    if len(files) != 16 or sum(path.suffix == ".whl" for path in files) != 15:
        raise RuntimeError("Expected the complete 15-wheel + 1-sdist release matrix")
    hashes = {}
    for path in files:
        if path.suffix == ".whl":
            with zipfile.ZipFile(path) as archive:
                names = [
                    name for name in archive.namelist() if name.endswith(".dist-info/METADATA")
                ]
                if len(names) != 1:
                    raise RuntimeError(f"Invalid wheel metadata: {path.name}")
                metadata = archive.read(names[0]).decode()
        elif path.name.endswith(".tar.gz"):
            with tarfile.open(path) as archive:
                members = [
                    m
                    for m in archive.getmembers()
                    if m.name.count("/") == 1 and m.name.endswith("/PKG-INFO")
                ]
                if len(members) != 1:
                    raise RuntimeError(f"Invalid sdist metadata: {path.name}")
                metadata = archive.extractfile(members[0]).read().decode()
        else:
            raise RuntimeError(f"Unexpected release artifact: {path.name}")
        if not re.search(r"(?m)^Name: data[-_]flow[-_]control$", metadata):
            raise RuntimeError(f"Wrong package: {path.name}")
        if not re.search(rf"(?m)^Version: {re.escape(version)}$", metadata):
            raise RuntimeError(f"Wrong artifact version: {path.name}")
        hashes[path.name] = hashlib.sha256(path.read_bytes()).hexdigest()
    return hashes


def missing_files(expected, published):
    existing = {item["filename"]: item["digests"]["sha256"] for item in published}
    if set(existing) - set(expected):
        raise RuntimeError("PyPI has unexpected files for this version; refusing overwrite")
    for filename, digest in existing.items():
        if expected[filename] != digest:
            raise RuntimeError(f"PyPI file differs from this build: {filename}")
    return sorted(set(expected) - set(existing))


def output(**values):
    with open(os.environ["GITHUB_OUTPUT"], "a") as stream:
        for key, value in values.items():
            stream.write(f"{key}={value}\n")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("phase", choices=["preflight", "publish", "verify"])
    args = parser.parse_args()
    repository = os.environ["GITHUB_REPOSITORY"]
    sha = os.environ["RELEASE_SHA"]
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    if not trusted_candidate(
        os.environ["GITHUB_EVENT_NAME"], event, repository, os.environ["GITHUB_REF"], sha
    ):
        raise RuntimeError("Only same-repository main push runs or main dispatch may release")
    checked_out = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    if checked_out != sha:
        raise RuntimeError("Checkout is not the tested release SHA")
    if args.phase == "verify":
        version = version_from_toml(Path("pyproject.toml").read_text(), "project")
        expected = artifact_hashes("dist", version)
        # PyPI metadata can lag an accepted upload. Bound retries to propagation,
        # not retries of publishing itself, which could duplicate uploads.
        for attempt in range(12):
            published = request_json(
                f"https://pypi.org/pypi/data-flow-control/{quote(version)}/json", missing_ok=True
            )
            missing = missing_files(expected, [] if published is None else published["urls"])
            if not missing:
                print("PyPI contains the complete release with matching SHA256 hashes")
                return
            if attempt < 11:
                time.sleep(10)
        raise RuntimeError(f"PyPI release remains incomplete: {missing}")
    api = GitHub(repository, os.environ["GH_TOKEN"])
    version, bumped = deliberate_version_bump()
    main_sha = api.get("/git/ref/heads/main")["object"]["sha"]
    ready = bumped and main_sha == sha and checks_passed(api, sha, event["repository"]["id"])
    if not ready:
        if args.phase == "publish":
            raise RuntimeError(
                "Release gate changed: stale head, missing version bump, or tests not green"
            )
        print(
            "No release: requires a version bump on current main and all exact-SHA push tests green"
        )
        output(release="false")
        return
    if args.phase == "preflight":
        if already_released(api, sha, os.environ["GITHUB_RUN_ID"]):
            print("Exact SHA already completed its Release job; skipping duplicate trigger")
            output(release="false")
        else:
            output(release="true", sha=sha, version=version)
        return
    expected = artifact_hashes("dist", version)
    published = request_json(
        f"https://pypi.org/pypi/data-flow-control/{quote(version)}/json", missing_ok=True
    )
    missing = missing_files(expected, [] if published is None else published["urls"])
    pending = Path("pending-dist")
    if pending.exists():
        raise RuntimeError("Unexpected pre-existing pending-dist directory")
    pending.mkdir()
    for name in missing:
        shutil.copyfile(Path("dist") / name, pending / name)
    Path("release-manifest.json").write_text(
        json.dumps({"source_sha": sha, "version": version, "sha256": expected}, indent=2) + "\n"
    )
    print(f"Validated {len(expected)} artifacts; {len(missing)} files remain to publish")
    output(publish="true" if missing else "false")


if __name__ == "__main__":
    main()
