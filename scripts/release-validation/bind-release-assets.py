#!/usr/bin/env python3
"""Bind Cargo/native inventory and audit records to exact release package hashes."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
import uuid
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath
from typing import Any
from urllib.parse import parse_qsl, urlsplit


PACKAGE_SUFFIXES = {".msi", ".zip", ".dmg", ".deb", ".rpm", ".appimage", ".flatpak", ".bundle"}
TARGETS = {"windows-x64", "windows-arm64", "macos-universal", "linux-x64"}
HASH_PATTERN = re.compile(r"^[0-9a-fA-F]{64}$")
COMMIT_PATTERN = re.compile(r"^[0-9a-fA-F]{40}$")
TAG_PATTERN = re.compile(r"^v([0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.-]+)?)$")
SECRET_ASSIGNMENT = re.compile(r"(?i)(?:api[_-]?key|access[_-]?token|auth(?:orization)?|password|token)\s*[:=]\s*[^\s&#]+")
SECRET_QUERY_KEYS = {"api_key", "apikey", "token", "access_token", "authorization", "password"}


def contains_secret_like_text(value: str) -> bool:
    if SECRET_ASSIGNMENT.search(value):
        return True
    try:
        parts = urlsplit(value)
    except ValueError:
        return True
    if parts.username is not None or parts.password is not None:
        return True
    return any(key.casefold() in SECRET_QUERY_KEYS and val for key, val in parse_qsl(parts.query, keep_blank_values=False))


def safe_label(value: str) -> str:
    return "[REDACTED]" if contains_secret_like_text(value) else value


def is_safe_https_url(value: str) -> bool:
    try:
        parts = urlsplit(value)
    except ValueError:
        return False
    return (
        parts.scheme == "https"
        and bool(parts.hostname)
        and parts.username is None
        and parts.password is None
        and not contains_secret_like_text(value)
    )


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError("could not read valid JSON from %s (%s)" % (safe_label(path.name), type(exc).__name__)) from None
    if not isinstance(value, dict):
        raise ValueError("JSON root in %s must be an object" % safe_label(path.name))
    return value


def read_pinned_libmpv_manifest(path: Path) -> dict[str, dict[str, str]]:
    """Read this repository's intentionally flat platform TOML tables."""
    tables: dict[str, dict[str, str]] = {}
    current: str | None = None
    for raw_line in path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        section = re.fullmatch(r"\[platforms\.([a-z0-9-]+)\]", line)
        if section:
            current = section.group(1)
            tables[current] = {}
            continue
        if current is None or "=" not in line:
            continue
        key, value = (part.strip() for part in line.split("=", 1))
        if value.startswith('"') and value.endswith('"'):
            try:
                parsed = json.loads(value)
            except json.JSONDecodeError:
                raise ValueError("pinned libmpv manifest contains an unsupported string") from None
            if not isinstance(parsed, str):
                raise ValueError("pinned libmpv manifest value is not a string")
            tables[current][key] = parsed
    required = {"libmpv_version", "url", "sha256", "library_member", "license", "status"}
    for target in TARGETS:
        entry = tables.get(target)
        if not entry or not required.issubset(entry):
            raise ValueError("pinned libmpv manifest lacks required fields for %s" % target)
        if entry["status"] != "pinned" or not HASH_PATTERN.fullmatch(entry["sha256"]):
            raise ValueError("pinned libmpv manifest has an incomplete entry for %s" % target)
    return tables


def read_flatpak_archive_pins(path: Path) -> dict[str, dict[str, str]]:
    """Read archive URL/checksum pairs from the simple Flatpak module source blocks."""
    modules: dict[str, list[dict[str, str]]] = {}
    module: str | None = None
    current_archive: dict[str, str] | None = None
    for raw_line in path.read_text(encoding="utf-8").splitlines():
        name_match = re.match(r"^\s*-\s+name:\s*([A-Za-z0-9._+-]+)\s*$", raw_line)
        if name_match:
            module = name_match.group(1)
            current_archive = None
            modules.setdefault(module, [])
            continue
        archive_match = re.match(r"^\s*-\s+type:\s*archive\s*$", raw_line)
        if module is not None and archive_match:
            current_archive = {}
            modules[module].append(current_archive)
            continue
        if module is None or current_archive is None:
            continue
        scalar_match = re.match(r"^\s+(url|sha256):\s*(.*?)\s*$", raw_line)
        if scalar_match:
            key, value = scalar_match.groups()
            if value.startswith(('"', "'")) and value.endswith(('"', "'")):
                value = value[1:-1]
            current_archive[key] = value

    result: dict[str, dict[str, str]] = {}
    for name in ("libmpv", "libass", "uchardet"):
        candidates = [item for item in modules.get(name, []) if item.get("url") and item.get("sha256")]
        if len(candidates) != 1 or not HASH_PATTERN.fullmatch(candidates[0]["sha256"]):
            raise ValueError("Flatpak manifest must pin exactly one archive URL and SHA-256 for %s" % name)
        result[name] = candidates[0]
    return result


def version_from_archive_url(name: str, url: str) -> str | None:
    filename = PurePosixPath(urlsplit(url).path).name
    match = re.fullmatch(re.escape(name) + r"-([0-9][A-Za-z0-9.+~_-]*)\.tar\.(?:xz|gz|bz2)", filename)
    return match.group(1) if match else None


def safe_relative_file(root: Path, relative_value: Any, label: str) -> Path:
    if not isinstance(relative_value, str) or not relative_value:
        raise ValueError("%s must be a non-empty relative path" % label)
    if contains_secret_like_text(relative_value):
        raise ValueError("%s contains credential-like text" % label)
    if "\\" in relative_value or "\x00" in relative_value:
        raise ValueError("%s contains an unsafe path" % label)
    relative = PurePosixPath(relative_value)
    if relative.is_absolute() or any(part in ("", ".", "..") for part in relative.parts):
        raise ValueError("%s must stay beneath the artifact directory" % label)
    root_resolved = root.resolve()
    candidate = root.joinpath(*relative.parts)
    if os.path.commonpath((str(root_resolved), str(candidate.resolve()))) != str(root_resolved):
        raise ValueError("%s escapes the artifact directory" % label)
    if not candidate.is_file() or candidate.is_symlink():
        raise ValueError("%s does not identify a regular file" % label)
    return candidate


def load_binary_audit_index(
    index_path: Path | None,
    asset_root: Path,
    output_dir: Path,
    issues: list[str],
) -> tuple[dict[tuple[str, str, str], dict[str, Any]], dict[str, Any] | None]:
    if index_path is None:
        return {}, None
    try:
        index = read_json(index_path)
    except ValueError as exc:
        issues.append(str(exc))
        return {}, None
    if index.get("schema_version") != 1 or index.get("tool") != "cargo-audit" or index.get("scan_mode") != "bin":
        issues.append("package executable audit index has an unsupported schema or tool")
        return {}, None
    if index.get("completeness") != "partial_panic_recovery":
        issues.append("package executable audit index must disclose partial panic-string recovery")
    index_scope = index.get("scope")
    if not isinstance(index_scope, str) or "not a complete" not in index_scope:
        issues.append("package executable audit index does not disclose incomplete binary coverage")
    try:
        index_relative = index_path.resolve(strict=True).relative_to(output_dir.resolve(strict=True)).as_posix()
    except (OSError, ValueError):
        issues.append("package executable audit index must stay inside the SBOM output directory")
        return {}, None

    entries = index.get("records")
    if not isinstance(entries, list) or not entries:
        issues.append("package executable audit index contains no records")
        return {}, {
            "path": index_relative,
            "sha256": sha256_file(index_path),
            "completeness": index.get("completeness"),
            "scope": index_scope,
        }
    by_subject: dict[tuple[str, str, str], dict[str, Any]] = {}
    for entry in entries:
        if not isinstance(entry, dict):
            issues.append("package executable audit index contains a malformed record")
            continue
        binary_sha = entry.get("binary_sha256")
        record_file = entry.get("record_file")
        record_sha = entry.get("record_sha256")
        if not isinstance(binary_sha, str) or not HASH_PATTERN.fullmatch(binary_sha):
            issues.append("package executable audit index contains an invalid binary SHA-256")
            continue
        if not isinstance(record_sha, str) or not HASH_PATTERN.fullmatch(record_sha):
            issues.append("package executable audit index contains an invalid record SHA-256")
            continue
        try:
            record_path = safe_relative_file(output_dir, record_file, "package executable audit record")
        except ValueError as exc:
            issues.append(str(exc))
            continue
        actual_record_sha = sha256_file(record_path)
        if actual_record_sha != record_sha.lower():
            issues.append("package executable audit record SHA-256 does not match its index")
        try:
            record = read_json(record_path)
        except ValueError as exc:
            issues.append(str(exc))
            continue
        if (
            record.get("schema_version") != 1
            or record.get("tool") != "cargo-audit"
            or record.get("scan_mode") != "bin"
            or record.get("binary_sha256") != binary_sha.lower()
            or record.get("completeness") != "partial_panic_recovery"
        ):
            issues.append("package executable audit record does not match its index subject")
        scope = record.get("scope")
        if not isinstance(scope, str) or "not a complete" not in scope:
            issues.append("package executable audit record does not disclose incomplete coverage")
        audit_report = record.get("audit_report")
        if not isinstance(audit_report, dict):
            issues.append("package executable audit record has no cargo-audit JSON report")
            audit_report = {}
        vulnerability_report = audit_report.get("vulnerabilities")
        if isinstance(vulnerability_report, dict) and (
            vulnerability_report.get("found") is True
            or (
                isinstance(vulnerability_report.get("count"), int)
                and vulnerability_report.get("count", 0) > 0
            )
        ):
            issues.append("cargo audit bin reported vulnerabilities for executable SHA-256 %s" % binary_sha.lower())
        if record.get("audit_exit_code") != 0:
            issues.append("cargo audit bin did not pass for executable SHA-256 %s" % binary_sha.lower())

        package_refs = entry.get("packages")
        if not isinstance(package_refs, list) or not package_refs:
            issues.append("package executable audit record is not linked to a release package")
            continue
        for package_ref in package_refs:
            if not isinstance(package_ref, dict):
                issues.append("package executable audit record contains a malformed package reference")
                continue
            sidecar = package_ref.get("sidecar")
            package_file = package_ref.get("package_file")
            evidence_file = package_ref.get("file")
            if any(
                not isinstance(value, str) or not value.strip() or contains_secret_like_text(value)
                for value in (sidecar, package_file, evidence_file)
            ):
                issues.append("package executable audit record contains an invalid or credential-like package reference")
                continue
            try:
                sidecar_path = safe_relative_file(asset_root, sidecar, "package executable audit sidecar")
                safe_relative_file(sidecar_path.parent, evidence_file, "package executable audit evidence")
            except ValueError as exc:
                issues.append(str(exc))
                continue
            if sidecar_path.name != "release-native-components.json":
                issues.append("package executable audit record names a non-release sidecar")
                continue
            key = (sidecar_path.relative_to(asset_root).as_posix(), package_file, binary_sha.lower())
            if key in by_subject:
                issues.append("package executable audit index repeats an executable/package association")
                continue
            by_subject[key] = {
                "record_path": record_path,
                "record_file": record_path.relative_to(output_dir).as_posix(),
                "record_sha256": actual_record_sha,
                "binary_sha256": binary_sha.lower(),
                "completeness": "partial_panic_recovery",
                "scope": scope if isinstance(scope, str) else "",
                "evidence_file": evidence_file,
                "audit_report": audit_report,
            }
    return by_subject, {
        "path": index_relative,
        "sha256": sha256_file(index_path),
        "completeness": index.get("completeness"),
        "scope": index_scope,
    }


def package_suffix(path: Path) -> str | None:
    suffix = path.suffix.lower()
    return suffix if suffix in PACKAGE_SUFFIXES else None


def verify_sidecar_checksum_files(asset_root: Path) -> list[str]:
    errors: list[str] = []
    for sidecar in asset_root.rglob("*.sha256"):
        if sidecar.is_symlink() or not sidecar.is_file():
            errors.append("checksum sidecar is not a regular file: %s" % safe_label(sidecar.name))
            continue
        try:
            content = sidecar.read_text(encoding="ascii").strip().splitlines()
        except (OSError, UnicodeDecodeError):
            errors.append("checksum sidecar is not readable ASCII: %s" % safe_label(sidecar.name))
            continue
        if len(content) != 1:
            errors.append("checksum sidecar must contain exactly one record: %s" % safe_label(sidecar.name))
            continue
        match = re.fullmatch(r"([0-9a-fA-F]{64})\s+\*?(.+)", content[0].strip())
        if not match:
            errors.append("checksum sidecar has an invalid record: %s" % safe_label(sidecar.name))
            continue
        expected, name = match.groups()
        if "/" in name or "\\" in name or name in (".", ".."):
            errors.append("checksum sidecar target must be a sibling: %s" % safe_label(sidecar.name))
            continue
        target = sidecar.parent / name
        if not target.is_file() or sha256_file(target).lower() != expected.lower():
            errors.append("checksum sidecar does not match its sibling asset: %s" % safe_label(sidecar.name))
    return errors


def native_component_ref(target: str, package_sha: str, component: dict[str, Any]) -> str:
    material = "|".join(
        (
            target,
            package_sha,
            str(component.get("name", "")),
            str(component.get("packaged_sha256", "")),
        )
    )
    return "urn:kattri:native:" + hashlib.sha256(material.encode("utf-8")).hexdigest()[:32]


def validate_native_component(
    component: Any,
    target: str,
    package_file: str,
    manifest: dict[str, dict[str, str]],
    flatpak_pins: dict[str, dict[str, str]],
    issues: list[str],
    artifact_dir: Path,
) -> dict[str, Any] | None:
    if not isinstance(component, dict):
        issues.append("native component entry is not an object for %s" % package_file)
        return None
    name = component.get("name")
    version = component.get("version")
    source_url = component.get("source_url")
    archive_hash = component.get("source_archive_sha256")
    license_claim = component.get("license_claim")
    packaged_file = component.get("packaged_file")
    packaged_hash = component.get("packaged_sha256")
    evidence_file = component.get("evidence_file")
    declared_package = component.get("package_file")

    if not isinstance(name, str) or not name.strip():
        issues.append("native component has no name in %s" % package_file)
        return None
    if contains_secret_like_text(name):
        issues.append("native component name contains credential-like text")
        return None
    if declared_package != package_file:
        issues.append("native component package_file does not match its manifest package")
    if not isinstance(source_url, str) or not is_safe_https_url(source_url):
        issues.append("native component %s lacks a pinned HTTPS source URL" % name)
        source_url = None
    if isinstance(version, str) and contains_secret_like_text(version):
        issues.append("native component %s version contains credential-like text" % name)
        version = None
    if not isinstance(license_claim, str) or not license_claim.strip():
        issues.append("native component %s lacks a source-backed license claim" % name)
    elif contains_secret_like_text(license_claim):
        issues.append("native component %s license claim contains credential-like text" % name)
        license_claim = None
    if not isinstance(packaged_file, str) or not packaged_file.strip():
        issues.append("native component %s lacks a packaged file identity" % name)
    elif contains_secret_like_text(packaged_file):
        issues.append("native component %s packaged file identity contains credential-like text" % name)
        packaged_file = None
    if not isinstance(packaged_hash, str) or not HASH_PATTERN.fullmatch(packaged_hash):
        issues.append("native component %s lacks a valid packaged-file SHA-256" % name)
    if not isinstance(evidence_file, str) or not evidence_file:
        issues.append("native component %s lacks a byte-for-byte evidence file" % name)
    else:
        try:
            evidence_path = safe_relative_file(artifact_dir, evidence_file, "native component evidence file")
            if isinstance(packaged_hash, str) and HASH_PATTERN.fullmatch(packaged_hash):
                if sha256_file(evidence_path).lower() != packaged_hash.lower():
                    issues.append("native component evidence file SHA-256 mismatch for %s" % name)
        except ValueError as exc:
            issues.append(str(exc))
    if archive_hash is not None and (not isinstance(archive_hash, str) or not HASH_PATTERN.fullmatch(archive_hash)):
        issues.append("native component %s has an invalid source archive SHA-256" % name)
    if version is not None and not isinstance(version, str):
        issues.append("native component %s version must be a source-provided string or null" % name)
    elif isinstance(version, str) and not version.strip():
        issues.append("native component %s version must be a non-empty source-provided string or null" % name)

    if name.casefold() == "libmpv":
        pinned = manifest[target]
        if version != pinned["libmpv_version"]:
            issues.append("libmpv version claim differs from packaging/libmpv/manifest.toml for %s" % target)
        if source_url != pinned["url"]:
            issues.append("libmpv source URL differs from packaging/libmpv/manifest.toml for %s" % target)
        if archive_hash != pinned["sha256"]:
            issues.append("libmpv source archive hash differs from packaging/libmpv/manifest.toml for %s" % target)
        if license_claim != pinned["license"]:
            issues.append("libmpv license claim differs from packaging/libmpv/manifest.toml for %s" % target)
        if isinstance(packaged_file, str) and Path(packaged_file).name != Path(pinned["library_member"]).name:
            issues.append("libmpv packaged file differs from the pinned library member for %s" % target)

    if package_file.casefold().endswith(".flatpak") and name in ("libass", "uchardet"):
        pinned = flatpak_pins.get(name)
        if pinned is None:
            issues.append("Flatpak manifest does not provide a pinned archive for %s" % name)
        else:
            if source_url != pinned["url"]:
                issues.append("Flatpak %s source URL differs from packaging/flatpak manifest" % name)
            if archive_hash != pinned["sha256"]:
                issues.append("Flatpak %s archive checksum differs from packaging/flatpak manifest" % name)
            expected_version = version_from_archive_url(name, pinned["url"])
            if expected_version is None or version != expected_version:
                issues.append("Flatpak %s version differs from its pinned archive filename" % name)

    license_files = component.get("license_files")
    if not isinstance(license_files, list) or not license_files:
        issues.append("native component %s has no packaged license-file records" % name)
        license_files = []
    normalized_license_files: list[dict[str, str]] = []
    for license_file in license_files:
        if not isinstance(license_file, dict):
            issues.append("native component %s has a malformed license-file record" % name)
            continue
        file_path = license_file.get("path")
        file_hash = license_file.get("sha256")
        evidence_file_path = license_file.get("evidence_file")
        if not isinstance(file_path, str) or not file_path.strip() or not isinstance(file_hash, str) or not HASH_PATTERN.fullmatch(file_hash) or not isinstance(evidence_file_path, str) or not evidence_file_path:
            issues.append("native component %s has an invalid license-file path or hash" % name)
            continue
        if contains_secret_like_text(file_path):
            issues.append("native component %s license-file path contains credential-like text" % name)
            continue
        try:
            evidence_path = safe_relative_file(artifact_dir, evidence_file_path, "license evidence file")
            if sha256_file(evidence_path).lower() != file_hash.lower():
                issues.append("license evidence file SHA-256 mismatch for %s" % name)
        except ValueError as exc:
            issues.append(str(exc))
            continue
        normalized_license_files.append(
            {"path": file_path, "sha256": file_hash.lower(), "evidence_file": evidence_file_path}
        )

    build_info_path = component.get("build_info_path")
    build_info_hash = component.get("build_info_sha256")
    build_info_evidence_file = component.get("build_info_evidence_file")
    if (build_info_path is None) != (build_info_hash is None):
        issues.append("native component %s has incomplete build-info path/hash metadata" % name)
    if build_info_hash is not None and (not isinstance(build_info_hash, str) or not HASH_PATTERN.fullmatch(build_info_hash)):
        issues.append("native component %s has invalid build-info SHA-256" % name)
    if build_info_path is not None and build_info_hash is not None:
        if not isinstance(build_info_path, str) or contains_secret_like_text(build_info_path):
            issues.append("native component %s build-info path is invalid or credential-like" % name)
            build_info_path = None
        if not isinstance(build_info_evidence_file, str) or not build_info_evidence_file:
            issues.append("native component %s lacks a build-info evidence file" % name)
        else:
            try:
                evidence_path = safe_relative_file(artifact_dir, build_info_evidence_file, "build-info evidence file")
                if sha256_file(evidence_path).lower() != build_info_hash.lower():
                    issues.append("native component build-info evidence SHA-256 mismatch for %s" % name)
            except ValueError as exc:
                issues.append(str(exc))
    elif build_info_evidence_file is not None:
        issues.append("native component %s has build-info evidence without build-info metadata" % name)

    return {
        "name": name,
        "version": version,
        "source_url": source_url,
        "source_archive_sha256": archive_hash.lower() if isinstance(archive_hash, str) and HASH_PATTERN.fullmatch(archive_hash) else None,
        "license_claim": license_claim,
        "packaged_file": packaged_file,
        "packaged_sha256": packaged_hash.lower() if isinstance(packaged_hash, str) and HASH_PATTERN.fullmatch(packaged_hash) else None,
        "evidence_file": safe_label(evidence_file) if isinstance(evidence_file, str) else None,
        "license_files": normalized_license_files,
        "build_info_path": build_info_path,
        "build_info_sha256": build_info_hash.lower() if isinstance(build_info_hash, str) and HASH_PATTERN.fullmatch(build_info_hash) else None,
        "build_info_evidence_file": safe_label(build_info_evidence_file) if isinstance(build_info_evidence_file, str) else None,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--asset-root", type=Path, required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--release-tag", required=True)
    parser.add_argument("--cargo-lock", type=Path, required=True)
    parser.add_argument("--rust-sbom", type=Path, required=True)
    parser.add_argument("--binary-audit", type=Path, required=True)
    parser.add_argument("--audited-binary", type=Path, required=True)
    parser.add_argument("--binary-audit-index", type=Path)
    parser.add_argument("--libmpv-manifest", type=Path, required=True)
    parser.add_argument("--flatpak-manifest", type=Path, required=True)
    parser.add_argument("--collection", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    return _bind(args)


def build_binding(**kwargs: Any) -> int:
    """Testable entry point used by offline fixtures."""
    return _bind(argparse.Namespace(**kwargs))


def _bind(args: argparse.Namespace) -> int:

    issues: list[str] = []
    asset_root = args.asset_root.resolve()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    binary_audit_index_path = getattr(args, "binary_audit_index", None)
    binary_audit_entries, binary_audit_index_record = load_binary_audit_index(
        binary_audit_index_path,
        asset_root,
        args.output_dir,
        issues,
    )
    consumed_binary_audit_entries: set[tuple[str, str, str]] = set()
    release_version_match = TAG_PATTERN.fullmatch(args.release_tag)
    release_version = release_version_match.group(1) if release_version_match else None

    if not COMMIT_PATTERN.fullmatch(args.source_commit):
        issues.append("source commit must be a full 40-character SHA")
    if release_version is None:
        issues.append("release tag must be vX.Y.Z")

    try:
        manifest = read_pinned_libmpv_manifest(args.libmpv_manifest)
    except (OSError, ValueError) as exc:
        manifest = {}
        issues.append("could not validate pinned libmpv manifest (%s)" % type(exc).__name__)

    try:
        flatpak_pins = read_flatpak_archive_pins(args.flatpak_manifest)
    except (OSError, ValueError) as exc:
        flatpak_pins = {}
        issues.append("could not validate pinned Flatpak native archive sources (%s)" % type(exc).__name__)

    try:
        rust_sbom = read_json(args.rust_sbom)
        if rust_sbom.get("bomFormat") != "CycloneDX" or not isinstance(rust_sbom.get("components"), list):
            issues.append("Rust dependency SBOM is not a CycloneDX BOM with components")
    except ValueError as exc:
        rust_sbom = {}
        issues.append(str(exc))

    try:
        audit_record = read_json(args.binary_audit)
    except ValueError as exc:
        audit_record = {}
        issues.append(str(exc))

    for label, path in (
        ("Cargo.lock", args.cargo_lock),
        ("audited binary", args.audited_binary),
    ):
        if not path.is_file():
            issues.append("%s is missing" % label)

    try:
        collection = read_json(args.collection)
        if collection.get("source_commit") != args.source_commit:
            issues.append("artifact collection commit does not match the tag commit")
        if collection.get("release_tag") != args.release_tag:
            issues.append("artifact collection tag does not match the requested release tag")
    except ValueError as exc:
        collection = {}
        issues.append(str(exc))

    sidecars = sorted(asset_root.rglob("release-native-components.json"))
    if not sidecars:
        issues.append("release workflow artifacts contain no release-native-components.json sidecars")

    package_assets: dict[str, dict[str, Any]] = {}
    native_components: list[dict[str, Any]] = []
    disclosed_gaps: list[dict[str, str]] = []
    audit_gaps: list[dict[str, str]] = []
    sidecar_digests: list[dict[str, str]] = []

    for sidecar_path in sidecars:
        if sidecar_path.is_symlink() or not sidecar_path.is_file():
            issues.append("native inventory sidecar is not a regular file")
            continue
        try:
            sidecar = read_json(sidecar_path)
        except ValueError as exc:
            issues.append(str(exc))
            continue
        sidecar_digests.append(
            {
                "path": sidecar_path.relative_to(asset_root).as_posix(),
                "sha256": sha256_file(sidecar_path),
            }
        )
        target = sidecar.get("target")
        if sidecar.get("schema_version") != 1:
            issues.append("native inventory sidecar has an unsupported schema version")
        if sidecar.get("source_commit") != args.source_commit:
            issues.append("native inventory sidecar commit does not match the release commit")
        if target not in TARGETS:
            issues.append("native inventory sidecar has an unknown platform target")
            continue
        if not isinstance(sidecar.get("inventory_gaps", []), list):
            issues.append("native inventory gaps must be a list")
        else:
            for gap in sidecar.get("inventory_gaps", []):
                if not isinstance(gap, str) or not gap.strip():
                    issues.append("native inventory sidecar contains an invalid gap record")
                    continue
                if contains_secret_like_text(gap):
                    issues.append("native inventory gap contains credential-like text")
                    gap = "[REDACTED UNSAFE INVENTORY GAP]"
                disclosed_gaps.append(
                    {
                        "target": target,
                        "sidecar": sidecar_path.relative_to(asset_root).as_posix(),
                        "detail": gap,
                    }
                )

        package_entries = sidecar.get("packages")
        if not isinstance(package_entries, list) or not package_entries:
            issues.append("native inventory sidecar lists no packages")
            continue

        validated_packages: dict[str, dict[str, Any]] = {}
        for package_entry in package_entries:
            if not isinstance(package_entry, dict):
                issues.append("package record is not an object in %s" % sidecar_path.name)
                continue
            package_file = package_entry.get("file")
            if isinstance(package_file, str) and contains_secret_like_text(package_file):
                issues.append("package file path contains credential-like text")
                continue
            try:
                package_path = safe_relative_file(sidecar_path.parent, package_file, "package file")
            except ValueError as exc:
                issues.append("%s: %s" % (sidecar_path.name, exc))
                continue
            suffix = package_suffix(package_path)
            if suffix is None:
                issues.append("package manifest references a non-package file: %s" % package_path.name)
                continue
            actual_sha = sha256_file(package_path)
            actual_size = package_path.stat().st_size
            expected_sha = package_entry.get("sha256")
            expected_size = package_entry.get("size_bytes")
            if not isinstance(expected_sha, str) or not HASH_PATTERN.fullmatch(expected_sha) or actual_sha != expected_sha.lower():
                issues.append("package SHA-256 does not match release-native-components.json: %s" % package_path.name)
            if not isinstance(expected_size, int) or actual_size != expected_size:
                issues.append("package size does not match release-native-components.json: %s" % package_path.name)
            relative_path = package_path.relative_to(asset_root).as_posix()
            if relative_path in package_assets:
                issues.append("package asset is listed in more than one sidecar: %s" % package_path.name)
                continue
            package_record = {
                "path": relative_path,
                "file": package_path.name,
                "target": target,
                "format": suffix.lstrip("."),
                "sha256": actual_sha,
                "size_bytes": actual_size,
                "member_content_verification": {
                    "status": "not_verified_by_binder",
                    "scope": "The package file and sidecar evidence are hashed separately; this binder does not extract the package to compare internal members.",
                },
                "native_component_refs": [],
                "package_executables": [],
                "sidecar": sidecar_path.relative_to(asset_root).as_posix(),
            }
            package_assets[relative_path] = package_record
            validated_packages[package_file] = package_record

        native_entries = sidecar.get("native_components")
        if not isinstance(native_entries, list):
            issues.append("native_components must be a list in %s" % sidecar_path.name)
            native_entries = []
        components_by_package: dict[str, int] = {}
        for entry in native_entries:
            package_file = entry.get("package_file") if isinstance(entry, dict) else None
            if not isinstance(package_file, str) or package_file not in validated_packages:
                issues.append("native component package_file must match a package in %s" % sidecar_path.name)
                continue
            package = validated_packages[package_file]
            component = validate_native_component(
                entry,
                target,
                package_file,
                manifest,
                flatpak_pins,
                issues,
                sidecar_path.parent,
            )
            if component is None:
                continue
            if not component.get("version"):
                disclosed_gaps.append(
                    {
                        "target": target,
                        "sidecar": sidecar_path.relative_to(asset_root).as_posix(),
                        "detail": "version is not stated for native component %s in package %s" % (component["name"], package_file),
                    }
                )
            if not component.get("source_archive_sha256"):
                disclosed_gaps.append(
                    {
                        "target": target,
                        "sidecar": sidecar_path.relative_to(asset_root).as_posix(),
                        "detail": "no source archive SHA-256 is stated for native component %s in package %s" % (component["name"], package_file),
                    }
                )
            if component["name"].casefold() == "libmpv":
                components_by_package[package_file] = components_by_package.get(package_file, 0) + 1
            component["target"] = target
            component["package_path"] = package["path"]
            component["package_sha256"] = package["sha256"]
            component["bom_ref"] = native_component_ref(target, package["sha256"], component)
            package["native_component_refs"].append(component["bom_ref"])
            native_components.append(component)

        for package_file in validated_packages:
            if components_by_package.get(package_file, 0) != 1:
                issues.append("each package must have exactly one pinned libmpv entry: %s" % package_file)

        executable_entries = sidecar.get("executables", [])
        if not isinstance(executable_entries, list):
            issues.append("executables must be a list in %s" % sidecar_path.name)
            executable_entries = []
        executable_counts: dict[str, int] = {}
        for executable in executable_entries:
            if not isinstance(executable, dict):
                issues.append("executable record is not an object in %s" % sidecar_path.name)
                continue
            package_file = executable.get("package_file")
            if package_file not in validated_packages:
                issues.append("executable package_file does not match a package in %s" % sidecar_path.name)
                continue
            executable_sha = executable.get("sha256")
            if not isinstance(executable_sha, str) or not HASH_PATTERN.fullmatch(executable_sha):
                issues.append("executable lacks a valid SHA-256 in %s" % sidecar_path.name)
                continue
            executable_file = executable.get("file")
            uploaded_binary_path = None
            if executable_file is not None:
                try:
                    uploaded_binary_path = safe_relative_file(sidecar_path.parent, executable_file, "release executable")
                    actual_executable_sha = sha256_file(uploaded_binary_path)
                    if actual_executable_sha != executable_sha.lower():
                        issues.append("release executable hash does not match its sidecar in %s" % sidecar_path.name)
                except ValueError as exc:
                    issues.append("%s: %s" % (sidecar_path.name, exc))
            audit_scope = executable.get("audit_scope", sidecar.get("audit_scope"))
            if not isinstance(audit_scope, str) or not audit_scope.strip():
                issues.append("executable needs an explicit audit_scope in %s" % sidecar_path.name)
            executable_record: dict[str, Any] = {
                "package_file": package_file,
                "sidecar_path": sidecar_path.relative_to(asset_root).as_posix(),
                "file": executable_file,
                "uploaded_path": uploaded_binary_path.relative_to(asset_root).as_posix() if uploaded_binary_path else None,
                "sha256": executable_sha.lower(),
                "audit_scope": audit_scope,
                "audit_record_path": None,
                "audit_record_sha256": None,
                "audit_binary_sha256": None,
                "audit_completeness": None,
                "audit_record_location": None,
            }
            audit_file = executable.get("cargo_audit_record_file")
            audit_file_sha = executable.get("cargo_audit_record_sha256")
            if audit_file is not None or audit_file_sha is not None:
                try:
                    audit_path = safe_relative_file(sidecar_path.parent, audit_file, "executable audit record")
                    audit_digest = sha256_file(audit_path)
                    if not isinstance(audit_file_sha, str) or audit_digest != audit_file_sha.lower():
                        issues.append("executable audit record hash mismatch in %s" % sidecar_path.name)
                    audit_binary_sha = executable.get("cargo_audit_binary_sha256")
                    if not isinstance(audit_binary_sha, str) or audit_binary_sha.lower() != executable_sha.lower():
                        issues.append("executable audit record is not bound to the listed binary SHA-256 in %s" % sidecar_path.name)
                    try:
                        read_json(audit_path)
                    except ValueError as exc:
                        issues.append("%s: %s" % (sidecar_path.name, exc))
                    executable_record["audit_record_path"] = audit_path.relative_to(asset_root).as_posix()
                    executable_record["audit_record_sha256"] = audit_digest
                    executable_record["audit_binary_sha256"] = audit_binary_sha.lower() if isinstance(audit_binary_sha, str) else None
                    executable_record["audit_record_location"] = "release-artifact"
                except ValueError as exc:
                    issues.append("%s: %s" % (sidecar_path.name, exc))

            audit_key = (executable_record["sidecar_path"], package_file, executable_sha.lower())
            indexed_audit = binary_audit_entries.get(audit_key)
            if indexed_audit is not None:
                if indexed_audit["evidence_file"] != executable_file:
                    issues.append("binary audit evidence path does not match its release sidecar executable")
                if executable_record["audit_record_sha256"] is not None:
                    issues.append("executable has both sidecar and SBOM-indexed audit records")
                else:
                    executable_record["audit_record_path"] = indexed_audit["record_file"]
                    executable_record["audit_record_sha256"] = indexed_audit["record_sha256"]
                    executable_record["audit_binary_sha256"] = indexed_audit["binary_sha256"]
                    executable_record["audit_completeness"] = indexed_audit["completeness"]
                    executable_record["audit_record_location"] = "sbom-output"
                    executable_record["audit_scope"] = indexed_audit["scope"]
                    consumed_binary_audit_entries.add(audit_key)
            elif binary_audit_index_path is not None:
                issues.append("package executable is missing from the binary audit index: %s" % package["file"])
            validated_packages[package_file]["package_executables"].append(executable_record)
            executable_counts[package_file] = executable_counts.get(package_file, 0) + 1

        for package_file in validated_packages:
            if executable_counts.get(package_file, 0) == 0:
                issues.append("package has no exact executable payload hash: %s" % package_file)

    if binary_audit_index_path is not None:
        if not binary_audit_index_path.is_file():
            issues.append("package executable audit index is missing")
        unused_audits = set(binary_audit_entries) - consumed_binary_audit_entries
        if unused_audits:
            issues.append("package executable audit index contains unmatched release executable entries")

    discovered_packages: dict[str, Path] = {}
    for path in asset_root.rglob("*"):
        if path.is_symlink() or not path.is_file() or package_suffix(path) is None:
            continue
        relative_path = path.relative_to(asset_root).as_posix()
        if relative_path in discovered_packages:
            issues.append("duplicate package path in collected artifacts")
            continue
        discovered_packages[relative_path] = path
    unlisted = sorted(set(discovered_packages) - set(package_assets))
    missing = sorted(set(package_assets) - set(discovered_packages))
    if unlisted:
        issues.append("release package files are not in a native manifest: %s" % ", ".join(safe_label(Path(p).name) for p in unlisted))
    if missing:
        issues.append("native manifests reference missing release packages")
    issues.extend(verify_sidecar_checksum_files(asset_root))

    if not package_assets:
        issues.append("no final release package files were found")

    package_member_gaps = [
        {
            "target": package["target"],
            "package": package["path"],
            "detail": "The binder does not extract this package to compare embedded native, executable, and license members with sidecar hashes.",
        }
        for package in package_assets.values()
    ]

    for package in package_assets.values():
        if not package["native_component_refs"]:
            issues.append("package has no native component inventory: %s" % package["file"])
        if not package["package_executables"]:
            issues.append("package has no executable hash record: %s" % package["file"])

    native_bom_components: list[dict[str, Any]] = []
    dependencies: list[dict[str, Any]] = []
    for package in sorted(package_assets.values(), key=lambda item: item["path"]):
        package_ref = "urn:kattri:package:" + package["sha256"]
        package_properties = [
            {"name": "kattri:target", "value": package["target"]},
            {"name": "kattri:package-path", "value": package["path"]},
        ]
        for executable in package["package_executables"]:
            package_properties.extend(
                [
                    {"name": "kattri:package-executable-sha256", "value": executable["sha256"]},
                    {
                        "name": "kattri:package-executable-audit-completeness",
                        "value": executable.get("audit_completeness")
                        or ("sidecar_record_unscoped" if executable.get("audit_record_sha256") else "not_run"),
                    },
                    {
                        "name": "kattri:package-executable-audit-record-sha256",
                        "value": executable.get("audit_record_sha256") or "",
                    },
                ]
            )
        native_bom_components.append(
            {
                "type": "application",
                "bom-ref": package_ref,
                "name": package["file"],
                "version": release_version,
                "hashes": [{"alg": "SHA-256", "content": package["sha256"]}],
                "properties": package_properties,
            }
        )
        dependencies.append({"ref": package_ref, "dependsOn": package["native_component_refs"]})

    for component in native_components:
        license_value = component.get("license_claim")
        native_bom_components.append(
            {
                "type": "library",
                "bom-ref": component["bom_ref"],
                "name": component["name"],
                **({"version": component["version"]} if component.get("version") else {}),
                "hashes": (
                    [{"alg": "SHA-256", "content": component["packaged_sha256"]}]
                    if component.get("packaged_sha256")
                    else []
                ),
                "licenses": [{"license": {"name": license_value}}] if license_value else [],
                "externalReferences": (
                    [{"type": "distribution", "url": component["source_url"]}]
                    if isinstance(component.get("source_url"), str)
                    else []
                ),
                "properties": [
                    {"name": "kattri:target", "value": component["target"]},
                    {"name": "kattri:package-path", "value": component["package_path"]},
                    {"name": "kattri:package-sha256", "value": component["package_sha256"]},
                    {"name": "kattri:packaged-file", "value": str(component.get("packaged_file", ""))},
                    {"name": "kattri:packaged-file-sha256", "value": str(component.get("packaged_sha256", ""))},
                    {"name": "kattri:evidence-file", "value": str(component.get("evidence_file", ""))},
                    {"name": "kattri:license-claim-source", "value": "package sidecar; libmpv cross-checked against packaging/libmpv/manifest.toml"},
                    {"name": "kattri:license-claim", "value": str(license_value or "")},
                    {"name": "kattri:source-archive-sha256", "value": str(component.get("source_archive_sha256") or "")},
                    *[
                        {"name": "kattri:license-file", "value": "%s sha256:%s evidence:%s" % (item["path"], item["sha256"], item["evidence_file"])}
                        for item in component["license_files"]
                    ],
                    *(
                        [{"name": "kattri:build-info-sha256", "value": component["build_info_sha256"]}]
                        if component.get("build_info_sha256")
                        else []
                    ),
                    *(
                        [{"name": "kattri:build-info-evidence-file", "value": component["build_info_evidence_file"]}]
                        if component.get("build_info_evidence_file")
                        else []
                    ),
                ],
            }
        )

    native_bom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.6",
        "serialNumber": "urn:uuid:" + str(uuid.uuid5(uuid.NAMESPACE_URL, args.source_commit + ":native:" + args.release_tag)),
        "version": 1,
        "metadata": {
            "timestamp": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
            "tools": {"components": [{"type": "application", "name": "kattri-release-validation", "version": "1"}]},
            "component": {"type": "application", "name": "Jellyfin Kattri native release components", "version": release_version or args.release_tag},
            "properties": [
                {"name": "kattri:source-commit", "value": args.source_commit.lower()},
                {"name": "kattri:release-tag", "value": args.release_tag},
                {"name": "kattri:inventory-gap-count", "value": str(len(disclosed_gaps))},
                {"name": "kattri:package-member-verification-gap-count", "value": str(len(package_member_gaps))},
            ],
        },
        "components": native_bom_components,
        "dependencies": dependencies,
    }

    supporting_assets: list[dict[str, Any]] = []
    for path in asset_root.rglob("*"):
        if path.is_symlink() or not path.is_file():
            continue
        relative = path.relative_to(asset_root).as_posix()
        if relative in package_assets or relative.endswith("release-native-components.json") or relative == "collection.json":
            continue
        if contains_secret_like_text(relative):
            issues.append("supporting artifact path contains credential-like text")
            relative = "[REDACTED]"
        supporting_assets.append({"path": relative, "sha256": sha256_file(path), "size_bytes": path.stat().st_size})

    cargo_lock_sha = sha256_file(args.cargo_lock) if args.cargo_lock.is_file() else None
    rust_sbom_sha = sha256_file(args.rust_sbom) if args.rust_sbom.is_file() else None
    audit_json_sha = sha256_file(args.binary_audit) if args.binary_audit.is_file() else None
    audit_binary_sha = sha256_file(args.audited_binary) if args.audited_binary.is_file() else None
    sidecar_sha_by_path = {item["path"]: item["sha256"] for item in sidecar_digests}

    for package in package_assets.values():
        package["sidecar_sha256"] = sidecar_sha_by_path.get(package["sidecar"])
        package["rust_sbom_sha256"] = rust_sbom_sha
        package["native_sbom_sha256"] = None
        package["cargo_audit_record_sha256"] = audit_json_sha
        package["cargo_audit_binary_sha256"] = audit_binary_sha
        package["direct_executable_audits"] = [
            item for item in package["package_executables"] if item.get("audit_record_sha256")
        ]
        audit_completeness = {
            item.get("audit_completeness")
            for item in package["direct_executable_audits"]
            if item.get("audit_completeness")
        }
        if audit_completeness == {"partial_panic_recovery"}:
            package["cargo_audit_relation"] = "package-executable-partial-binary-scan-attested"
        elif package["direct_executable_audits"]:
            package["cargo_audit_relation"] = "package-executable-audit-attested"
        elif any(item.get("sha256") == audit_binary_sha for item in package["package_executables"]):
            package["cargo_audit_relation"] = "direct executable hash match"
        else:
            package["cargo_audit_relation"] = "source-commit-only"
        for executable in package["package_executables"]:
            if not executable.get("audit_record_sha256"):
                audit_gaps.append(
                    {
                        "target": package["target"],
                        "sidecar": package["sidecar"],
                        "detail": "no cargo audit bin record for executable SHA-256 %s in package %s; only the separate source-matched SBOM build is audited"
                        % (executable["sha256"], package["file"]),
                    }
                )
            elif executable.get("audit_completeness") == "partial_panic_recovery":
                audit_gaps.append(
                    {
                        "target": package["target"],
                        "sidecar": package["sidecar"],
                        "detail": "cargo audit bin scanned the exact executable evidence SHA-256 %s for package %s, but no cargo-auditable metadata is embedded; the panic-string recovery scan is partial"
                        % (executable["sha256"], package["file"]),
                    }
                )

    native_bom_path = args.output_dir / "native-components.cdx.json"
    native_bom_bytes = (json.dumps(native_bom, indent=2, sort_keys=True) + "\n").encode("utf-8")
    native_bom_path.write_bytes(native_bom_bytes)
    native_bom_sha = hashlib.sha256(native_bom_bytes).hexdigest()
    for package in package_assets.values():
        package["native_sbom_sha256"] = native_bom_sha

    status = "verified_with_disclosed_gaps" if disclosed_gaps or audit_gaps or package_member_gaps else "verified"
    if issues:
        status = "incomplete"
    binding = {
        "schema_version": 1,
        "status": status,
        "release_tag": args.release_tag,
        "release_version": release_version,
        "source_commit": args.source_commit.lower(),
        "cargo_lock": {"path": "Cargo.lock", "sha256": cargo_lock_sha, "scope": "locked Rust crate dependency metadata"},
        "rust_dependency_sbom": {
            "file": args.rust_sbom.name,
            "sha256": rust_sbom_sha,
            "scope": "Cargo.lock Rust dependencies only; native libraries are listed in native-components.cdx.json",
        },
        "native_component_sbom": {
            "file": native_bom_path.name,
            "sha256": native_bom_sha,
            "scope": "native component claims and byte evidence from platform package sidecars; pinned libmpv fields are cross-checked against packaging/libmpv/manifest.toml; package member bytes are not comprehensively re-extracted by this binder",
            "inventory_gaps": disclosed_gaps,
        },
        "package_member_verification": {
            "status": "not_comprehensively_verified" if package_member_gaps else "no_package_assets",
            "scope": "The package-container SHA-256 and sidecar payload evidence are verified separately; embedded member bytes are not comprehensively re-extracted by this binder.",
            "gaps": package_member_gaps,
        },
        "package_binary_audits": {
            "index_file": binary_audit_index_record["path"] if binary_audit_index_record else None,
            "index_sha256": binary_audit_index_record["sha256"] if binary_audit_index_record else None,
            "completeness": binary_audit_index_record["completeness"] if binary_audit_index_record else "not_run",
            "scope": binary_audit_index_record["scope"] if binary_audit_index_record else "no package-executable audit index was supplied",
            "gaps": audit_gaps,
        },
        "cargo_audit": {
            "record_file": args.binary_audit.name,
            "record_sha256": audit_json_sha,
            "audited_binary_file": args.audited_binary.name,
            "audited_binary_sha256": audit_binary_sha,
            "scope": "the separate cargo-auditable build named above; this is not represented as a scan of every packaged executable",
        },
        "release_assets": sorted(package_assets.values(), key=lambda item: item["path"]),
        "supporting_assets": sorted(supporting_assets, key=lambda item: item["path"]),
        "workflow_artifact_collection": {
            "file": args.collection.name,
            "sha256": sha256_file(args.collection) if args.collection.is_file() else None,
        },
        "sidecars": sidecar_digests,
        "issues": issues,
    }
    binding_path = args.output_dir / "release-asset-binding.json"
    binding_path.write_text(json.dumps(binding, indent=2, sort_keys=True) + "\n", encoding="utf-8")

    print(
        "SBOM binding %s: %d release packages, %d native component entries, %d native inventory gaps, %d package audit gaps, %d package member gaps, %d validation issues"
        % (status, len(package_assets), len(native_components), len(disclosed_gaps), len(audit_gaps), len(package_member_gaps), len(issues))
    )
    if issues:
        for issue in issues:
            print("ERROR: " + issue, file=sys.stderr)
        return 1
    if disclosed_gaps:
        for gap in disclosed_gaps:
            print("DISCLOSED GAP (%s): %s" % (gap["target"], gap["detail"]))
    if package_member_gaps:
        for gap in package_member_gaps:
            print("PACKAGE MEMBER SCOPE GAP (%s): %s" % (gap["package"], gap["detail"]))
    if audit_gaps:
        for gap in audit_gaps:
            print("PACKAGE BINARY AUDIT GAP (%s): %s" % (gap["target"], gap["detail"]))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
