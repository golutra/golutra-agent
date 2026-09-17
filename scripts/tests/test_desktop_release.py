from __future__ import annotations

import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import desktop_release
import package_npm
import package_release

ROOT = Path(__file__).resolve().parents[2]


def fake_pack(staging, output, _npm):
    output.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(output, "w:gz") as archive:
        for path in sorted(staging.rglob("*")):
            archive.add(path, arcname=Path("package") / path.relative_to(staging), recursive=False)
    return output


class DesktopReleaseTest(unittest.TestCase):
    def test_old_command_or_home_cannot_claim_the_new_launch_contract(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            native, _ = self.make_platform(root, "aarch64-apple-darwin")
            for key, value in (("contract_version", 1), ("cli", "bin/golutra"), ("home_env", "GOLUTRA_HOME")):
                manifest = package_release.verify_package(native.archive)
                manifest["desktop_launch"][key] = value
                with patch.object(desktop_release, "verify_package", return_value=manifest):
                    with self.subTest(key=key), self.assertRaisesRegex(package_release.PackageError, "contract 2"):
                        desktop_release.inventory(root / "dist", allow_partial=True)

    def make_platform(self, root, target):
        binaries = root / target
        binaries.mkdir()
        suffix = ".exe" if "windows" in target else ""
        for name, _ in package_release.BINARY_SPECS:
            (binaries / f"{name}{suffix}").write_bytes(f"fixture:{target}:{name}".encode())
        with patch.object(package_release, "git_identity", return_value=("a" * 40, 1700000000, False)):
            native = package_release.package_release(root=ROOT, output_dir=root / "dist", version="9.8.7",
                target=target, skip_build=True, binary_dir=binaries)
        with patch.object(package_npm, "_pack", side_effect=fake_pack):
            npm = package_npm.build_platform_package(root=ROOT, output_dir=root / "dist/npm", version="9.8.7",
                target=target, binary_dir=binaries, npm_bin="unused")
        # 这里只测门禁协议；fixture 明确不是原生运行验收证据。
        Path(f"{native.archive}.smoke.json").write_text(json.dumps({
            "passed": True, "target": target, "archive_sha256": desktop_release.digest(native.archive),
            "npm_sha256": desktop_release.digest(npm.tarball), "checks": ["unit fixture"], "system": {"fixture": True},
        }))
        (root / "dist" / f"shared-versions-{target}.json").write_text(json.dumps({
            "passed": True, "binaries": [{"sha256": "b" * 64},
                {"sha256": desktop_release.digest(binaries / f"golutra-agent{suffix}")}],
            "expected_compatibility": "reject", "shared_data_compatible": False,
            "limitations": ["unit fixture, not runtime evidence"],
        }))
        return native, npm

    def test_complete_six_platform_inventory_with_fixed_urls_and_byte_parity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for target in package_npm.PLATFORMS:
                self.make_platform(root, target)
            result = desktop_release.inventory(root / "dist")
            self.assertTrue(result["complete"])
            self.assertFalse(result["development_only"])
            self.assertEqual(len(result["artifacts"]), 6)
            for artifact in result["artifacts"]:
                self.assertIn("/releases/download/v9.8.7/", artifact["url"])
                self.assertEqual(len(artifact["sha256"]), 64)
                self.assertFalse(artifact["launch"]["node_required"])
                self.assertEqual(artifact["launch"]["contract_version"], 2)
                self.assertEqual(artifact["launch"]["home_env"], "GOLUTRA_AGENT_HOME")
                self.assertEqual(artifact["launch"]["update_owner"], "distributor")
                suffix = ".exe" if artifact["platform"] == "win32" else ""
                self.assertEqual(artifact["launch"]["cli"], f"bin/golutra-agent{suffix}")
                self.assertEqual(artifact["launch"]["tui"], f"bin/golutra-agent-tui{suffix}")

    def test_missing_platform_or_stale_smoke_is_not_a_release(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            native, _ = self.make_platform(root, "aarch64-apple-darwin")
            with self.assertRaisesRegex(package_release.PackageError, "missing native"):
                desktop_release.inventory(root / "dist")
            partial = desktop_release.inventory(root / "dist", repository="seekskyworld/golutra-agent", allow_partial=True)
            self.assertIn("github.com/seekskyworld/golutra-agent/", partial["artifacts"][0]["url"])
            path = Path(f"{native.archive}.smoke.json")
            report = json.loads(path.read_text())
            report["archive_sha256"] = "0" * 64
            path.write_text(json.dumps(report))
            with self.assertRaisesRegex(package_release.PackageError, "stale"):
                desktop_release.inventory(root / "dist", allow_partial=True)

    def test_different_npm_binary_is_rejected_even_with_valid_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = "aarch64-apple-darwin"
            native, _ = self.make_platform(root, target)
            (root / target / "golutra-agent").write_bytes(b"different build")
            with patch.object(package_npm, "_pack", side_effect=fake_pack):
                npm = package_npm.build_platform_package(root=ROOT, output_dir=root / "dist/npm", version="9.8.7",
                    target=target, binary_dir=root / target, npm_bin="unused")
            with self.assertRaisesRegex(package_release.PackageError, "payload mismatch"):
                desktop_release.verify_npm_parity(package_release.verify_package(native.archive), npm.tarball)

    def test_archive_paths_reject_windows_drive_ads_and_backslash(self):
        for name in ("C:/outside", "root/file:stream", "root\\..\\outside", "/absolute", "root/../outside"):
            with self.subTest(name=name), self.assertRaises(package_release.PackageError):
                package_release._safe_archive_path(name)

    def test_cross_build_report_must_match_the_actual_candidate(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = "aarch64-apple-darwin"
            self.make_platform(root, target)
            report_path = root / "dist" / f"shared-versions-{target}.json"
            report = json.loads(report_path.read_text())
            report["binaries"][1]["sha256"] = "c" * 64
            report_path.write_text(json.dumps(report))
            with self.assertRaisesRegex(package_release.PackageError, "stale cross-build"):
                desktop_release.inventory(root / "dist", allow_partial=True)
            report_path.unlink()
            with self.assertRaisesRegex(package_release.PackageError, "missing cross-build"):
                desktop_release.inventory(root / "dist", allow_partial=True)

    def test_obsolete_upgrade_acceptance_cannot_satisfy_current_release_gate(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = "aarch64-apple-darwin"
            self.make_platform(root, target)
            report_path = root / "dist" / f"shared-versions-{target}.json"
            report = json.loads(report_path.read_text())
            report["expected_compatibility"] = "upgrade_then_reject"
            report_path.write_text(json.dumps(report))
            with self.assertRaisesRegex(package_release.PackageError, "stale cross-build"):
                desktop_release.inventory(root / "dist", allow_partial=True)

    def test_legacy_baseline_limitation_is_only_allowed_for_verified_windows_boundary(self):
        for target in ("aarch64-pc-windows-msvc", "aarch64-apple-darwin"):
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                self.make_platform(root, target)
                path = root / "dist" / f"shared-versions-{target}.json"
                report = json.loads(path.read_text())
                report.update({"expected_compatibility": "legacy_startup_unavailable",
                               "legacy_baseline_available": False,
                               "limited_acceptance": {
                                   "reason": "windows-sqlite-url",
                                   "legacy_reader_blocked_without_data_changes": True,
                                   "candidate_rejected_old_schema_without_data_changes": True,
                                   "old_schema_fixture": {"version": 5, "synthetic": True}}})
                report["binaries"][0]["version"] = "golutra 0.2.0"
                path.write_text(json.dumps(report))
                if "windows" not in target:
                    with self.assertRaisesRegex(package_release.PackageError, "stale cross-build"):
                        desktop_release.inventory(root / "dist", allow_partial=True)
                    continue
                result = desktop_release.inventory(root / "dist", allow_partial=True)
                self.assertEqual(result["artifacts"][0]["shared_data_acceptance"]["expected_compatibility"],
                                 "legacy_startup_unavailable")
                report["limited_acceptance"]["candidate_rejected_old_schema_without_data_changes"] = False
                path.write_text(json.dumps(report))
                with self.assertRaisesRegex(package_release.PackageError, "stale cross-build"):
                    desktop_release.inventory(root / "dist", allow_partial=True)


if __name__ == "__main__":
    unittest.main()
