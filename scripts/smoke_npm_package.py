#!/usr/bin/env python3
"""Run a native smoke test against a root and platform Golutra npm package."""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path, PurePosixPath


class SmokeError(RuntimeError):
    pass


def extract_package(archive_path: Path, destination: Path) -> Path:
    destination.mkdir(parents=True, exist_ok=True)
    destination_root = destination.resolve()
    seen_names: set[str] = set()
    with tarfile.open(archive_path, "r:gz") as archive:
        members = archive.getmembers()
        for member in members:
            relative = PurePosixPath(member.name)
            if (
                relative.is_absolute()
                or ".." in relative.parts
                or not relative.parts
                or relative.parts[0] != "package"
                or "\\" in member.name
            ):
                raise SmokeError(f"unsafe npm archive path: {member.name}")
            if member.issym() or member.islnk() or not (member.isdir() or member.isfile()):
                raise SmokeError(f"unsupported npm archive member: {member.name}")
            if member.name in seen_names:
                raise SmokeError(f"duplicate npm archive member: {member.name}")
            seen_names.add(member.name)

            target = destination.joinpath(*relative.parts)
            resolved_target = target.resolve()
            if not resolved_target.is_relative_to(destination_root):
                raise SmokeError(f"npm archive path escapes destination: {member.name}")
            if target.is_symlink():
                raise SmokeError(f"destination contains a symlink: {member.name}")
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                target.chmod(member.mode & 0o777)
                continue

            target.parent.mkdir(parents=True, exist_ok=True)
            if target.exists() and target.is_symlink():
                raise SmokeError(f"destination contains a symlink: {member.name}")
            source = archive.extractfile(member)
            if source is None:
                raise SmokeError(f"npm archive member cannot be read: {member.name}")
            target.write_bytes(source.read())
            target.chmod(member.mode & 0o777)
    package_dir = destination / "package"
    if not package_dir.is_dir():
        raise SmokeError(f"npm archive has no package directory: {archive_path}")
    return package_dir


def run_command(command: list[str], cwd: Path) -> str:
    result = subprocess.run(command, cwd=cwd, capture_output=True, text=True)
    if result.returncode != 0:
        raise SmokeError(
            f"command failed ({result.returncode}): {' '.join(command)}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result.stdout


def smoke(root_archive: Path, platform_archive: Path, node_bin: str) -> None:
    with tempfile.TemporaryDirectory(prefix="golutra-npm-smoke-") as temp_dir:
        root_dir = extract_package(root_archive.resolve(), Path(temp_dir) / "root")
        platform_dir = extract_package(
            platform_archive.resolve(), Path(temp_dir) / "platform"
        )
        root_package = json.loads((root_dir / "package.json").read_text(encoding="utf-8"))
        platform_package = json.loads(
            (platform_dir / "package.json").read_text(encoding="utf-8")
        )
        package_name = platform_package["name"]
        if package_name not in root_package.get("optionalDependencies", {}):
            raise SmokeError(f"root package does not declare {package_name}")

        dependency_dir = root_dir / "node_modules" / "@golutra" / package_name.rsplit("/", 1)[1]
        dependency_dir.parent.mkdir(parents=True, exist_ok=True)
        shutil.copytree(platform_dir, dependency_dir)

        version = root_package["version"]
        for entrypoint in ("golutra.js", "golutra-tui.js"):
            output = run_command(
                [node_bin, str(root_dir / "bin" / entrypoint), "--version"],
                root_dir,
            )
            expected_name = entrypoint.removesuffix(".js")
            if output.strip() != f"{expected_name} {version}":
                raise SmokeError(
                    f"unexpected version output for {entrypoint}: {output!r}"
                )

        run_command([node_bin, str(root_dir / "bin" / "golutra.js"), "--help"], root_dir)
        run_command(
            [node_bin, str(root_dir / "bin" / "golutra-tui.js"), "--help"], root_dir
        )


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root-tarball", type=Path, required=True)
    parser.add_argument("--platform-tarball", type=Path, required=True)
    parser.add_argument("--node-bin", default="node")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        smoke(args.root_tarball, args.platform_tarball, args.node_bin)
    except (OSError, subprocess.SubprocessError, SmokeError, tarfile.TarError, ValueError) as error:
        print(f"npm package smoke test failed: {error}", file=sys.stderr)
        return 1
    print("npm package smoke test passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
