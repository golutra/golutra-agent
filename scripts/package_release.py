#!/usr/bin/env python3
from __future__ import annotations

import argparse
import gzip
import hashlib
import hmac
import io
import json
import os
import string
import subprocess
import sys
import tarfile
import time
import tomllib
import zipfile
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath
from typing import Any


SCHEMA_VERSION = 2
PACKAGE_NAME = "golutra-agent"
BINARY_SPECS = (
    ("golutra-cli", "golutra"),
    ("golutra-tui", "golutra-tui"),
    ("golutra-app-server", "golutra-app-server"),
    ("golutra-vis", "golutra-vis"),
    ("golutra-supervisor", "golutra-supervisor"),
    ("golutra-launcher", "golutra-launcher"),
    ("golutra-eval-worker", "golutra-eval-worker"),
)
LEGAL_FILE_SPECS = (
    ("LICENSE", "LICENSE"),
    ("NOTICE", "NOTICE"),
)
BUILD_PACKAGES = (
    "golutra-cli",
    "golutra-tui",
    "golutra-app-server",
    "golutra-vis",
    "golutra-supervisor",
    "golutra-release",
    "golutra-eval-worker",
)


class PackageError(RuntimeError):
    pass


@dataclass(frozen=True)
class PackageResult:
    archive: Path
    manifest: Path
    checksum: Path


def repository_root() -> Path:
    return Path(__file__).resolve().parents[1]


def workspace_version(root: Path) -> str:
    with (root / "Cargo.toml").open("rb") as source:
        value = tomllib.load(source)
    version = value.get("workspace", {}).get("package", {}).get("version")
    if not isinstance(version, str) or not version:
        raise PackageError("workspace.package.version is missing from Cargo.toml")
    return version


def host_target(root: Path) -> str:
    output = subprocess.run(
        ["rustc", "-vV"], cwd=root, check=True, capture_output=True, text=True
    ).stdout
    for line in output.splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ").strip()
    raise PackageError("rustc did not report a host target")


def git_identity(root: Path) -> tuple[str, int, bool]:
    try:
        commit = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=root,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        timestamp = int(
            subprocess.run(
                ["git", "log", "-1", "--format=%ct"],
                cwd=root,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
        )
        dirty = bool(
            subprocess.run(
                ["git", "status", "--porcelain", "--untracked-files=all"],
                cwd=root,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
        )
        return commit, timestamp, dirty
    except (OSError, subprocess.CalledProcessError, ValueError):
        return "unknown", int(time.time()), True


def build_binaries(root: Path, target: str, profile: str) -> None:
    command = ["cargo", "build", "--locked", "--profile", profile, "--target", target]
    for package in BUILD_PACKAGES:
        command.extend(("-p", package))
    subprocess.run(command, cwd=root, check=True)


def package_release(
    *,
    root: Path,
    output_dir: Path,
    version: str,
    target: str,
    profile: str = "release",
    skip_build: bool = False,
    binary_dir: Path | None = None,
    source_date_epoch: int | None = None,
) -> PackageResult:
    if not version or any(character.isspace() for character in version):
        raise PackageError("version must be non-empty and contain no whitespace")
    if not target or any(character.isspace() for character in target):
        raise PackageError("target must be non-empty and contain no whitespace")
    if profile not in {"release", "dev"}:
        raise PackageError("profile must be release or dev")
    if not skip_build:
        build_binaries(root, target, profile)
    profile_dir = "release" if profile == "release" else "debug"
    source_dir = binary_dir or root / "target" / target / profile_dir
    if not source_dir.is_absolute():
        source_dir = (root / source_dir).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    commit, commit_epoch, git_dirty = git_identity(root)
    epoch = source_date_epoch
    if epoch is None:
        configured_epoch = os.environ.get("SOURCE_DATE_EPOCH")
        epoch = int(configured_epoch) if configured_epoch else commit_epoch
    epoch = max(0, epoch)
    created_at = datetime.fromtimestamp(epoch, timezone.utc).isoformat().replace("+00:00", "Z")
    windows = "windows" in target
    executable_suffix = ".exe" if windows else ""
    package_root = f"{PACKAGE_NAME}-v{version}-{target}"

    files: list[dict[str, Any]] = []
    contents: dict[str, bytes] = {}
    modes: dict[str, int] = {}
    for source_name, installed_name in BINARY_SPECS:
        source = source_dir / f"{source_name}{executable_suffix}"
        if source.is_symlink() or not source.is_file():
            raise PackageError(f"required release binary is missing or unsafe: {source}")
        content = source.read_bytes()
        if not content:
            raise PackageError(f"release binary is empty: {source}")
        destination = f"bin/{installed_name}{executable_suffix}"
        contents[destination] = content
        modes[destination] = 0o755
        files.append(
            {
                "path": destination,
                "kind": "binary",
                "size": len(content),
                "sha256": hashlib.sha256(content).hexdigest(),
                "mode": "0755",
                "source_binary": source.name,
            }
        )

    for source_name, destination in LEGAL_FILE_SPECS:
        source = root / source_name
        if source.is_symlink() or not source.is_file():
            raise PackageError(f"required legal file is missing or unsafe: {source}")
        content = source.read_bytes()
        if not content:
            raise PackageError(f"required legal file is empty: {source}")
        contents[destination] = content
        modes[destination] = 0o644
        files.append(
            {
                "path": destination,
                "kind": "document",
                "size": len(content),
                "sha256": hashlib.sha256(content).hexdigest(),
                "mode": "0644",
                "source_file": source.name,
            }
        )

    manifest_value = {
        "schema_version": SCHEMA_VERSION,
        "package": PACKAGE_NAME,
        "version": version,
        "target": target,
        "profile": profile,
        "git_commit": commit,
        "git_dirty": git_dirty,
        "created_at": created_at,
        "package_root": package_root,
        "files": files,
    }
    manifest_bytes = (
        json.dumps(manifest_value, indent=2, sort_keys=True, ensure_ascii=True).encode("utf-8")
        + b"\n"
    )
    archive_suffix = ".zip" if windows else ".tar.gz"
    archive = output_dir / f"{package_root}{archive_suffix}"
    external_manifest = Path(f"{archive}.manifest.json")
    checksum = Path(f"{archive}.sha256")

    if windows:
        _write_zip(archive, package_root, contents, modes, manifest_bytes, epoch)
    else:
        _write_tar_gz(archive, package_root, contents, modes, manifest_bytes, epoch)
    external_manifest.write_bytes(manifest_bytes)
    archive_digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    checksum.write_text(f"{archive_digest}  {archive.name}\n", encoding="ascii")
    verify_package(archive)
    return PackageResult(archive, external_manifest, checksum)


def verify_package(archive: Path) -> dict[str, Any]:
    archive = archive.resolve()
    checksum_path = Path(f"{archive}.sha256")
    external_manifest_path = Path(f"{archive}.manifest.json")
    if not archive.is_file() or archive.is_symlink():
        raise PackageError(f"archive is missing or unsafe: {archive}")
    if not checksum_path.is_file() or checksum_path.is_symlink():
        raise PackageError(f"checksum sidecar is missing or unsafe: {checksum_path}")
    checksum_parts = checksum_path.read_text(encoding="ascii").strip().split()
    if (
        len(checksum_parts) != 2
        or checksum_parts[1] != archive.name
        or not _is_sha256(checksum_parts[0])
    ):
        raise PackageError("checksum sidecar has an invalid format or filename")
    actual_archive_digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    if not _constant_time_equal(checksum_parts[0], actual_archive_digest):
        raise PackageError("archive SHA-256 does not match its sidecar")

    entries, modes = _read_archive(archive)
    manifest_entries = [path for path in entries if path.endswith("/manifest.json")]
    if len(manifest_entries) != 1:
        raise PackageError("archive must contain exactly one top-level manifest")
    manifest_path = manifest_entries[0]
    try:
        manifest = json.loads(entries[manifest_path])
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise PackageError(f"archive manifest is invalid: {error}") from error
    if not isinstance(manifest, dict):
        raise PackageError("archive manifest must be a JSON object")
    if manifest.get("schema_version") != SCHEMA_VERSION:
        raise PackageError("archive manifest schema is unsupported")
    if manifest.get("package") != PACKAGE_NAME:
        raise PackageError("archive manifest package is invalid")
    package_root = manifest.get("package_root")
    if not isinstance(package_root, str) or manifest_path != f"{package_root}/manifest.json":
        raise PackageError("archive root does not match manifest package_root")
    version = manifest.get("version")
    target = manifest.get("target")
    if (
        not isinstance(version, str)
        or not isinstance(target, str)
        or package_root != f"{PACKAGE_NAME}-v{version}-{target}"
    ):
        raise PackageError("archive root does not match manifest version and target")
    expected_paths = {manifest_path}
    files = manifest.get("files")
    if not isinstance(files, list) or not files:
        raise PackageError("archive manifest contains no files")
    executable_suffix = ".exe" if "windows" in target else ""
    expected_binary_paths = {
        f"bin/{installed_name}{executable_suffix}"
        for _source_name, installed_name in BINARY_SPECS
    }
    expected_legal_paths = {destination for _source_name, destination in LEGAL_FILE_SPECS}
    expected_relative_paths = expected_binary_paths | expected_legal_paths
    declared_relative_paths: set[str] = set()
    for record in files:
        if not isinstance(record, dict) or not isinstance(record.get("path"), str):
            raise PackageError("archive manifest contains an invalid file record")
        relative_path = _safe_relative_path(record["path"])
        if relative_path in declared_relative_paths:
            raise PackageError(f"manifest contains a duplicate file: {relative_path}")
        declared_relative_paths.add(relative_path)
        archive_path = f"{package_root}/{relative_path}"
        expected_paths.add(archive_path)
        content = entries.get(archive_path)
        if content is None:
            raise PackageError(f"manifest file is missing from archive: {relative_path}")
        if record.get("size") != len(content):
            raise PackageError(f"manifest size mismatch: {relative_path}")
        digest = hashlib.sha256(content).hexdigest()
        if (
            not isinstance(record.get("sha256"), str)
            or not _is_sha256(record["sha256"])
            or not _constant_time_equal(record["sha256"], digest)
        ):
            raise PackageError(f"manifest SHA-256 mismatch: {relative_path}")
        expected_kind = "binary" if relative_path in expected_binary_paths else "document"
        expected_mode = "0755" if expected_kind == "binary" else "0644"
        if record.get("kind") != expected_kind:
            raise PackageError(f"manifest kind is invalid: {relative_path}")
        if record.get("mode") != expected_mode:
            raise PackageError(f"manifest mode is invalid: {relative_path}")
        archive_mode = modes.get(archive_path, 0)
        if archive_mode & 0o777 != int(expected_mode, 8):
            raise PackageError(f"packaged {expected_kind} mode is invalid: {relative_path}")
    if declared_relative_paths != expected_relative_paths:
        raise PackageError("archive manifest does not contain the required file set")
    unexpected = set(entries) - expected_paths
    if unexpected:
        raise PackageError(f"archive contains unmanifested files: {sorted(unexpected)}")
    if not external_manifest_path.is_file() or external_manifest_path.is_symlink():
        raise PackageError("external manifest is missing or unsafe")
    if external_manifest_path.read_bytes() != entries[manifest_path]:
        raise PackageError("external manifest differs from archive manifest")
    return manifest


def _write_tar_gz(
    archive: Path,
    package_root: str,
    contents: dict[str, bytes],
    modes: dict[str, int],
    manifest: bytes,
    epoch: int,
) -> None:
    with archive.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as output:
                _add_tar_directory(output, package_root, epoch)
                _add_tar_directory(output, f"{package_root}/bin", epoch)
                for relative_path, content in sorted(contents.items()):
                    _add_tar_file(
                        output,
                        f"{package_root}/{relative_path}",
                        content,
                        modes[relative_path],
                        epoch,
                    )
                _add_tar_file(
                    output, f"{package_root}/manifest.json", manifest, 0o644, epoch
                )


def _add_tar_directory(output: tarfile.TarFile, path: str, epoch: int) -> None:
    record = tarfile.TarInfo(path)
    record.type = tarfile.DIRTYPE
    record.mode = 0o755
    record.mtime = epoch
    record.uid = 0
    record.gid = 0
    record.uname = ""
    record.gname = ""
    output.addfile(record)


def _add_tar_file(
    output: tarfile.TarFile, path: str, content: bytes, mode: int, epoch: int
) -> None:
    record = tarfile.TarInfo(path)
    record.size = len(content)
    record.mode = mode
    record.mtime = epoch
    record.uid = 0
    record.gid = 0
    record.uname = ""
    record.gname = ""
    output.addfile(record, io.BytesIO(content))


def _write_zip(
    archive: Path,
    package_root: str,
    contents: dict[str, bytes],
    modes: dict[str, int],
    manifest: bytes,
    epoch: int,
) -> None:
    zip_epoch = max(epoch, 315532800)
    date_time = time.gmtime(zip_epoch)[:6]
    with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as output:
        for relative_path, content in sorted(contents.items()):
            _write_zip_file(
                output,
                f"{package_root}/{relative_path}",
                content,
                modes[relative_path],
                date_time,
            )
        _write_zip_file(
            output, f"{package_root}/manifest.json", manifest, 0o644, date_time
        )


def _write_zip_file(
    output: zipfile.ZipFile,
    path: str,
    content: bytes,
    mode: int,
    date_time: tuple[int, int, int, int, int, int],
) -> None:
    record = zipfile.ZipInfo(path, date_time=date_time)
    record.create_system = 3
    record.external_attr = (stat_mode(mode) & 0xFFFF) << 16
    record.compress_type = zipfile.ZIP_DEFLATED
    output.writestr(record, content, compresslevel=9)


def stat_mode(mode: int) -> int:
    return 0o100000 | mode


def _read_archive(archive: Path) -> tuple[dict[str, bytes], dict[str, int]]:
    entries: dict[str, bytes] = {}
    modes: dict[str, int] = {}
    if archive.name.endswith(".tar.gz"):
        with tarfile.open(archive, "r:gz") as source:
            for member in source.getmembers():
                _safe_archive_path(member.name)
                if member.issym() or member.islnk():
                    raise PackageError(f"archive link is forbidden: {member.name}")
                if member.isdir():
                    continue
                if not member.isfile():
                    raise PackageError(f"archive special file is forbidden: {member.name}")
                if member.name in entries:
                    raise PackageError(f"archive contains a duplicate member: {member.name}")
                extracted = source.extractfile(member)
                if extracted is None:
                    raise PackageError(f"archive member cannot be read: {member.name}")
                entries[member.name] = extracted.read()
                modes[member.name] = member.mode
    elif archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as source:
            for member in source.infolist():
                _safe_archive_path(member.filename)
                mode = (member.external_attr >> 16) & 0xFFFF
                if mode & 0o170000 == 0o120000:
                    raise PackageError(f"archive link is forbidden: {member.filename}")
                if member.is_dir():
                    continue
                if member.filename in entries:
                    raise PackageError(
                        f"archive contains a duplicate member: {member.filename}"
                    )
                entries[member.filename] = source.read(member)
                modes[member.filename] = mode
    else:
        raise PackageError("archive must end in .tar.gz or .zip")
    return entries, modes


def _safe_archive_path(path: str) -> str:
    value = PurePosixPath(path)
    if value.is_absolute() or ".." in value.parts or not value.parts:
        raise PackageError(f"archive path is unsafe: {path}")
    return value.as_posix()


def _safe_relative_path(path: str) -> str:
    normalized = _safe_archive_path(path)
    if normalized.startswith("/") or normalized == "manifest.json":
        raise PackageError(f"manifest path is unsafe: {path}")
    return normalized


def _constant_time_equal(left: str, right: str) -> bool:
    return hmac.compare_digest(left, right)


def _is_sha256(value: str) -> bool:
    return len(value) == 64 and all(character in string.hexdigits for character in value)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build, package, and verify Golutra Agent release archives"
    )
    parser.add_argument("--output-dir", type=Path, default=Path("dist"))
    parser.add_argument("--version")
    parser.add_argument("--target")
    parser.add_argument("--profile", choices=("release", "dev"), default="release")
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--binary-dir", type=Path)
    parser.add_argument("--source-date-epoch", type=int)
    parser.add_argument("--verify", type=Path, metavar="ARCHIVE")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    root = repository_root()
    try:
        if args.verify is not None:
            manifest = verify_package(args.verify)
            print(
                json.dumps(
                    {
                        "verified": str(args.verify.resolve()),
                        "version": manifest["version"],
                        "target": manifest["target"],
                    },
                    sort_keys=True,
                )
            )
            return 0
        if args.binary_dir is not None and not args.skip_build:
            raise PackageError("--binary-dir requires --skip-build")
        target = args.target or host_target(root)
        version = args.version or workspace_version(root)
        if os.environ.get("GITHUB_REF_TYPE") == "tag":
            release_tag = os.environ.get("GITHUB_REF_NAME", "")
            if release_tag != f"v{version}":
                raise PackageError(
                    f"release tag {release_tag!r} does not match workspace version v{version}"
                )
        result = package_release(
            root=root,
            output_dir=args.output_dir.resolve(),
            version=version,
            target=target,
            profile=args.profile,
            skip_build=args.skip_build,
            binary_dir=args.binary_dir,
            source_date_epoch=args.source_date_epoch,
        )
        print(
            json.dumps(
                {
                    "archive": str(result.archive),
                    "manifest": str(result.manifest),
                    "checksum": str(result.checksum),
                },
                sort_keys=True,
            )
        )
        return 0
    except (OSError, subprocess.CalledProcessError, PackageError, ValueError) as error:
        print(f"package release failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
