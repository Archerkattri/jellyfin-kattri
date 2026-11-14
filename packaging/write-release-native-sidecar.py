#!/usr/bin/env python3
"""Hash package and native payload files into a release sidecar.

The platform workflow supplies source-backed metadata and artifact-relative evidence paths.
This helper measures bytes from the files produced by that workflow; it does not infer
versions, licenses, source URLs, or package contents.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any


HASH = re.compile(r"^[0-9a-fA-F]{64}$")
COMMIT = re.compile(r"^[0-9a-fA-F]{40}$")
TOML_STRING = re.compile(r'^([a-z0-9_]+)\s*=\s*"([^"]*)"$')


def artifact_file(root: Path, relative: Any, label: str) -> tuple[Path, str]:
    if not isinstance(relative, str) or not relative.strip():
        raise ValueError(f"{label} path is empty")
    candidate = Path(relative)
    if candidate.is_absolute() or any(part in ("", ".", "..") for part in candidate.parts):
        raise ValueError(f"{label} path must be a safe relative file: {relative!r}")
    resolved = (root / candidate).resolve(strict=True)
    if not resolved.is_relative_to(root):
        raise ValueError(f"{label} path escapes the artifact directory: {relative!r}")
    if not resolved.is_file():
        raise ValueError(f"{label} is not a file: {relative!r}")
    return resolved, candidate.as_posix()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def require_string(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{label} must be a nonempty string")
    return value


def read_libmpv_pin(manifest: Path, platform: str) -> dict[str, str]:
    section = f"[platforms.{platform}]"
    active = False
    values: dict[str, str] = {}
    for raw_line in manifest.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("[") and line.endswith("]"):
            active = line == section
            continue
        if active:
            match = TOML_STRING.fullmatch(line)
            if match:
                values[match.group(1)] = match.group(2)
    for key in ("status", "libmpv_version", "url", "sha256", "library_member", "license"):
        if not values.get(key):
            raise ValueError(f"libmpv manifest lacks {key} for {platform}")
    if values["status"] != "pinned" or not HASH.fullmatch(values["sha256"]):
        raise ValueError(f"libmpv manifest entry is not pinned for {platform}")
    return values


def build_sidecar(root: Path, spec: dict[str, Any], manifest: Path | None) -> dict[str, Any]:
    source_commit = require_string(spec.get("source_commit"), "source_commit")
    if not COMMIT.fullmatch(source_commit):
        raise ValueError("source_commit must be a 40-character Git SHA")
    target = require_string(spec.get("target"), "target")

    package_paths: dict[str, tuple[Path, str]] = {}
    packages: list[dict[str, Any]] = []
    package_entries = spec.get("packages")
    if not isinstance(package_entries, list) or not package_entries:
        raise ValueError("packages must be a nonempty list")
    for entry in package_entries:
        package_path, package_file = artifact_file(root, entry, "package")
        if package_file in package_paths:
            raise ValueError(f"duplicate package entry: {package_file}")
        package_paths[package_file] = (package_path, package_file)
        packages.append(
            {
                "file": package_file,
                "sha256": sha256_file(package_path),
                "size_bytes": package_path.stat().st_size,
            }
        )

    native_components: list[dict[str, Any]] = []
    for original in spec.get("native_components", []):
        if not isinstance(original, dict):
            raise ValueError("each native component must be an object")
        component = dict(original)
        package_file = require_string(component.get("package_file"), "native component package_file")
        if package_file not in package_paths:
            raise ValueError(f"native component names an unknown package: {package_file}")
        manifest_platform = component.pop("manifest_platform", None)
        if manifest_platform is not None:
            component_name = component.get("name")
            if not isinstance(component_name, str) or component_name.casefold() != "libmpv":
                raise ValueError("manifest_platform may only be used for the libmpv component")
            if manifest is None:
                raise ValueError("libmpv manifest metadata was requested without --libmpv-manifest")
            pin = read_libmpv_pin(manifest, require_string(manifest_platform, "manifest_platform"))
            component["version"] = pin["libmpv_version"]
            component["source_url"] = pin["url"]
            component["source_archive_sha256"] = pin["sha256"]
            component["license_claim"] = pin["license"]
        evidence_path, evidence_file = artifact_file(root, component.get("evidence_file"), "native component evidence")
        component["evidence_file"] = evidence_file
        component["packaged_sha256"] = sha256_file(evidence_path)

        license_entries = component.get("license_files")
        if not isinstance(license_entries, list) or not license_entries:
            raise ValueError(f"native component {component.get('name')!r} has no license files")
        measured_licenses: list[dict[str, str]] = []
        for license_entry in license_entries:
            if not isinstance(license_entry, dict):
                raise ValueError("license file entry must be an object")
            license_path, evidence_file = artifact_file(root, license_entry.get("evidence_file"), "license evidence")
            packaged_path = require_string(license_entry.get("path"), "license package path")
            measured_licenses.append(
                {
                    "path": packaged_path,
                    "sha256": sha256_file(license_path),
                    "evidence_file": evidence_file,
                }
            )
        component["license_files"] = measured_licenses

        build_info_evidence = component.pop("build_info_evidence_file", None)
        if build_info_evidence is not None:
            build_info_path, evidence_file = artifact_file(root, build_info_evidence, "build-info evidence")
            component["build_info_sha256"] = sha256_file(build_info_path)
            component["build_info_evidence_file"] = evidence_file
            require_string(component.get("build_info_path"), "build_info_path")
        elif component.get("build_info_path") is not None or component.get("build_info_sha256") is not None:
            raise ValueError("build_info_path and build_info_sha256 require build_info_evidence_file")

        native_components.append(component)

    executables: list[dict[str, Any]] = []
    executable_entries = spec.get("executables", [])
    if not isinstance(executable_entries, list):
        raise ValueError("executables must be a list")
    for original in executable_entries:
        if not isinstance(original, dict):
            raise ValueError("each executable must be an object")
        executable = dict(original)
        package_file = require_string(executable.get("package_file"), "executable package_file")
        if package_file not in package_paths:
            raise ValueError(f"executable names an unknown package: {package_file}")
        executable_path, executable_file = artifact_file(root, executable.get("file"), "executable evidence")
        executable["file"] = executable_file
        executable["sha256"] = sha256_file(executable_path)
        require_string(executable.get("audit_scope"), "executable audit_scope")
        executables.append(executable)

    gaps = spec.get("inventory_gaps", [])
    if not isinstance(gaps, list) or any(not isinstance(gap, str) or not gap.strip() for gap in gaps):
        raise ValueError("inventory_gaps must be a list of nonempty strings")

    sidecar: dict[str, Any] = {
        "schema_version": 1,
        "source_commit": source_commit,
        "target": target,
        "packages": packages,
        "native_components": native_components,
        "executables": executables,
        "inventory_gaps": gaps,
    }
    if spec.get("audit_scope") is not None:
        sidecar["audit_scope"] = require_string(spec.get("audit_scope"), "audit_scope")
    return sidecar


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact-root", type=Path, required=True)
    parser.add_argument("--spec", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--libmpv-manifest", type=Path)
    args = parser.parse_args()

    root = args.artifact_root.resolve(strict=True)
    output = (root / args.output).resolve()
    if not output.is_relative_to(root):
        raise SystemExit("error: sidecar output must stay under the artifact directory")
    try:
        spec = json.loads(args.spec.read_text(encoding="utf-8"))
        manifest = args.libmpv_manifest.resolve(strict=True) if args.libmpv_manifest else None
        sidecar = build_sidecar(root, spec, manifest)
        output.write_text(json.dumps(sidecar, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as exc:
        raise SystemExit(f"error: could not write native release sidecar: {exc}") from exc
    print(f"Wrote native release sidecar: {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
