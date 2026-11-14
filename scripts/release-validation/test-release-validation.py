#!/usr/bin/env python3
"""Focused offline checks for release artifact collection and SBOM binding."""

from __future__ import annotations

import hashlib
import importlib.util
import contextlib
import io
import json
import os
import stat
import struct
import tempfile
import urllib.error
import urllib.request
import unittest
import zipfile
from unittest import mock
from pathlib import Path
from typing import Any


SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parents[1]


def load_script(module_name: str, path: Path):
    spec = importlib.util.spec_from_file_location(module_name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError("could not load validation helper")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


collector = load_script("collect_release_artifacts", SCRIPT_DIR / "collect-release-artifacts.py")
binding = load_script("bind_release_assets", SCRIPT_DIR / "bind-release-assets.py")
audit_executables = load_script("audit_release_executables", SCRIPT_DIR / "audit-release-executables.py")


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class ReleaseValidationTests(unittest.TestCase):
    def _bind_quietly(self, **kwargs) -> int:
        kwargs.setdefault("flatpak_manifest", REPO_ROOT / "packaging/flatpak/org.kattri.JellyfinKattri.yml")
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return binding.build_binding(**kwargs)

    def test_collector_extracts_safe_artifact_files(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "artifact.zip"
            destination = root / "unpacked"
            destination.mkdir()
            with zipfile.ZipFile(archive, "w") as zipped:
                zipped.writestr("dist/release.txt", "offline fixture\n")

            count, unpacked = collector.extract_artifact(archive, destination, 1024)
            self.assertEqual(count, 1)
            self.assertEqual(unpacked, len(b"offline fixture\n"))
            self.assertEqual((destination / "dist" / "release.txt").read_text(), "offline fixture\n")

    def test_collector_rejects_path_traversal(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "bad.zip"
            destination = root / "unpacked"
            destination.mkdir()
            with zipfile.ZipFile(archive, "w") as zipped:
                zipped.writestr("../outside.txt", "must not escape")

            with self.assertRaises(RuntimeError):
                collector.extract_artifact(archive, destination, 1024)
            self.assertFalse((root / "outside.txt").exists())

    def test_collector_enforces_unpacked_size_limit(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "large.zip"
            destination = root / "unpacked"
            destination.mkdir()
            with zipfile.ZipFile(archive, "w") as zipped:
                zipped.writestr("dist/data.bin", b"x" * 128)

            with self.assertRaises(RuntimeError):
                collector.extract_artifact(archive, destination, 64)
            self.assertFalse((destination / "dist" / "data.bin").exists())

    def test_artifact_redirect_never_forwards_github_authorization(self) -> None:
        request = urllib.request.Request(
            "https://api.github.com/repos/example/project/actions/artifacts/1/zip",
            headers={"Authorization": "Bearer synthetic-test-token", "Accept": "application/vnd.github+json"},
        )
        handler = collector.SafeRedirectHandler()
        redirected = handler.redirect_request(
            request,
            None,
            302,
            "Found",
            None,
            "https://artifact-storage.example.invalid/signed-object",
        )
        self.assertIsNotNone(redirected)
        self.assertIsNone(redirected.get_header("Authorization"))
        with self.assertRaises(urllib.error.HTTPError):
            handler.redirect_request(request, None, 302, "Found", None, "http://artifact-storage.invalid/object")
        with self.assertRaises(ValueError):
            collector.github_request("https://example.invalid/api", "synthetic-test-token", "application/json")

    def test_collector_requires_the_downloaded_size_to_match_github_metadata(self) -> None:
        class FakeOpener:
            def __init__(self, payload: bytes) -> None:
                self.payload = payload

            def open(self, _request, timeout: int):
                self.assert_timeout = timeout
                return io.BytesIO(self.payload)

        payload = b"synthetic artifact bytes"
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "artifact.zip"
            with mock.patch.object(collector, "opener", return_value=FakeOpener(payload)):
                digest = collector.download_artifact_zip(
                    "example/project", 1, destination, "synthetic-token", len(payload)
                )
            self.assertEqual(digest, sha256(payload))
            self.assertEqual(destination.read_bytes(), payload)

        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "artifact.zip"
            with mock.patch.object(collector, "opener", return_value=FakeOpener(payload)):
                with self.assertRaisesRegex(RuntimeError, "size did not match GitHub metadata"):
                    collector.download_artifact_zip(
                        "example/project", 1, destination, "synthetic-token", len(payload) + 1
                    )

    def _write_binding_fixture(
        self,
        root: Path,
        bad_package_hash: bool = False,
        with_package_audit: bool = False,
        package_suffix: str = ".zip",
    ) -> tuple[Path, Path, Path]:
        asset_root = root / "assets"
        artifact_dir = asset_root / "release-windows-x64-artifact"
        artifact_dir.mkdir(parents=True)
        output_dir = root / "sbom"
        output_dir.mkdir()

        package_name = "JellyfinKattri-1.2.3-x64" + package_suffix
        package = artifact_dir / package_name
        library_bytes = b"synthetic native library fixture\n"
        license_bytes = b"synthetic license evidence fixture\n"
        project_license_bytes = b"synthetic project license fixture\n"
        exe_bytes = b"synthetic release executable fixture\n"
        (artifact_dir / "libmpv-2.dll").write_bytes(library_bytes)
        (artifact_dir / "LICENSE.LGPL").write_bytes(license_bytes)
        (artifact_dir / "LICENSE").write_bytes(project_license_bytes)
        (artifact_dir / "jellyfin-kattri.exe").write_bytes(exe_bytes)

        zip_prefix = Path(package_name).stem + "/"
        if package_suffix.casefold() == ".zip":
            package_members = [
                {
                    "package_file": package_name,
                    "path": zip_prefix + "jellyfin-kattri.exe",
                    "role": "executable",
                    "evidence_file": "jellyfin-kattri.exe",
                    "sha256": sha256(exe_bytes),
                    "size_bytes": len(exe_bytes),
                },
                {
                    "package_file": package_name,
                    "path": zip_prefix + "libmpv-2.dll",
                    "role": "native",
                    "evidence_file": "libmpv-2.dll",
                    "sha256": sha256(library_bytes),
                    "size_bytes": len(library_bytes),
                },
                {
                    "package_file": package_name,
                    "path": zip_prefix + "licenses/libmpv/LICENSE.LGPL",
                    "role": "license",
                    "evidence_file": "LICENSE.LGPL",
                    "sha256": sha256(license_bytes),
                    "size_bytes": len(license_bytes),
                },
                {
                    "package_file": package_name,
                    "path": zip_prefix + "LICENSE",
                    "role": "license",
                    "evidence_file": "LICENSE",
                    "sha256": sha256(project_license_bytes),
                    "size_bytes": len(project_license_bytes),
                },
            ]
            with zipfile.ZipFile(package, "w", compression=zipfile.ZIP_DEFLATED) as zipped:
                zipped.writestr(package_members[0]["path"], exe_bytes)
                zipped.writestr(package_members[1]["path"], library_bytes)
                zipped.writestr(package_members[2]["path"], license_bytes)
                zipped.writestr(package_members[3]["path"], project_license_bytes)
        else:
            package_members = []
            package.write_bytes(b"release package fixture; never a real installer\n")
        package_bytes = package.read_bytes()
        package_sha = sha256(package_bytes)
        (artifact_dir / (package_name + ".sha256")).write_text(
            "%s  %s\n" % (package_sha, package_name), encoding="ascii"
        )

        manifest = binding.read_pinned_libmpv_manifest(REPO_ROOT / "packaging/libmpv/manifest.toml")
        pinned = manifest["windows-x64"]
        executable = {
            "package_file": package_name,
            "file": "jellyfin-kattri.exe",
            "sha256": sha256(exe_bytes),
            "audit_scope": "No package-specific cargo audit record; source-matched build only.",
        }
        if package_suffix.casefold() == ".zip":
            executable["packaged_file"] = zip_prefix + "jellyfin-kattri.exe"
        if with_package_audit:
            package_audit_bytes = json.dumps({"vulnerabilities": {"found": 0}}).encode("utf-8")
            (artifact_dir / "cargo-audit-package.json").write_bytes(package_audit_bytes)
            executable.update(
                {
                    "cargo_audit_record_file": "cargo-audit-package.json",
                    "cargo_audit_record_sha256": sha256(package_audit_bytes),
                    "cargo_audit_binary_sha256": sha256(exe_bytes),
                }
            )
        sidecar = {
            "schema_version": 1,
            "source_commit": "a" * 40,
            "target": "windows-x64",
            "packages": [
                {
                    "file": package_name,
                    "sha256": ("0" * 64 if bad_package_hash else package_sha),
                    "size_bytes": len(package_bytes),
                }
            ],
            "native_components": [
                {
                    "name": "libmpv",
                    "version": pinned["libmpv_version"],
                    "source_url": pinned["url"],
                    "source_archive_sha256": pinned["sha256"],
                    "license_claim": pinned["license"],
                    "package_file": package_name,
                    "packaged_file": (
                        zip_prefix + "libmpv-2.dll"
                        if package_suffix.casefold() == ".zip"
                        else "Jellyfin Kattri/libmpv-2.dll"
                    ),
                    "packaged_sha256": sha256(library_bytes),
                    "evidence_file": "libmpv-2.dll",
                    "license_files": [
                        {
                            "path": (
                                zip_prefix + "licenses/libmpv/LICENSE.LGPL"
                                if package_suffix.casefold() == ".zip"
                                else "Jellyfin Kattri/LICENSE.LGPL"
                            ),
                            "sha256": sha256(license_bytes),
                            "evidence_file": "LICENSE.LGPL",
                        }
                    ],
                }
            ],
            "executables": [executable],
            "package_members": package_members,
            "inventory_gaps": ["The pinned Windows archive has no build-info file."],
        }
        sidecar_path = artifact_dir / "release-native-components.json"
        sidecar_path.write_text(json.dumps(sidecar, indent=2) + "\n", encoding="utf-8")

        cargo_lock = root / "Cargo.lock"
        cargo_lock.write_text("# lock fixture\n", encoding="utf-8")
        rust_sbom = root / "rust-dependencies.cdx.json"
        rust_sbom.write_text(
            json.dumps(
                {
                    "bomFormat": "CycloneDX",
                    "specVersion": "1.6",
                    "version": 1,
                    "components": [{"type": "library", "name": "lock-fixture", "version": "1.0.0"}],
                }
            ),
            encoding="utf-8",
        )
        audit_record = root / "source-build-audit.json"
        audit_record.write_text(json.dumps({"vulnerabilities": {"found": 0}}), encoding="utf-8")
        audited_binary = root / "source-build.exe"
        audited_binary.write_bytes(b"separate cargo-auditable source build fixture\n")
        collection = asset_root / "collection.json"
        collection.write_text(
            json.dumps({"release_tag": "v1.2.3", "source_commit": "a" * 40}),
            encoding="utf-8",
        )
        return asset_root, output_dir, sidecar_path

    def _rewrite_fixture_zip(
        self,
        package: Path,
        replace_member: tuple[str, bytes] | None = None,
        extra_name: str | None = None,
        extra_bytes: bytes = b"fixture member\n",
        extra_mode: int = 0,
        extra_compression: int = zipfile.ZIP_DEFLATED,
    ) -> None:
        with zipfile.ZipFile(package, "r") as source:
            entries = [(info.filename, source.read(info), info.external_attr) for info in source.infolist()]
        with zipfile.ZipFile(package, "w", compression=zipfile.ZIP_DEFLATED) as target:
            for name, data, external_attr in entries:
                if replace_member is not None and name == replace_member[0]:
                    data = replace_member[1]
                info = zipfile.ZipInfo(name)
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = external_attr
                target.writestr(info, data)
            if extra_name is not None:
                info = zipfile.ZipInfo(extra_name)
                info.compress_type = extra_compression
                info.external_attr = extra_mode
                target.writestr(info, extra_bytes)

    def _refresh_fixture_package_hashes(self, sidecar_path: Path) -> None:
        sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
        package_entry = sidecar["packages"][0]
        package_path = sidecar_path.parent / package_entry["file"]
        package_bytes = package_path.read_bytes()
        package_hash = sha256(package_bytes)
        package_entry["sha256"] = package_hash
        package_entry["size_bytes"] = len(package_bytes)
        sidecar_path.write_text(json.dumps(sidecar, indent=2) + "\n", encoding="utf-8")
        (sidecar_path.parent / (package_entry["file"] + ".sha256")).write_text(
            "%s  %s\n" % (package_hash, package_entry["file"]),
            encoding="ascii",
        )

    def _run_fixture_binding(self, root: Path, asset_root: Path, output_dir: Path) -> int:
        return self._bind_quietly(
            asset_root=asset_root,
            source_commit="a" * 40,
            release_tag="v1.2.3",
            cargo_lock=root / "Cargo.lock",
            rust_sbom=root / "rust-dependencies.cdx.json",
            binary_audit=root / "source-build-audit.json",
            audited_binary=root / "source-build.exe",
            libmpv_manifest=REPO_ROOT / "packaging/libmpv/manifest.toml",
            collection=asset_root / "collection.json",
            output_dir=output_dir,
        )

    def test_binder_links_real_package_hashes_and_preserves_claims(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, _ = self._write_binding_fixture(root)
            status = self._bind_quietly(
                asset_root=asset_root,
                source_commit="a" * 40,
                release_tag="v1.2.3",
                cargo_lock=root / "Cargo.lock",
                rust_sbom=root / "rust-dependencies.cdx.json",
                binary_audit=root / "source-build-audit.json",
                audited_binary=root / "source-build.exe",
                libmpv_manifest=REPO_ROOT / "packaging/libmpv/manifest.toml",
                collection=asset_root / "collection.json",
                output_dir=output_dir,
            )
            self.assertEqual(status, 0)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            self.assertEqual(report["status"], "verified_with_disclosed_gaps")
            self.assertEqual(len(report["release_assets"]), 1)
            package = report["release_assets"][0]
            package_file = asset_root / "release-windows-x64-artifact" / "JellyfinKattri-1.2.3-x64.zip"
            self.assertEqual(package["sha256"], sha256(package_file.read_bytes()))
            self.assertEqual(package["member_content_verification"]["status"], "verified_declared_members")
            self.assertEqual(package["member_content_verification"]["verified_member_count"], 4)
            self.assertEqual(package["member_content_verification"]["unverified_file_member_count"], 0)
            self.assertEqual(report["package_member_verification"]["status"], "verified")
            self.assertEqual(report["package_member_verification"]["gaps"], [])
            self.assertTrue(package["cargo_audit_record_sha256"])
            self.assertEqual(package["cargo_audit_relation"], "source-commit-only")
            native_bom = json.loads((output_dir / "native-components.cdx.json").read_text(encoding="utf-8"))
            libmpv = next(item for item in native_bom["components"] if item["name"] == "libmpv")
            manifest = binding.read_pinned_libmpv_manifest(REPO_ROOT / "packaging/libmpv/manifest.toml")
            self.assertEqual(libmpv["licenses"][0]["license"]["name"], manifest["windows-x64"]["license"])
            self.assertNotIn("expression", libmpv["licenses"][0]["license"])

    def test_binder_rejects_zip_member_bytes_that_differ_from_sidecar_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, sidecar_path = self._write_binding_fixture(root)
            sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
            package = sidecar_path.parent / sidecar["packages"][0]["file"]
            native_path = next(
                member["path"] for member in sidecar["package_members"] if member["role"] == "native"
            )
            with zipfile.ZipFile(package, "r") as zipped:
                original_native = zipped.read(native_path)
            self._rewrite_fixture_zip(package, replace_member=(native_path, b"X" * len(original_native)))
            self._refresh_fixture_package_hashes(sidecar_path)

            status = self._run_fixture_binding(root, asset_root, output_dir)
            self.assertEqual(status, 1)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            self.assertTrue(any("ZIP package member bytes do not match" in issue for issue in report["issues"]))

    def test_binder_rejects_zip_path_traversal(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, sidecar_path = self._write_binding_fixture(root)
            package_name = json.loads(sidecar_path.read_text(encoding="utf-8"))["packages"][0]["file"]
            package = sidecar_path.parent / package_name
            self._rewrite_fixture_zip(package, extra_name="../outside.txt")
            self._refresh_fixture_package_hashes(sidecar_path)

            status = self._run_fixture_binding(root, asset_root, output_dir)
            self.assertEqual(status, 1)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            self.assertTrue(any("package member verification failed" in issue for issue in report["issues"]))

    def test_binder_rejects_case_ambiguous_zip_members(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, sidecar_path = self._write_binding_fixture(root)
            sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
            package = sidecar_path.parent / sidecar["packages"][0]["file"]
            exe_path = next(
                member["path"] for member in sidecar["package_members"] if member["role"] == "executable"
            )
            self._rewrite_fixture_zip(package, extra_name=exe_path.upper())
            self._refresh_fixture_package_hashes(sidecar_path)

            status = self._run_fixture_binding(root, asset_root, output_dir)
            self.assertEqual(status, 1)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            self.assertTrue(any("duplicate or case-colliding" in issue for issue in report["issues"]))

    def test_binder_rejects_zip_symlink_members(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, sidecar_path = self._write_binding_fixture(root)
            package_name = json.loads(sidecar_path.read_text(encoding="utf-8"))["packages"][0]["file"]
            package = sidecar_path.parent / package_name
            self._rewrite_fixture_zip(
                package,
                extra_name="JellyfinKattri-1.2.3-x64/links/payload",
                extra_bytes=b"../jellyfin-kattri.exe",
                extra_mode=(stat.S_IFLNK | 0o777) << 16,
            )
            self._refresh_fixture_package_hashes(sidecar_path)

            status = self._run_fixture_binding(root, asset_root, output_dir)
            self.assertEqual(status, 1)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            self.assertTrue(any("link or reparse-point" in issue for issue in report["issues"]))

    def test_binder_rejects_zip_file_directory_path_collisions(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, sidecar_path = self._write_binding_fixture(root)
            sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
            package = sidecar_path.parent / sidecar["packages"][0]["file"]
            exe_path = next(
                member["path"] for member in sidecar["package_members"] if member["role"] == "executable"
            )
            self._rewrite_fixture_zip(package, extra_name=exe_path + "/child.txt")
            self._refresh_fixture_package_hashes(sidecar_path)

            status = self._run_fixture_binding(root, asset_root, output_dir)
            self.assertEqual(status, 1)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            self.assertTrue(any("file/directory prefix collision" in issue for issue in report["issues"]))

    def test_binder_rejects_windows_alias_zip_member_paths(self) -> None:
        for alias in ("CON.txt", "trailing-name."):
            with self.subTest(alias=alias), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                asset_root, output_dir, sidecar_path = self._write_binding_fixture(root)
                package_name = json.loads(sidecar_path.read_text(encoding="utf-8"))["packages"][0]["file"]
                package = sidecar_path.parent / package_name
                prefix = Path(package_name).stem + "/"
                self._rewrite_fixture_zip(package, extra_name=prefix + alias)
                self._refresh_fixture_package_hashes(sidecar_path)

                status = self._run_fixture_binding(root, asset_root, output_dir)
                self.assertEqual(status, 1)
                report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
                self.assertTrue(any("Windows" in issue for issue in report["issues"]))

    def test_binder_streams_unclaimed_zip_entries_for_integrity(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, sidecar_path = self._write_binding_fixture(root)
            package_name = json.loads(sidecar_path.read_text(encoding="utf-8"))["packages"][0]["file"]
            package = sidecar_path.parent / package_name
            unclaimed_path = Path(package_name).stem + "/README.txt"
            self._rewrite_fixture_zip(
                package,
                extra_name=unclaimed_path,
                extra_bytes=b"unclaimed but CRC-checked fixture\n",
                extra_compression=zipfile.ZIP_STORED,
            )
            with zipfile.ZipFile(package, "r") as archive:
                info = archive.getinfo(unclaimed_path)
            with package.open("r+b") as stream:
                stream.seek(info.header_offset + 26)
                name_length, extra_length = struct.unpack("<HH", stream.read(4))
                data_offset = info.header_offset + 30 + name_length + extra_length
                stream.seek(data_offset + 2)
                original_byte = stream.read(1)
                stream.seek(data_offset + 2)
                stream.write(bytes((original_byte[0] ^ 0xFF,)))
            self._refresh_fixture_package_hashes(sidecar_path)

            status = self._run_fixture_binding(root, asset_root, output_dir)
            self.assertEqual(status, 1)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            self.assertTrue(any("could not be safely streamed" in issue for issue in report["issues"]))

    def test_binder_rejects_zip_member_size_budget_excess(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, _ = self._write_binding_fixture(root)
            with mock.patch.object(binding, "MAX_PACKAGE_ZIP_UNCOMPRESSED_BYTES", 64):
                status = self._run_fixture_binding(root, asset_root, output_dir)
            self.assertEqual(status, 1)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            self.assertTrue(any("uncompressed total exceeds" in issue for issue in report["issues"]))

    def test_binder_rejects_zip_per_member_size_and_ratio_excess(self) -> None:
        cases = (
            ("MAX_PACKAGE_ZIP_MEMBER_BYTES", 8, "size is outside the verification limit"),
            ("MAX_PACKAGE_ZIP_COMPRESSION_RATIO", 0, "compression ratio exceeds the verification limit"),
        )
        for constant, limit, expected_issue in cases:
            with self.subTest(constant=constant), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                asset_root, output_dir, _ = self._write_binding_fixture(root)
                with mock.patch.object(binding, constant, limit):
                    status = self._run_fixture_binding(root, asset_root, output_dir)
                self.assertEqual(status, 1)
                report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
                self.assertTrue(any(expected_issue in issue for issue in report["issues"]))

    def test_binder_discloses_unclaimed_zip_files_as_partial(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, sidecar_path = self._write_binding_fixture(root)
            sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
            package = sidecar_path.parent / sidecar["packages"][0]["file"]
            unclaimed_path = Path(package.name).stem + "/README.txt"
            self._rewrite_fixture_zip(package, extra_name=unclaimed_path)
            self._refresh_fixture_package_hashes(sidecar_path)

            status = self._run_fixture_binding(root, asset_root, output_dir)
            self.assertEqual(status, 0)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            member_report = report["release_assets"][0]["member_content_verification"]
            self.assertEqual(member_report["verified_member_count"], 4)
            self.assertEqual(member_report["unverified_file_member_count"], 1)
            self.assertEqual(report["package_member_verification"]["status"], "partial")
            self.assertTrue(any("outside the declared" in gap["detail"] for gap in report["package_member_verification"]["gaps"]))

    def test_binder_rejects_package_hash_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, _ = self._write_binding_fixture(root, bad_package_hash=True)
            status = self._bind_quietly(
                asset_root=asset_root,
                source_commit="a" * 40,
                release_tag="v1.2.3",
                cargo_lock=root / "Cargo.lock",
                rust_sbom=root / "rust-dependencies.cdx.json",
                binary_audit=root / "source-build-audit.json",
                audited_binary=root / "source-build.exe",
                libmpv_manifest=REPO_ROOT / "packaging/libmpv/manifest.toml",
                collection=asset_root / "collection.json",
                output_dir=output_dir,
            )
            self.assertEqual(status, 1)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            self.assertEqual(report["status"], "incomplete")
            self.assertTrue(any("SHA-256 does not match" in item for item in report["issues"]))

    def test_binder_rejects_missing_license_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, _ = self._write_binding_fixture(root)
            (asset_root / "release-windows-x64-artifact" / "LICENSE.LGPL").unlink()
            status = self._bind_quietly(
                asset_root=asset_root,
                source_commit="a" * 40,
                release_tag="v1.2.3",
                cargo_lock=root / "Cargo.lock",
                rust_sbom=root / "rust-dependencies.cdx.json",
                binary_audit=root / "source-build-audit.json",
                audited_binary=root / "source-build.exe",
                libmpv_manifest=REPO_ROOT / "packaging/libmpv/manifest.toml",
                collection=asset_root / "collection.json",
                output_dir=output_dir,
            )
            self.assertEqual(status, 1)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            self.assertTrue(any("license evidence file" in item for item in report["issues"]))

    def test_binder_rejects_forged_pinned_libmpv_claim(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, sidecar_path = self._write_binding_fixture(root)
            sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
            sidecar["native_components"][0]["license_claim"] = "MIT"
            sidecar_path.write_text(json.dumps(sidecar), encoding="utf-8")
            status = self._bind_quietly(
                asset_root=asset_root,
                source_commit="a" * 40,
                release_tag="v1.2.3",
                cargo_lock=root / "Cargo.lock",
                rust_sbom=root / "rust-dependencies.cdx.json",
                binary_audit=root / "source-build-audit.json",
                audited_binary=root / "source-build.exe",
                libmpv_manifest=REPO_ROOT / "packaging/libmpv/manifest.toml",
                collection=asset_root / "collection.json",
                output_dir=output_dir,
            )
            self.assertEqual(status, 1)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            self.assertTrue(any("license claim differs" in item for item in report["issues"]))

    def test_binder_does_not_copy_credential_like_sidecar_text_to_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, sidecar_path = self._write_binding_fixture(root)
            sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
            synthetic_credential = "api" + "_key=synthetic-secret"
            sidecar["native_components"][0]["source_url"] = (
                "https://example.invalid/archive?" + synthetic_credential
            )
            sidecar["native_components"][0]["license_claim"] = "LGPL " + synthetic_credential
            sidecar_path.write_text(json.dumps(sidecar), encoding="utf-8")
            status = self._bind_quietly(
                asset_root=asset_root,
                source_commit="a" * 40,
                release_tag="v1.2.3",
                cargo_lock=root / "Cargo.lock",
                rust_sbom=root / "rust-dependencies.cdx.json",
                binary_audit=root / "source-build-audit.json",
                audited_binary=root / "source-build.exe",
                libmpv_manifest=REPO_ROOT / "packaging/libmpv/manifest.toml",
                collection=asset_root / "collection.json",
                output_dir=output_dir,
            )
            self.assertEqual(status, 1)
            evidence_text = "\n".join(
                path.read_text(encoding="utf-8")
                for path in output_dir.iterdir()
                if path.is_file() and path.suffix == ".json"
            )
            self.assertNotIn("synthetic-secret", evidence_text)
            self.assertTrue(any("credential-like" in item for item in json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))["issues"]))

    def test_binder_attaches_package_audit_to_exact_executable_hash(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, _ = self._write_binding_fixture(root, with_package_audit=True)
            status = self._bind_quietly(
                asset_root=asset_root,
                source_commit="a" * 40,
                release_tag="v1.2.3",
                cargo_lock=root / "Cargo.lock",
                rust_sbom=root / "rust-dependencies.cdx.json",
                binary_audit=root / "source-build-audit.json",
                audited_binary=root / "source-build.exe",
                libmpv_manifest=REPO_ROOT / "packaging/libmpv/manifest.toml",
                collection=asset_root / "collection.json",
                output_dir=output_dir,
            )
            self.assertEqual(status, 0)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            package = report["release_assets"][0]
            self.assertEqual(len(package["direct_executable_audits"]), 1)
            audit = package["direct_executable_audits"][0]
            self.assertEqual(audit["sha256"], sha256(b"synthetic release executable fixture\n"))
            self.assertTrue(audit["audit_record_sha256"])
            self.assertEqual(package["cargo_audit_relation"], "package-executable-audit-attested")

    def test_package_binary_scan_is_hash_bound_and_discloses_partial_recovery(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, sidecar_path = self._write_binding_fixture(root)
            sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
            sidecar["executables"][0]["audit_scope"] = (
                "This release job did not produce cargo-auditable metadata or a cargo audit bin record."
            )
            sidecar_path.write_text(json.dumps(sidecar), encoding="utf-8")

            audit_report = {
                "database": {"advisory-count": 1402},
                "lockfile": {"dependency-count": 1},
                "vulnerabilities": {"found": False, "count": 0, "list": []},
                "warnings": {},
            }

            def fake_run(command: list[str], **_kwargs: Any) -> mock.Mock:
                if command[-1] == "--version":
                    return mock.Mock(returncode=0, stdout="cargo-audit-audit 0.22.2\n", stderr="")
                self.assertEqual(command[:7], ["cargo", "audit", "--no-fetch", "--color", "never", "--json", "bin"])
                return mock.Mock(returncode=0, stdout=json.dumps(audit_report), stderr="")

            with mock.patch.object(audit_executables.subprocess, "run", side_effect=fake_run):
                with contextlib.redirect_stdout(io.StringIO()):
                    index, found_vulnerabilities = audit_executables.write_audit_index(asset_root, output_dir)
            self.assertFalse(found_vulnerabilities)
            self.assertEqual(index["completeness"], "partial_panic_recovery")
            self.assertEqual(len(index["records"]), 1)

            status = self._bind_quietly(
                asset_root=asset_root,
                source_commit="a" * 40,
                release_tag="v1.2.3",
                cargo_lock=root / "Cargo.lock",
                rust_sbom=root / "rust-dependencies.cdx.json",
                binary_audit=root / "source-build-audit.json",
                audited_binary=root / "source-build.exe",
                binary_audit_index=output_dir / "binary-audit-index.json",
                libmpv_manifest=REPO_ROOT / "packaging/libmpv/manifest.toml",
                collection=asset_root / "collection.json",
                output_dir=output_dir,
            )
            self.assertEqual(status, 0)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            package = report["release_assets"][0]
            self.assertEqual(package["cargo_audit_relation"], "package-executable-partial-binary-scan-attested")
            self.assertEqual(package["direct_executable_audits"][0]["audit_completeness"], "partial_panic_recovery")
            self.assertEqual(
                package["direct_executable_audits"][0]["sha256"],
                sha256(b"synthetic release executable fixture\n"),
            )
            self.assertEqual(report["package_binary_audits"]["completeness"], "partial_panic_recovery")
            self.assertTrue(any("panic-string recovery scan is partial" in gap["detail"] for gap in report["package_binary_audits"]["gaps"]))
            self.assertIn("separate cargo-auditable build", report["cargo_audit"]["scope"])
            native_bom = json.loads((output_dir / "native-components.cdx.json").read_text(encoding="utf-8"))
            package_component = next(
                item for item in native_bom["components"] if item["type"] == "application"
            )
            properties = {item["name"]: item["value"] for item in package_component["properties"]}
            self.assertEqual(properties["kattri:package-executable-sha256"], sha256(b"synthetic release executable fixture\n"))
            self.assertEqual(properties["kattri:package-executable-audit-completeness"], "partial_panic_recovery")
            self.assertTrue(properties["kattri:package-executable-audit-record-sha256"])

    def test_binary_audit_refuses_to_follow_executable_evidence_symlinks(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, sidecar_path = self._write_binding_fixture(root)
            artifact_dir = sidecar_path.parent
            try:
                (artifact_dir / "evidence-link.exe").symlink_to("jellyfin-kattri.exe")
            except (OSError, NotImplementedError) as exc:
                self.skipTest("symlink creation is unavailable: %s" % type(exc).__name__)
            sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
            sidecar["executables"][0]["file"] = "evidence-link.exe"
            sidecar["executables"][0]["audit_scope"] = (
                "This release job did not produce cargo-auditable metadata or a cargo audit bin record."
            )
            sidecar_path.write_text(json.dumps(sidecar), encoding="utf-8")

            with self.assertRaisesRegex(ValueError, "contains a symlink"):
                audit_executables.collect_subjects(asset_root)

    def test_binary_audit_refuses_to_overwrite_an_existing_index(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, sidecar_path = self._write_binding_fixture(root)
            sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
            sidecar["executables"][0]["audit_scope"] = (
                "This release job did not produce cargo-auditable metadata or a cargo audit bin record."
            )
            sidecar_path.write_text(json.dumps(sidecar), encoding="utf-8")
            index_path = output_dir / "binary-audit-index.json"
            index_path.write_text("preserve existing evidence\n", encoding="utf-8")

            with mock.patch.object(audit_executables.subprocess, "run") as run:
                with self.assertRaisesRegex(ValueError, "refusing to overwrite package binary audit index"):
                    audit_executables.write_audit_index(asset_root, output_dir)
                run.assert_not_called()

    def test_binder_rejects_a_package_binary_audit_index_subject_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, sidecar_path = self._write_binding_fixture(root)
            sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
            sidecar["executables"][0]["audit_scope"] = (
                "This release job did not produce cargo-auditable metadata or a cargo audit bin record."
            )
            sidecar_path.write_text(json.dumps(sidecar), encoding="utf-8")
            audit_report = {
                "database": {"advisory-count": 1402},
                "lockfile": {"dependency-count": 1},
                "vulnerabilities": {"found": False, "count": 0, "list": []},
                "warnings": {},
            }

            def fake_run(command: list[str], **_kwargs: Any) -> mock.Mock:
                if command[-1] == "--version":
                    return mock.Mock(returncode=0, stdout="cargo-audit-audit 0.22.2\n", stderr="")
                return mock.Mock(returncode=0, stdout=json.dumps(audit_report), stderr="")

            with mock.patch.object(audit_executables.subprocess, "run", side_effect=fake_run):
                with contextlib.redirect_stdout(io.StringIO()):
                    audit_executables.write_audit_index(asset_root, output_dir)
            index_path = output_dir / "binary-audit-index.json"
            index = json.loads(index_path.read_text(encoding="utf-8"))
            index["records"][0]["binary_sha256"] = "0" * 64
            index_path.write_text(json.dumps(index), encoding="utf-8")

            status = self._bind_quietly(
                asset_root=asset_root,
                source_commit="a" * 40,
                release_tag="v1.2.3",
                cargo_lock=root / "Cargo.lock",
                rust_sbom=root / "rust-dependencies.cdx.json",
                binary_audit=root / "source-build-audit.json",
                audited_binary=root / "source-build.exe",
                binary_audit_index=index_path,
                libmpv_manifest=REPO_ROOT / "packaging/libmpv/manifest.toml",
                collection=asset_root / "collection.json",
                output_dir=output_dir,
            )
            self.assertEqual(status, 1)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            self.assertEqual(report["status"], "incomplete")
            self.assertTrue(any("does not match its index subject" in issue for issue in report["issues"]))

    def test_unsupported_installer_format_remains_a_disclosed_member_gap(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, sidecar_path = self._write_binding_fixture(
                root,
                with_package_audit=True,
                package_suffix=".msi",
            )
            sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
            sidecar["inventory_gaps"] = []
            sidecar_path.write_text(json.dumps(sidecar), encoding="utf-8")
            status = self._bind_quietly(
                asset_root=asset_root,
                source_commit="a" * 40,
                release_tag="v1.2.3",
                cargo_lock=root / "Cargo.lock",
                rust_sbom=root / "rust-dependencies.cdx.json",
                binary_audit=root / "source-build-audit.json",
                audited_binary=root / "source-build.exe",
                libmpv_manifest=REPO_ROOT / "packaging/libmpv/manifest.toml",
                collection=asset_root / "collection.json",
                output_dir=output_dir,
            )
            self.assertEqual(status, 0)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            self.assertEqual(report["status"], "verified_with_disclosed_gaps")
            self.assertEqual(report["native_component_sbom"]["inventory_gaps"], [])
            self.assertEqual(report["package_member_verification"]["status"], "partial")
            self.assertEqual(len(report["package_member_verification"]["gaps"]), 1)
            native_bom = json.loads((output_dir / "native-components.cdx.json").read_text(encoding="utf-8"))
            properties = {item["name"]: item["value"] for item in native_bom["metadata"]["properties"]}
            self.assertEqual(properties["kattri:inventory-gap-count"], "0")
            self.assertEqual(properties["kattri:package-member-verification-gap-count"], "1")
            self.assertEqual(
                report["native_component_sbom"]["sha256"],
                sha256((output_dir / "native-components.cdx.json").read_bytes()),
            )

    def test_platform_sidecar_writer_output_matches_binder_contract(self) -> None:
        writer_path = REPO_ROOT / "packaging" / "write-release-native-sidecar.py"
        if not writer_path.is_file():
            self.skipTest("platform lane sidecar writer is not present in this checkout")
        writer = load_script("platform_release_sidecar", writer_path)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root = root / "assets"
            artifact_dir = asset_root / "windows-x64-artifact"
            evidence_dir = artifact_dir / "release-validation" / "native"
            evidence_dir.mkdir(parents=True)
            output_dir = root / "sbom"
            output_dir.mkdir()
            package_name = "JellyfinKattri-1.2.3-x64.zip"
            package = artifact_dir / package_name
            zip_prefix = Path(package_name).stem + "/"
            libmpv_bytes = b"platform native evidence fixture\n"
            license_bytes = b"platform license evidence fixture\n"
            exe_bytes = b"platform executable evidence fixture\n"
            (evidence_dir / "libmpv-2.dll").write_bytes(libmpv_bytes)
            (evidence_dir / "LICENSE.LGPL").write_bytes(license_bytes)
            (evidence_dir / "jellyfin-kattri.exe").write_bytes(exe_bytes)
            with zipfile.ZipFile(package, "w", compression=zipfile.ZIP_DEFLATED) as zipped:
                zipped.writestr(zip_prefix + "jellyfin-kattri.exe", exe_bytes)
                zipped.writestr(zip_prefix + "libmpv-2.dll", libmpv_bytes)
                zipped.writestr(zip_prefix + "licenses/libmpv/LICENSE.LGPL", license_bytes)
            sidecar = writer.build_sidecar(
                artifact_dir.resolve(),
                {
                    "schema_version": 1,
                    "source_commit": "a" * 40,
                    "target": "windows-x64",
                    "packages": [package_name],
                    "native_components": [
                        {
                            "name": "libmpv",
                            "manifest_platform": "windows-x64",
                            "package_file": package_name,
                            "packaged_file": zip_prefix + "libmpv-2.dll",
                            "evidence_file": "release-validation/native/libmpv-2.dll",
                            "license_files": [
                                {
                                    "path": zip_prefix + "licenses/libmpv/LICENSE.LGPL",
                                    "evidence_file": "release-validation/native/LICENSE.LGPL",
                                }
                            ],
                        }
                    ],
                    "executables": [
                        {
                            "package_file": package_name,
                            "file": "release-validation/native/jellyfin-kattri.exe",
                            "packaged_file": zip_prefix + "jellyfin-kattri.exe",
                            "audit_scope": "No package-specific cargo audit record; source-matched build only.",
                        }
                    ],
                    "package_members": [
                        {
                            "package_file": package_name,
                            "path": zip_prefix + "jellyfin-kattri.exe",
                            "role": "executable",
                            "evidence_file": "release-validation/native/jellyfin-kattri.exe",
                        },
                        {
                            "package_file": package_name,
                            "path": zip_prefix + "libmpv-2.dll",
                            "role": "native",
                            "evidence_file": "release-validation/native/libmpv-2.dll",
                        },
                        {
                            "package_file": package_name,
                            "path": zip_prefix + "licenses/libmpv/LICENSE.LGPL",
                            "role": "license",
                            "evidence_file": "release-validation/native/LICENSE.LGPL",
                        },
                    ],
                    "inventory_gaps": ["The pinned Windows archive has no build-info file."],
                },
                REPO_ROOT / "packaging/libmpv/manifest.toml",
            )
            executable_member = next(member for member in sidecar["package_members"] if member["role"] == "executable")
            self.assertEqual(executable_member["size_bytes"], len(exe_bytes))
            self.assertEqual(executable_member["sha256"], sha256(exe_bytes))
            sidecar_path = artifact_dir / "release-native-components.json"
            sidecar_path.write_text(json.dumps(sidecar, indent=2) + "\n", encoding="utf-8")

            cargo_lock = root / "Cargo.lock"
            cargo_lock.write_text("# lock fixture\n", encoding="utf-8")
            rust_sbom = root / "rust-dependencies.cdx.json"
            rust_sbom.write_text(json.dumps({"bomFormat": "CycloneDX", "specVersion": "1.6", "components": []}), encoding="utf-8")
            audit_record = root / "source-build-audit.json"
            audit_record.write_text(json.dumps({"vulnerabilities": {"found": 0}}), encoding="utf-8")
            audited_binary = root / "source-build.exe"
            audited_binary.write_bytes(b"separate audited source build\n")
            collection = asset_root / "collection.json"
            collection.write_text(json.dumps({"release_tag": "v1.2.3", "source_commit": "a" * 40}), encoding="utf-8")

            status = self._bind_quietly(
                asset_root=asset_root,
                source_commit="a" * 40,
                release_tag="v1.2.3",
                cargo_lock=cargo_lock,
                rust_sbom=rust_sbom,
                binary_audit=audit_record,
                audited_binary=audited_binary,
                libmpv_manifest=REPO_ROOT / "packaging/libmpv/manifest.toml",
                collection=collection,
                output_dir=output_dir,
            )
            self.assertEqual(status, 0)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            self.assertEqual(report["release_assets"][0]["sha256"], sha256(package.read_bytes()))
            self.assertEqual(report["status"], "verified_with_disclosed_gaps")

    def test_flatpak_sidecar_sources_match_pinned_archive_urls_and_checksums(self) -> None:
        writer_path = REPO_ROOT / "packaging" / "write-release-native-sidecar.py"
        if not writer_path.is_file():
            self.skipTest("platform lane sidecar writer is not present in this checkout")
        writer = load_script("flatpak_release_sidecar", writer_path)
        flatpak_manifest = REPO_ROOT / "packaging/flatpak/org.kattri.JellyfinKattri.yml"
        pins = binding.read_flatpak_archive_pins(flatpak_manifest)

        def run_fixture(root: Path, corrupt_libass_hash: bool = False) -> tuple[int, Path]:
            asset_root = root / "assets"
            artifact_dir = asset_root / "linux-flatpak-artifact"
            evidence = artifact_dir / "release-validation"
            (evidence / "native").mkdir(parents=True)
            for directory in (evidence / "licenses/libmpv", evidence / "licenses/libass", evidence / "licenses/uchardet", evidence / "executables"):
                directory.mkdir(parents=True, exist_ok=True)
            output_dir = root / "sbom"
            output_dir.mkdir()
            package_name = "kattri-1.2.3-linux-x64.flatpak"
            package_bytes = b"synthetic Flatpak bundle fixture\n"
            (artifact_dir / package_name).write_bytes(package_bytes)

            native_components = []
            native_specs = [
                ("libmpv", "libmpv.so.2", "LGPL build", None),
                ("libass", "libass.so.9", "ISC", pins["libass"]),
                ("uchardet", "libuchardet.so.0", "MPL-1.1", pins["uchardet"]),
            ]
            for index, (name, library_name, license_claim, source_pin) in enumerate(native_specs):
                library_bytes = ("synthetic %s bytes\n" % name).encode("utf-8")
                license_bytes = ("synthetic %s COPYING text\n" % name).encode("utf-8")
                license_name = "COPYING" if name != "libmpv" else "LICENSE.mpv.LGPL"
                library_evidence = "native/%s" % library_name
                license_evidence = "%s/%s" % (
                    "licenses/libmpv" if name == "libmpv" else "licenses/" + name,
                    license_name,
                )
                (evidence / library_evidence).write_bytes(library_bytes)
                (evidence / license_evidence).write_bytes(license_bytes)
                component = {
                    "name": name,
                    "package_file": package_name,
                    "packaged_file": "lib/" + library_name,
                    "evidence_file": "release-validation/" + library_evidence,
                    "license_files": [{
                        "path": "share/licenses/" + name + "/" + license_name,
                        "evidence_file": "release-validation/" + license_evidence,
                    }],
                }
                if name == "libmpv":
                    component["manifest_platform"] = "linux-x64"
                    component["license_claim"] = binding.read_pinned_libmpv_manifest(REPO_ROOT / "packaging/libmpv/manifest.toml")["linux-x64"]["license"]
                else:
                    component.update({
                        "version": binding.version_from_archive_url(name, source_pin["url"]),
                        "source_url": source_pin["url"],
                        "source_archive_sha256": ("0" * 64 if corrupt_libass_hash and name == "libass" else source_pin["sha256"]),
                        "license_claim": license_claim,
                    })
                native_components.append(component)

            exe_bytes = b"synthetic Flatpak executable bytes\n"
            exe_path = evidence / "executables/kattri"
            exe_path.write_bytes(exe_bytes)
            sidecar = writer.build_sidecar(
                artifact_dir.resolve(),
                {
                    "schema_version": 1,
                    "source_commit": "b" * 40,
                    "target": "linux-x64",
                    "packages": [package_name],
                    "native_components": native_components,
                    "executables": [{
                        "package_file": package_name,
                        "file": "release-validation/executables/kattri",
                        "audit_scope": "Synthetic fixture; no package audit was run.",
                    }],
                    "inventory_gaps": ["Host runtime dependencies are outside this fixture."],
                },
                REPO_ROOT / "packaging/libmpv/manifest.toml",
            )
            (artifact_dir / "release-native-components.json").write_text(json.dumps(sidecar, indent=2) + "\n", encoding="utf-8")
            cargo_lock = root / "Cargo.lock"
            cargo_lock.write_text("# lock fixture\n", encoding="utf-8")
            rust_sbom = root / "rust-dependencies.cdx.json"
            rust_sbom.write_text(json.dumps({"bomFormat": "CycloneDX", "specVersion": "1.6", "components": []}), encoding="utf-8")
            audit_record = root / "source-build-audit.json"
            audit_record.write_text(json.dumps({"vulnerabilities": {"found": 0}}), encoding="utf-8")
            audited_binary = root / "source-build"
            audited_binary.write_bytes(b"separate source build\n")
            collection = asset_root / "collection.json"
            collection.write_text(json.dumps({"release_tag": "v1.2.3", "source_commit": "b" * 40}), encoding="utf-8")
            result = self._bind_quietly(
                asset_root=asset_root,
                source_commit="b" * 40,
                release_tag="v1.2.3",
                cargo_lock=cargo_lock,
                rust_sbom=rust_sbom,
                binary_audit=audit_record,
                audited_binary=audited_binary,
                libmpv_manifest=REPO_ROOT / "packaging/libmpv/manifest.toml",
                flatpak_manifest=flatpak_manifest,
                collection=collection,
                output_dir=output_dir,
            )
            return result, output_dir

        with tempfile.TemporaryDirectory() as temporary:
            status, output_dir = run_fixture(Path(temporary) / "valid")
            self.assertEqual(status, 0)
            bom = json.loads((output_dir / "native-components.cdx.json").read_text(encoding="utf-8"))
            self.assertTrue({"libmpv", "libass", "uchardet"}.issubset({item["name"] for item in bom["components"]}))

        with tempfile.TemporaryDirectory() as temporary:
            status, output_dir = run_fixture(Path(temporary) / "bad", corrupt_libass_hash=True)
            self.assertEqual(status, 1)
            report = json.loads((output_dir / "release-asset-binding.json").read_text(encoding="utf-8"))
            self.assertTrue(any("Flatpak libass archive checksum differs" in item for item in report["issues"]))

if __name__ == "__main__":
    unittest.main(verbosity=2)
