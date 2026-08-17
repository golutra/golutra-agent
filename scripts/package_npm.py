#!/usr/bin/env python3
"""Build the Golutra npm launcher and platform-native packages."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any


PACKAGE_NAME = "@golutra/agent"
PACKAGE_ROOT = "npm/agent"
SCHEMA_VERSION = 1
NATIVE_BINARY_SPECS = (
    ("golutra-cli", "golutra"),
    ("golutra-tui", "golutra-tui"),
)
LEGAL_FILES = ("LICENSE", "NOTICE")
VERSION_PATTERN = re.compile(
    r"^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$"
)


@dataclass(frozen=True)
class Platform:
    target: str
    npm_suffix: str
    os: str
    cpu: str

    @property
    def package_name(self) -> str:
        return f"{PACKAGE_NAME}-{self.npm_suffix}"


PLATFORMS = {
    platform.target: platform
    for platform in (
        Platform("x86_64-unknown-linux-gnu", "linux-x64", "linux", "x64"),
        Platform("aarch64-unknown-linux-gnu", "linux-arm64", "linux", "arm64"),
        Platform("x86_64-apple-darwin", "darwin-x64", "darwin", "x64"),
        Platform("aarch64-apple-darwin", "darwin-arm64", "darwin", "arm64"),
        Platform("x86_64-pc-windows-msvc", "win32-x64", "win32", "x64"),
        Platform("aarch64-pc-windows-msvc", "win32-arm64", "win32", "arm64"),
    )
}


class NpmPackageError(RuntimeError):
    """Raised when an npm package cannot be built or verified."""


@dataclass(frozen=True)
class PackageResult:
    tarball: Path
    manifest: Path
    checksum: Path


def repository_root() -> Path:
    return Path(__file__).resolve().parents[1]


def workspace_version(root: Path) -> str:
    with (root / "Cargo.toml").open("rb") as source:
        value = tomllib.load(source)
    version = value.get("workspace", {}).get("package", {}).get("version")
    if not isinstance(version, str) or not VERSION_PATTERN.fullmatch(version):
        raise NpmPackageError("workspace.package.version is not a valid npm version")
    return version


def validate_version(version: str) -> str:
    if not VERSION_PATTERN.fullmatch(version):
        raise NpmPackageError(f"invalid npm version: {version!r}")
    return version


def package_root(root: Path) -> Path:
    value = root / PACKAGE_ROOT
    if not value.is_dir():
        raise NpmPackageError(f"npm package template is missing: {value}")
    return value


def package_targets(targets: list[str] | None) -> list[Platform]:
    selected = targets or list(PLATFORMS)
    if not selected:
        raise NpmPackageError("at least one npm target is required")
    result: list[Platform] = []
    seen_suffixes: set[str] = set()
    for target in selected:
        platform = PLATFORMS.get(target)
        if platform is None:
            raise NpmPackageError(
                f"unsupported npm target {target!r}; choose from {sorted(PLATFORMS)}"
            )
        if platform.npm_suffix in seen_suffixes:
            raise NpmPackageError(f"duplicate npm platform: {platform.npm_suffix}")
        seen_suffixes.add(platform.npm_suffix)
        result.append(platform)
    return result


def build_root_package(
    *,
    root: Path,
    output_dir: Path,
    version: str,
    targets: list[str] | None,
    npm_bin: str,
) -> PackageResult:
    platforms = package_targets(targets)
    with tempfile.TemporaryDirectory(prefix="golutra-npm-root-") as temp_dir:
        staging_dir = Path(temp_dir) / "package"
        _copy_root_template(root, staging_dir)
        package_json = _read_json(staging_dir / "package.json")
        package_json["version"] = version
        package_json["optionalDependencies"] = {
            platform.package_name: version for platform in platforms
        }
        _write_json(staging_dir / "package.json", package_json)

        tarball = _pack(
            staging_dir,
            output_dir / f"golutra-agent-npm-{version}.tgz",
            npm_bin,
        )
        _verify_tarball(
            tarball,
            expected_name=PACKAGE_NAME,
            expected_version=version,
            required_paths={
                "package/package.json",
                "package/bin/golutra.js",
                "package/bin/golutra-tui.js",
                "package/bin/run.js",
                "package/LICENSE",
                "package/NOTICE",
                "package/README.md",
            },
        )
        manifest = _write_manifest(
            tarball,
            {
                "schema_version": SCHEMA_VERSION,
                "kind": "root",
                "package": PACKAGE_NAME,
                "version": version,
                "platforms": [platform.target for platform in platforms],
                "optional_dependencies": [platform.package_name for platform in platforms],
            },
        )
        checksum = _write_checksum(tarball)
        return PackageResult(tarball, manifest, checksum)


def build_platform_package(
    *,
    root: Path,
    output_dir: Path,
    version: str,
    target: str,
    binary_dir: Path,
    npm_bin: str,
) -> PackageResult:
    platform = package_targets([target])[0]
    binary_dir = binary_dir.resolve()
    executable_suffix = ".exe" if platform.os == "win32" else ""

    with tempfile.TemporaryDirectory(prefix=f"golutra-npm-{platform.npm_suffix}-") as temp_dir:
        staging_dir = Path(temp_dir) / "package"
        vendor_bin = staging_dir / "vendor" / "bin"
        vendor_bin.mkdir(parents=True)
        binary_records: list[dict[str, Any]] = []

        for source_name, installed_name in NATIVE_BINARY_SPECS:
            source = binary_dir / f"{source_name}{executable_suffix}"
            if source.is_symlink() or not source.is_file():
                raise NpmPackageError(f"native binary is missing or unsafe: {source}")
            content = source.read_bytes()
            if not content:
                raise NpmPackageError(f"native binary is empty: {source}")
            destination = vendor_bin / f"{installed_name}{executable_suffix}"
            shutil.copyfile(source, destination)
            if platform.os != "win32":
                destination.chmod(0o755)
            binary_records.append(
                {
                    "path": f"vendor/bin/{installed_name}{executable_suffix}",
                    "source": source.name,
                    "size": len(content),
                    "sha256": hashlib.sha256(content).hexdigest(),
                    "mode": "0755",
                }
            )

        _copy_legal_files(root, staging_dir)
        shutil.copy2(package_root(root) / "README.md", staging_dir / "README.md")
        _write_json(
            vendor_bin.parent / "manifest.json",
            {
                "schema_version": SCHEMA_VERSION,
                "package": platform.package_name,
                "version": version,
                "target": platform.target,
                "binaries": binary_records,
            },
        )

        package_json = {
            "name": platform.package_name,
            "version": version,
            "description": f"Golutra native binaries for {platform.npm_suffix}",
            "license": "Apache-2.0",
            "os": [platform.os],
            "cpu": [platform.cpu],
            "files": ["vendor", "LICENSE", "NOTICE", "README.md"],
            "engines": {"node": ">=18"},
            "repository": {
                "type": "git",
                "url": "https://github.com/golutra/golutra-agent.git",
                "directory": "npm/agent",
            },
            "homepage": "https://github.com/golutra/golutra-agent",
            "publishConfig": {"access": "public"},
        }
        _write_json(staging_dir / "package.json", package_json)

        tarball = _pack(
            staging_dir,
            output_dir / f"golutra-agent-npm-{platform.npm_suffix}-{version}.tgz",
            npm_bin,
        )
        _verify_tarball(
            tarball,
            expected_name=platform.package_name,
            expected_version=version,
            required_paths={
                "package/package.json",
                "package/vendor/manifest.json",
                "package/LICENSE",
                "package/NOTICE",
                "package/README.md",
                *{
                    f"package/vendor/bin/{installed_name}{executable_suffix}"
                    for _source_name, installed_name in NATIVE_BINARY_SPECS
                },
            },
        )
        manifest = _write_manifest(
            tarball,
            {
                "schema_version": SCHEMA_VERSION,
                "kind": "platform",
                "package": platform.package_name,
                "version": version,
                "target": platform.target,
                "npm_suffix": platform.npm_suffix,
                "binaries": binary_records,
            },
        )
        checksum = _write_checksum(tarball)
        return PackageResult(tarball, manifest, checksum)


def _copy_root_template(root: Path, destination: Path) -> None:
    template = package_root(root)
    destination.mkdir(parents=True)
    shutil.copytree(template / "bin", destination / "bin")
    _copy_legal_files(root, destination)
    shutil.copy2(template / "README.md", destination / "README.md")
    shutil.copy2(template / "package.json", destination / "package.json")


def _copy_legal_files(root: Path, destination: Path) -> None:
    for name in LEGAL_FILES:
        source = root / name
        if source.is_symlink() or not source.is_file():
            raise NpmPackageError(f"required legal file is missing or unsafe: {source}")
        shutil.copy2(source, destination / name)


def _pack(staging_dir: Path, output_path: Path, npm_bin: str) -> Path:
    output_path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="golutra-npm-pack-") as temp_dir:
        npm_args = [
            npm_bin,
            "pack",
            "--ignore-scripts",
            "--json",
            "--pack-destination",
            temp_dir,
        ]
        result = subprocess.run(
            _npm_command(npm_args),
            cwd=staging_dir,
            check=False,
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            raise NpmPackageError(
                f"npm pack failed ({result.returncode}): {result.stderr.strip()}"
            )
        try:
            metadata = json.loads(result.stdout)
        except json.JSONDecodeError as error:
            raise NpmPackageError("npm pack returned invalid JSON") from error
        if not isinstance(metadata, list) or not metadata:
            raise NpmPackageError("npm pack returned no tarball")
        filename = metadata[0].get("filename")
        if not isinstance(filename, str):
            raise NpmPackageError("npm pack did not report a tarball filename")
        packed = Path(temp_dir) / filename
        if not packed.is_file() or packed.is_symlink():
            raise NpmPackageError(f"npm pack output is missing or unsafe: {packed}")
        output_path.unlink(missing_ok=True)
        shutil.move(packed, output_path)
    return output_path


def _npm_command(arguments: list[str]) -> list[str]:
    if os.name != "nt":
        return arguments
    command_processor = os.environ.get("COMSPEC", "cmd.exe")
    return [command_processor, "/d", "/s", "/c", subprocess.list2cmdline(arguments)]


def _verify_tarball(
    tarball: Path,
    *,
    expected_name: str,
    expected_version: str,
    required_paths: set[str],
) -> None:
    try:
        with tarfile.open(tarball, "r:gz") as archive:
            members = archive.getmembers()
            names = {member.name for member in members}
            if len(names) != len(members):
                raise NpmPackageError("npm package contains duplicate members")
            for member in members:
                relative = PurePosixPath(member.name)
                if (
                    relative.is_absolute()
                    or ".." in relative.parts
                    or not relative.parts
                    or relative.parts[0] != "package"
                    or "\\" in member.name
                ):
                    raise NpmPackageError(
                        f"npm package contains an unsafe path: {member.name}"
                    )
                if member.issym() or member.islnk():
                    raise NpmPackageError("npm package contains a symbolic link")
                if not (member.isdir() or member.isfile()):
                    raise NpmPackageError(
                        f"npm package contains a special file: {member.name}"
                    )
            if not required_paths.issubset(names):
                missing = sorted(required_paths - names)
                raise NpmPackageError(f"npm package is missing files: {missing}")
            package_json_member = archive.extractfile("package/package.json")
            if package_json_member is None:
                raise NpmPackageError("npm package.json is missing")
            package_json = json.loads(package_json_member.read())
            if not isinstance(package_json, dict):
                raise NpmPackageError("npm package.json must be an object")
            if package_json.get("name") != expected_name:
                raise NpmPackageError("npm package name does not match its tarball")
            if package_json.get("version") != expected_version:
                raise NpmPackageError("npm package version does not match its tarball")
    except (OSError, tarfile.TarError, json.JSONDecodeError) as error:
        raise NpmPackageError(f"invalid npm tarball {tarball}: {error}") from error


def _write_manifest(tarball: Path, value: dict[str, Any]) -> Path:
    manifest = Path(f"{tarball}.manifest.json")
    value = {**value, "tarball": tarball.name}
    manifest.write_text(
        json.dumps(value, indent=2, sort_keys=True, ensure_ascii=True) + "\n",
        encoding="utf-8",
    )
    return manifest


def _write_checksum(tarball: Path) -> Path:
    checksum = Path(f"{tarball}.sha256")
    digest = hashlib.sha256(tarball.read_bytes()).hexdigest()
    checksum.write_text(f"{digest}  {tarball.name}\n", encoding="ascii")
    return checksum


def _read_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise NpmPackageError(f"expected a JSON object in {path}")
    return value


def _write_json(path: Path, value: dict[str, Any]) -> None:
    path.write_text(
        json.dumps(value, indent=2, sort_keys=False, ensure_ascii=True) + "\n",
        encoding="utf-8",
    )


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build and verify Golutra npm launcher packages"
    )
    parser.add_argument("--package", choices=("root", "platform"), required=True)
    parser.add_argument("--version")
    parser.add_argument("--target", choices=sorted(PLATFORMS))
    parser.add_argument(
        "--targets",
        nargs="+",
        choices=sorted(PLATFORMS),
        help="Targets included in the root package; defaults to all supported targets.",
    )
    parser.add_argument("--binary-dir", type=Path)
    parser.add_argument("--output-dir", type=Path, default=Path("dist/npm"))
    parser.add_argument("--npm-bin", default="npm")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    root = repository_root()
    try:
        version = validate_version(args.version or workspace_version(root))
        output_dir = args.output_dir.resolve()
        if args.package == "root":
            if args.target is not None or args.binary_dir is not None:
                raise NpmPackageError("root packages do not accept --target or --binary-dir")
            result = build_root_package(
                root=root,
                output_dir=output_dir,
                version=version,
                targets=args.targets,
                npm_bin=args.npm_bin,
            )
        else:
            if args.target is None or args.binary_dir is None:
                raise NpmPackageError("platform packages require --target and --binary-dir")
            if args.targets is not None:
                raise NpmPackageError("platform packages do not accept --targets")
            result = build_platform_package(
                root=root,
                output_dir=output_dir,
                version=version,
                target=args.target,
                binary_dir=args.binary_dir,
                npm_bin=args.npm_bin,
            )
        print(
            json.dumps(
                {
                    "tarball": str(result.tarball),
                    "manifest": str(result.manifest),
                    "checksum": str(result.checksum),
                },
                sort_keys=True,
            )
        )
        return 0
    except (OSError, subprocess.SubprocessError, NpmPackageError, ValueError) as error:
        print(f"npm packaging failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
