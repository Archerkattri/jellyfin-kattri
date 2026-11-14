#!/usr/bin/env python3
"""Download successful release workflow artifacts for one pushed tag."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import stat
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from pathlib import Path, PurePosixPath
from typing import Any


API_ROOT = "https://api.github.com"
WORKFLOWS = ("release-windows.yml", "release-macos.yml", "release-linux.yml")
MAX_ARTIFACT_BYTES = 2 * 1024 * 1024 * 1024
MAX_TOTAL_BYTES = 6 * 1024 * 1024 * 1024
MAX_ARCHIVE_FILES = 5000
MAX_UNPACKED_BYTES = 6 * 1024 * 1024 * 1024
CHUNK_SIZE = 1024 * 1024
TAG_PATTERN = re.compile(r"^v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.-]+)?$")


class SafeRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Do not forward the GitHub token to signed artifact-storage redirects."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):  # type: ignore[override]
        parsed = urllib.parse.urlparse(newurl)
        if parsed.scheme != "https":
            raise urllib.error.HTTPError(newurl, code, "non-HTTPS redirect refused", headers, fp)
        return urllib.request.Request(
            newurl,
            headers={"User-Agent": "kattri-release-validation"},
            method="GET",
        )


def opener() -> urllib.request.OpenerDirector:
    return urllib.request.build_opener(SafeRedirectHandler())


def github_request(url: str, token: str, accept: str, timeout: int = 60) -> bytes:
    parsed = urllib.parse.urlparse(url)
    if parsed.scheme != "https" or parsed.hostname != "api.github.com":
        raise ValueError("GitHub API URL is outside the expected HTTPS origin")
    request = urllib.request.Request(
        url,
        headers={
            "Accept": accept,
            "Authorization": "Bearer " + token,
            "User-Agent": "kattri-release-validation",
            "X-GitHub-Api-Version": "2022-11-28",
        },
        method="GET",
    )
    try:
        with opener().open(request, timeout=timeout) as response:
            payload = response.read(MAX_ARTIFACT_BYTES + 1)
    except urllib.error.HTTPError as exc:
        raise RuntimeError("GitHub API request failed with HTTP %d" % exc.code) from None
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        raise RuntimeError("GitHub API request failed (%s)" % type(exc).__name__) from None
    if len(payload) > MAX_ARTIFACT_BYTES:
        raise RuntimeError("GitHub API response exceeded the configured size limit")
    return payload


def api_json(url: str, token: str) -> dict[str, Any]:
    payload = github_request(url, token, "application/vnd.github+json")
    try:
        value = json.loads(payload)
    except (UnicodeDecodeError, json.JSONDecodeError):
        raise RuntimeError("GitHub API returned invalid JSON") from None
    if not isinstance(value, dict):
        raise RuntimeError("GitHub API returned an unexpected JSON value")
    return value


def validate_repository(value: str) -> str:
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", value):
        raise ValueError("repository must have owner/name form")
    return value


def get_run(repository: str, run_id: int, token: str) -> dict[str, Any]:
    return api_json("%s/repos/%s/actions/runs/%d" % (API_ROOT, repository, run_id), token)


def latest_tag_run(
    repository: str,
    workflow: str,
    commit: str,
    tag: str,
    token: str,
    known_run_id: int | None = None,
) -> dict[str, Any] | None:
    if known_run_id is not None:
        value = get_run(repository, known_run_id, token)
        if value.get("head_sha") != commit or value.get("head_branch") != tag:
            raise RuntimeError("triggering release workflow does not match the requested tag commit")
        if value.get("event") != "push":
            raise RuntimeError("release workflow was not triggered by a tag push")
        return value

    workflow_segment = urllib.parse.quote(workflow, safe="")
    query = urllib.parse.urlencode({"head_sha": commit, "per_page": "100"})
    url = "%s/repos/%s/actions/workflows/%s/runs?%s" % (
        API_ROOT,
        repository,
        workflow_segment,
        query,
    )
    value = api_json(url, token)
    runs = value.get("workflow_runs")
    if not isinstance(runs, list):
        raise RuntimeError("GitHub API did not return workflow runs")
    matching = [
        run
        for run in runs
        if isinstance(run, dict)
        and run.get("head_sha") == commit
        and run.get("head_branch") == tag
        and run.get("event") == "push"
    ]
    if not matching:
        return None
    matching.sort(
        key=lambda run: (
            int(run.get("run_number", 0)),
            int(run.get("run_attempt", 0)),
            str(run.get("created_at", "")),
        ),
        reverse=True,
    )
    return matching[0]


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(CHUNK_SIZE), b""):
            digest.update(chunk)
    return digest.hexdigest()


def download_artifact_zip(repository: str, artifact_id: int, destination: Path, token: str, size: int) -> str:
    if size < 0 or size > MAX_ARTIFACT_BYTES:
        raise RuntimeError("release artifact exceeds the configured download bound")
    url = "%s/repos/%s/actions/artifacts/%d/zip" % (API_ROOT, repository, artifact_id)
    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": "Bearer " + token,
            "User-Agent": "kattri-release-validation",
            "X-GitHub-Api-Version": "2022-11-28",
        },
        method="GET",
    )
    digest = hashlib.sha256()
    total = 0
    try:
        with opener().open(request, timeout=120) as response, destination.open("xb") as output:
            while True:
                chunk = response.read(CHUNK_SIZE)
                if not chunk:
                    break
                total += len(chunk)
                if total > MAX_ARTIFACT_BYTES:
                    raise RuntimeError("downloaded artifact exceeded the configured size limit")
                digest.update(chunk)
                output.write(chunk)
    except urllib.error.HTTPError as exc:
        raise RuntimeError("artifact download failed with HTTP %d" % exc.code) from None
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        raise RuntimeError("artifact download failed (%s)" % type(exc).__name__) from None
    if total != size:
        raise RuntimeError("downloaded artifact size did not match GitHub metadata")
    return digest.hexdigest()


def extract_artifact(archive: Path, destination: Path, remaining_limit: int) -> tuple[int, int]:
    root = destination.resolve()
    count = 0
    unpacked = 0
    seen: set[str] = set()
    try:
        with zipfile.ZipFile(archive) as zipped:
            entries = zipped.infolist()
            if len(entries) > MAX_ARCHIVE_FILES:
                raise RuntimeError("artifact archive contains too many entries")
            declared_total = sum(item.file_size for item in entries if not item.is_dir())
            if declared_total > remaining_limit:
                raise RuntimeError("unpacked artifacts exceed the total size bound")

            for item in entries:
                name = item.filename
                if "\\" in name or "\x00" in name or re.match(r"^[A-Za-z]:", name):
                    raise RuntimeError("artifact archive contains an unsafe path")
                relative = PurePosixPath(name)
                if relative.is_absolute() or any(part in ("", ".", "..") for part in relative.parts):
                    raise RuntimeError("artifact archive contains an unsafe path")
                key = relative.as_posix().casefold()
                if key in seen:
                    raise RuntimeError("artifact archive contains duplicate paths")
                seen.add(key)

                mode = (item.external_attr >> 16) & 0xFFFF
                if stat.S_ISLNK(mode):
                    raise RuntimeError("artifact archive contains a symbolic link")
                target = destination.joinpath(*relative.parts)
                resolved_parent = target.parent.resolve()
                if os.path.commonpath((str(root), str(resolved_parent))) != str(root):
                    raise RuntimeError("artifact archive path escapes its output directory")
                if item.is_dir():
                    target.mkdir(parents=True, exist_ok=True)
                    continue

                target.parent.mkdir(parents=True, exist_ok=True)
                with zipped.open(item) as source, target.open("xb") as output:
                    copied = 0
                    while True:
                        chunk = source.read(CHUNK_SIZE)
                        if not chunk:
                            break
                        copied += len(chunk)
                        unpacked += len(chunk)
                        if copied > item.file_size or unpacked > remaining_limit:
                            raise RuntimeError("artifact archive exceeded its declared size bound")
                        output.write(chunk)
                if copied != item.file_size:
                    raise RuntimeError("artifact archive entry size did not match its directory")
                count += 1
    except zipfile.BadZipFile:
        raise RuntimeError("GitHub artifact payload was not a valid ZIP archive") from None
    return count, unpacked


def collect_artifacts(
    repository: str,
    run: dict[str, Any],
    workflow: str,
    token: str,
    output: Path,
    download_budget: int,
    unpacked_budget: int,
) -> tuple[list[dict[str, Any]], int, int]:
    run_id = int(run["id"])
    page = 1
    records: list[dict[str, Any]] = []
    total_bytes = 0
    total_unpacked_bytes = 0
    while True:
        url = "%s/repos/%s/actions/runs/%d/artifacts?per_page=100&page=%d" % (
            API_ROOT,
            repository,
            run_id,
            page,
        )
        payload = api_json(url, token)
        artifacts = payload.get("artifacts")
        if not isinstance(artifacts, list):
            raise RuntimeError("GitHub API did not return workflow artifacts")
        for artifact in artifacts:
            if artifact.get("expired"):
                continue
            artifact_id = int(artifact["id"])
            artifact_size = int(artifact.get("size_in_bytes", 0))
            if artifact_size <= 0:
                raise RuntimeError("release workflow uploaded an empty artifact")
            if artifact_size > download_budget - total_bytes:
                raise RuntimeError("release artifacts exceed the configured total download bound")

            folder_name = "%s__run-%d__artifact-%d__%s" % (
                workflow[:-4] if workflow.endswith(".yml") else workflow,
                run_id,
                artifact_id,
                "collected",
            )
            artifact_dir = output / folder_name
            artifact_dir.mkdir(parents=True, exist_ok=False)
            archive_path = artifact_dir / ".workflow-artifact.zip"
            archive_digest = download_artifact_zip(
                repository,
                artifact_id,
                archive_path,
                token,
                artifact_size,
            )
            expected_digest = artifact.get("digest")
            if isinstance(expected_digest, str) and expected_digest.startswith("sha256:"):
                if archive_digest != expected_digest.split(":", 1)[1].lower():
                    raise RuntimeError("workflow artifact digest did not match GitHub metadata")

            file_count, unpacked_bytes = extract_artifact(
                archive_path,
                artifact_dir,
                unpacked_budget - total_unpacked_bytes,
            )
            archive_path.unlink()
            total_bytes += artifact_size
            total_unpacked_bytes += unpacked_bytes
            records.append(
                {
                    "workflow": workflow,
                    "run_id": run_id,
                    "artifact_id": artifact_id,
                    "archive_sha256": archive_digest,
                    "archive_size_bytes": artifact_size,
                    "extracted_file_count": file_count,
                    "extracted_size_bytes": unpacked_bytes,
                    "directory": folder_name,
                }
            )
        if len(artifacts) < 100:
            break
        page += 1
    if not records:
        raise RuntimeError("release workflow has no unexpired artifacts")
    return records, total_bytes, total_unpacked_bytes


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY"), required=False)
    parser.add_argument("--commit", default=os.environ.get("RELEASE_SHA"), required=False)
    parser.add_argument("--tag", default=os.environ.get("RELEASE_TAG"), required=False)
    parser.add_argument("--linux-run-id", type=int, default=None)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--wait-seconds", type=int, default=7200)
    parser.add_argument("--poll-seconds", type=int, default=30)
    args = parser.parse_args()

    try:
        repository = validate_repository(args.repository or "")
        commit = args.commit or ""
        tag = args.tag or ""
        if not re.fullmatch(r"[0-9a-fA-F]{40}", commit):
            raise ValueError("commit must be a full 40-character SHA")
        if not TAG_PATTERN.fullmatch(tag):
            raise ValueError("tag must be a version tag of the form vX.Y.Z")
        if args.wait_seconds < 0 or args.poll_seconds < 1:
            raise ValueError("wait and poll intervals must be non-negative")
        token = os.environ.get("GH_TOKEN", "")
        if not token:
            raise ValueError("GH_TOKEN is required and is read only from the process environment")

        args.output_dir.mkdir(parents=True, exist_ok=True)
        start = time.monotonic()
        deadline = start + args.wait_seconds
        runs: dict[str, dict[str, Any]] = {}
        while True:
            pending: list[str] = []
            for workflow in WORKFLOWS:
                run_id = args.linux_run_id if workflow == "release-linux.yml" else None
                run = latest_tag_run(repository, workflow, commit, tag, token, run_id)
                if run is None or run.get("status") != "completed":
                    pending.append(workflow)
                    continue
                if run.get("conclusion") != "success":
                    raise RuntimeError("%s run did not succeed for this tag" % workflow)
                runs[workflow] = run
            if not pending:
                break
            if time.monotonic() >= deadline:
                raise RuntimeError("timed out waiting for successful release workflows: %s" % ", ".join(pending))
            print("Waiting for release workflows: %s" % ", ".join(pending), flush=True)
            time.sleep(min(args.poll_seconds, max(1, int(deadline - time.monotonic()))))

        total_budget = MAX_TOTAL_BYTES
        artifact_records: list[dict[str, Any]] = []
        total_bytes = 0
        total_unpacked_bytes = 0
        for workflow in WORKFLOWS:
            records, used, unpacked = collect_artifacts(
                repository,
                runs[workflow],
                workflow,
                token,
                args.output_dir,
                total_budget - total_bytes,
                MAX_UNPACKED_BYTES - total_unpacked_bytes,
            )
            artifact_records.extend(records)
            total_bytes += used
            total_unpacked_bytes += unpacked

        collection = {
            "schema_version": 1,
            "release_tag": tag,
            "source_commit": commit.lower(),
            "release_workflows": [
                {
                    "workflow": workflow,
                    "run_id": int(runs[workflow]["id"]),
                    "conclusion": runs[workflow].get("conclusion"),
                    "head_sha": runs[workflow].get("head_sha"),
                    "head_branch": runs[workflow].get("head_branch"),
                    "event": runs[workflow].get("event"),
                }
                for workflow in WORKFLOWS
            ],
            "workflow_artifacts": artifact_records,
            "downloaded_bytes": total_bytes,
            "unpacked_bytes": total_unpacked_bytes,
        }
        collection_path = args.output_dir / "collection.json"
        collection_path.write_text(json.dumps(collection, indent=2) + "\n", encoding="utf-8")
        print("Downloaded %d workflow artifacts from three successful release runs." % len(artifact_records))
        return 0
    except (OSError, ValueError, RuntimeError, KeyError, TypeError) as exc:
        print("release artifact collection failed: %s" % str(exc), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
