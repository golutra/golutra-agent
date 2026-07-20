from __future__ import annotations

import importlib.util
import os
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "package_release.py"
SPEC = importlib.util.spec_from_file_location("package_release", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
package_release = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = package_release
SPEC.loader.exec_module(package_release)


class PackageReleaseTest(unittest.TestCase):
    def test_packages_and_verifies_deterministic_unix_archive(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary_dir = root / "bin"
            output_dir = root / "dist"
            binary_dir.mkdir()
            for source_name, _installed_name in package_release.BINARY_SPECS:
                binary = binary_dir / source_name
                binary.write_bytes(f"fixture:{source_name}\n".encode())
                binary.chmod(0o755)
            first = package_release.package_release(
                root=Path(__file__).resolve().parents[2],
                output_dir=output_dir,
                version="9.8.7",
                target="x86_64-unknown-linux-gnu",
                skip_build=True,
                binary_dir=binary_dir,
                source_date_epoch=1_700_000_000,
            )
            first_digest = first.checksum.read_text().split()[0]
            manifest = package_release.verify_package(first.archive)
            self.assertEqual(manifest["version"], "9.8.7")
            self.assertEqual(len(manifest["files"]), len(package_release.BINARY_SPECS))

            second = package_release.package_release(
                root=Path(__file__).resolve().parents[2],
                output_dir=output_dir,
                version="9.8.7",
                target="x86_64-unknown-linux-gnu",
                skip_build=True,
                binary_dir=binary_dir,
                source_date_epoch=1_700_000_000,
            )
            self.assertEqual(first_digest, second.checksum.read_text().split()[0])

    def test_verifier_rejects_tampered_archive(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary_dir = root / "bin"
            binary_dir.mkdir()
            for source_name, _installed_name in package_release.BINARY_SPECS:
                binary = binary_dir / source_name
                binary.write_bytes(os.urandom(32))
                binary.chmod(0o755)
            result = package_release.package_release(
                root=Path(__file__).resolve().parents[2],
                output_dir=root / "dist",
                version="1.0.0",
                target="aarch64-apple-darwin",
                skip_build=True,
                binary_dir=binary_dir,
                source_date_epoch=1_700_000_000,
            )
            with result.archive.open("ab") as archive:
                archive.write(b"tampered")
            with self.assertRaisesRegex(package_release.PackageError, "SHA-256"):
                package_release.verify_package(result.archive)

    def test_packages_and_verifies_windows_zip(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary_dir = root / "bin"
            binary_dir.mkdir()
            for source_name, _installed_name in package_release.BINARY_SPECS:
                binary = binary_dir / f"{source_name}.exe"
                binary.write_bytes(f"fixture:{source_name}\n".encode())
            result = package_release.package_release(
                root=Path(__file__).resolve().parents[2],
                output_dir=root / "dist",
                version="1.2.3",
                target="x86_64-pc-windows-msvc",
                skip_build=True,
                binary_dir=binary_dir,
                source_date_epoch=1_700_000_000,
            )
            self.assertEqual(result.archive.suffix, ".zip")
            manifest = package_release.verify_package(result.archive)
            self.assertTrue(
                all(record["path"].endswith(".exe") for record in manifest["files"])
            )


if __name__ == "__main__":
    unittest.main()
