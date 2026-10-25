"""Offline fail-closed tests for release decisions; no credentials or network."""

import importlib.util
from pathlib import Path

import pytest

SPEC = importlib.util.spec_from_file_location(
    "release_gate", Path(__file__).resolve().parents[2] / "scripts" / "release_gate.py"
)
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)
SHA = "a" * 40


def event():
    return {
        "repository": {"id": 1},
        "workflow_run": {
            "event": "push",
            "head_branch": "main",
            "head_sha": SHA,
            "head_repository": {"id": 1, "full_name": "owner/repo"},
        },
    }


def test_only_trusted_main_events():
    value = event()
    assert gate.trusted_candidate("workflow_run", value, "owner/repo", "refs/heads/main", SHA)
    for key, replacement in [
        ("event", "pull_request"),
        ("head_branch", "feature"),
        ("head_sha", "b" * 40),
    ]:
        changed = event()
        changed["workflow_run"][key] = replacement
        assert not gate.trusted_candidate("workflow_run", changed, "owner/repo", "main", SHA)
    value["workflow_run"]["head_repository"]["id"] = 2
    assert not gate.trusted_candidate("workflow_run", value, "owner/repo", "main", SHA)
    assert not gate.trusted_candidate("workflow_dispatch", {}, "owner/repo", "refs/heads/pr", SHA)
    assert gate.trusted_candidate("workflow_dispatch", {}, "owner/repo", "refs/heads/main", SHA)


class Checks:
    def __init__(self):
        self.runs = [
            {
                "id": 1,
                "head_sha": SHA,
                "event": "push",
                "head_branch": "main",
                "head_repository": {"id": 1},
                "status": "completed",
                "conclusion": "success",
            }
        ]
        self.jobs = [
            {"name": name, "status": "completed", "conclusion": "success"}
            for names in gate.WORKFLOWS.values()
            for name in names
        ]

    def pages(self, path, key):
        assert "filter=latest" in path if key == "jobs" else "event=push" in path
        return self.jobs if key == "jobs" else self.runs


def test_all_exact_sha_jobs_required():
    api = Checks()
    assert gate.checks_passed(api, SHA, 1)
    api.jobs.pop()
    assert not gate.checks_passed(api, SHA, 1)
    api.jobs = []
    assert not gate.checks_passed(api, SHA, 1)


@pytest.mark.parametrize("conclusion", ["skipped", "cancelled", "failure", None])
def test_no_skipped_or_failed_jobs(conclusion):
    api = Checks()
    api.jobs[0]["conclusion"] = conclusion
    assert not gate.checks_passed(api, SHA, 1)


@pytest.mark.parametrize("status,conclusion", [("in_progress", None), ("completed", "failure")])
def test_newer_run_cannot_hide_behind_old_success(status, conclusion):
    api = Checks()
    api.runs.append(dict(api.runs[0], id=2, status=status, conclusion=conclusion))
    assert not gate.checks_passed(api, SHA, 1)


def test_wrong_sha_repo_event_or_branch_not_evidence():
    for key, replacement in [
        ("head_sha", "b" * 40),
        ("head_repository", {"id": 2}),
        ("event", "pull_request"),
        ("head_branch", "feature"),
    ]:
        api = Checks()
        api.runs[0][key] = replacement
        assert not gate.checks_passed(api, SHA, 1)


def test_git_parent_must_show_version_increase(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    (tmp_path / "pyproject.toml").write_text('[project]\nversion = "0.1.8"\n')
    (tmp_path / "Cargo.toml").write_text('[workspace.package]\nversion = "0.1.8"\n')
    for previous, expected in [("0.1.7", True), ("0.1.8", False), ("0.1.9", False)]:
        monkeypatch.setattr(
            gate.subprocess, "check_output", lambda *a, **k: f'[project]\nversion = "{previous}"\n'
        )
        assert gate.deliberate_version_bump() == ("0.1.8", expected)
    (tmp_path / "Cargo.toml").write_text('[workspace.package]\nversion = "0.1.9"\n')
    with pytest.raises(RuntimeError, match="differ"):
        gate.deliberate_version_bump()


@pytest.mark.parametrize(
    "text",
    [
        '[project]\nversion = "0.1.8rc1"\n',
        '[project]\nversion = "0.1.8"\nversion = "0.1.9"\n',
        '[other]\nversion = "0.1.8"\n',
    ],
)
def test_ambiguous_or_unsupported_versions_fail(text):
    with pytest.raises(RuntimeError):
        gate.version_from_toml(text, "project")


def test_complete_and_partial_uploads_require_identical_hashes():
    expected = {"a.whl": "aaa", "b.tar.gz": "bbb"}
    first = {"filename": "a.whl", "digests": {"sha256": "aaa"}}
    assert gate.missing_files(expected, []) == ["a.whl", "b.tar.gz"]
    assert gate.missing_files(expected, [first]) == ["b.tar.gz"]
    assert (
        gate.missing_files(
            expected, [first, {"filename": "b.tar.gz", "digests": {"sha256": "bbb"}}]
        )
        == []
    )
    with pytest.raises(RuntimeError, match="differs"):
        gate.missing_files(expected, [{"filename": "a.whl", "digests": {"sha256": "bad"}}])
    with pytest.raises(RuntimeError, match="unexpected"):
        gate.missing_files(expected, [{"filename": "c.whl", "digests": {"sha256": "ccc"}}])


def test_pagination_collects_all_pages():
    api = gate.GitHub("owner/repo", "not-a-real-token")
    calls = []

    def get(path):
        calls.append(path)
        return {"jobs": [1] * (100 if path.endswith("&page=1") else 2)}

    api.get = get
    assert len(api.pages("/jobs?filter=latest", "jobs")) == 102
    assert len(calls) == 2


def test_missing_or_malformed_api_evidence_fails_closed():
    api = Checks()
    api.runs = []
    assert not gate.checks_passed(api, SHA, 1)
    api.runs = [{}]
    with pytest.raises(KeyError):
        gate.checks_passed(api, SHA, 1)


def make_artifacts(directory, version="0.1.8"):
    import io
    import tarfile
    import zipfile

    metadata = f"Name: data-flow-control\nVersion: {version}\n"
    for index in range(15):
        with zipfile.ZipFile(
            directory / f"data_flow_control-{version}-platform{index}.whl", "w"
        ) as archive:
            archive.writestr(f"data_flow_control-{version}.dist-info/METADATA", metadata)
    with tarfile.open(directory / f"data_flow_control-{version}.tar.gz", "w:gz") as archive:
        content = metadata.encode()
        info = tarfile.TarInfo(f"data_flow_control-{version}/PKG-INFO")
        info.size = len(content)
        archive.addfile(info, io.BytesIO(content))


def test_artifacts_must_be_complete_and_same_version(tmp_path):
    make_artifacts(tmp_path)
    assert len(gate.artifact_hashes(tmp_path, "0.1.8")) == 16
    with pytest.raises(RuntimeError, match="Wrong artifact version"):
        gate.artifact_hashes(tmp_path, "0.1.9")
    next(tmp_path.glob("*.whl")).unlink()
    with pytest.raises(RuntimeError, match="complete"):
        gate.artifact_hashes(tmp_path, "0.1.8")


def test_api_error_is_not_treated_as_unpublished(monkeypatch):
    from urllib.error import HTTPError

    def fail(request, timeout):
        raise HTTPError(request.full_url, 503, "unavailable", {}, None)

    monkeypatch.setattr(gate, "urlopen", fail)
    with pytest.raises(HTTPError):
        gate.request_json("https://pypi.org/pypi/data-flow-control/0.1.8/json", missing_ok=True)


@pytest.mark.parametrize("phase", ["preflight", "publish"])
@pytest.mark.parametrize("reason", ["stale", "unchanged", "checks"])
def test_final_gate_rejects_changed_evidence(tmp_path, monkeypatch, phase, reason):
    import json

    event_path = tmp_path / "event.json"
    event_path.write_text(json.dumps(event()))
    output = tmp_path / "output"
    for name, value in {
        "GITHUB_REPOSITORY": "owner/repo",
        "RELEASE_SHA": SHA,
        "GITHUB_EVENT_PATH": str(event_path),
        "GITHUB_EVENT_NAME": "workflow_run",
        "GITHUB_REF": "refs/heads/main",
        "GH_TOKEN": "fake",
        "GITHUB_OUTPUT": str(output),
    }.items():
        monkeypatch.setenv(name, value)
    monkeypatch.setattr("sys.argv", ["release_gate.py", phase])
    monkeypatch.setattr(gate.subprocess, "check_output", lambda *a, **k: SHA)
    monkeypatch.setattr(gate, "deliberate_version_bump", lambda: ("0.1.8", reason != "unchanged"))
    monkeypatch.setattr(
        gate.GitHub, "get", lambda *a: {"object": {"sha": "b" * 40 if reason == "stale" else SHA}}
    )
    monkeypatch.setattr(gate, "checks_passed", lambda *a: reason != "checks")
    if phase == "publish":
        with pytest.raises(RuntimeError, match="Release gate changed"):
            gate.main()
        assert not output.exists()
    else:
        gate.main()
        assert output.read_text() == "release=false\n"


def test_duplicate_skip_requires_completed_successful_release_job():
    class Releases:
        def __init__(self):
            self.jobs = [{"name": "Release", "status": "completed", "conclusion": "success"}]

        def pages(self, path, key):
            if key == "jobs":
                return self.jobs
            return [{"id": 1, "status": "completed", "conclusion": "success", "head_sha": SHA}]

    api = Releases()
    assert gate.already_released(api, SHA, "2")
    assert not gate.already_released(api, SHA, "1")
    for job in [
        {"name": "preflight", "status": "completed", "conclusion": "success"},
        {"name": "Release", "status": "completed", "conclusion": "skipped"},
        {"name": "Release", "status": "in_progress", "conclusion": None},
    ]:
        api.jobs = [job]
        assert not gate.already_released(api, SHA, "2")
