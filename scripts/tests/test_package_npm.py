import importlib.util
import io
import json
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "package_npm.py"
SPEC = importlib.util.spec_from_file_location("package_npm", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
package_npm = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = package_npm
SPEC.loader.exec_module(package_npm)

SMOKE_SCRIPT = Path(__file__).resolve().parents[1] / "smoke_npm_package.py"
SMOKE_SPEC = importlib.util.spec_from_file_location("smoke_npm_package", SMOKE_SCRIPT)
assert SMOKE_SPEC is not None and SMOKE_SPEC.loader is not None
smoke_npm_package = importlib.util.module_from_spec(SMOKE_SPEC)
sys.modules[SMOKE_SPEC.name] = smoke_npm_package
SMOKE_SPEC.loader.exec_module(smoke_npm_package)


def fake_pack(staging_dir: Path, output_path: Path, _npm_bin: str) -> Path:
    output_path.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(output_path, "w:gz") as archive:
        for path in sorted(staging_dir.rglob("*")):
            archive.add(path, arcname=Path("package") / path.relative_to(staging_dir), recursive=False)
    return output_path


class NpmPackageTest(unittest.TestCase):
    def test_root_package_has_only_selected_platform_dependencies(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            output_dir = Path(temp_dir) / "dist"
            with patch.object(package_npm, "_pack", side_effect=fake_pack):
                result = package_npm.build_root_package(
                    root=SCRIPT.parents[1],
                    output_dir=output_dir,
                    version="0.1.0",
                    targets=["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"],
                    npm_bin="fake-npm",
                )

            with tarfile.open(result.tarball, "r:gz") as archive:
                package_json = json.loads(archive.extractfile("package/package.json").read())
            self.assertEqual(package_json["name"], "@golutra/agent")
            self.assertEqual(package_json["version"], "0.1.0")
            self.assertEqual(
                package_json["optionalDependencies"],
                {
                    "@golutra/agent-darwin-arm64": "0.1.0",
                    "@golutra/agent-linux-x64": "0.1.0",
                },
            )
            self.assertTrue(result.manifest.is_file())
            self.assertTrue(result.checksum.is_file())

    def test_platform_package_contains_verified_cli_and_tui(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = SCRIPT.parents[1]
            binary_dir = Path(temp_dir) / "release"
            binary_dir.mkdir()
            for source_name, _installed_name in package_npm.NATIVE_BINARY_SPECS:
                source = binary_dir / source_name
                source.write_bytes(f"fixture:{source_name}".encode())
                source.chmod(0o755)

            with patch.object(package_npm, "_pack", side_effect=fake_pack):
                result = package_npm.build_platform_package(
                    root=root,
                    output_dir=Path(temp_dir) / "dist",
                    version="0.1.0",
                    target="aarch64-apple-darwin",
                    binary_dir=binary_dir,
                    npm_bin="fake-npm",
                )

            with tarfile.open(result.tarball, "r:gz") as archive:
                package_json = json.loads(archive.extractfile("package/package.json").read())
                manifest = json.loads(
                    archive.extractfile("package/vendor/manifest.json").read()
                )
                names = {member.name for member in archive.getmembers()}
            self.assertEqual(package_json["name"], "@golutra/agent-darwin-arm64")
            self.assertEqual(package_json["os"], ["darwin"])
            self.assertEqual(package_json["cpu"], ["arm64"])
            self.assertEqual(manifest["target"], "aarch64-apple-darwin")
            self.assertIn("package/vendor/bin/golutra", names)
            self.assertIn("package/vendor/bin/golutra-tui", names)

    def test_windows_platform_package_uses_executable_suffix_and_cpu_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = SCRIPT.parents[1]
            binary_dir = Path(temp_dir) / "release"
            binary_dir.mkdir()
            for source_name, _installed_name in package_npm.NATIVE_BINARY_SPECS:
                (binary_dir / f"{source_name}.exe").write_bytes(
                    f"fixture:{source_name}".encode()
                )

            with patch.object(package_npm, "_pack", side_effect=fake_pack):
                result = package_npm.build_platform_package(
                    root=root,
                    output_dir=Path(temp_dir) / "dist",
                    version="0.1.0",
                    target="aarch64-pc-windows-msvc",
                    binary_dir=binary_dir,
                    npm_bin="fake-npm",
                )

            with tarfile.open(result.tarball, "r:gz") as archive:
                package_json = json.loads(archive.extractfile("package/package.json").read())
                names = {member.name for member in archive.getmembers()}
            self.assertEqual(package_json["name"], "@golutra/agent-win32-arm64")
            self.assertEqual(package_json["os"], ["win32"])
            self.assertEqual(package_json["cpu"], ["arm64"])
            self.assertIn("package/vendor/bin/golutra.exe", names)
            self.assertIn("package/vendor/bin/golutra-tui.exe", names)

    def test_platform_package_rejects_missing_binary(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            with self.assertRaisesRegex(package_npm.NpmPackageError, "missing or unsafe"):
                package_npm.build_platform_package(
                    root=SCRIPT.parents[1],
                    output_dir=Path(temp_dir) / "dist",
                    version="0.1.0",
                    target="aarch64-apple-darwin",
                    binary_dir=Path(temp_dir),
                    npm_bin="fake-npm",
                )

    def test_smoke_extractor_is_compatible_with_python_311_and_preserves_mode(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            archive_path = Path(temp_dir) / "package.tgz"
            with tarfile.open(archive_path, "w:gz") as archive:
                directory = tarfile.TarInfo("package/bin")
                directory.type = tarfile.DIRTYPE
                directory.mode = 0o755
                archive.addfile(directory)
                binary = tarfile.TarInfo("package/bin/golutra")
                binary.mode = 0o755
                binary.size = 7
                archive.addfile(binary, io.BytesIO(b"fixture"))

            package_dir = smoke_npm_package.extract_package(
                archive_path, Path(temp_dir) / "extracted"
            )
            executable = package_dir / "bin" / "golutra"
            self.assertEqual(executable.read_bytes(), b"fixture")
            self.assertEqual(executable.stat().st_mode & 0o777, 0o755)

    def test_smoke_extractor_rejects_archive_path_traversal(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            archive_path = Path(temp_dir) / "unsafe.tgz"
            with tarfile.open(archive_path, "w:gz") as archive:
                member = tarfile.TarInfo("../outside")
                member.size = 1
                archive.addfile(member, io.BytesIO(b"x"))

            with self.assertRaisesRegex(smoke_npm_package.SmokeError, "unsafe"):
                smoke_npm_package.extract_package(
                    archive_path, Path(temp_dir) / "extracted"
                )


if __name__ == "__main__":
    unittest.main()
