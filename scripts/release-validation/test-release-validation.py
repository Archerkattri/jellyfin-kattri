#!/usr/bin/env python3
"""Focused offline checks for release artifact collection and SBOM binding."""

from __future__ import annotations

import hashlib
import importlib.util
import contextlib
import io
import json
import os
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
    ) -> tuple[Path, Path, Path]:
        asset_root = root / "assets"
        artifact_dir = asset_root / "release-windows-x64-artifact"
        artifact_dir.mkdir(parents=True)
        output_dir = root / "sbom"
        output_dir.mkdir()

        package_name = "JellyfinKattri-1.2.3-x64.zip"
        package_bytes = b"release package fixture; never a real installer\n"
        package = artifact_dir / package_name
        package.write_bytes(package_bytes)
        package_sha = sha256(package_bytes)
        (artifact_dir / (package_name + ".sha256")).write_text(
            "%s  %s\n" % (package_sha, package_name), encoding="ascii"
        )

        library_bytes = b"synthetic native library fixture\n"
        license_bytes = b"synthetic license evidence fixture\n"
        exe_bytes = b"synthetic release executable fixture\n"
        (artifact_dir / "libmpv-2.dll").write_bytes(library_bytes)
        (artifact_dir / "LICENSE.LGPL").write_bytes(license_bytes)
        (artifact_dir / "jellyfin-kattri.exe").write_bytes(exe_bytes)

        manifest = binding.read_pinned_libmpv_manifest(REPO_ROOT / "packaging/libmpv/manifest.toml")
        pinned = manifest["windows-x64"]
        executable = {
            "package_file": package_name,
            "file": "jellyfin-kattri.exe",
            "sha256": sha256(exe_bytes),
            "audit_scope": "No package-specific cargo audit record; source-matched build only.",
        }
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
                    "packaged_file": "libmpv-2.dll",
                    "packaged_sha256": sha256(library_bytes),
                    "evidence_file": "libmpv-2.dll",
                    "license_files": [
                        {"path": "LICENSE.LGPL", "sha256": sha256(license_bytes), "evidence_file": "LICENSE.LGPL"}
                    ],
                }
            ],
            "executables": [executable],
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
            self.assertEqual(package["sha256"], sha256(b"release package fixture; never a real installer\n"))
            self.assertEqual(package["member_content_verification"]["status"], "not_verified_by_binder")
            self.assertIn("does not extract the package", package["member_content_verification"]["scope"])
            self.assertEqual(report["package_member_verification"]["status"], "not_comprehensively_verified")
            self.assertEqual(len(report["package_member_verification"]["gaps"]), 1)
            self.assertTrue(package["cargo_audit_record_sha256"])
            self.assertEqual(package["cargo_audit_relation"], "source-commit-only")
            native_bom = json.loads((output_dir / "native-components.cdx.json").read_text(encoding="utf-8"))
            libmpv = next(item for item in native_bom["components"] if item["name"] == "libmpv")
            manifest = binding.read_pinned_libmpv_manifest(REPO_ROOT / "packaging/libmpv/manifest.toml")
            self.assertEqual(libmpv["licenses"][0]["license"]["name"], manifest["windows-x64"]["license"])
            self.assertNotIn("expression", libmpv["licenses"][0]["license"])

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

    def test_member_scope_gap_prevents_unqualified_verified_status(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset_root, output_dir, sidecar_path = self._write_binding_fixture(root, with_package_audit=True)
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
            package_bytes = b"platform sidecar integration fixture\n"
            (artifact_dir / package_name).write_bytes(package_bytes)
            libmpv_bytes = b"platform native evidence fixture\n"
            license_bytes = b"platform license evidence fixture\n"
            exe_bytes = b"platform executable evidence fixture\n"
            (evidence_dir / "libmpv-2.dll").write_bytes(libmpv_bytes)
            (evidence_dir / "LICENSE.LGPL").write_bytes(license_bytes)
            (evidence_dir / "jellyfin-kattri.exe").write_bytes(exe_bytes)
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
                            "packaged_file": "libmpv-2.dll",
                            "evidence_file": "release-validation/native/libmpv-2.dll",
                            "license_files": [
                                {
                                    "path": "LICENSE.LGPL",
                                    "evidence_file": "release-validation/native/LICENSE.LGPL",
                                }
                            ],
                        }
                    ],
                    "executables": [
                        {
                            "package_file": package_name,
                            "file": "release-validation/native/jellyfin-kattri.exe",
                            "audit_scope": "No package-specific cargo audit record; source-matched build only.",
                        }
                    ],
                    "inventory_gaps": ["The pinned Windows archive has no build-info file."],
                },
                REPO_ROOT / "packaging/libmpv/manifest.toml",
            )
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
            self.assertEqual(report["release_assets"][0]["sha256"], sha256(package_bytes))
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
