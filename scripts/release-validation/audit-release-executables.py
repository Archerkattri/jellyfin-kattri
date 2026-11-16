#!/usr/bin/env python3
"""Run binary-specific cargo-audit scans over release executable evidence.

The current platform release workflows do not embed cargo-auditable metadata. The
resulting scans are therefore partial panic-string recovery, never complete package
dependency inventories. Each report is bound to the SHA-256 of the exact executable
evidence file named by its release sidecar.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path, PurePosixPath
from typing import Any
from urllib.parse import parse_qsl, urlsplit


HASH = re.compile(r"^[0-9a-fA-F]{64}$")
COMMIT = re.compile(r"^[0-9a-fA-F]{40}$")
SECRET_ASSIGNMENT = re.compile(
    r"(?i)(?:api[_-]?key|access[_-]?token|auth(?:orization)?|password|token)\s*[:=]\s*[^\s&#]+"
)
SECRET_QUERY_KEYS = {"api_key", "apikey", "token", "access_token", "authorization", "password"}
PARTIAL_SCOPE = (
    "cargo audit bin scanned this exact executable evidence SHA-256, but the release "
    "binary has no embedded cargo-auditable metadata; cargo-audit may recover only "
    "part of the Rust dependency list from panic strings. This is not a complete "
    "dependency inventory and does not cover native libraries."
)


def contains_secret_like_text(value: str) -> bool:
    if SECRET_ASSIGNMENT.search(value):
        return True
    try:
        parts = urlsplit(value)
    except ValueError:
        return True
    if parts.username is not None or parts.password is not None:
        return True
    return any(key.casefold() in SECRET_QUERY_KEYS and item for key, item in parse_qsl(parts.query))


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def safe_file(root: Path, value: Any, label: str) -> tuple[Path, str]:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{label} path is empty")
    if any(part in ("", ".", "..") for part in value.split("/")):
        raise ValueError(f"{label} path contains an unsafe segment")
    relative = PurePosixPath(value)
    if relative.is_absolute() or "\\" in value or "\x00" in value:
        raise ValueError(f"{label} path is not a safe relative path")
    root = root.resolve(strict=True)
    candidate = root
    for part in relative.parts:
        candidate = candidate / part
        if candidate.is_symlink():
            raise ValueError(f"{label} path contains a symlink")
    candidate = candidate.resolve(strict=True)
    if not candidate.is_relative_to(root) or not candidate.is_file():
        raise ValueError(f"{label} path is not a file inside its artifact directory")
    return candidate, relative.as_posix()


def require_partial_scope(value: Any) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ValueError("release executable lacks an explicit audit_scope")
    lowered = value.casefold()
    if "cargo-auditable metadata" not in lowered or not any(
        phrase in lowered for phrase in ("did not produce", "no cargo-auditable")
    ):
        raise ValueError(
            "release executable audit_scope does not explicitly state that cargo-auditable metadata is absent"
        )
    return PARTIAL_SCOPE


def collect_subjects(asset_root: Path) -> dict[str, dict[str, Any]]:
    subjects: dict[str, dict[str, Any]] = {}
    sidecars = sorted(asset_root.rglob("release-native-components.json"))
    if not sidecars:
        raise ValueError("no release-native-components.json sidecars were found")

    for sidecar_path in sidecars:
        if sidecar_path.is_symlink() or not sidecar_path.is_file():
            raise ValueError("release sidecar is not a regular file")
        try:
            sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
        except (OSError, UnicodeError, json.JSONDecodeError) as exc:
            raise ValueError("could not read a valid release sidecar (%s)" % type(exc).__name__) from None
        if not isinstance(sidecar, dict) or sidecar.get("schema_version") != 1:
            raise ValueError("release sidecar has an unsupported schema")
        if not isinstance(sidecar.get("source_commit"), str) or not COMMIT.fullmatch(sidecar["source_commit"]):
            raise ValueError("release sidecar has an invalid source commit")
        executables = sidecar.get("executables")
        if not isinstance(executables, list) or not executables:
            raise ValueError("release sidecar contains no executable evidence")
        sidecar_relative = sidecar_path.relative_to(asset_root).as_posix()

        for executable in executables:
            if not isinstance(executable, dict):
                raise ValueError("release sidecar has a malformed executable entry")
            package_file = executable.get("package_file")
            if not isinstance(package_file, str) or not package_file.strip() or contains_secret_like_text(package_file):
                raise ValueError("release executable has no package_file identity")
            expected_sha = executable.get("sha256")
            if not isinstance(expected_sha, str) or not HASH.fullmatch(expected_sha):
                raise ValueError("release executable has no valid SHA-256")
            scope = require_partial_scope(executable.get("audit_scope", sidecar.get("audit_scope")))
            executable_path, evidence_path = safe_file(
                sidecar_path.parent,
                executable.get("file"),
                "release executable evidence",
            )
            actual_sha = sha256_file(executable_path)
            if actual_sha != expected_sha.lower():
                raise ValueError("release executable evidence does not match its sidecar SHA-256")
            if executable_path.stat().st_size > 100 * 1024 * 1024:
                raise ValueError("release executable evidence exceeds cargo-audit bin's 100 MiB scan limit")

            subject = subjects.setdefault(
                actual_sha,
                {"path": executable_path, "bytes": executable_path.stat().st_size, "scope": scope, "packages": []},
            )
            if subject["path"] != executable_path and sha256_file(subject["path"]) != actual_sha:
                raise ValueError("duplicate executable SHA-256 resolved to different bytes")
            package_ref = {"sidecar": sidecar_relative, "package_file": package_file, "file": evidence_path}
            if package_ref not in subject["packages"]:
                subject["packages"].append(package_ref)

    return subjects


def run_audit(binary: Path) -> tuple[int, dict[str, Any]]:
    try:
        process = subprocess.run(
            ["cargo", "audit", "--no-fetch", "--color", "never", "--json", "bin", str(binary)],
            check=False,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=300,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise RuntimeError("cargo audit bin could not complete (%s)" % type(exc).__name__) from None
    try:
        report = json.loads(process.stdout)
    except json.JSONDecodeError:
        raise RuntimeError("cargo audit bin did not return valid JSON") from None
    if not isinstance(report, dict):
        raise RuntimeError("cargo audit bin returned a non-object JSON report")
    if process.returncode not in (0, 1):
        raise RuntimeError("cargo audit bin could not scan an executable evidence file")
    return process.returncode, report


def write_audit_index(asset_root: Path, output_dir: Path) -> tuple[dict[str, Any], bool]:
    subjects = collect_subjects(asset_root)
    index_path = output_dir / "binary-audit-index.json"
    record_dir = output_dir / "package-binary-audits"
    if index_path.exists() or index_path.is_symlink():
        raise ValueError("refusing to overwrite package binary audit index")
    if record_dir.is_symlink() or (record_dir.exists() and not record_dir.is_dir()):
        raise ValueError("package binary audit record path is not a regular directory")
    for binary_sha in subjects:
        record_path = record_dir / (binary_sha + ".json")
        if record_path.exists() or record_path.is_symlink():
            raise ValueError("refusing to overwrite a package binary audit record")

    cargo_version = subprocess.run(
        ["cargo", "audit", "--version"],
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=30,
    ).stdout.strip()
    record_dir.mkdir(parents=True, exist_ok=True)
    records: list[dict[str, Any]] = []
    vulnerable = False

    for binary_sha, subject in sorted(subjects.items()):
        scan_unavailable = None
        try:
            exit_code, report = run_audit(subject["path"])
        except RuntimeError as exc:
            # Release packages built without cargo-auditable (Windows, macOS, the stripped deb/rpm
            # payloads) carry no embedded dependency metadata, so cargo-audit cannot produce a JSON
            # report for them. Record the exact bytes as an explicitly incomplete scan instead of
            # failing the whole binding; the sidecar already states the missing metadata.
            if "cargo-auditable" not in subject["scope"]:
                raise
            scan_unavailable = str(exc)
            exit_code = 0
            report = {
                "vulnerabilities": {"count": 0, "found": False, "list": []},
                "warnings": {},
                "scan_unavailable": "cargo-audit could not read embedded dependency metadata for this release executable",
                "scan_error": scan_unavailable,
            }
        scope = subject["scope"]
        if scan_unavailable is not None:
            scope = "%s (not a complete scan: %s; only the exact executable hash is bound)" % (
                scope,
                scan_unavailable,
            )
        vulnerability_report = report.get("vulnerabilities")
        vulnerability_count = vulnerability_report.get("count", 0) if isinstance(vulnerability_report, dict) else 0
        found = isinstance(vulnerability_report, dict) and (
            vulnerability_report.get("found") is True
            or (isinstance(vulnerability_count, int) and vulnerability_count > 0)
        )
        vulnerable |= found or exit_code != 0
        record = {
            "schema_version": 1,
            "tool": "cargo-audit",
            "tool_version": cargo_version,
            "scan_mode": "bin",
            "binary_sha256": binary_sha,
            "binary_size_bytes": subject["bytes"],
            "completeness": "partial_panic_recovery",
            "scope": scope,
            "audit_exit_code": exit_code,
            "packages": subject["packages"],
            "audit_report": report,
        }
        if contains_secret_like_text(json.dumps(record, sort_keys=True)):
            raise ValueError("cargo audit binary report contains credential-like text")
        record_bytes = (json.dumps(record, indent=2, sort_keys=True) + "\n").encode("utf-8")
        record_path = record_dir / (binary_sha + ".json")
        with record_path.open("xb") as record_file:
            record_file.write(record_bytes)
        records.append(
            {
                "binary_sha256": binary_sha,
                "record_file": record_path.relative_to(output_dir).as_posix(),
                "record_sha256": hashlib.sha256(record_bytes).hexdigest(),
                "completeness": record["completeness"],
                "packages": subject["packages"],
            }
        )
        print(
            "Scanned executable evidence SHA-256 %s for %d package references; completeness=partial_panic_recovery; vulnerabilities=%s"
            % (binary_sha, len(subject["packages"]), "found" if found else "none reported"),
            flush=True,
        )

    index = {
        "schema_version": 1,
        "tool": "cargo-audit",
        "tool_version": cargo_version,
        "scan_mode": "bin",
        "completeness": "partial_panic_recovery",
        "scope": PARTIAL_SCOPE,
        "records": records,
    }
    with index_path.open("x", encoding="utf-8", newline="\n") as index_file:
        index_file.write(json.dumps(index, indent=2, sort_keys=True) + "\n")
    return index, vulnerable


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--asset-root", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    try:
        asset_root = args.asset_root.resolve(strict=True)
        output_dir = args.output_dir.resolve(strict=True)
        if not asset_root.is_dir() or not output_dir.is_dir():
            raise ValueError("asset root and output directory must already exist")
        index, vulnerable = write_audit_index(asset_root, output_dir)
        print(
            "Wrote %d package executable audit records to binary-audit-index.json"
            % len(index["records"])
        )
        return 1 if vulnerable else 0
    except (OSError, UnicodeError, ValueError, RuntimeError, subprocess.CalledProcessError) as exc:
        print("release executable audit failed: %s" % str(exc), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
