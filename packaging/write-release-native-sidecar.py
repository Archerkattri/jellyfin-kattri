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
from pathlib import Path, PurePosixPath
from typing import Any


HASH = re.compile(r"^[0-9a-fA-F]{64}$")
COMMIT = re.compile(r"^[0-9a-fA-F]{40}$")
TOML_STRING = re.compile(r'^([a-z0-9_]+)\s*=\s*"([^"]*)"$')
PACKAGE_MEMBER_ROLES = {"executable", "native", "license"}
WINDOWS_RESERVED_MEMBER_NAMES = {
    "aux",
    "con",
    "conin$",
    "conout$",
    "nul",
    "prn",
    *("com%d" % index for index in range(1, 10)),
    *("lpt%d" % index for index in range(1, 10)),
}
WINDOWS_INVALID_MEMBER_CHARS = set('<>:"|?*')


def artifact_file(root: Path, relative: Any, label: str) -> tuple[Path, str]:
    if not isinstance(relative, str) or not relative.strip():
        raise ValueError(f"{label} path is empty")
    candidate = Path(relative)
    if candidate.is_absolute() or any(part in ("", ".", "..") for part in candidate.parts):
        raise ValueError(f"{label} path must be a safe relative file: {relative!r}")
    source_path = root / candidate
    if source_path.is_symlink():
        raise ValueError(f"{label} must not be a symlink: {relative!r}")
    resolved = source_path.resolve(strict=True)
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


def package_member_path(value: Any) -> str:
    path = require_string(value, "package member path")
    parts = path.split("/")
    if (
        "\\" in path
        or "\x00" in path
        or ":" in path
        or any(ord(char) < 32 for char in path)
        or len(path) > 4096
        or path.startswith("/")
        or any(part in ("", ".", "..") for part in parts)
        or PurePosixPath(path).is_absolute()
    ):
        raise ValueError("package member path must be a safe relative POSIX path")
    for part in parts:
        if len(part) > 255 or part.endswith((".", " ")) or any(
            ord(char) < 32 or char in WINDOWS_INVALID_MEMBER_CHARS for char in part
        ):
            raise ValueError("package member path contains a Windows-invalid segment")
        device_name = part.split(".", 1)[0].rstrip(" .").casefold()
        if device_name in WINDOWS_RESERVED_MEMBER_NAMES:
            raise ValueError("package member path contains a reserved Windows device name")
    return path


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
        if executable.get("packaged_file") is not None:
            executable["packaged_file"] = package_member_path(executable["packaged_file"])
        require_string(executable.get("audit_scope"), "executable audit_scope")
        executables.append(executable)

    package_member_entries = spec.get("package_members", [])
    if not isinstance(package_member_entries, list) or len(package_member_entries) > 4096:
        raise ValueError("package_members must be a list containing at most 4096 records")
    package_members: list[dict[str, str]] = []
    member_paths: set[tuple[str, str]] = set()
    for original in package_member_entries:
        if not isinstance(original, dict):
            raise ValueError("each package member must be an object")
        package_file = require_string(original.get("package_file"), "package member package_file")
        if package_file not in package_paths:
            raise ValueError("package member names an unknown package: " + package_file)
        if Path(package_file).suffix.casefold() not in {".zip", ".msi"}:
            raise ValueError("member inventories are supported only for ZIP and MSI packages")
        path = package_member_path(original.get("path"))
        role = require_string(original.get("role"), "package member role")
        if role not in PACKAGE_MEMBER_ROLES:
            raise ValueError("package member role must be executable, native, or license")
        evidence_path, evidence_file = artifact_file(root, original.get("evidence_file"), "package member evidence")
        member_key = (package_file.casefold(), path.casefold())
        if member_key in member_paths:
            raise ValueError("duplicate package member path: " + path)
        member_paths.add(member_key)
        package_members.append(
            {
                "package_file": package_file,
                "path": path,
                "role": role,
                "evidence_file": evidence_file,
                "sha256": sha256_file(evidence_path),
                "size_bytes": evidence_path.stat().st_size,
            }
        )

    members_by_package: dict[str, list[dict[str, str]]] = {}
    for member in package_members:
        members_by_package.setdefault(member["package_file"], []).append(member)
    expected_by_package: dict[str, set[tuple[str, str, str, str]]] = {}
    for executable in executables:
        expected_by_package.setdefault(executable["package_file"], set()).add(
            ("executable", executable.get("packaged_file", ""), executable["file"], executable["sha256"])
        )
    for component in native_components:
        expected_by_package.setdefault(component["package_file"], set()).add(
            ("native", component["packaged_file"], component["evidence_file"], component["packaged_sha256"])
        )
        for license_file in component["license_files"]:
            expected_by_package.setdefault(component["package_file"], set()).add(
                ("license", license_file["path"], license_file["evidence_file"], license_file["sha256"])
            )
    for package_file, (_, normalized_package_file) in package_paths.items():
        if Path(normalized_package_file).suffix.casefold() not in {".zip", ".msi"}:
            continue
        actual = {
            (member["role"], member["path"], member["evidence_file"], member["sha256"])
            for member in members_by_package.get(package_file, [])
        }
        missing = expected_by_package.get(package_file, set()) - actual
        if missing:
            raise ValueError("package member manifest does not cover all executable, native, and license claims: " + normalized_package_file)
        executable_claims = {
            (member["path"], member["evidence_file"], member["sha256"])
            for member in members_by_package.get(package_file, [])
            if member["role"] == "executable"
        }
        expected_executables = {
            (executable.get("packaged_file", ""), executable["file"], executable["sha256"])
            for executable in executables
            if executable["package_file"] == package_file
        }
        if executable_claims != expected_executables:
            raise ValueError("package executable member claims do not match the executable evidence: " + normalized_package_file)
        native_claims = {
            (member["path"], member["evidence_file"], member["sha256"])
            for member in members_by_package.get(package_file, [])
            if member["role"] == "native"
        }
        expected_native = {
            (component["packaged_file"], component["evidence_file"], component["packaged_sha256"])
            for component in native_components
            if component["package_file"] == package_file
        }
        if native_claims != expected_native:
            raise ValueError("package native member claims do not match native component evidence: " + normalized_package_file)

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
        "package_members": package_members,
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
